# The `odind` daemon

`odind` runs workflows from **events** — cron schedules and signed GitHub webhooks — instead
of an explicit `odin run`. Point it at a directory of workflows and a repo; it resumes any
incomplete runs, serves each workflow's triggers, and dispatches runs concurrently.

```sh
odind --workflows ./workflows --repo . \
      [--db ./.odin/state.db] \
      [--webhook-addr 127.0.0.1:9292] [--webhook-secret <SECRET>] \
      [--max-concurrent-runs 4]
```

## Flags & environment

| Flag | Default | Meaning |
|------|---------|---------|
| `--workflows <DIR>` | — (**required**) | Directory of workflow files; every `*.yaml`/`*.yml` is loaded (sorted; a bad file is skipped with a warning, not fatal). |
| `--repo <DIR>` | `.` | Git repository the engine provisions workspaces from. |
| `--db <FILE>` | `<repo>/.odin/state.db` | SQLite state database. |
| `--max-concurrent-runs <N>` | `4` | Ceiling on runs executing at once across the whole daemon (clamped to ≥ 1). |
| `--webhook-addr <ADDR>` | `127.0.0.1:9292` | Address the HTTP server binds to. Always started (serves `/metrics` + `/health`); also `/webhook` and/or `/approve` when configured. |
| `--webhook-secret <SECRET>` | `$ODIN_WEBHOOK_SECRET` | HMAC secret for verifying webhook signatures. |
| `--webhook-allow-unsigned` | off | Explicitly run the webhook server **without** signature verification (local testing only). |
| `--log-format <text\|json>` | `text` | Diagnostic-log format (level via `$ODIN_LOG`/`$RUST_LOG`, default `info`). See [observability](observability.md). |
| `--otlp-endpoint <URL>` | — | Export spans to an OpenTelemetry OTLP collector. Honored only when built with `--features otlp`; otherwise ignored with a warning. |

`ODIN_WEBHOOK_SECRET` is the webhook secret (empty = "no secret"); `ODIN_LOG` (then `RUST_LOG`)
sets the log level (logs go to **stderr** — see [observability](observability.md));
`ODIN_SQLITE_SYNCHRONOUS=full` upgrades the state DB from the default WAL `NORMAL` durability to
`FULL` (no last-checkpoint loss on power failure, at a write-latency cost — see below).

The state database is **versioned** (`PRAGMA user_version`) and migrates itself forward on
open; a database written by a *newer* `odind` is refused rather than silently misread. Under
WAL the durability is `synchronous=NORMAL` by default: corruption-safe, with the only failure
mode being the loss of the most recent checkpoint(s) on a power loss — which resume re-runs
idempotently. Set `ODIN_SQLITE_SYNCHRONOUS=full` for zero-loss at higher write latency.

## What startup does

1. Load every `*.yaml`/`*.yml` in `--workflows` (sorted; unreadable/unparseable files are
   skipped with a warning). An empty result is fatal.
