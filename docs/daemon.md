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
| `--webhook-addr <ADDR>` | `127.0.0.1:9292` | Address the webhook HTTP server binds to (only started if a workflow declares a `github_webhook` trigger). |
| `--webhook-secret <SECRET>` | `$ODIN_WEBHOOK_SECRET` | HMAC secret for verifying webhook signatures. |
| `--webhook-allow-unsigned` | off | Explicitly run the webhook server **without** signature verification (local testing only). |

`ODIN_WEBHOOK_SECRET` is the only environment variable; an empty value counts as "no secret".

## What startup does

1. Load every `*.yaml`/`*.yml` in `--workflows` (sorted; unreadable/unparseable files are
   skipped with a warning). An empty result is fatal.
2. Open the SQLite store and build the engine over `--repo`.
3. Build the webhook server from every `github_webhook` trigger, and enforce the
   [fail-closed secret rule](#security).
4. Derive a cron trigger per `cron` declaration.
5. **Resume** any incomplete (durable) runs found in the store — crash recovery comes first.
6. Serve all triggers, dispatching runs concurrently, until `ctrl-c` — which **drains**
   in-flight runs before exiting.

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
5. **Dispatches** matching runs and returns `202 Accepted`. (Non-2xx would make GitHub retry,
   so routing/queue problems are logged, never surfaced as a delivery failure.)

This unlocks the marquee flow — label an issue, and `issues.labeled → issue-to-pr` runs. See
[`examples/issue-to-pr.yaml`](../examples/issue-to-pr.yaml).

### Limits

- Request bodies are capped at **25 MiB** (GitHub's payload cap).
- Each subscription has a bounded queue (64); during a slow run, excess events are dropped
  with a log rather than applying unbounded back-pressure. Together with
  `--max-concurrent-runs`, this bounds the work a flood of valid deliveries can spawn.

---

## Security

`odind` is **fail-closed**: if a workflow declares a `github_webhook` trigger and **no secret**
is configured (neither `--webhook-secret` nor a non-empty `$ODIN_WEBHOOK_SECRET`), the daemon
**refuses to start** — unless you explicitly pass `--webhook-allow-unsigned` for local
testing. A network-facing endpoint without signature verification would accept requests from
anyone.

There is **no built-in TLS** and **no HTTP-edge rate limiting** by design — both belong at a
fronting reverse proxy:

- **TLS** → terminate it at a reverse proxy (nginx, a cloud load balancer). The default bind
  is loopback; binding to a non-loopback address over plain HTTP logs a warning (signatures
  would otherwise travel in cleartext).
- **Rate limiting** → the body cap + bounded queues + `--max-concurrent-runs` already bound
  the work valid deliveries can spawn, and unsigned floods are rejected cheaply at the
  signature check; edge rate-limiting is a proxy concern.

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
