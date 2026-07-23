use super::MEMORY_WORKER_SESSION_SOURCE_NAME;
use super::PipelineReport;
use super::run_memories_pipeline;
use codex_config::types::MemoriesConfig;
use codex_features::Feature;
use codex_git_utils::diff_since_latest_init;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionSource;
use codex_rollout::INTERACTIVE_SESSION_SOURCES;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

/// Proves that [`run_memories_pipeline`] genuinely awaits Phase 2 consolidation (baseline reset +
/// DB persistence) to completion before returning, and that `phase2_changed` is derived from the
/// consolidation *output* (MEMORY.md / memory_summary.md content) rather than from
/// `workspace_diff.has_changes()` (which only reflects the *input* diff and would be `true` here
/// since a new stage-1 output landed).
///
/// The absence of any polling/retry loop after `.await` returns is itself the proof: under the
/// old fire-and-forget design (`tokio::spawn` inside `phase2::run`), the workspace baseline reset
/// and DB persistence would still be in flight on a background task when this function returned,
/// so an immediate synchronous check would be flaky. Here it must never be.
#[tokio::test]
async fn run_memories_pipeline_awaits_phase2_to_completion_and_reports_stats() -> anyhow::Result<()>
{
    let server = start_mock_server().await;
    let home = Arc::new(TempDir::new()?);
    let test = build_test_codex(&server, home.clone()).await?;

    let db = test
        .codex
        .state_db()
        .ok_or_else(|| anyhow::anyhow!("state db should be enabled for pipeline test"))?;
    seed_stage1_candidate(
        db.as_ref(),
        home.path(),
        chrono::Utc::now() - chrono::Duration::hours(2),
        "pipeline-happy",
        SessionSource::Cli,
    )
    .await?;

    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-phase1"),
                ev_assistant_message(
                    "msg-phase1",
                    r#"{"raw_memory":"raw memory","rollout_summary":"rollout summary","rollout_slug":"pipeline-happy"}"#,
                ),
                ev_completed("resp-phase1"),
            ]),
            sse(vec![
                ev_response_created("resp-phase2"),
                ev_assistant_message("msg-phase2", "phase2 complete, nothing to change"),
                ev_completed("resp-phase2"),
            ]),
        ],
    )
    .await;

    let config = pipeline_config_from(&test);

    let report = run_memories_pipeline(
        Arc::clone(&test.thread_manager),
        test.thread_manager.auth_manager(),
        config,
    )
    .await?;

    assert_eq!(report.phase1_claimed, 1, "expected report: {report:?}");
    assert_eq!(report.phase1_succeeded, 1, "expected report: {report:?}");
    assert!(report.phase2_ran, "phase2 should have run: {report:?}");
    assert!(
        !report.phase2_changed,
        "the mocked consolidation agent made no MEMORY.md/memory_summary.md edits, so \
         phase2_changed must be false here even though phase2 had new *input* to consider -- \
         this is what proves the source is the before/after content snapshot, not \
         workspace_diff.has_changes(): {report:?}"
    );

    // No polling loop: by the time `.await` returns above, Phase 2 consolidation, the workspace
    // baseline reset, and DB persistence must already be fully complete.
    let memory_root = home.path().join("memories");
    assert!(
        !tokio::fs::try_exists(memory_root.join("phase2_workspace_diff.md")).await?,
        "phase2 workspace diff file should already have been removed"
    );
    let diff = diff_since_latest_init(&memory_root).await?;
    assert!(
        !diff.has_changes(),
        "memory workspace baseline should already be reset"
    );

    // Likewise, both the phase-1 and phase-2 model requests must already have landed --
    // proving the whole pipeline (not just phase 2) ran to completion synchronously.
    assert_eq!(
        responses.requests().len(),
        2,
        "expected exactly one phase1 request and one phase2 request"
    );

    shutdown_test_codex(&test).await?;
    Ok(())
}

/// Anti-self-pollution guarantee (behavioral, end-to-end half): even when a rollout exists in the
/// DB with valid unclaimed content tagged with the dedicated memory-worker session source (as the
/// orchestration thread's own bookkeeping rollout would be, if it were ever reconciled), Phase 1
/// must never claim or extract memories from it -- only genuinely interactive rollouts (e.g. CLI)
/// are eligible.
///
/// (The orchestration thread built by [`build_worker_context`] is intentionally a bare bookkeeping
/// identity that never has any turn/item recorded against it, so it is never itself reconciled into
/// the DB -- see `rollout::recorder::RolloutWriterState::shutdown`'s early-return for
/// still-deferred, item-less threads. That makes a direct "is *my* thread's row tagged correctly"
/// assertion architecturally moot; this test instead proves the allowlist boundary behaviorally by
/// seeding a candidate that impersonates the worker's session source directly.)
#[tokio::test]
async fn phase1_never_claims_a_rollout_tagged_with_the_memory_worker_session_source()
-> anyhow::Result<()> {
    let server = start_mock_server().await;
    let home = Arc::new(TempDir::new()?);
    let test = build_test_codex(&server, home.clone()).await?;

    let db = test
        .codex
        .state_db()
        .ok_or_else(|| anyhow::anyhow!("state db should be enabled for pipeline test"))?;

    // A normal, genuinely claimable interactive candidate ...
    seed_stage1_candidate(
        db.as_ref(),
        home.path(),
        chrono::Utc::now() - chrono::Duration::hours(2),
        "claimable-cli",
        SessionSource::Cli,
    )
    .await?;
    // ... and one tagged with the memory worker's own dedicated session source, which must never
    // be claimed even though its content is otherwise perfectly valid and unclaimed.
    seed_stage1_candidate(
        db.as_ref(),
        home.path(),
        chrono::Utc::now() - chrono::Duration::hours(2),
        "worker-self",
        SessionSource::Custom(MEMORY_WORKER_SESSION_SOURCE_NAME.to_string()),
    )
    .await?;

    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-phase1"),
                ev_assistant_message(
                    "msg-phase1",
                    r#"{"raw_memory":"raw memory","rollout_summary":"rollout summary","rollout_slug":"claimable-cli"}"#,
                ),
                ev_completed("resp-phase1"),
            ]),
            sse(vec![
                ev_response_created("resp-phase2"),
                ev_assistant_message("msg-phase2", "phase2 complete, nothing to change"),
                ev_completed("resp-phase2"),
            ]),
        ],
    )
    .await;

    let config = pipeline_config_from(&test);

    let report = run_memories_pipeline(
        Arc::clone(&test.thread_manager),
        test.thread_manager.auth_manager(),
        config,
    )
    .await?;

    assert_eq!(
        report.phase1_claimed, 1,
        "only the CLI-sourced rollout should have been claimed, not the worker-sourced one: {report:?}"
    );
    assert_eq!(report.phase1_succeeded, 1, "expected report: {report:?}");
    assert_eq!(
        responses.requests().len(),
        2,
        "expected exactly one phase1 request (for the single claimable rollout) and one phase2 \
         request -- a third request here would mean the worker-sourced rollout was wrongly claimed"
    );

    shutdown_test_codex(&test).await?;
    Ok(())
}

