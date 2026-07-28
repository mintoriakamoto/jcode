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

### Infra / CI-CD (audited)

All runners are GitHub-hosted (no `self-hosted` labels anywhere) — if the repo
is public, Actions minutes are $0 and these are wall-clock/queue-time wins;
the telemetry worker (Cloudflare) is the only unconditionally metered infra.

**Telemetry worker — the only traffic-scaling cost, and it's unbounded:**

- Every event costs 2-5+ separate D1 statements with no `env.DB.batch()`
  anywhere (`telemetry-worker/src/worker.js:672,974,1598,1078-1120`) — each a
  billed statement and a subrequest. Batch them (also makes writes atomic).
- No sampling on high-volume events: `turn_end` fires per agent turn and takes
  the full D1 path plus an Analytics Engine write. The AE firehose is already
  designated the primary store for these (`wrangler.toml:26-33`) — sample the
  redundant D1 raw tail 1-in-N.
- `install`/`feedback`/`subscription_activated`/`account_linked` are never
  pruned, and `daily_active_users` grows one row per user per day forever
  against D1's hard 500 MB cap (`worker.js:556-564`). Nightly prune is capped
  at 120k rows/run (`worker.js:583-584`) and can silently fall behind into the
  emergency half-all-retention path (`worker.js:544`).
- The public `POST /v1/event` endpoint has `ALLOWED_ORIGIN = "*"`, no auth, no
  rate limiting (`worker.js:351-470`) — write volume (= cost) is
  attacker-controlled. Add Cloudflare rate limiting.

**CI (every PR/push):**

- Three `cargo install`s compile from source on every run:
  `cargo-machete` (`ci.yml:107`), `cargo-audit` (`ci.yml:290`), and
  `cargo-xwin` from **unpinned git** (`ci.yml:538` — also a supply-chain
  risk). rust-cache does not cache `~/.cargo/bin`. Swap for
  `taiki-e/install-action` prebuilt binaries — best effort-to-payoff fix.
- `quality` runs `cargo check --all-targets --all-features` then clippy with
  the same flags (`ci.yml:46,49`) — the check step is duplicated work; delete
  it.
- `windows-build-test` (150-min timeout, `ci.yml:293-295`) does a release
  build on every PR and largely duplicates `release.yml:170`; PRs could use
  check/debug profile.
- `windows-cross-check` (`ci.yml:508`) overlaps the native Windows job for
  x64; its unique ARM64 leg is already advisory (`ci.yml:545`). Reduce to the
  ARM64 check or drop.

**Release pipeline (per tag):**

- FreeBSD builds under QEMU with no cargo cache — cold full release build
  every tag, 180-min ceiling (`release.yml:414-419`). Largest single
  minute-consumer; cache into the VM or cross-compile.
- Two full macOS release builds (aarch64 + intel, `release.yml:80,83`, 10×
  rate) — ship a universal binary from one runner, or drop Intel.
- A dedicated `windows-latest` runner boots just to sign artifacts
  (`release.yml:295-297`) — fold into the build matrix leg.
- The release Windows leg recompiles the e2e harness that already ran in CI
  (`release.yml:279-289`) — gate behind `workflow_dispatch`.

**iOS (per `ios/**` push to master):**

- Three sequential macOS jobs (up to 120 macOS-minutes) with zero caching (no
  SPM/DerivedData/Homebrew cache), `xcodegen` brew-installed twice, and a
  TestFlight upload burning a build number on every push
  (`ios-testflight.yml:23,40,67,44,73`). Merge test+compile-check, add
  caches, gate upload to tags.

**Hygiene:** `freebsd-smoke.yml` weekly cron has no `github.repository` guard,
so forks run the 120-min QEMU build weekly.

### Wasted/duplicate API calls (audit pending)

### Per-request context bloat (audited)

Per-request input floor is ~15-17k tokens; **~85-90% of it is tool schemas**.
The system prompt itself is small (~490 tokens) and correctly cache-split.

- **~13-15k tokens of always-on tool schemas, no lazy loading.** ~28 tools are
  always registered (`jcode-app-core/src/tool/mod.rs:152-256`) and
  `definitions()` returns all of them every request (`mod.rs:319`). Four tools
  (swarm ~14 KB, schedule ~8.7 KB, todo ~7.3 KB, session_search ~5 KB of
  description+schema source) are ~half the budget, and
  `ensure_intent_in_schema` adds ~1k tokens across all tools
  (`jcode-tool-core/src/lib.rs:25`). Cached, so steady-state is the 10%
  cache-read rate — but it's the whole cache prefix, and a mid-session MCP
  registration busts it (`turn_execution.rs:425`). Highest-leverage fix:
  deferred tool schemas, or gate swarm/schedule/gmail/selfdev by session mode.
- **No stale-tool-result elision.** Old tool outputs ride verbatim until the
  80% compaction threshold; `emergency_truncate_tool_results` exists but is
  only reachable after a 413/overflow failure
  (`jcode-compaction-core/src/lib.rs:543,561`). Age-based downgrade of old
  bash/grep/read dumps to head+tail stubs is a cheap big win.
- **`read` has no output cap of its own** — 5,000 lines × 2,000 chars/line
  (`tool/read.rs:12-13`); the only backstop is the 30%-of-budget guard
  (`tool/mod.rs:541,645`), which on a 200k window still admits a single 60k
  token read. Every other tool is sensibly capped (bash 30k chars, webfetch
  40k, etc.).
- **Images are clamped for API limits, not tokens** (8000px / 9 MB targets in
  `jcode-base/src/provider/image_clamp.rs`), never downscaled to the ~1568px
  useful ceiling, and stay in history verbatim — 20 accumulated screenshots
  ≈ 25-30k resident tokens.
- **Instruction files are read uncapped** (`AGENTS.md`, prompt overlay,
  preferred-tools; `prompt.rs:823,868,911`) — cached, but they define the
  cache prefix. The swarm deep-effort directive adds ~700 tokens to the
  *uncached* dynamic block per request at that effort (`prompt.rs:91`).
- The per-turn dynamic system-reminder is inserted after the last user message
  (`jcode-message-types/src/lib.rs:507`), capping how much message history can
  stay cached. Memory injection is well capped (5/turn) — no issue.

### Wasted/duplicate API calls (audit pending)
