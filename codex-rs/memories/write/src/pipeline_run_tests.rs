use super::MEMORY_WORKER_SESSION_SOURCE_NAME;
use super::PipelineReport;
use super::run_memories_pipeline;
use crate::memory_root;
use codex_config::types::MemoriesConfig;
use codex_core::config::Config;
use codex_features::Feature;
use codex_git_utils::diff_since_latest_init;
use codex_git_utils::reset_git_repository;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_rollout::INTERACTIVE_SESSION_SOURCES;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use codex_state::Phase2JobClaimOutcome;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

/// The memory worker's dedicated session source must never appear in phase
/// 1's interactive claim allowlist. This is the first, construction-time
/// half of the double exclusion described on [`run_memories_pipeline`]:
/// even before a worker thread exists, no rollout tagged with this source
/// can ever be claimed by phase 1's startup query.
#[test]
fn worker_session_source_is_excluded_from_phase1_claim_allowlist() {
    let worker_source = SessionSource::Custom(MEMORY_WORKER_SESSION_SOURCE_NAME.to_string());
    assert!(
        !INTERACTIVE_SESSION_SOURCES.contains(&worker_source),
        "the memory worker's dedicated session source must stay outside phase 1's \
         interactive claim allowlist so it can never claim its own rollouts"
    );
}

#[tokio::test]
async fn run_memories_pipeline_returns_zero_report_when_memory_tool_feature_disabled()
-> anyhow::Result<()> {
    let server = start_mock_server().await;
    let home = Arc::new(TempDir::new()?);
    let test = build_test_codex(&server, home).await?;
    // `Feature::MemoryTool` is intentionally left disabled here (`build_test_codex`
    // only enables `Feature::Sqlite`), mirroring `start_memories_startup_task`'s
    // gating.
    let config = Arc::new(test.config.clone());

    let report = run_memories_pipeline(
        Arc::clone(&test.thread_manager),
        test.thread_manager.auth_manager(),
        config,
    )
    .await?;

    assert_eq!(report, PipelineReport::default());

    shutdown_test_codex(&test).await?;
    Ok(())
}

#[tokio::test]
async fn run_memories_pipeline_returns_zero_report_when_generate_memories_disabled()
-> anyhow::Result<()> {
    let server = start_mock_server().await;
    let home = Arc::new(TempDir::new()?);
    let test = build_test_codex(&server, home).await?;
    let mut config = test.config.clone();
    config
        .features
        .enable(Feature::MemoryTool)
        .expect("test config should allow feature update");
    config.memories.generate_memories = false;
    let config = Arc::new(config);

    let report = run_memories_pipeline(
        Arc::clone(&test.thread_manager),
        test.thread_manager.auth_manager(),
        config,
    )
    .await?;

    assert_eq!(report, PipelineReport::default());

    shutdown_test_codex(&test).await?;
    Ok(())
}

#[tokio::test]
async fn phase1_never_claims_a_rollout_tagged_with_the_memory_worker_session_source()
-> anyhow::Result<()> {
    let server = start_mock_server().await;
    let home = Arc::new(TempDir::new()?);
    let db = init_state_db(&home).await?;

    // A candidate rollout that would otherwise be eligible for phase-1
    // claiming, except that it is tagged with the memory worker's own
    // dedicated session source -- the same source `run_memories_pipeline`
    // uses for the orchestration thread it builds below.
    seed_stage1_candidate(
        db.as_ref(),
        home.path(),
        chrono::Utc::now() - chrono::Duration::hours(2),
        "worker-owned",
        SessionSource::Custom(MEMORY_WORKER_SESSION_SOURCE_NAME.to_string()),
    )
    .await?;

    let test = build_test_codex(&server, Arc::clone(&home)).await?;
    let config = memory_tool_enabled_config(&test);

    let report = run_memories_pipeline(
        Arc::clone(&test.thread_manager),
        test.thread_manager.auth_manager(),
        config,
    )
    .await?;

    assert_eq!(
        report.phase1_claimed, 0,
        "phase 1 must never claim a rollout produced by the memory worker's own \
         session source"
    );

    shutdown_test_codex(&test).await?;
    Ok(())
}

