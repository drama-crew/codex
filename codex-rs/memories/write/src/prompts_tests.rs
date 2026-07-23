use super::*;
use codex_models_manager::model_info::model_info_from_slug;
use tempfile::tempdir;

#[test]
fn build_stage_one_input_message_truncates_rollout_using_model_context_window() {
    let input = format!("{}{}{}", "a".repeat(700_000), "middle", "z".repeat(700_000));
    let mut model_info = model_info_from_slug("gpt-5.3-codex");
    model_info.context_window = Some(123_000);
    let expected_rollout_token_limit = usize::try_from(
        ((123_000_i64 * model_info.effective_context_window_percent) / 100)
            * crate::stage_one::CONTEXT_WINDOW_PERCENT
            / 100,
    )
    .unwrap();
    let expected_truncated = truncate_text(
        &input,
        TruncationPolicy::Tokens(expected_rollout_token_limit),
    );
    let message = build_stage_one_input_message(
        &model_info,
        Path::new("/tmp/rollout.jsonl"),
        Path::new("/tmp"),
        &input,
    )
    .unwrap();

    assert!(expected_truncated.contains("tokens truncated"));
    assert!(expected_truncated.starts_with('a'));
    assert!(expected_truncated.ends_with('z'));
    assert!(message.contains(&expected_truncated));
}

#[test]
fn build_stage_one_input_message_uses_default_limit_when_model_context_window_missing() {
    let input = format!("{}{}{}", "a".repeat(700_000), "middle", "z".repeat(700_000));
    let mut model_info = model_info_from_slug("gpt-5.3-codex");
    model_info.context_window = None;
    model_info.max_context_window = None;
    let expected_truncated = truncate_text(
        &input,
        TruncationPolicy::Tokens(crate::stage_one::DEFAULT_ROLLOUT_TOKEN_LIMIT),
    );
    let message = build_stage_one_input_message(
        &model_info,
        Path::new("/tmp/rollout.jsonl"),
        Path::new("/tmp"),
        &input,
    )
    .unwrap();

    assert!(message.contains(&expected_truncated));
}

#[test]
fn build_consolidation_prompt_points_to_workspace_diff_and_extension_tree() {
    let temp = tempdir().unwrap();
    let memory_root = temp.path().join("memories");
    let memory_extensions_root = memory_root.join("extensions");
    std::fs::create_dir_all(&memory_extensions_root).unwrap();

    let prompt = build_consolidation_prompt(&memory_root);

    assert!(prompt.contains("Memory workspace diff:"));
    assert!(prompt.contains("phase2_workspace_diff.md"));
    assert!(prompt.contains(&format!(
        "Memory extensions (under {}/):",
        memory_extensions_root.display()
    )));
    assert!(prompt.contains("workspace diff shows deleted extension resource files"));
}

#[test]
fn stage_one_system_prompt_scopes_extraction_to_user_level_drama_preferences_in_chinese() {
    let prompt = crate::stage_one::PROMPT;

    // Domain boundary: only user-level preferences/habits, never project creative content.
    assert!(prompt.contains("用户级偏好"));
    assert!(prompt.contains("项目规则节点"));
    assert!(prompt.contains("剧情"));
    assert!(prompt.contains("角色"));
    assert!(prompt.contains("世界观"));

    // Output language mandate for the produced artifacts.
    assert!(prompt.contains("简体中文"));

    // Preserve the NO-OP gate and JSON deliverable contract verbatim.
    assert!(prompt.contains(r#"{"rollout_summary":"","rollout_slug":"","raw_memory":""}"#));
    assert!(prompt.contains("`rollout_summary` (string)"));
    assert!(prompt.contains("`rollout_slug` (string)"));
    assert!(prompt.contains("`raw_memory` (string)"));
}

#[test]
fn build_stage_one_input_message_scopes_to_user_level_drama_preferences_and_preserves_placeholders()
 {
    let model_info = model_info_from_slug("gpt-5.3-codex");
    let rollout_path = Path::new("/tmp/rollout-drama.jsonl");
    let rollout_cwd = Path::new("/tmp/drama-project");
    let rollout_contents = "unique-rollout-content-marker";

    let message = build_stage_one_input_message(
        &model_info,
        rollout_path,
        rollout_cwd,
        rollout_contents,
    )
    .unwrap();

    // All render variables must still be substituted.
    assert!(message.contains(&rollout_path.display().to_string()));
    assert!(message.contains(&rollout_cwd.display().to_string()));
    assert!(message.contains(rollout_contents));

    // Domain boundary + output language reminders added for this call site.
    assert!(message.contains("用户级偏好"));
    assert!(message.contains("项目规则节点"));
    assert!(message.contains("简体中文"));
}

#[test]
fn build_consolidation_prompt_scopes_to_user_level_drama_preferences_and_declares_priority() {
    let temp = tempdir().unwrap();
    let memory_root = temp.path().join("memories");
    let memory_extensions_root = memory_root.join("extensions");
    std::fs::create_dir_all(&memory_extensions_root).unwrap();

    let prompt = build_consolidation_prompt(&memory_root);

    // Render variables must still be substituted (regression check alongside the new content).
    assert!(prompt.contains("Memory workspace diff:"));
    assert!(prompt.contains("phase2_workspace_diff.md"));
    assert!(prompt.contains(&format!(
        "Memory extensions (under {}/):",
        memory_extensions_root.display()
    )));

    // Domain boundary: only user-level preferences into MEMORY.md/memory_summary.md.
    assert!(prompt.contains("用户级偏好"));
    assert!(prompt.contains("项目规则节点"));
    assert!(prompt.contains("剧情"));
    assert!(prompt.contains("角色"));
    assert!(prompt.contains("世界观"));

    // Output language mandate.
    assert!(prompt.contains("简体中文"));

    // Priority declaration: user memory yields to project settings/rules, fills gaps only.
    assert!(prompt.contains("用户记忆让位于项目设定与项目规则，仅留空时补位"));
}

#[test]
fn ad_hoc_instructions_declare_drama_boundary_language_and_priority() {
    let instructions = include_str!("../templates/extensions/ad_hoc/instructions.md");

    // Original ad-hoc note handling instructions must remain intact.
    assert!(instructions.contains("Never delete a note file."));
    assert!(instructions.contains("[ad-hoc note]"));

    // Domain boundary + output language + priority declaration for consolidated notes.
    assert!(instructions.contains("用户级偏好"));
    assert!(instructions.contains("项目规则节点"));
    assert!(instructions.contains("简体中文"));
    assert!(instructions.contains("用户记忆让位于项目设定与项目规则，仅留空时补位"));
}