2. Open the SQLite store and build the engine over `--repo`.
3. Build the HTTP server: `/webhook` per `github_webhook` trigger and `/approve` if any workflow
   has an approval gate (enforcing the [fail-closed secret rule](#security) for those), plus the
   always-on read-only [`/metrics`](#metrics) and `/health`.
4. Derive a cron trigger per `cron` declaration.
5. **Resume** any incomplete (durable) runs found in the store — crash recovery comes first.
6. Serve the HTTP server and all triggers, dispatching runs concurrently, until `ctrl-c` — which
   **drains** in-flight runs before exiting.

All logging goes to stderr.

---

## Concurrent dispatch & graceful shutdown

A single semaphore (`--max-concurrent-runs`, default 4) bounds runs across the whole daemon.
A burst of events — say, a flurry of webhooks — runs concurrently up to that limit; the rest
queue for a free slot rather than spawning unbounded runs. Each run gets its own isolated
worktree, so concurrent runs don't interfere.

A failing run never takes the daemon down (the error is logged; the trigger keeps firing). On
`ctrl-c`, triggers stop accepting new events and the daemon **awaits in-flight runs to
completion** before exiting. Durable runs that *are* interrupted (e.g. a hard kill) resume
from their last checkpoint on the next start.

---

## Cron triggers

```yaml
triggers:
  - type: cron
    schedule: "0 3 * * *"   # every day at 03:00 UTC
```

- **Standard 5-field cron** (`minute hour day-of-month month day-of-week`). The seconds-based
  6/7-field Quartz form is rejected.
- **POSIX day-of-week**: `0`/`7` = Sunday, `1` = Monday … `6` = Saturday. (`0 3 * * 1` means
  Monday.) Ranges, lists, and steps work (`1-5`, `1,3,5`, `*/2`).
- **UTC**: schedules evaluate in UTC, not server-local time — deterministic and DST-safe,
  matching hosted cron (GitHub Actions). `0 3 * * *` fires at 03:00 UTC.

A cron run carries **no params** (and no payload), so cron suits param-less workflows
(nightly maintenance, scheduled audits). A cron pointed at a workflow with a *required* param
surfaces a validation error at dispatch (logged; the daemon keeps running). The schedule is
also checked at `odin validate` time ([ODIN020](workflow-reference.md#odin020)). See
[`examples/nightly-maintenance.yaml`](../examples/nightly-maintenance.yaml).

---

## Webhook triggers

```yaml
triggers:
  - type: github_webhook
    events: ["issues.labeled"]    # bare "issues" matches any action
    repo: marlboro-red/odin       # optional owner/repo filter
    params:
      issue_url: issue.html_url   # map event fields → run params (dot-path)
```

When any workflow declares a `github_webhook` trigger, `odind` starts an HTTP server
(`--webhook-addr`, default `127.0.0.1:9292`) exposing:

- `POST /webhook` — the event-ingest endpoint.
- `GET /health` — an unauthenticated `200 ok` for liveness probes.

Point a GitHub webhook (content type `application/json`, secret = your `--webhook-secret`) at
`POST /webhook`. For each delivery the server:

1. **Verifies the HMAC-SHA256 signature** (`X-Hub-Signature-256`) over the raw body, in
   constant time. A missing/invalid signature is rejected `400`/`401` (when a secret is set).
2. **De-duplicates** by `X-GitHub-Delivery` — GitHub re-delivers on a non-2xx/timeout, so a
   redelivery is acked `200` without starting a second run (a bounded recent-set; resets on
   restart).
3. **Matches** the event: `X-GitHub-Event` + the payload's `action` against each
   subscription's `events` (`"issues.labeled"` is exact; bare `"issues"` matches any action) —
   the event type and action are compared case-insensitively — filtered by the optional
   `repo` (also case-insensitive).
4. **Maps params**: the full event payload is delivered to the run as `trigger.*`, and each
   `params` entry extracts a field by dot-path (object keys only; array indices aren't
   supported) into a typed run param — so a webhook can satisfy a required param. An
   unresolvable path is skipped (the run then fails param validation, surfacing the mistake;
   an undeclared mapping key warns at validate time, [ODIN027](workflow-reference.md#odin027)).
5. **Dispatches** matching runs. The delivery is recorded in step 2's recent-set **only after**
   every matched subscription enqueues; on full success (and on a no-match delivery) the server
   returns `202 Accepted`. If a subscription's bounded queue is **full**, the delivery is left
   *unrecorded* and the server returns `503 Service Unavailable` so GitHub **retries** it —
   delivery is **at-least-once**, not best-effort. A retry re-runs the match, so subscriptions
   that *did* enqueue on the first attempt may enqueue again: a flooded delivery can start a run
   more than once, which is preferred over silently losing the event.

This unlocks the marquee flow — label an issue, and `issues.labeled → issue-to-pr` runs. See
[`examples/issue-to-pr.yaml`](../examples/issue-to-pr.yaml).

### Approving a paused run over HTTP

When any loaded workflow has an [`approval` gate](workflow-reference.md#approval-step), the same
HTTP server also exposes `POST /approve` — the daemon-side equivalent of
[`odin approve`/`reject`](cli.md#approving-a-paused-run). (The presence of an approval gate, like
a webhook trigger, is enough to start the server even with no webhooks declared.)

It is **signature-verified with the same secret** as `/webhook` (`X-Hub-Signature-256`,
HMAC-SHA256 over the raw body) and subject to the same [fail-closed rule](#security): the daemon
refuses to start if a gate is present but no secret is configured (unless
`--webhook-allow-unsigned`). The JSON body is:

```jsonc
{
  "run_id": "…",           // the paused run's id (UUID)
  "decision": "approved",  // or "rejected"
  "approver": "alice",     // optional (default "http"); recorded for the audit trail
  "note": "lgtm",          // required on a reject (the feedback)
  "rerun": false           // reject only: also start a fresh run carrying the note as feedback
}
```

Unlike `/webhook` (which only enqueues), `/approve` records the decision and **resumes the run
inline**, then answers with the resulting [`RunSummary`](cli.md#json-shapes) as JSON — so the
caller sees whether the run completed, **failed** (a reject), or paused again at a later gate.
With `"rerun": true` on a reject it instead returns a `RerunOutcome` — `{rejected, rerun}`, the
failed original plus the fresh run started with `params.feedback` (the daemon-side
[`reject --rerun`](cli.md#approving-a-paused-run)). Responses: `200` applied; `400` malformed
body / bad run id / a reject with no note / `rerun` on an approve; `401`/`400` bad/missing
signature; `404` unknown run; `409` the run isn't awaiting approval (e.g. already decided) or its
workflow isn't loaded by this daemon; `503` no workflow has an approval gate. A resumed run is
**not** counted against `--max-concurrent-runs` — an approval is a rare operator action, not
trigger-driven load.

```sh
curl -sS http://127.0.0.1:9292/approve \
  -H "X-Hub-Signature-256: sha256=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$SECRET" | awk '{print $2}')" \
  -d "$BODY"   # BODY='{"run_id":"…","decision":"approved","approver":"alice"}'
```

### Limits

- Request bodies are capped at **25 MiB** (GitHub's payload cap).
- Each subscription has a bounded queue (64). When a burst outpaces a slow run and the queue
  fills, the delivery fails with `503` (GitHub retries it — see step 5) rather than applying
  unbounded back-pressure to the HTTP handler. Together with `--max-concurrent-runs`, this
  bounds the work a flood of valid deliveries can spawn.

---

## Metrics

The HTTP server always exposes **`GET /metrics`** in [Prometheus text exposition
format](https://prometheus.io/docs/instrumentation/exposition_formats/) — a cheap aggregate read
of the run-state store (one indexed `GROUP BY`, no run blobs parsed), so the server runs even for
a cron-only daemon with no webhooks or approvals.

```text
# HELP odin_runs_total Completed runs by workflow and terminal status.
# TYPE odin_runs_total counter
odin_runs_total{workflow="issue-to-pr",status="succeeded"} 142
odin_runs_total{workflow="issue-to-pr",status="failed"} 7
# TYPE odin_runs_in_flight gauge
odin_runs_in_flight 3
# TYPE odin_runs_awaiting_approval gauge
odin_runs_awaiting_approval 2
# TYPE odin_runs_pending gauge
odin_runs_pending 0
```

- **`odin_runs_total{workflow,status}`** (counter) — runs that reached a terminal status
  (`succeeded`/`failed`/`cancelled`); monotonic as runs finish.
- **`odin_runs_in_flight`**, **`odin_runs_awaiting_approval`**, **`odin_runs_pending`** (gauges)
  — the live counts of the corresponding non-terminal statuses, summed across workflows.

`/metrics` (like `/health`) is **unauthenticated** — it's read-only operational data and
Prometheus doesn't sign scrapes. Keep it on the loopback default or behind the same reverse
proxy / network boundary as the rest of the server (it should not face the public internet).
For span-level tracing and OTLP export, see [observability](observability.md); `/metrics` is the
pull-based counterpart for dashboards/alerting.

---

## Security

`odind` is **fail-closed**: if a workflow declares a `github_webhook` trigger **or an
[`approval` gate](#approving-a-paused-run-over-http)** and **no secret** is configured (neither
`--webhook-secret` nor a non-empty `$ODIN_WEBHOOK_SECRET`), the daemon **refuses to start** —
unless you explicitly pass `--webhook-allow-unsigned` for local testing. A network-facing
endpoint without signature verification would accept requests from anyone — and `/approve`
mutates run state.

There is **no built-in TLS** and **no HTTP-edge rate limiting** by design — both belong at a
fronting reverse proxy:

- **TLS** → terminate it at a reverse proxy (nginx, a cloud load balancer). The default bind
  is loopback; binding to a non-loopback address over plain HTTP logs a warning (signatures
  would otherwise travel in cleartext).
- **Rate limiting** → the body cap + bounded queues + `--max-concurrent-runs` already bound
  the work valid deliveries can spawn, and unsigned floods are rejected cheaply at the
  signature check; edge rate-limiting is a proxy concern.

The daemon's webhook secret is **not** exposed to the agents it runs: `ODIN_WEBHOOK_SECRET`
is scrubbed from every subprocess the engine spawns (provider CLIs, `run:`/gate shells,
actions). Other environment is still inherited, so don't place unrelated secrets in `odind`'s
environment if the agents it runs are untrusted — see the
[trust boundaries](architecture.md#security--trust-boundaries).

For a durable webhook-triggered workflow, note that the full event payload is checkpointed
into the run's persisted state — GitHub events can carry PII, so prefer mapping the few fields
you need into `params` over relying on whole-event `trigger.*` if you'd rather not persist it.

---

## Embedding the daemon

`odind` is a thin runner over the library. To build your own daemon, use
[`Daemon`](integration-guide.md), [`WebhookServer`], and [`CronTrigger`] from the
`odin-daemon` crate, or implement the [`Trigger`](integration-guide.md#trigger) trait for an
entirely new event source. See the [integration guide](integration-guide.md).

[`WebhookServer`]: integration-guide.md
[`CronTrigger`]: integration-guide.md
