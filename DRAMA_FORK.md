# drama-crew/codex — fork tracking

Fork of **openai/codex**, maintained for the **Drama platform**.

## Why this fork exists
Drama's studio renders the sandbox agent's tool calls as a producer-style "behind‑the‑scenes" log. We want each tool call to carry a **user‑language one‑line summary authored by the model** (so the log shows e.g. "正在查看项目总体进度" instead of a raw shell command), cleanly — without polluting the executed command.

codex‑core owns the `exec_command` tool **schema** the model fills, so the clean mechanism requires a small core patch here.

## Pinned upstream
- Working branch `drama/summary-titles` is based on tag **`rust-v0.137.0`** (commit `f221438`) — the exact codex version that **zed-industries/codex-acp** (which we also fork at `drama-crew/codex-acp`) depends on via its cargo git deps.
- `main` is kept clean = upstream, for easy syncing (`gh repo sync drama-crew/codex`).

## Planned patch (on `drama/summary-titles`)
- `codex-rs/core/src/tools/handlers/shell_spec.rs`: add an optional `summary` string property to the `exec_command` input schema (model‑authored, one line, user language).
- `codex-rs/core/src/exec.rs` (`ExecParams`) + the tool‑call event/notification consumed by codex‑acp: plumb `summary` through so codex‑acp can read it.

## Consumer / release
- `drama-crew/codex-acp` (`drama/summary-titles`) pins its cargo deps to this branch via `[patch."https://github.com/openai/codex"]` and uses `summary` as the ACP `toolCall` title / `_meta`. The **released artifact is the codex‑acp binary**, built into the drama sandbox image.

## Tracking workflow
1. `gh repo sync drama-crew/codex` (main ← upstream).
2. When bumping codex versions, rebase `drama/summary-titles` onto the new `rust-vX.Y.Z` tag that the matching codex‑acp release pins, re‑apply the schema+plumbing patch, retest.

## Relationship to the shipped interim mechanism
The drama platform currently ships an **interim marker approach** (agent appends a `#__DRAMA_SUMMARY__` comment to commands; the drama acp‑host extracts it) — validated and merged on `drama-crew/drama-platform` main. This fork is the **clean production mechanism** that removes the command‑pollution of that interim approach; it replaces the marker prompt once released.
