# Cost Optimization Plan

Ranked plan for reducing real dollar spend: LLM API tokens first (the dominant
recurring cost for pay-per-token users), then CI/infra compute. Sourced from a
2026-07 audit of provider crates, compaction, swarm, and the CI workflows.

Per the optimization skill (`.jcode/skills/optimization/SKILL.md`): measure
first, macro-optimize before micro-optimizing, and don't claim a saving without
before/after evidence. Item 7 (spend tracking) exists so the rest can be
verified.

## Baseline: what already works

- Anthropic prompt caching is thorough: tools breakpoint
  (`jcode-provider-anthropic/src/lib.rs:507,522`), static/dynamic system split
  (`lib.rs:643-681`), sliding two-marker message window (`lib.rs:710-775`),
  user-togglable 1h TTL (`/cache 1h`).
- Compaction is automatic: 0.80 threshold, 0.95 synchronous hard compact
  (`jcode-compaction-core/src/lib.rs:9-16`), reactive recovery on
  context-limit errors.
- A cheap-model sidecar tier exists (`jcode-base/src/sidecar.rs`) — currently
  used only by memory.
- Embeddings are local ONNX MiniLM, zero API cost (`jcode-embedding`).
- Per-turn cost is computed with correct cache accounting
  (`jcode-provider-core/src/pricing.rs`, `jcode-tui/.../misc_ui.rs:25-79`).

## Ranked gaps

### 1. Compaction summaries run on the expensive session model — HIGH, low effort

`generate_compaction_artifact` calls `provider.complete_simple(...)` on the
live session provider (`crates/jcode-base/src/compaction.rs:1737-1742`) with a
prompt sized to `context_window - 4000` tokens (`:1733`) — a near-full-context
call on an Opus-class model whose only output is a summary. Fires automatically
every time a long session crosses the 80% threshold.

**Fix**: route compaction summarization through the existing `Sidecar`
cheap-model selection (`crates/jcode-base/src/sidecar.rs:118-135`), falling
back to the session model only when no cheap model is authenticated.

### 2. Bedrock has zero prompt caching — HIGH, medium effort

`crates/jcode-provider-bedrock/src/lib.rs` never emits a `cachePoint` block and
hardcodes `cache_read_input_tokens: None, cache_creation_input_tokens: None`
(`:1301-1302`). Bedrock is pure pay-per-token; Anthropic models there support
`cachePoint` with ~90% cache-read discount.

**Fix**: add `cachePoint` blocks mirroring the native Anthropic breakpoint
strategy (tools, static system, sliding message window) and map the returned
cache metrics into usage.

### 3. OpenRouter cannot cache system/tools — HIGH (for OpenRouter users), medium effort

The system prompt is serialized as a bare string
(`crates/jcode-provider-openrouter/src/request.rs:186-192`), so `cache_control`
can't attach to it; the code only forwards pre-existing breakpoints
(`request.rs:241-243`) and nothing ever sets one.

**Fix**: for Anthropic models routed via OpenRouter, serialize system as
content blocks and set breakpoints like the native provider.

### 4. Non-streaming Anthropic path busts its own system cache — MED, trivial effort

`Provider::complete()` caches the entire system prompt as one block, dynamic
parts included (`crates/jcode-provider-anthropic-runtime/src/lib.rs:1034` →
`build_system_param`), so date/git-status churn invalidates it. The streaming
path (`:1395`) already uses the correct `build_system_param_split`.

**Fix**: use the split builder on the `complete()` path too.

### 5. No model tiering for subagents or ambient subtasks — MED-HIGH, medium effort

Subagents inherit the parent model with no override
(`crates/jcode-app-core/src/tool/mod.rs:95,109`); swarm fan-out multiplies
Opus spend. Ambient mode deliberately defaults to the strongest model
(`docs/AMBIENT_MODE.md:660-664`), which is costly on pay-per-token routes
(`:655`) even for cheap subtasks (triage, scoring).

**Fix**: add a `subagent_model`/tier config and route mechanical subtasks
(and ambient triage) through the sidecar tier.

### 6. CI runs the full expensive matrix on docs-only changes — MED, trivial effort

`ci.yml` has no path filtering: a README/`docs/`/`changelog/` change triggers
macOS runners (10× Linux per-minute rate), the Windows job (150-minute
budget), and the xwin cross-check. Also, `cargo install cargo-machete` /
`cargo-audit` / `cargo-xwin` compile from source each run.

**Fix**: `paths-ignore` (or a change-detection gate so required checks still
report) for `**.md`, `docs/**`, `changelog/**`, `assets/**`; use
`taiki-e/install-action` for prebuilt tool binaries.

*Caveat*: GitHub-hosted runners are free on public repos — this item matters
only for private forks/mirrors, but costs wall-clock time regardless.

### 7. No aggregate dollar tracking — enabler, low effort

Pricing tables and per-turn TUI cost exist, but `jcode-usage-types` carries
token counts only — `DayUsage`/`MonthUsage`/`AllTimeUsage`
(`crates/jcode-usage-types/src/lib.rs:39-57`) have no cost field, and
`jcode-telemetry-core` accumulates cache tokens without pricing them
(`src/lib.rs:1889-1917`).

**Fix**: add USD fields to the usage rollups, priced via
`jcode-provider-core/src/pricing.rs`. This is the measurement backbone for
verifying items 1-5.

## Sequencing

1. Item 7 (measurement) + item 4 (trivial) first.
2. Item 1 (biggest recurring win, plumbing exists).
3. Items 2-3 (provider caching parity).
4. Item 5 (tiering — needs config surface design).
5. Item 6 (CI) anytime; independent.

## Additional findings (2026-07 deep audit)

_Pending: wasted/duplicate API call audit, per-request context bloat audit,
release/infra pipeline audit._
