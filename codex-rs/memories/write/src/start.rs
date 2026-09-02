use crate::ensure_layout;
use crate::extensions::seed_extension_instructions;
use crate::guard;
use crate::memory_root;
use crate::metrics::MEMORY_STARTUP;
use crate::phase1;
use crate::phase2;
use crate::runtime::MemoryStartupContext;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use tracing::warn;

/// Owned inputs threaded through the awaitable startup pipeline body.
///
/// This is shared by both the fire-and-forget session-startup task
/// (`start_memories_startup_task`, which spawns a call to
/// [`run_pipeline_inner`] and does not await it) and the standalone,
/// awaitable `run_memories_pipeline` entry point (in `pipeline_run.rs`),
/// which awaits it directly so a worker process can observe completion and
/// aggregate [`crate::PipelineReport`] stats.
pub(crate) struct PipelineCtx {
    pub(crate) context: Arc<MemoryStartupContext>,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) config: Arc<Config>,
    pub(crate) parent_permission_profile: PermissionProfile,
}

/// Runs the startup memory pipeline body (root layout, extension seeding,
/// phase 1 pruning, rate-limit guard, phase 1 extraction, phase 2
/// consolidation) to completion and returns aggregate [`crate::PipelineReport`]
/// stats.
///
/// Callers that only care about fire-and-forget semantics (the session
/// startup task) can spawn this without awaiting it; callers that need to
/// observe completion (the standalone memory worker) can await it directly.
pub(crate) async fn run_pipeline_inner(ctx: PipelineCtx) -> crate::PipelineReport {
    let PipelineCtx {
        context,
        auth_manager,
        config,
        parent_permission_profile,
    } = ctx;

    let root = memory_root(&config.codex_home);
    if let Err(err) = ensure_layout(&root).await {
        warn!("failed preparing memories root: {err}");
        return crate::PipelineReport::default();
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
        return crate::PipelineReport::default();
    }

    // Run phase 1.
    let phase1_stats = phase1::run(Arc::clone(&context), Arc::clone(&config)).await;
    // Run phase 2.
    let phase2::Phase2Report { ran, changed } =
        phase2::run(context, config, parent_permission_profile).await;

    crate::PipelineReport {
        phase1_claimed: phase1_stats.claimed,
        phase1_succeeded: phase1_stats.succeeded_with_output + phase1_stats.succeeded_no_output,
        phase2_ran: ran,
        phase2_changed: changed,
    }
}

/// Starts the asynchronous startup memory pipeline for an eligible root session.
///
/// The pipeline is skipped for ephemeral sessions, disabled feature flags, and
/// subagent sessions.
pub fn start_memories_startup_task(
    thread_manager: Arc<ThreadManager>,
    auth_manager: Arc<AuthManager>,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    config: Arc<Config>,
    parent_permission_profile: PermissionProfile,
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
            parent_permission_profile,
        })
        .await;
    });
}
