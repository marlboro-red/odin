# Changelog

All notable changes to Odin are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project adheres to
[SemVer](https://semver.org/) with the pre-1.0 convention described in the
[versioning policy](README.md#versioning) (the minor is the breaking slot). The library crates and
the CLIs share one version.

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
