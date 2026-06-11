# Changelog

All notable changes to Odin are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project adheres to
[SemVer](https://semver.org/) with the pre-1.0 convention described in the
[versioning policy](README.md#versioning) (the minor is the breaking slot). The library crates and
the CLIs share one version.

## [0.1.2] – 2026-06-11

Security fixes (found by running Odin's own `deep-review` workflow against this repo and verifying
the findings) plus a first-run smoothing. No breaking changes.

### Security

- **codex prompt arg-injection** — `codex exec` took the prompt as a bare trailing positional, so a
  rendered prompt beginning with `-` (an injected step output, or a webhook param at the start of a
  prompt) was parsed as codex flags — an injected `--dangerously-bypass-approvals-and-sandbox` could
  override the sandbox. The prompt is now passed after a `--` option terminator (and the slot-pool
  `git` invocations gained the same guard).
- **webhook-mapped params into a shell** — the shell-injection lint flagged untrusted `trigger.*`
  (ODIN031) but not a `params.*` value mapped from a webhook payload, which the daemon fills from the
  attacker-controlled event body. New **ODIN046** catches it; `| shquote` now *clears* the
  shell-injection lints (previously they fired even when you applied the fix).
- **the daemon now validates workflows at load** — it previously parsed but never validated served
  workflows, so the injection lints never fired on the live webhook path. A workflow with validation
  errors is now refused; warnings (including the injection lints) are logged at startup.

### Changed

- First-run: the quickstart example is now `durable`, so the documented `run → list → show → logs`
  flow works (it previously dead-ended on an empty store); the empty-store CLI messages now explain
  how a run gets recorded.

## [0.1.1] – 2026-06-11

A small additive follow-up to 0.1.0 (no breaking changes).

### Added

- The web dashboard now renders each run's wall-clock duration and per-step durations (the
  `duration_ms` the engine already projects).
- `odin_steps_in_flight` gauge on `/metrics` — steps in an active execution segment right now,
  across all runs (run-level activity remains the store-backed `odin_runs_in_flight` /
  `odin_runs_pending` gauges).
- `docs/workflow.schema.json` — a JSON Schema for workflow files, for editor autocomplete and
  inline typo-catching via a YAML language server. A CI test keeps it in sync with every shipped
  example. (`odin validate` remains authoritative.)

## [0.1.0] – 2026-06-11

A developer-experience milestone: the engine was already strong at "run, then read the result", but
walled the moment you needed to **act mid-run, watch a run live, or observe a non-durable run**.
This release closes those gaps — progress hooks, scriptable CLIs, disk-spooled logs, metrics &
timings, a first-class generic webhook, and a hardened, versioned, documented HTTP API.

### Added

- **Progress callback hook** — `EngineBuilder::on_event(Fn(RunId, &RunEvent))`, fired at the engine's
  single `emit()` choke point for **every** run (durable or not). New events: `RunSuspended`,
  `RunCancelled`, `RunResumed`, `ApprovalDecided` (with `SuspendReason` / `CancelReason`).
- **In-memory mirror** — `Engine::recent()` / `summary()` now also see **non-durable** runs via a
  bounded, light snapshot, so observability no longer requires `durable: true`.
- **Scriptable CLIs** — `--json` on `run`, `approve`, `reject`, `cancel`, `validate`, `status`, and
  `prune`, with a unified `{ok, …}` / `{error, code}` envelope (stdout is never empty on error).
- **Step-output logs** — full, un-clipped per-step output spooled to
  `<repo>/.odin/logs/<run_id>/<step>.<attempt>.log`; `EngineBuilder::logs_dir(dir)` to opt in;
  retention + `prune` clean them up.
- **Per-step timings** — `started_at` / `finished_at` on each step; `duration_ms` + `created_at` on
  run/step views; `odin run` prints `[210ms]` per step and a total.
- **`odin status --url <daemon>`** — read a remote `odind`'s run list over HTTP (with `--watch` /
  `--json`).
- **Prometheus metrics** (`/metrics`) — `odin_run_duration_seconds` / `odin_step_duration_seconds`
  histograms (active-execution only — approval-waits and pre-resume time excluded) and
  `odin_webhook_deliveries_total{result}` (accepted / duplicate / rejected).
- **Generic `webhook` trigger** — `type: webhook` for any service that can POST JSON (not just
  GitHub): `X-Odin-Event` matching, `X-Odin-Signature-256` HMAC, `X-Odin-Delivery` de-dup, and body
  param-mapping by dot-path. Shares the GitHub path's signing, dedup, and at-least-once dispatch.
- **HTTP API hardening** — JSON error bodies `{error, code}` on every endpoint (including
  axum-level 404/405/413 rejections); the `/webhook` `202` returns the matched workflow names; an
  `X-Odin-Api-Version` header on every response; and an [OpenAPI spec](docs/openapi.yaml).
- **`Provider::version()`** wired into `RunState.provider_versions` (bounded with a 5s timeout).
- **Embedding ergonomics** — public `InvocationCtx::new` / `ActionCtx::new` / `AcquireCtx::new`
  constructors; `MemStore` now records events and serves `recent()` / `metrics()`; a store-less +
  approval-gate fail-fast guard.
- **Docs** — a [glossary](docs/glossary.md), an [environment-variable reference](docs/environment.md),
  this changelog, and a [versioning policy](README.md#versioning).

### Changed

- Failure observability: a step timeout vs. a cancellation vs. a crash now produce distinct,
  correctly-leveled log lines from every terminal path (including resume).
- `summary().finished_at` is `None` until a run is terminal.

### Fixed

- A webhook dedup cross-type collision (a GitHub and a generic delivery sharing an id) that could
  silently drop the second delivery — delivery ids are now namespaced by trigger kind.
- The LibreSSL HMAC documentation recipe (`awk '{print $2}'` → `$NF`), which printed an empty
  signature on macOS.

## [0.0.1] – [0.0.5]

The foundation and its hardening. The workflow IR + validator (45 `ODIN###` diagnostics), the
templating/context model, the five integration traits (Provider / Workspace / Store / Action /
Trigger), the durable SQLite store with crash-resume + per-step git snapshots, worktree + slot-pool
workspaces, the three provider adapters, built-in actions, LLM-as-judge + retry, the concurrent
executor (`max_parallel` + `scratch:` fan-out), `case:` / `loop:` control flow, default step
timeouts + run cancellation, live `--stream` output, the recipe catalog, the `odin` CLI, and the
`odind` daemon (cron + signed GitHub webhooks + concurrent dispatch + approval endpoint + dashboard).
Plus several adversarial-audit hardening campaigns and Windows compatibility (`0.0.2`–`0.0.5`).
