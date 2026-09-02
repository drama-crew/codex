use crate::runtime::MemoryStartupContext;
use crate::runtime::SpawnedConsolidationAgent;
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
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use tracing::warn;

/// Session-source tag for the standalone memory worker's dedicated,
/// non-interactive orchestration thread.
///
/// This name is deliberately absent from
/// `codex_rollout::INTERACTIVE_SESSION_SOURCES`, so phase 1's startup-job
/// claim allowlist can never select rollouts produced by the worker's own
/// thread, by construction. As a second, independent line of defense,
/// phase 1's claim query also excludes the claiming thread's own id --
/// which self-excludes the worker thread automatically once it becomes the
/// one doing the claiming, with no extra logic required here.
pub const MEMORY_WORKER_SESSION_SOURCE_NAME: &str = "drama-memory-worker";

/// Aggregate outcome of one [`run_memories_pipeline`] run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipelineReport {
    /// Number of stage-1 (phase 1) jobs claimed during this run.
    pub phase1_claimed: usize,
    /// Number of claimed stage-1 jobs that finished successfully (with or
    /// without producing output).
    pub phase1_succeeded: usize,
    /// Whether this run held the global phase-2 job lock and attempted a
    /// consolidation pass.
    pub phase2_ran: bool,
    /// Whether phase 2 consolidation actually changed the committed memory
    /// content (`MEMORY.md` / `memory_summary.md`).
    pub phase2_changed: bool,
}

/// Runs the full memories pipeline (root layout, extension seeding, phase 1
/// extraction, and phase 2 consolidation) to completion on a dedicated,
/// non-interactive orchestration thread, and returns aggregate
/// [`PipelineReport`] stats.
///
/// Unlike [`crate::start_memories_startup_task`] (which spawns the pipeline
/// fire-and-forget from an existing interactive session and never awaits
/// it), this entry point is meant for a standalone memory-worker process: it
/// builds and owns its own throwaway orchestration thread (tagged
/// [`SessionSource::Custom`] with [`MEMORY_WORKER_SESSION_SOURCE_NAME`]),
/// awaits the pipeline through phase 1, phase 2 agent completion, workspace
/// baseline reset, and DB persistence, and shuts that thread down before
/// returning -- so a long-lived worker process does not leak registered
/// threads across repeated runs.
///
/// Gating mirrors [`crate::start_memories_startup_task`]: an ephemeral
/// config, a disabled memory-tool feature flag, disabled
/// `memories.generate_memories`, or a missing state DB all result in a
/// zero-value [`PipelineReport`] rather than a panic or error.
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

    let environments = thread_manager
        .default_environment_selections(&config.cwd, config.permissions.workspace_roots());
    let NewThread {
        thread_id, thread, ..
    } = thread_manager
        .start_thread(StartThreadOptions {
            session_source: Some(SessionSource::Custom(
                MEMORY_WORKER_SESSION_SOURCE_NAME.to_string(),
            )),
            environments: Some(environments),
            ..StartThreadOptions::new(config.as_ref().clone())
        })
        .await?;

    // Keep a second, independent handle to the context alive across the
    // `run_pipeline_inner` call below, which moves and fully consumes its
    // own `Arc<MemoryStartupContext>` by the time phase 2 completes. This
    // clone survives that call so the worker's own top-level orchestration
    // thread can still be shut down afterward.
    let context = Arc::new(MemoryStartupContext::new(
        Arc::clone(&thread_manager),
        Arc::clone(&auth_manager),
        thread_id,
        Arc::clone(&thread),
        config.as_ref(),
        SessionSource::Custom(MEMORY_WORKER_SESSION_SOURCE_NAME.to_string()),
    ));
    let context_for_shutdown = Arc::clone(&context);

    if context.state_db().is_none() {
        warn!("state db unavailable for memories pipeline worker; skipping");
        shutdown_worker_thread(&context_for_shutdown, thread_id, thread).await;
        return Ok(PipelineReport::default());
    }

    let parent_permission_profile = config.permissions.effective_permission_profile();

    let report = run_pipeline_inner(PipelineCtx {
        context,
        auth_manager,
        config,
        parent_permission_profile,
    })
    .await;

    shutdown_worker_thread(&context_for_shutdown, thread_id, thread).await;

    Ok(report)
}

async fn shutdown_worker_thread(
    context: &MemoryStartupContext,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
) {
    if let Err(err) = context
        .shutdown_consolidation_agent(SpawnedConsolidationAgent { thread_id, thread })
        .await
    {
        warn!("failed to shut down memory pipeline worker thread {thread_id}: {err}");
    }
}

#[cfg(test)]
#[path = "pipeline_run_tests.rs"]
mod tests;