/// Anti-self-pollution guarantee (allowlist half): the worker's own session source must never be
/// a member of the Phase 1 claim allowlist, or the pipeline could claim and extract memories from
/// its own bookkeeping rollout.
#[test]
fn worker_session_source_is_excluded_from_phase1_claim_allowlist() {
    let worker_source = SessionSource::Custom(MEMORY_WORKER_SESSION_SOURCE_NAME.to_string());
    let allowed_sources: Vec<String> = INTERACTIVE_SESSION_SOURCES
        .iter()
        .map(ToString::to_string)
        .collect();

    assert!(
        !allowed_sources.contains(&worker_source.to_string()),
        "the memory worker's own orchestration session source must never be claimable by phase 1: \
         allowed sources = {allowed_sources:?}"
    );
}

#[tokio::test]
async fn run_memories_pipeline_returns_zero_report_when_memory_tool_feature_disabled()
-> anyhow::Result<()> {
    let server = start_mock_server().await;
    let home = Arc::new(TempDir::new()?);
    let test = build_test_codex(&server, home).await?;

    // `Feature::MemoryTool` is intentionally left disabled here (unlike `pipeline_config_from`).
    let config = Arc::new(test.config.clone());

    let report = run_memories_pipeline(
        Arc::clone(&test.thread_manager),
        test.thread_manager.auth_manager(),
        config,
    )
    .await?;

    assert_eq!(
        report,
        PipelineReport::default(),
        "pipeline must no-op (not panic) when the memory tool feature flag is disabled"
    );

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

    assert_eq!(
        report,
        PipelineReport::default(),
        "pipeline must no-op (not panic) when config.memories.generate_memories is false"
    );

    shutdown_test_codex(&test).await?;
    Ok(())
}

fn pipeline_config_from(test: &TestCodex) -> Arc<codex_core::config::Config> {
    let mut config = test.config.clone();
    config
        .features
        .enable(Feature::MemoryTool)
        .expect("test config should allow feature update");
    Arc::new(config)
}

fn startup_test_memories_config() -> MemoriesConfig {
    MemoriesConfig {
        max_raw_memories_for_consolidation: 1,
        min_rollout_idle_hours: 0,
        ..MemoriesConfig::default()
    }
}

async fn build_test_codex(
    server: &wiremock::MockServer,
    home: Arc<TempDir>,
) -> anyhow::Result<TestCodex> {
    build_test_codex_with_memories_config(server, home, startup_test_memories_config()).await
}

async fn build_test_codex_with_memories_config(
    server: &wiremock::MockServer,
    home: Arc<TempDir>,
    memories: MemoriesConfig,
) -> anyhow::Result<TestCodex> {
    test_codex()
        .with_home(home)
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Sqlite)
                .expect("test config should allow feature update");
            config.memories = memories;
        })
        .build(server)
        .await
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
        item: RolloutItem::ResponseItem(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "remember this pipeline test conversation".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }),
    };
    let jsonl = serde_json::to_string(&line)?;
    tokio::fs::write(&rollout_path, format!("{jsonl}\n")).await?;

    let mut metadata_builder =
        codex_state::ThreadMetadataBuilder::new(thread_id, rollout_path, updated_at, source);
    metadata_builder.cwd = codex_home.join(format!("workspace-{rollout_slug}"));
    metadata_builder.model_provider = Some("test-provider".to_string());
    metadata_builder.git_branch = Some(format!("branch-{rollout_slug}"));
    let mut metadata = metadata_builder.build("test-provider");
    metadata.preview = Some("remember this pipeline test conversation".to_string());
    metadata.first_user_message = metadata.preview.clone();
    db.upsert_thread(&metadata).await?;
    db.set_thread_memory_mode(thread_id, "enabled").await?;

    Ok(thread_id)
}

async fn shutdown_test_codex(test: &TestCodex) -> anyhow::Result<()> {
    test.codex.submit(Op::Shutdown {}).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;
    Ok(())
}