/// The central guarantee of [`run_memories_pipeline`]: the returned future
/// resolves only once phase 2 consolidation has genuinely finished -- the
/// HTTP round trip completed, the workspace baseline was reset, and the
/// job outcome was persisted to the DB -- not merely started. All
/// assertions below run immediately after the single `.await`, with no
/// polling or retrying, to prove that.
#[tokio::test]
async fn run_memories_pipeline_awaits_phase2_to_completion_and_reports_stats()
-> anyhow::Result<()> {
    let server = start_mock_server().await;
    let home = Arc::new(TempDir::new()?);
    let db = init_state_db(&home).await?;
    let root = memory_root(&home.path().abs());

    seed_stage1_output(
        db.as_ref(),
        home.path(),
        chrono::Utc::now(),
        "raw memory",
        "rollout summary",
        "pipeline-worker",
    )
    .await?;
    seed_required_memory_artifacts(&root).await?;
    reset_git_repository(&root).await?;

    let phase2 = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-phase2-worker"),
            ev_assistant_message("msg-phase2-worker", "phase2 complete"),
            ev_completed("resp-phase2-worker"),
        ]),
    )
    .await;

    let test = build_test_codex(&server, Arc::clone(&home)).await?;
    let config = memory_tool_enabled_config(&test);

    let threads_before = test.thread_manager.list_thread_ids().await.len();

    let report = run_memories_pipeline(
        Arc::clone(&test.thread_manager),
        test.thread_manager.auth_manager(),
        config,
    )
    .await?;

    // Exactly one phase-2 request must have already happened by now -- no
    // `wait_for_*` helper is used here, unlike the fire-and-forget
    // `start_memories_startup_task` tests in `startup_tests.rs`, precisely
    // because this call is awaited to completion.
    phase2.single_request();

    assert_eq!(
        report,
        PipelineReport {
            phase1_claimed: 0,
            phase1_succeeded: 0,
            phase2_ran: true,
            // Derived from the before/after consolidation output snapshot,
            // not `workspace_diff.has_changes()`: the mocked consolidation
            // agent in this test never edits MEMORY.md / memory_summary.md,
            // so the pipeline must report no change even though phase 2 ran.
            phase2_changed: false,
        }
    );

    // The workspace baseline reset already happened -- checked directly,
    // with no retry loop.
    assert!(!tokio::fs::try_exists(root.join("phase2_workspace_diff.md")).await?);
    assert!(!diff_since_latest_init(&root).await?.has_changes());

    // The job outcome is already persisted: a fresh claim attempt sees the
    // just-finished cooldown, not a still-running job.
    assert_eq!(
        db.memories()
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 3_600)
            .await?,
        Phase2JobClaimOutcome::SkippedCooldown,
        "phase 2 job must already be finished and persisted, not merely started"
    );

    // The worker's dedicated orchestration thread must not have leaked.
    let threads_after = test.thread_manager.list_thread_ids().await.len();
    assert_eq!(
        threads_after, threads_before,
        "run_memories_pipeline must not leak its dedicated worker thread"
    );

    shutdown_test_codex(&test).await?;
    Ok(())
}

fn pipeline_test_memories_config() -> MemoriesConfig {
    MemoriesConfig {
        max_raw_memories_for_consolidation: 1,
        min_rollout_idle_hours: 0,
        ..MemoriesConfig::default()
    }
}

fn memory_tool_enabled_config(test: &TestCodex) -> Arc<Config> {
    let mut config = test.config.clone();
    config
        .features
        .enable(Feature::MemoryTool)
        .expect("test config should allow feature update");
    Arc::new(config)
}

