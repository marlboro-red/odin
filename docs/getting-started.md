# Getting started

This walks you from a clean checkout to validating, running, and serving workflows. For the
full schema see the [workflow reference](workflow-reference.md); to embed Odin in your own
program see the [integration guide](integration-guide.md).

## Build

Odin is a Rust workspace (edition 2024, MSRV 1.85). Build the two binaries:

```sh
cargo build                       # the `odin` CLI and the `odind` daemon
```

This produces `odin` (the CLI) and `odind` (the daemon) under `target/`.

## 1. Validate a workflow

Start by writing — or borrowing — a workflow and checking it. `odin validate` reports *every*
problem in one pass, with "did you mean" hints:

```sh
odin validate --help                       # (or: cargo run -p odin-cli -- …)
odin validate examples/issue-to-pr.yaml
# ✓ examples/issue-to-pr.yaml is valid
```

A workflow is **valid** with zero errors; warnings are fine (the run still proceeds). See
[`examples/fix-flaky-test.yaml`](../examples/fix-flaky-test.yaml), which exercises the whole
IR and intentionally trips exactly one documented warning so you can see the warning path.

## 2. Run a workflow

`odin run` executes a workflow against a git repository. Steps that use `run:` execute for
free; `provider:` steps invoke the real coding-agent CLI (so `claude` / `codex` / `copilot`
must be installed, on `PATH`, and authenticated).

```sh
odin run examples/issue-to-pr.yaml \
  --repo . \
  --param issue_url=https://github.com/owner/repo/issues/1
```

```text
Run 7f3c… — succeeded
  ✓ plan
  ✓ implement (exit 0)
  ✓ review
  ✓ commit
  ✓ push
  ✓ open_pr
usage: 41310 in / 2204 out tokens, $0.1234
side-effects: 3
```

Pass `--param KEY=VALUE` (repeatable) for typed inputs — values parse as JSON when possible,
so `--param attempts=3` is a number and `--param dry_run=true` is a bool. Runs are
**durable** by default: state is checkpointed to `<repo>/.odin/state.db`, so a crashed run
resumes. Add `--no-store` to disable persistence.

The [`odin` CLI reference](cli.md) documents every command, flag, exit code, and `--json`
shape.

## 3. Inspect past runs

```sh
odin list                          # recent runs, newest first
odin show <RUN_ID>                 # one run's status, steps, and captured diff
odin logs <RUN_ID>                 # a run's event log
```

All three read the same store `odin run` (and the daemon) write to, and accept `--json`.

## 4. Serve workflows on events

`odind` runs workflows from **cron schedules** and **GitHub webhooks** instead of an explicit
`odin run`. Point it at a *directory* (it loads every `.yaml` inside):

```sh
# The example set includes two workflows with `github_webhook` triggers, so the daemon
# fails closed unless you give it a verification secret:
ODIN_WEBHOOK_SECRET=… odind --workflows examples --repo . --webhook-addr 127.0.0.1:9292
```

The webhook listener starts **only** if some loaded workflow declares a `github_webhook`
trigger. A directory of purely cron-scheduled workflows (like
[`nightly-maintenance.yaml`](../examples/nightly-maintenance.yaml)) starts no listener and
needs no secret:

```sh
odind --workflows ./cron-only --repo .          # no webhook triggers → no secret
```

For local development you can serve webhook workflows without a secret by adding
`--webhook-allow-unsigned` — it disables signature verification (it does *not* restrict the
bind address, so pair it with a loopback `--webhook-addr` like `127.0.0.1:9292`; a non-loopback
bind only warns). Intended for local testing only.

The daemon resumes incomplete runs on startup, dispatches up to `--max-concurrent-runs`
(default 4) at once, and drains in-flight runs on `ctrl-c`. See the
[daemon reference](daemon.md) for cron semantics (POSIX day-of-week, UTC), webhook signature
verification and dedup, and the fail-closed security model.

## The shipped examples

| File | Demonstrates |
|------|--------------|
| [`issue-to-pr.yaml`](../examples/issue-to-pr.yaml) | The canonical flow: plan → implement → self-review (cross-provider judge) → commit/push/open-PR, with gates, retry, `when:` guards, and a manual + webhook trigger. Validates cleanly. |
| [`fix-flaky-test.yaml`](../examples/fix-flaky-test.yaml) | A kitchen-sink exercising all three step kinds, all three trigger kinds, a slot-pool workspace, `prompt_file`, and a fan-in DAG — with one intentional warning. |
| [`nightly-maintenance.yaml`](../examples/nightly-maintenance.yaml) | A param-less, cron-served workflow for the daemon (refresh deps → verify → summarize → open PR). |
| [`multi-agent-eval.yaml`](../examples/multi-agent-eval.yaml) | `max_parallel` + isolated `scratch:` steps: fan a task out to three agents concurrently, then judge the candidate diffs. |

## Where next

- [Workflow reference](workflow-reference.md) — every field and all 30 diagnostics.
- [`odin` CLI](cli.md) and [`odind` daemon](daemon.md) references.
- [Integration guide](integration-guide.md) — embed `odin-core`, plug in custom
  providers/actions/workspaces/triggers.
- [Architecture](architecture.md) — the layered design and data-flow contracts.
- `cargo doc --open -p odin-core --all-features` — the API reference.

## Developing Odin

```sh
cargo fmt --all
cargo clippy --workspace --all-features --all-targets   # CI runs with -D warnings
cargo test  --workspace --all-features
cargo doc   --workspace --all-features --no-deps
```

The workspace forbids `unsafe`, denies warnings in CI, and runs clippy at the `pedantic`
level. Live provider smoke tests are double-gated (they never run by default):

```sh
ODIN_LIVE_PROVIDER_TESTS=1 cargo test -p odin-core live_claude_smoke -- --ignored --nocapture
```
