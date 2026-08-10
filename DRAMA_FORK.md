# drama-crew/codex - fork tracking

Fork of **openai/codex**, maintained for the **Drama platform**.

## Canonical branches

- `main` is the canonical Drama fork and the branch consumed by downstream
  projects. It is based on upstream tag **`rust-v0.144.3`** and contains the
  maintained Drama summary, image, and memory patches.
- `upstream/main` is the tracking reference for OpenAI's upstream `main`.
  Keep it current through the `upstream` remote; it is not the branch shipped
  by Drama.
- Historical versioned branches remain only to reproduce earlier releases.
  They are not active development or consumer targets.

## Why this fork exists

Drama's studio renders the sandbox agent's tool calls as a producer-style
behind-the-scenes log. The maintained summary patch lets a model provide a
short user-language description without contaminating the executed command.
The image and memory patches provide the corresponding Drama product
capabilities in codex-core.

## Consumers and release

`drama-crew/codex-acp` follows this repository's `main` through its Cargo
patch overrides. Its released binary is installed in the Drama sandbox and
embedded by the desktop application.

## Tracking workflow

1. Fetch the upstream remote and review `upstream/main` against Drama `main`.
2. For a Codex upgrade, replay the Drama patches on the selected upstream
   version in an upgrade branch, run the relevant Codex and codex-acp
   validation, then fast-forward or merge that branch into Drama `main`.
3. When code affecting rollout user-message classification changes, update
   codex-acp's `src/fork.rs` mirror and run its fork-session e2e check.

## Interim compatibility

The platform's historical `#__DRAMA_SUMMARY__` command marker remains only as
an interim compatibility mechanism. The codex-core summary schema is the
canonical production path.