async fn build_test_codex(
    server: &wiremock::MockServer,
    home: Arc<TempDir>,
) -> anyhow::Result<TestCodex> {
    test_codex()
        .with_home(home)
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Sqlite)
                .expect("test config should allow feature update");
            config.memories = pipeline_test_memories_config();
        })
        .build(server)
        .await
}

async fn init_state_db(home: &Arc<TempDir>) -> anyhow::Result<Arc<codex_state::StateRuntime>> {
    let db = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".into(),
    )
    .await?;
    db.mark_backfill_complete(/*last_watermark*/ None).await?;
    Ok(db)
}

async fn seed_stage1_output(
    db: &codex_state::StateRuntime,
    codex_home: &Path,
    updated_at: chrono::DateTime<chrono::Utc>,
    raw_memory: &str,
    rollout_summary: &str,
    rollout_slug: &str,
) -> anyhow::Result<ThreadId> {
    let thread_id = ThreadId::new();
    let mut metadata_builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        codex_home.join(format!("rollout-{thread_id}.jsonl")),
        updated_at,
        SessionSource::Cli,
    );
    metadata_builder.cwd = codex_home.join(format!("workspace-{rollout_slug}"));
    metadata_builder.model_provider = Some("test-provider".to_string());
    metadata_builder.git_branch = Some(format!("branch-{rollout_slug}"));
    let metadata = metadata_builder.build("test-provider");
    db.upsert_thread(&metadata).await?;

    let owner = ThreadId::new();
    let claim = db
        .memories()
        .try_claim_stage1_job(
            thread_id,
            owner,
            updated_at.timestamp(),
            /*lease_seconds*/ 3_600,
            /*max_running_jobs*/ 64,
        )
        .await?;
    let ownership_token = match claim {
        codex_state::Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
        other => panic!("unexpected stage-1 claim outcome: {other:?}"),
    };

    assert!(
        db.memories()
            .mark_stage1_job_succeeded(
                thread_id,
                &ownership_token,
                updated_at.timestamp(),
                raw_memory,
                rollout_summary,
                Some(rollout_slug),
            )
            .await?,
        "stage-1 success should enqueue global consolidation"
    );

    Ok(thread_id)
}

async fn seed_stage1_candidate(
    db: &codex_state::StateRuntime,
    codex_home: &Path,
    updated_at: chrono::DateTime<chrono::Utc>,
    rollout_slug: &str,
    source: SessionSource,
) -> anyhow::Result<ThreadId> {
    let thread_id = ThreadId::new();
    let rollout_path = codex_home.join(format!("rollout-{thread_id}.jsonl"));
    let line = RolloutLine {
        timestamp: updated_at.to_rfc3339(),
        ordinal: None,
        item: RolloutItem::ResponseItem(
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "remember this pipeline worker test conversation".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        ),
    };
    let jsonl = serde_json::to_string(&line)?;
    tokio::fs::write(&rollout_path, format!("{jsonl}\n")).await?;

    let mut metadata_builder =
        codex_state::ThreadMetadataBuilder::new(thread_id, rollout_path, updated_at, source);
    metadata_builder.cwd = codex_home.join(format!("workspace-{rollout_slug}"));
    metadata_builder.model_provider = Some("test-provider".to_string());
    metadata_builder.git_branch = Some(format!("branch-{rollout_slug}"));
    let mut metadata = metadata_builder.build("test-provider");
    metadata.preview = Some("remember this pipeline worker test conversation".to_string());
    metadata.first_user_message = metadata.preview.clone();
    db.upsert_thread(&metadata).await?;
    db.set_thread_memory_mode(thread_id, "enabled").await?;

    Ok(thread_id)
}

async fn seed_required_memory_artifacts(root: &Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(root).await?;
    tokio::fs::write(root.join("MEMORY.md"), "memory\n").await?;
    tokio::fs::write(root.join("memory_summary.md"), "v1\n\nsummary\n").await?;
    Ok(())
}

async fn shutdown_test_codex(test: &TestCodex) -> anyhow::Result<()> {
    test.codex.submit(Op::Shutdown {}).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;
    Ok(())
}
