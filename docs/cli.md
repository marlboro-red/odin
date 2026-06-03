# The `odin` CLI

`odin` is the command-line runner: validate a workflow, run one, and inspect past runs in
the durable store. (For event-driven execution — cron and webhooks — see the
[`odind` daemon](daemon.md).)

```
odin <command> [args]

Commands:
  validate <FILE>      Parse and validate a workflow, reporting all diagnostics
  run <FILE>           Run a workflow to completion
  list                 List recent runs from the store
  show <RUN_ID>        Show a run's details
  logs <RUN_ID>        Show a run's event log
  status               At-a-glance status of recent runs (--watch to live-refresh)
  approve <RUN_ID>     Approve a run paused at an approval gate
  reject  <RUN_ID>     Reject a paused run (optionally rerun with feedback)
  prune                Delete old/excess terminal runs from the store
```

Every command takes `--json` for machine-readable output on **stdout**. A command's normal
report — including `validate`'s diagnostics, errors and all — also goes to stdout. Failures go
to **stderr**: `validate`/`run` print `✗ <file>: parse error` on a malformed file, while an
I/O error, a bad UUID, or a store error prints an `error: …` line. Either way the process
exits non-zero.

---

## `odin validate <FILE> [--json]`

Parse and validate a workflow file, reporting **all** diagnostics at once.

| Flag | Meaning |
|------|---------|
| `<FILE>` | Path to the workflow YAML file (required). |
| `--json` | Emit the diagnostics report as JSON instead of human-readable text. |

```sh
$ odin validate examples/issue-to-pr.yaml
✓ examples/issue-to-pr.yaml is valid

$ odin validate examples/fix-flaky-test.yaml
warning[ODIN023]: on_fallback_provider is declared but routing/fallback is not implemented in v1; this field is inert
  --> steps[1].retry.on_fallback_provider

✓ examples/fix-flaky-test.yaml is valid (1 warning(s))
```

`--json` emits the full [`ValidationReport`](#json-shapes) (or, on a parse failure, a
`{"ok": false, "phase": "parse", "error": "…"}` object).

**Exit codes:** `0` valid (warnings are OK), `1` validation errors, `2` parse error or file
I/O error.

---

## `odin run <FILE> [flags]`

Run a workflow to completion against a git repository. Steps using `run:` execute for free;
`provider:` steps invoke the real agent CLI.

| Flag | Default | Meaning |
|------|---------|---------|
| `<FILE>` | — | Path to the workflow YAML file (required). |
| `--param <KEY=VALUE>` | — | A typed input param (repeatable). Values parse as JSON when possible (so `42`/`true` are typed), else as a string. |
| `--trigger <NAME>` | `manual` | The trigger name to record for this run. |
| `--repo <DIR>` | `.` | The git repository to provision workspaces from. |
| `--db <FILE>` | `<repo>/.odin/state.db` | The run-state SQLite database. |
| `--no-store` | off | Don't persist run state (no durability / resume). Ignores `--db`. |
| `--json` | off | Emit the run summary as JSON. |

```sh
odin run examples/issue-to-pr.yaml --repo . --param issue_url=https://github.com/owner/repo/issues/1
```

```text
Run 7f3c… — succeeded
  ✓ plan
  ✓ implement (exit 0)
  ✓ review
usage: 41310 in / 2204 out tokens, $0.1234
side-effects: 1
```

When a step fails, its glyph is `✗`, the first line of the recorded failure reason is printed
beneath it (`↳ …`), downstream steps are `⊘` skipped, and a final `error:` line carries the
run-level terminal error:

```text
Run a91b… — failed
  ✓ plan
  ✗ implement (exit 1)
      ↳ exited with code 1
  ⊘ review
usage: 18992 in / 1043 out tokens, $0.0571
error: step "implement" failed: exited with code 1
```

The `↳` line is the first line of the step's recorded reason — `exited with code N`
(with a `stderr:` tail when there is one), a failed `gate "<name>" failed`, or a
sub-threshold judge. The run-level `error:` line is `step "<id>" failed: <that reason>`.

