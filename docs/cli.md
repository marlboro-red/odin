# The `odin` CLI

`odin` is the command-line runner: validate a workflow, run one, and inspect past runs in
the durable store. (For event-driven execution — cron and webhooks — see the
[`odind` daemon](daemon.md).)

```
odin <command> [args]

Commands:
  validate <FILE|RECIPE>   Parse and validate a workflow, reporting all diagnostics
  run <FILE|RECIPE>        Run a workflow to completion
  list                     List recent runs from the store
  show <RUN_ID>            Show a run's details
  logs <RUN_ID>            Show a run's event log
  status                   At-a-glance status of recent runs (--watch to live-refresh)
  approve <RUN_ID>         Approve a run paused at an approval gate
  reject  <RUN_ID>         Reject a paused run (optionally rerun with feedback)
  cancel  <RUN_ID>         Request cancellation of an in-flight run
  prune                    Delete old/excess terminal runs from the store
  recipe <SUBCOMMAND>      Manage the by-name workflow recipe catalog
```

Wherever a command takes a workflow `<FILE>`, you can give a **recipe name** instead — a bare
name is resolved against the [recipe catalog](#odin-recipe-subcommand) when no file exists at
that path. An existing file path always wins.

Every command that produces a result takes `--json` for machine-readable output on **stdout** —
`validate`, `run`, `list`, `show`, `logs`, `status`, `prune`, `recipe list`, **`approve`**,
**`reject`**, and **`cancel`** (the `recipe init/add/show/path/new` management commands have no
`--json`). Under `--json`, stdout is **always** the JSON document and nothing else — including on
failures: `validate --json` and `run --json` emit the same envelope `{ "ok": false, "phase":
"validate" | "parse" | "error", "diagnostics": [...], "error": <string|null> }`, `approve`/`reject`
emit `{ "ok": false, "error": … }` on a not-found/error, and `cancel` emits `{ "run_id", "requested":
false }` — so a script never gets empty stdout, and `… --json | jq` is always safe. (The non-`--json`
human report differs by command: `validate` prints its diagnostics to **stdout** as its primary
output, while `run`'s per-step diagnostics and `✗ … error` lines go to **stderr** so stdout stays
the summary channel; a parse error always goes to stderr.) Either way a failure exits non-zero.

---

## `odin validate <FILE|RECIPE> [--json]`

Parse and validate a workflow, reporting **all** diagnostics at once.

| Flag | Meaning |
|------|---------|
| `<FILE\|RECIPE>` | Path to a workflow YAML file, or a [recipe name](#odin-recipe-subcommand) (required). |
| `--recipes-dir <DIR>` | Override the recipe-catalog directory used to resolve a name. |
| `--json` | Emit the diagnostics report as JSON instead of human-readable text. |

```sh
$ odin validate examples/issue-to-pr.yaml
✓ examples/issue-to-pr.yaml is valid

$ odin validate examples/fix-flaky-test.yaml
warning[ODIN044]: workflow is `durable` but uses a `slot_pool` workspace, whose lease state is in-memory and lost on restart …
  --> workspace

warning[ODIN023]: on_fallback_provider is declared but routing/fallback is not implemented in v1; this field is inert
  --> steps[1].retry.on_fallback_provider

✓ examples/fix-flaky-test.yaml is valid (2 warning(s))
```

`--json` emits a single envelope: `{ "ok": <bool>, "phase": "validate" | "parse", "diagnostics":
[ <diagnostic>, … ], "error": <string|null> }` — `ok` is true iff there are no error-severity
diagnostics (warnings keep it true), `phase` is `parse` for a malformed file (with `error` set and
`diagnostics: []`), else `validate`. `odin run --json` emits the **same** shape on a validation/parse
failure.

**Exit codes:** `0` valid (warnings are OK), `1` validation errors, `2` parse error or file
I/O error.

---

## `odin run <FILE|RECIPE> [flags]`

Run a workflow to completion against a git repository. Steps using `run:` execute for free;
`provider:` steps invoke the real agent CLI.

| Flag | Default | Meaning |
|------|---------|---------|
| `<FILE\|RECIPE>` | — | Path to a workflow YAML file, or a [recipe name](#odin-recipe-subcommand) (required). |
| `--recipes-dir <DIR>` | — | Override the recipe-catalog directory used to resolve a name. |
| `--param <KEY=VALUE>` | — | A typed input param (repeatable). Values parse as JSON when possible (so `42`/`true` are typed), else as a string. |
| `--trigger <NAME>` | `manual` | The trigger name to record for this run. |
| `--repo <DIR>` | `.` | The git repository to provision workspaces from. |
| `--db <FILE>` | `<repo>/.odin/state.db` | The run-state SQLite database. |
| `--no-store` | off | Don't persist run state (no durability / resume). Ignores `--db`. |
| `--mock` | off | Replace `provider:` steps with a mock that echoes their rendered prompt, so a provider-using workflow runs with **no** real agent CLI or authentication — a demo/offline aid (`run:`/`action:` steps still execute for real). |
| `--stream` | off | Tee each `provider:` / `run:` / gate step's subprocess output to **stderr** live (prefixed by step id) as the run proceeds, instead of only the final summary. See below. |
| `--json` | off | Emit the run summary as JSON. |

### `--stream`: watch steps as they run

By default `odin run` prints only a final summary; the diagnostic stream on stderr reports
*step finished* events but not the steps' own output. `--stream` tees every subprocess Odin
spawns — for `provider:`, `run:`, and gate steps — to **stderr** as the bytes arrive, line by
line, each prefixed with the step id so concurrent `scratch:` steps stay legible:

```text
emit │ alpha-line-1
emit │ alpha-line-2
review │ cargo test output…
```

Output goes to stderr so stdout stays a clean channel for the summary / `--json`
(`odin run … --stream 1>summary.txt 2>run.log` separates them). Lines never interleave
mid-line across steps. Caveats: `action:` steps (git/PR) and Odin's internal git calls are not
streamed; and because agent CLIs run **headless** (JSON output mode), a `provider:` step's
streamed stdout is its raw JSON(L) — `codex`/`copilot` show incremental activity, `claude`
emits one final document — so the cleanest live view is `run:`/gate output (builds, tests).
With `--mock`, `provider:` steps echo inline without spawning a subprocess, so they produce
**no** streamed output (only `run:`/gate steps stream). A newline-free stream (a huge one-line
blob, or a `\r`-only progress bar) is flushed in 64 KiB chunks rather than buffered to the end.
Streaming is off by default and never enabled by the daemon.

```sh
odin run examples/issue-to-pr.yaml --repo . --param issue_url=https://github.com/owner/repo/issues/1
odin run issue-to-pr --repo . --param issue_url=…   # same workflow, by recipe name
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

**Exit codes:** `0` the run succeeded **or paused at an approval gate** (awaiting input, not a
failure); `1` the run failed/was cancelled, or the workflow had validation errors; `2` a
parse/IO/engine-build/other runtime error.

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
| `--json` | Emit the resulting `RunSummary` (or, with `--rerun`, `{ "rejected": …, "rerun": … }`) as JSON on stdout, for an approval bot. |
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

## `odin cancel <RUN_ID> [--repo DIR | --db FILE] [--json]`

Requests cancellation of an **in-flight** run. Because the run may be executing inside a separate
`odind` process (whose in-memory cancel tokens this command can't reach), the request is written to
the shared run-state store; the engine running the run polls for it and stops the run terminally
(`cancelled`), killing its current step's subprocess. Cancellation takes effect within a few
seconds. Only **durable** runs (which have a store row) can be cancelled this way — a foreground
`odin run` is instead stopped with ctrl-C.

```sh
odin cancel 6f1c…          # → ⏹ cancel requested for run 6f1c…
odin cancel 6f1c… --json   # → {"run_id":"6f1c…","requested":true}
```

`--json` emits `{ "run_id": …, "requested": <bool> }` on stdout (`requested: false` + exit `2`
when no cancellable run with that id exists).

Exit codes: `0` requested; `2` no cancellable (non-terminal) run with that id, or a store error.

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

## `odin recipe <SUBCOMMAND>`

A **recipe catalog** is a directory of workflow `.yaml` files you can run, validate, and inspect
*by name* instead of by path — `odin run adversarial-review` rather than
`odin run ~/some/long/path/adversarial-review.yaml`. A recipe is just an ordinary workflow file;
its **catalog name is the filename stem** (`adversarial-review.yaml` → `adversarial-review`).

The catalog directory is resolved with this precedence:

1. an explicit `--recipes-dir <DIR>`,
2. the `ODIN_RECIPES_DIR` environment variable,
3. the platform **data-local** directory (via the `directories` crate):

| OS | Default catalog directory |
|----|---------------------------|
| macOS | `~/Library/Application Support/odin/recipes` |
| Linux | `~/.local/share/odin/recipes` (honors `$XDG_DATA_HOME`) |
| Windows | `%LOCALAPPDATA%\odin\data\recipes` |

The catalog starts empty. `odin recipe init` seeds it with the bundled starter recipes (the
[shipped examples](getting-started.md#the-shipped-examples)), which you can then edit freely.

| Subcommand | Meaning |
|------------|---------|
| `recipe list [--tag <T>] [--json]` | List the catalog: each recipe's name, `description:`, and tags (`--tag` filters; unparseable files are listed but flagged). |
| `recipe init [--force]` | Write the bundled starters into the catalog, skipping ones already present (`--force` overwrites). Safe to re-run. |
| `recipe new <NAME> --from <SRC> [--set k=v]…` | Scaffold a new workflow from a recipe/starter/file, optionally filling `@@VAR@@` template variables. Writes `./<NAME>.yaml`, or `--out <PATH>` / `--catalog` / `--stdout`. See [templating](recipe-templating.md). |
| `recipe add <FILE> [--as <NAME>] [--force]` | Copy a workflow file into the catalog under its stem (or `--as <NAME>`). Validates it first; won't overwrite without `--force`. |
| `recipe show <NAME>` | Print the recipe's YAML to stdout (provenance to stderr, so the output pipes cleanly). |
| `recipe path <NAME>` | Print the recipe's resolved filesystem path (for scripting). |

Every subcommand accepts `--recipes-dir <DIR>` to target a catalog other than the default.

```sh
$ odin recipe init
seeded 12 recipe(s) into /Users/you/Library/Application Support/odin/recipes

$ odin recipe list
recipes in /Users/you/Library/Application Support/odin/recipes:

  adversarial-review   Fan adversarial reviewers over a PR diff, synthesize a verdict, and post it on approval.
  issue-to-pr          Plan → implement → self-review → open a PR for a GitHub issue.
  …

$ odin run adversarial-review --repo . --param pr=42   # run it by name
$ odin recipe add ./my-review.yaml                      # add your own
```

**Exit:** `0` on success; `2` when a named recipe can't be found (the error lists what *is*
available), the catalog directory can't be resolved/created, or a file can't be read/written.

---

## JSON shapes

`--json` output is stable, serializable data with no engine internals:

- **`validate --json`** (and `run --json` on a workflow that fails to parse/validate) → the
  envelope `{ "ok": <bool>, "phase": "validate" | "parse" | "error", "diagnostics": [ { "severity",
  "code" ("ODIN0NN"), "message", "pointer", "help" }, … ], "error": <string|null> }`. `ok` is true
  iff there are no error-severity diagnostics; `phase` is `parse` (then `diagnostics: []`, `error`
  set) for a malformed file, `error` for an IO/engine failure, else `validate`.
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

Statuses serialize snake_case (`pending`, `running`, `awaiting_approval`, `succeeded`, `failed`,
`cancelled` for a run; `pending`, `running`, `awaiting_approval`, `passed`, `failed`, `skipped` for
a step). A run that paused at an approval gate exits `0` (it's awaiting input, not a failure). `cost_micros` is integer micro-dollars (cost is display-only; the engine never
loses precision to floats).

---

## Live provider runs

A `run:`-only or action-only workflow executes with no API cost. A `provider:` step invokes
the real agent CLI (`claude` / `codex` / `copilot`), which must be installed, on `PATH`, and
authenticated. Pin a provider per step with `provider:` and a judge with `judge.provider`.

---

## In CI (GitHub Actions, etc.)

Every read/run/mutation command is scriptable, so wrapping Odin in CI is just download → run →
parse. A minimal job that runs a workflow and fails the build if any step failed:

```yaml
# .github/workflows/odin.yml
jobs:
  review:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install odin
        run: |
          TAG=v0.0.5
          TARGET=x86_64-unknown-linux-gnu
          base="https://github.com/marlboro-red/odin/releases/download/$TAG"
          curl -fsSLO "$base/odin-$TAG-$TARGET.tar.gz"
          curl -fsSLO "$base/odin-$TAG-$TARGET.tar.gz.sha256"
          sha256sum -c "odin-$TAG-$TARGET.tar.gz.sha256"   # verify the checksum sidecar
          tar -xzf "odin-$TAG-$TARGET.tar.gz" && sudo mv odin odind /usr/local/bin/
      - name: Run a workflow
        run: |
          # `--mock` runs provider steps without a real agent CLI (drop it for a real run).
          odin run examples/quickstart.yaml --repo . --no-store --mock --json > summary.json
          jq -e '.status == "succeeded"' summary.json   # non-zero exit fails the job
          jq -r '.side_effects[]?' summary.json          # surface PRs/branches pushed, if any
```

Notes for scripting:

- **Exit codes** are stable per command (see each command above): `0` success / awaiting-approval,
  `1` run-failed or workflow-invalid, `2` parse/IO/store error or unknown run.
- On a **validation/parse failure**, `run --json` and `validate --json` emit the
  `{ ok, phase, diagnostics, error }` envelope on stdout — check `.ok` before trusting a result.
- `--json` keeps **stdout pure** (logs and the `--stream` view go to stderr), so `… --json | jq …`
  is always safe.
