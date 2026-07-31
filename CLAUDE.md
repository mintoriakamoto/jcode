# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

jcode is a Rust coding-agent harness: a fast, RAM-efficient TUI (plus desktop and iOS frontends) supporting many model providers, swarm coordination, and self-development ("selfdev") workflows. The workspace has ~80 crates plus a root `jcode` crate.

## Commands

```bash
# Fast iteration — prefer these over full builds
cargo check --all-targets --all-features
cargo check -p <crate>                      # focused check, e.g. -p jcode-desktop

# Run a focused test (preferred over broad filters — see Test policy below)
cargo test <filter>                         # root crate tests
cargo test -p <crate> <filter>              # crate-scoped
cargo test -p jcode-desktop2 profile:: -- --test-threads=1   # desktop2 profile tests need single-thread

# Before pushing — runs every gate from CI's Format + Quality Guardrails jobs
scripts/check_guardrails.sh                 # fmt, clippy -D warnings, ratchets
scripts/check_guardrails.sh --skip-slow     # skip cargo check/clippy/machete
scripts/check_guardrails.sh --fix           # rustfmt + rebaseline ratchets after intentional growth

# Builds
cargo build --profile selfdev               # fast optimized-deps build used for self-dev
scripts/remote_build.sh                     # offload heavy cargo work if local build gets OOM-killed

# Dependency boundary guard (after touching any *-types crate dependency)
python3 scripts/check_dependency_boundaries.py
```

CI tracks the `stable` toolchain; a stale local clippy can pass on lints CI enforces, so run `rustup update stable` if clippy results look inconsistent. CI's Windows jobs are the only ones using `--locked` — keep `Cargo.lock` up to date (`cargo metadata --locked` is one of the guardrail gates).

Logs are written to `~/.jcode/logs/jcode-YYYY-MM-DD.log`.

## Architecture

### Layering (root crate is a thin shell)

```
jcode-base  →  jcode-app-core  →  jcode-tui  →  jcode (root: src/)
(prompts,      (non-presentation   (TUI +        (CLI + entrypoint only)
 core agent)    app modules)        video_export)
```

The root crate does `pub use jcode_tui::*`, which transitively re-exports `jcode-app-core` and `jcode-base` — so `crate::config`, `crate::server`, `crate::tui`, etc. inside `src/cli/` resolve into those crates. When looking for a module referenced as `crate::<mod>` in root code, check `jcode-tui` and `jcode-app-core` first.

### Crate families

- **`jcode-*-types`** (session, task, tool, message, config, auth, …): stable DTO/data-contract crates. They must stay dependency-light — serde/chrono/other type crates only; no filesystem, network, TUI, provider, or storage deps. `scripts/check_dependency_boundaries.py` enforces this in CI.
- **`jcode-provider-*`** / **`jcode-provider-*-runtime`**: per-provider pairs (anthropic, openai, gemini, copilot, openrouter, antigravity, bedrock, cursor, claude-cli). Shared plumbing in `jcode-provider-core`, `jcode-provider-metadata`, `jcode-provider-env`; diagnostics in `jcode-provider-doctor`.
- **`jcode-tui-*`**: split presentation crates (markdown, mermaid, messages, render, style, permissions, tool-display, workspace, …) beneath `jcode-tui`.
- **`jcode-desktop`** / **`jcode-desktop2`**: desktop app crates (desktop2 is the newer rewrite; see `docs/DESKTOP_APP_ARCHITECTURE.md`). `crates/jcode-desktop/AGENTS.md` applies when working there — prefer `cargo check -p jcode-desktop` and desktop-scoped tests.
- **`jcode-harness-api`** / **`jcode-harness-api-server`**: the harness API surface (see `docs/HARNESS_API_AND_DESKTOP_REWRITE.md`); `jcode-swarm-core` for multi-agent swarm (see `docs/SWARM_ARCHITECTURE.md`).
- System prompts live in `crates/jcode-base/src/prompt/*.md`.

### Moving code between crates

The modularization goal is compile speed: shrink the root crate's recompilation surface. Before moving a type or helper out of root, follow `docs/CRATE_OWNERSHIP_BOUNDARIES.md` — key rules:

- Only move stable data contracts / pure helpers into `*-types` crates; behavior that needs storage, providers, TUI state, or process management stays in root or moves with its whole dependency boundary.
- Preserve serde representation exactly, and keep a `pub use` compatibility re-export at the old root path during migration.
- Validate with `cargo check --profile selfdev -p <type-crate> -p jcode --bin jcode` plus focused tests.

## Conventions

- **Ratchets, not lints**: CI enforces budgets for warnings, oversized files, oversized tests, panics, swallowed errors, dependency boundaries, and wildcard re-exports. If a change intentionally grows a budget, rebaseline with `scripts/check_guardrails.sh --fix`.
- **Test filters**: prefer precise test filters; broad ones (`side_panel`, `usage`, `session::`, `ambient`) pick up unrelated stateful/timing-sensitive/benchmark tests.
- **Commit as you go**: small focused commits per feature/fix; run the guardrails before pushing.
- **Docs placement**: repo root holds only meta files (README, CONTRIBUTING, RELEASING, AGENTS, LICENSE). Everything else goes in `docs/` — current behavior at top level, speculative material in `docs/plans/` or `docs/proposals/`. Prefer updating an existing doc over adding a near-duplicate. `docs/README.md` indexes the key entry points.
- **Releases**: bump the version in `Cargo.toml` (root `[package]`), choosing patch/minor from the changes since the last release; see `RELEASING.md` and `changelog/`.
- **Install channels** (for selfdev/testing): the launcher symlink is `~/.local/bin/jcode`; `~/.jcode/builds/current/` is the local source-build channel, `~/.jcode/builds/stable/` the release channel, `~/.jcode/builds/versions/<version>/` immutable binaries.
