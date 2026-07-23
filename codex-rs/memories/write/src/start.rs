use crate::extensions::seed_extension_instructions;
use crate::guard;
use crate::memory_root;
use crate::metrics::MEMORY_STARTUP;
use crate::phase1;
use crate::phase2;
use crate::phase2::Phase2Report;
use crate::pipeline_run::PipelineReport;
use crate::runtime::MemoryStartupContext;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use tracing::warn;

/// Shared context for a single pipeline run, used by both the fire-and-forget session-startup
/// entry point (`start_memories_startup_task`) and the public, awaitable
/// `run_memories_pipeline` entry point.
pub(crate) struct PipelineCtx {
    pub(crate) context: Arc<MemoryStartupContext>,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) config: Arc<Config>,
}

/// Starts the asynchronous startup memory pipeline for an eligible root session.
///
/// The pipeline is skipped for ephemeral sessions, disabled feature flags, and
/// subagent sessions. This spawns a call to [`run_pipeline_inner`] and does not await it: this
/// entry point is used from live interactive sessions, which must not block on the memory
/// pipeline.
pub fn start_memories_startup_task(
    thread_manager: Arc<ThreadManager>,
    auth_manager: Arc<AuthManager>,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    config: Arc<Config>,
    source: &SessionSource,
) {
    if config.ephemeral
        || !config.features.enabled(Feature::MemoryTool)
        || source.is_non_root_agent()
    {
        return;
    }

    let context = Arc::new(MemoryStartupContext::new(
        thread_manager,
        Arc::clone(&auth_manager),
        thread_id,
        thread,
        config.as_ref(),
        source.clone(),
    ));

    if context.state_db().is_none() {
        warn!("state db unavailable for memories startup pipeline; skipping");
        return;
    }

    tokio::spawn(async move {
        run_pipeline_inner(PipelineCtx {
            context,
            auth_manager,
            config,
        })
        .await;
    });
}

/// Runs the memories pipeline (extension seeding, pruning, rate-limit guard, Phase 1, Phase 2) to
/// completion and reports what happened.
///
/// This is the single shared implementation behind both `start_memories_startup_task` (which
/// spawns a call to this function and never awaits it, preserving that entry point's
/// non-blocking contract for live interactive sessions) and the public
/// [`crate::run_memories_pipeline`] (which awaits this function directly, so external callers --
/// e.g. a standalone memory-worker process with no long-lived session for an orphaned task to
/// leak into -- observe true completion of Phase 2 consolidation rather than a fire-and-forget
/// hand-off).
pub(crate) async fn run_pipeline_inner(ctx: PipelineCtx) -> PipelineReport {
    let PipelineCtx {
        context,
        auth_manager,
        config,
    } = ctx;

    let root = memory_root(&config.codex_home);
    if let Err(err) = tokio::fs::create_dir_all(&root).await {
        warn!("failed creating memories root: {err}");
        return PipelineReport::default();
    }
    if let Err(err) = seed_extension_instructions(&root).await {
        warn!("failed seeding memory extension instructions: {err}");
    }

    // Clean memories to make preserve DB size. This does not consume tokens so can be
    // done before the quota check.
    phase1::prune(context.as_ref(), &config).await;

    if !guard::rate_limits_ok(&auth_manager, &config).await {
        context.counter(
            MEMORY_STARTUP,
            /*inc*/ 1,
            &[("status", "skipped_rate_limit")],
        );
        return PipelineReport::default();
    }

    // Run phase 1.
    let phase1_stats = phase1::run(Arc::clone(&context), Arc::clone(&config)).await;
    // Run phase 2.
    let Phase2Report { ran, changed } = phase2::run(context, config).await;

    PipelineReport {
        phase1_claimed: phase1_stats.claimed,
        phase1_succeeded: phase1_stats.succeeded_with_output + phase1_stats.succeeded_no_output,
        phase2_ran: ran,
        phase2_changed: changed,
    }
}
