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
      ↳ gate "tests" failed: cargo test exited 101
  ⊘ review
usage: 18992 in / 1043 out tokens, $0.0571
error: step "implement" failed after 2 attempt(s)
```

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

Statuses serialize snake_case (`pending`, `running`, `succeeded`, `failed`, `cancelled` for a
run; `pending`, `running`, `passed`, `failed`, `skipped` for a step). `cost_micros` is integer micro-dollars (cost is display-only; the engine never
loses precision to floats).

---

## Live provider runs

A `run:`-only or action-only workflow executes with no API cost. A `provider:` step invokes
the real agent CLI (`claude` / `codex` / `copilot`), which must be installed, on `PATH`, and
authenticated. Pin a provider per step with `provider:` and a judge with `judge.provider`.