Step glyphs: `✓` passed, `✗` failed, `⊘` skipped, `·` other. `--json` emits the full
[`RunSummary`](#json-shapes).

**Exit codes:** `0` the run succeeded; `1` the run failed/was cancelled, or the workflow had
validation errors; `2` a parse/IO/engine-build/other runtime error.

---

## Inspecting runs

`list`, `show`, and `logs` read the durable store (the same SQLite database `odin run` and
`odind` write to). They resolve the database from `--db` if given, else `<repo>/.odin/state.db`
(with `--repo` defaulting to `.`). On a missing database they degrade gracefully — `--json`
still emits valid JSON (`[]` / `null`).

### `odin list [flags]`

List the most recent runs, newest first.

| Flag | Default | Meaning |
|------|---------|---------|
| `--repo <DIR>` | `.` | The git repo whose `.odin/state.db` to read. |
| `--db <FILE>` | `<repo>/.odin/state.db` | Database path (overrides `--repo`). |
| `--limit <N>` | `20` | Maximum number of runs to list. |
| `--json` | off | Emit the listing as a JSON array. |

```text
7f3c…  succeeded   issue-to-pr           2026-06-02T07:12:04.512874+00:00
a91b…  failed      nightly-maintenance   2026-06-01T03:00:11.094233+00:00
```

Timestamps are RFC 3339 (`DateTime::to_rfc3339()`): a `+00:00` UTC offset and sub-second
precision, not a bare `…Z`. `--json` emits a reduced projection per run: `{run_id, workflow, status, updated_at}`.
**Exit:** `0` (even with no database or no runs); `2` on a store error.

### `odin show <RUN_ID> [flags]`

Show one run's full state.

| Flag | Meaning |
|------|---------|
| `<RUN_ID>` | The run id (a UUID). |
| `--repo` / `--db` | Database location (as above). |
| `--json` | Emit the full run state as JSON. |

```text
run      7f3c…
workflow issue-to-pr
status   succeeded
created  2026-06-02T07:10:55.318204+00:00
updated  2026-06-02T07:12:04.512874+00:00
steps:
  plan         Passed
  implement    Passed exit 0
  review       Passed
diff     captured
```

`--json` emits the full [`RunState`](#json-shapes). **Exit:** `0` found; `1` not found or no
database; `2` invalid UUID or store error.

### `odin logs <RUN_ID> [flags]`

Show a run's append-only event log (one event per line).

| Flag | Meaning |
|------|---------|
| `<RUN_ID>` | The run id (a UUID). |
| `--repo` / `--db` | Database location (as above). |
| `--json` | Emit the events as a JSON array. |

Events are `run_started`, `step_started`, `gate_result`, `judge_result`, `step_finished`,
`run_finished`. Human mode prints compact JSON per line; `--json` pretty-prints the array.
**Exit:** `0` (incl. empty); `1` no database; `2` invalid UUID or store error.

### `odin status [flags]`

An at-a-glance view of recent runs — the terminal counterpart to the [web
dashboard](daemon.md#dashboard). A summary header (counts by status) over a row per run: a status
glyph, short id, workflow, step progress, and age; an awaiting-approval run shows its gate message.

| Flag | Default | Meaning |
|------|---------|---------|
| `--repo` / `--db` | `.` / `<repo>/.odin/state.db` | Database location (as above). |
| `--limit <N>` | `20` | Maximum number of runs to show. |
| `--watch` | off | Live-refresh every 2 s until `ctrl-c`. |
| `--json` | off | Emit the runs as JSON — the same [`RunView`](#json-shapes) shape as the daemon's `/api/runs`. |

```text
2 running  ·  1 awaiting approval  ·  14 succeeded  ·  1 failed

▸ running   a91b2c3d issue-to-pr          2/4   12s
⏸ awaiting  7f3ce8a1 gated-deploy         2/4    3m  ↳ Ship it?
✓ succeeded 2b8e1f4d nightly              4/4    1m
✗ failed    c4d5a6b7 fix-flaky            1/3    5m
```

**Exit:** `0` (incl. no database — `--json` still emits `[]`); `2` on a store error.

---

## Approving a paused run

A workflow with an [`approval` gate](workflow-reference.md#approval-step) pauses with status
`awaiting-approval` (visible in `odin list`). Resume it with a decision:

```sh
odin approve <RUN_ID> --workflow <FILE> --by alice --note "lgtm"
odin reject  <RUN_ID> --workflow <FILE> --by bob   --note "fix the failing test"
odin reject  <RUN_ID> --workflow <FILE> --by bob   --note "handle empty input too" --rerun
```

| Flag | Meaning |
|------|---------|
| `<RUN_ID>` | The paused run's id. |
| `--workflow <FILE>` | The workflow file the run was started from (needed to resume). |
| `--by <NAME>` | Who is deciding (recorded for the audit trail; default `cli`). |
| `--note <TEXT>` | Free-text note. **Required** on `reject` — it's the feedback, surfaced as `steps.<gate>.outputs.feedback`. |
| `--rerun` | (`reject` only) After failing the gate, start a **fresh run** of the workflow carrying the note as the `feedback` param. |
| `--repo` / `--db` | Database location (as above). |

**Approve** resumes the run (it continues to completion, or pauses again at a later gate).
**Reject** fails the gate (downstream skips) and the run ends `failed`, carrying the note.
The resumed run summary is printed. **Exit:** `0` succeeded or paused again; `1` failed
(incl. a reject); `2` unknown run / not awaiting / store error.

**`reject --rerun`** closes the loop: it fails the gate as above, then immediately starts a new
run of the same workflow with the note injected as `params.feedback` (alongside the original
run's params), so the agent can address it and try again. The workflow opts in by referencing
`{{ params.feedback }}` (declare a `feedback` string param). Both summaries are printed — the
failed original and the fresh run (which typically pauses at the gate again for another look):

```text
Run a91b… — failed
  ✗ gate
      ↳ rejected by bob: handle empty input too
  ⊘ ship
↻ rerunning as 9f2c… with your feedback
Run 9f2c… — awaiting approval
  ✓ implement (exit 0)
  ✓ review
  ⏸ gate
```

A long-running [`odind`](daemon.md) can also be decided over HTTP — a signed
[`POST /approve`](daemon.md#approving-a-paused-run-over-http) is the daemon-side equivalent of
these commands.

---

## `odin prune [flags]`

Bound the store's growth by deleting old or excess **terminal** runs (`succeeded`/`failed`/
`cancelled`) and their event logs, and reclaiming their git snapshot refs. **In-flight and
awaiting-approval runs are never touched** — only terminal runs are eligible. Requires an explicit
age or count limit (it refuses to run with neither).

| Flag | Meaning |
|------|---------|
| `--older-than <DURATION>` | Prune terminal runs last updated longer ago than this — `90d`, `12h`, `2w` (units `s`/`m`/`h`/`d`/`w`). |
| `--keep-last <N>` | Keep at most `N` terminal runs **per workflow** (newest first); prune the rest. |
| `--workflow <NAME>` | Restrict pruning to a single workflow. |
| `--dry-run` | Preview what would be pruned; delete nothing. |
| `--yes` | Skip the confirmation prompt (**required** to prune non-interactively). |
| `--json` | Emit the prune report as JSON. |
| `--repo` / `--db` | Database location (as above). |

Both limits may be combined — a run is pruned only if it satisfies **all** set limits (e.g.
`--older-than 90d --keep-last 200` keeps the newest 200 per workflow *and* anything younger than
90 days). Without `--yes` (and not a `--dry-run`), `prune` previews the selection and prompts
`y/N` on a TTY; **a non-interactive stdin auto-declines**, so an unattended `odin prune` without
`--yes` is a no-op.

```sh
$ odin prune --repo . --keep-last 1 --dry-run
Would prune 3 run(s), 0 event(s):
  3 prune-demo × succeeded

$ odin prune --repo . --older-than 90d --keep-last 200 --yes
Pruned 3 run(s), 12 event(s):
  3 prune-demo × succeeded
```

**Metrics stay correct:** pruning does *not* make the [`odin_runs_total`](daemon.md#metrics)
counter drop — each pruned run is folded into a persistent tally first, so the counter remains
monotonic (it reflects lifetime completions, not just retained rows). `odin list` naturally shows
fewer runs after a prune. **Exit:** `0` (incl. a dry-run, a no-op, or a declined prune); `2` on
an error (no age/count limit given, or a store/engine failure).

---

## JSON shapes

`--json` output is stable, serializable data with no engine internals:

- **`validate --json`** → a `ValidationReport`: `{ "diagnostics": [ { "severity", "code"
  ("ODIN0NN"), "message", "pointer", "help" }, … ] }`.
- **`run --json`** → a `RunSummary`: `{ "run_id", "workflow", "status", "steps": [ {
  "id", "status", "attempts", "exit_code", "outputs", "gates", "judge_score", "usage",
  "error" } ], "usage": { "input_tokens", "output_tokens", "cost_micros" }, "side_effects",
  "diff", "error", "started_at", "finished_at" }`. A failed step's `error` carries the exit
  code + a stderr tail (or the failed gate / sub-threshold judge), and the run-level `error`
  names the first failed step and its reason.
- **`show --json`** → the full `RunState` (the persisted checkpoint).
- **`list --json`** → `[{ "run_id", "workflow", "status", "updated_at" }, …]`.
- **`logs --json`** → an array of `RunEvent` (each tagged by `kind`).
- **`status --json`** → `[ RunView, … ]`, a `RunView` being `{ "run_id", "workflow", "status",
  "updated_at", "steps": [ { "id", "status", "exit_code", "error" } ], "gate": { "step",
  "message" } | null }`. This is the **same shape** the daemon's
  [`GET /api/runs`](daemon.md#dashboard) returns (and `/api/runs/{id}` adds `"diff"` + `"error"`),
  so one schema serves the CLI, the API, and the dashboard.

Statuses serialize snake_case (`pending`, `running`, `succeeded`, `failed`, `cancelled` for a
run; `pending`, `running`, `passed`, `failed`, `skipped` for a step). `cost_micros` is integer micro-dollars (cost is display-only; the engine never
loses precision to floats).

---

## Live provider runs

A `run:`-only or action-only workflow executes with no API cost. A `provider:` step invokes
the real agent CLI (`claude` / `codex` / `copilot`), which must be installed, on `PATH`, and
authenticated. Pin a provider per step with `provider:` and a judge with `judge.provider`.
