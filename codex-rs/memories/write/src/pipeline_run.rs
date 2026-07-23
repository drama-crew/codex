use crate::runtime::MemoryStartupContext;
use crate::start::PipelineCtx;
use crate::start::run_pipeline_inner;
use codex_core::CodexThread;
use codex_core::NewThread;
use codex_core::StartThreadOptions;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use std::time::Duration;

/// Session source used to tag the internal orchestration thread created by
/// [`run_memories_pipeline`] for itself.
///
/// This is intentionally *not* a member of [`codex_rollout::INTERACTIVE_SESSION_SOURCES`] (the
/// allowlist consulted by the Phase 1 claim query, see `phase1::claim_startup_jobs`). That keeps
/// the pipeline's own orchestration thread from ever being claimed by Phase 1 as a rollout to
/// extract memories from -- i.e. the memory pipeline can never pollute the memory store with a
/// summary of its own bookkeeping. (The Phase 2 consolidation sub-agent thread is separately
/// excluded via `SessionSource::Internal(InternalSessionSource::MemoryConsolidation)`, set up in
/// `MemoryStartupContext::spawn_consolidation_agent`.)
const MEMORY_WORKER_SESSION_SOURCE_NAME: &str = "drama-memory-worker";

/// Statistics reported by one run of [`run_memories_pipeline`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipelineReport {
    /// Number of stage-1 (per-rollout) extraction jobs claimed by this run.
    pub phase1_claimed: usize,
    /// Number of claimed stage-1 jobs that completed without failure (with or without output).
    pub phase1_succeeded: usize,
    /// Whether this run held the global Phase 2 (consolidation) job lock and attempted a
    /// consolidation pass. `false` if another worker already held the lock (or any other claim
    /// failure), or if this run was skipped entirely (e.g. by a feature gate).
    pub phase2_ran: bool,
    /// Whether Phase 2 consolidation actually changed the committed memory content
    /// (`MEMORY.md` / `memory_summary.md`), as opposed to merely having new inputs to consider.
    pub phase2_changed: bool,
}

/// Builds the internal, non-interactive orchestration thread and [`MemoryStartupContext`] used by
/// [`run_memories_pipeline`].
///
/// The thread is tagged with `SessionSource::Custom(`[`MEMORY_WORKER_SESSION_SOURCE_NAME`]`)`,
/// which is deliberately excluded from the Phase 1 claim allowlist -- see the module docs above.
///
/// Returns the thread id and thread handle alongside the context so the caller can shut the
/// thread down again once the pipeline run has finished (see [`shutdown_worker_thread`]): unlike
/// `start_memories_startup_task`, which reuses the caller's already-live session thread and
/// leaves its lifecycle to the surrounding session, this creates a brand-new thread purely for
/// its own bookkeeping that nothing else will ever tear down.
async fn build_worker_context(
    thread_manager: &Arc<ThreadManager>,
    auth_manager: &Arc<AuthManager>,
    config: &Arc<Config>,
) -> anyhow::Result<(Arc<MemoryStartupContext>, ThreadId, Arc<CodexThread>)> {
    let source = SessionSource::Custom(MEMORY_WORKER_SESSION_SOURCE_NAME.to_string());
    let environments = thread_manager.default_environment_selections(&config.cwd);
    let NewThread {
        thread_id, thread, ..
    } = thread_manager
        .start_thread_with_options(StartThreadOptions {
            config: config.as_ref().clone(),
            allow_provider_model_fallback: false,
            initial_history: InitialHistory::New,
            history_mode: None,
            session_source: Some(source.clone()),
            thread_source: None,
            dynamic_tools: Vec::new(),
            metrics_service_name: None,
            parent_trace: None,
            environments,
            thread_extension_init: Default::default(),
            supports_openai_form_elicitation: false,
        })
        .await?;

    let context = Arc::new(MemoryStartupContext::new(
        Arc::clone(thread_manager),
        Arc::clone(auth_manager),
        thread_id,
        Arc::clone(&thread),
        config.as_ref(),
        source,
    ));

    Ok((context, thread_id, thread))
}

/// Shuts down and unregisters the dedicated orchestration thread created by
/// [`build_worker_context`].
///
/// Without this, a worker process that invokes [`run_memories_pipeline`] repeatedly within a
/// single long-lived process would leak one registered, never-shut-down thread per call. Failures
/// here are logged but do not fail the overall pipeline run -- the useful work (Phase 1 / Phase 2)
/// has already completed by the time this runs.
async fn shutdown_worker_thread(
    thread_manager: &Arc<ThreadManager>,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
) {
    let thread = thread_manager
        .remove_thread(&thread_id)
        .await
        .unwrap_or(thread);
    match tokio::time::timeout(Duration::from_secs(10), thread.shutdown_and_wait()).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!("failed to shut down memory worker orchestration thread {thread_id}: {err}");
        }
        Err(_) => {
            tracing::warn!("memory worker orchestration thread {thread_id} shutdown timed out");
        }
    }
}

/// Runs the full memories pipeline (Phase 1 extraction + Phase 2 consolidation) to completion and
/// returns statistics about what happened.
///
/// Unlike [`crate::start_memories_startup_task`] (spawned, fire-and-forget, used from live
/// interactive sessions), this function is `async` and awaits the entire pipeline -- including
/// Phase 2 consolidation, baseline reset, and DB persistence -- before returning. It is intended
/// for a standalone, session-less memory-worker process that has no long-lived session for an
/// orphaned task to leak into: without this, a short-lived worker process exiting right after
/// dispatching consolidation would silently kill an in-flight consolidation run before it
/// finished.
///
/// Returns a zero-value [`PipelineReport`] (rather than an error) when the pipeline is gated off
/// by configuration -- an ephemeral config, the [`Feature::MemoryTool`] feature flag being
/// disabled, or `config.memories.generate_memories` being `false` -- or when the state DB is
/// unavailable. This mirrors the gating already performed by
/// [`crate::start_memories_startup_task`], plus the additional `generate_memories` check specific
/// to this entry point.
pub async fn run_memories_pipeline(
    thread_manager: Arc<ThreadManager>,
    auth_manager: Arc<AuthManager>,
    config: Arc<Config>,
) -> anyhow::Result<PipelineReport> {
    if config.ephemeral
        || !config.features.enabled(Feature::MemoryTool)
        || !config.memories.generate_memories
    {
        return Ok(PipelineReport::default());
    }

    let (context, worker_thread_id, worker_thread) =
        build_worker_context(&thread_manager, &auth_manager, &config).await?;

    if context.state_db().is_none() {
        tracing::warn!("state db unavailable for memories pipeline; skipping");
        shutdown_worker_thread(&thread_manager, worker_thread_id, worker_thread).await;
        return Ok(PipelineReport::default());
    }

    let report = run_pipeline_inner(PipelineCtx {
        context,
        auth_manager,
        config,
    })
    .await;

    shutdown_worker_thread(&thread_manager, worker_thread_id, worker_thread).await;

    Ok(report)
}

#[cfg(test)]
#[path = "pipeline_run_tests.rs"]
mod tests;
