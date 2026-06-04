# Workflow reference

A workflow is a YAML file describing a directed acyclic graph of steps that Odin runs.
This page documents **every field** of the schema and **every validator diagnostic**
(`ODIN001`–`ODIN044`).

Two phases govern a workflow file, and it helps to keep them distinct:

- **Parsing** (`Workflow::from_yaml_str`) is fail-fast and only catches *structural*
  problems — malformed YAML, an unknown field on a nested object, or a step that declares
  more than one kind. Parsing also rejects a workflow whose `schema_version` **major**
  differs from the engine's.
- **Validation** (`odin validate`) is the second phase. It assumes the file parsed and then
  runs *every* rule, collecting *all* problems into one report so you see them in a single
  pass. Validation is where semantic checks live (unknown providers, dependency cycles, bad
  template references, …).

```sh
odin validate path/to/workflow.yaml          # human-readable, all problems at once
odin validate --json path/to/workflow.yaml   # machine-readable diagnostics
```

---

## The workflow at a glance

```yaml
schema_version: "1.0"            # optional; only the major gates compatibility
name: issue-to-pr                # REQUIRED — the workflow's stable id + display name
version: "1.0.0"                 # optional; your semantic version of the content
description: …                   # optional
durable: true                    # optional, default true — checkpoint runs for crash-resume
workspace: { type: worktree }    # optional, default a per-run git worktree
max_parallel: 3                  # optional; >1 runs independent steps concurrently

triggers:                        # optional; empty = manual-only
  - type: manual
  - type: cron
    schedule: "0 3 * * *"

params:                          # optional; the typed inputs callers supply
  issue_url: { type: string, required: true }

defaults:                        # optional; per-step fallbacks
  timeout: "30m"

steps:                           # REQUIRED — the DAG
  - id: plan
    provider: claude
    prompt: "Read {{ params.issue_url }} and write a plan to plan.md."
```

---

## `Workflow` (root)

| Key | Type | Required | Default | Meaning |
|-----|------|----------|---------|---------|
| `schema_version` | `"MAJOR.MINOR"` string | no | `"1.0"` | File-format version. Only the **major** gates compatibility (a mismatched major is a parse error); a newer **minor** loads but warns ([ODIN026](#odin026)). |
| `name` | string | **yes** | — | Stable identity and display name of the workflow. |
| `version` | string | no | — | *Your* semantic version of the workflow content. Opaque to the engine. |
| `description` | string | no | — | Free-text description. |
| `durable` | bool | no | `true` | Checkpoint runs to the [store](#durability--resume) so a crashed run resumes. |
| `workspace` | [`WorkspaceConfig`](#workspaces) | no | `{ type: worktree }` | How each run's working directory is provisioned. |
| `max_parallel` | integer ≥ 1 | no | `1` | Max steps running at once within a run. `1` (or omitted) is sequential. See [concurrency](#concurrency). |
| `triggers` | list of [`TriggerDecl`](#triggers) | no | `[]` (manual-only) | What starts a run. Served by the `odind` daemon. |
| `params` | map of name → [`ParamSpec`](#parameters) | no | `{}` | Typed inputs. Insertion order is preserved. |
| `defaults` | [`WorkflowDefaults`](#defaults) | no | empty | Per-step fallbacks (`timeout`, `retry`). |
| `steps` | list of [`Step`](#steps) | **yes** | — | The DAG. Must be non-empty ([ODIN001](#odin001)). |

The root object **tolerates unknown keys** (they parse) but a stray key is reported as a
warning ([ODIN025](#odin025)) so typos surface. Every nested object below instead **rejects**
unknown fields at parse time.

### Defaults

```yaml
defaults:
  timeout: "20m"          # default per-step wall-clock timeout
  retry: { max: 1 }       # default retry policy
```

---

## Steps

A step is exactly one of six **kinds**, chosen by which key it carries:

| Kind | Key | Body runs |
|------|-----|-----------|
| **provider** | `provider:` | a coding-agent CLI (`claude` / `codex` / `copilot`, or your own) with a rendered prompt |
| **run** | `run:` | a shell command line in the step's working directory |
| **action** | `action:` | a registered named side-effect (e.g. `github.open_pr`) |
| **approval** | `approval:` | nothing — it **pauses** the run for a human to approve or reject ([below](#approval-step)) |
| **case** | `case:` | nothing — a **selector** that records which branch to take ([below](#case-step)) |
| **loop** | `loop:` | a block of steps, iterated until a verification condition holds ([below](#loop-step)) |

Declaring zero or more than one kind is a **parse error**, as is putting a provider-only key
(`prompt`/`prompt_file`) on a `run`/`action`/`approval`/`case`/`loop` step, or `with:` on a non-action step.

### Common step fields (all kinds)

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `id` | string | — (**required**) | Stable id, unique within the workflow. Must start with a letter or `_`, then letters/digits/`_` (no hyphens — the id is used as a dotted template path segment, `steps.<id>.…`) ([ODIN002](#odin002)–[ODIN004](#odin004)). |
| `depends_on` | list of step ids | `[]` | Steps that must finish before this one. Defines the DAG edges. |
| `when` | string | — | A minijinja boolean expression; the step is **skipped** when it renders false. |
| `gates` | map name → shell command | `{}` | After the body, every gate command must exit `0` or the step **fails**. Order preserved. |
| `judge` | [`JudgeSpec`](#judge) | — | Score the step's output with an LLM and fail it below a threshold. |
| `retry` | [`RetrySpec`](#retry) | no retry | Re-attempt the step on failure. |
| `timeout` | duration string | workflow `defaults.timeout`, else none | Wall-clock limit for the body (e.g. `"15m"`). |
| `artifacts` | [`Artifacts`](#artifacts) | empty | Named data-flow on top of the shared workdir. |
| `scratch` | bool | `false` | Run in an **isolated** throwaway worktree instead of the shared one (see [concurrency](#concurrency)). |

### Provider step

```yaml
- id: plan
  provider: claude            # a registered provider key (ODIN005)
  prompt: |                   # an inline minijinja template …
    Read {{ params.issue_url }} and write a plan to plan.md.
  # prompt_file: prompts/plan.j2   # … OR a template file (not both — ODIN009)
```

A provider step must have exactly one of `prompt` / `prompt_file` ([ODIN006](#odin006),
[ODIN009](#odin009)). The rendered prompt is passed to the provider, which runs the agent in
the step's working directory.

### Run step

```yaml
- id: build
  run: "cargo build --workspace"    # a shell command line, templated
```

### Action step

```yaml
- id: open_pr
  action: github.open_pr            # a registered action name (ODIN010)
  with:                             # templated args for the action
    title: "Implement {{ params.issue_url }}"
    body: "{{ steps.review.outputs.stdout }}"
```

Built-in actions: `shell.exec`, `git.commit`, `git.push`, `github.open_pr`.

### Approval step

A **human-in-the-loop gate**: the run *pauses* here (status `awaiting-approval`) until a person
approves or rejects it — letting you run agents unattended while keeping a human in control of
the risky, outward-facing step.

```yaml
- id: gate
  approval:
    message: "Review the diff, then approve to push."   # shown to the approver (optional, templated)
  depends_on: [review]
- id: push
  action: git.push                                       # only runs once the gate is approved
  depends_on: [gate]
```

The workflow must be `durable: true` ([ODIN032](#odin032)) — a pause is resumed from the store.
Decide it with `odin approve`/`odin reject` (see the [CLI reference](cli.md)) or the daemon's
`POST /approve`. **Approve** → the gate passes and downstream proceeds. **Reject** → the gate
*fails* (downstream skips), and the reviewer's note is surfaced as `steps.<gate>.outputs.feedback`
— the input to act on for a re-run. A paused run is **not** crash-resumed; it waits indefinitely
for a decision.

To close the loop, `reject --rerun` (and `POST /approve` with `"rerun": true`) fails the gate and
then starts a **fresh run** of the same workflow with the note injected as `params.feedback`
(alongside the original run's params). Reference it with `{{ params.feedback }}` — declaring a
`feedback` string param — so the agent addresses the feedback and tries again:

```yaml
params:
  feedback: { type: string, description: "Reviewer feedback (set by reject --rerun)." }
steps:
  - id: implement
    provider: claude
    prompt: |
      {{ params.task }}
      {% if params.feedback %}A previous attempt was rejected — address: {{ params.feedback }}{% endif %}
```

`feedback` must be a **string** param if declared (the note is injected as one; a mismatched type
is refused before the gate is failed). It carries the approver's free text — prefer interpolating
it into a **provider prompt** (as above) over a `run:`/shell command, where untrusted text would
reach a shell unescaped.

### Case step

A **conditional-branching selector**. It evaluates each branch's `when:` guard in order and records
the **first** matching label (or `else`) as `steps.<id>.outputs.selected`. It **always passes** —
branching is a decision, not a failure.

```yaml
- id: route
  depends_on: [classify]
  case:
    branches:
      - { label: bug,  when: "steps.classify.outputs.stdout | trim == 'bug'" }
      - { label: docs, when: "steps.classify.outputs.stdout | trim == 'docs'" }
    else: other                  # selected when no guard matches (optional; default is "")
```

You write the branch **bodies** as ordinary steps gated on the decision:

```yaml
# a branch entry gates on `selected`; further steps in the branch just depend on it (and so skip
# with it on the other branch — skip-propagation), so a multi-step body needs only the one guard.
- id: fix_bug
  provider: claude
  depends_on: [route]
  when: "steps.route.outputs.selected == 'bug'"
  prompt: "Fix the bug."
- id: commit
  action: git.commit
  depends_on: [fix_bug]          # no guard needed
  with: { message: "Fix the bug" }

# a merge-back depends on the SELECTOR (which always passes), so it runs after the decision on
# every path — unlike depending on the branch heads, where the not-taken branch's `Skipped`
# status would make the join skip itself.
- id: summary
  run: "echo 'took {{ steps.route.outputs.selected }}'"
  depends_on: [route]
```

A guard is a minijinja boolean checked like a [`when:`](#common-step-fields-all-kinds); a guard
referencing an unknown root is flagged ([ODIN017](#odin017)). A `case:` step must declare at least
one branch ([ODIN033](#odin033)) with unique ([ODIN034](#odin034)), non-empty ([ODIN035](#odin035))
labels, and shouldn't carry `gates:`/`judge:` ([ODIN036](#odin036)). The branches are tried in
order, so a guard-less branch (an explicit catch-all) shadows any branch after it. See
[`examples/triage.yaml`](../examples/triage.yaml).

> A merge-back depends on the selector, so it runs once the branch is **decided**, not once the
> chosen branch's **work** finishes. To order a step after a branch's work, make it part of that
> branch (depend on the branch's last step), as `commit` does above.

### Loop step

A `loop:` step iterates a **block** of steps until a verification condition holds — the multi-step
sibling of single-step [`retry` feedback](#retry). It is the natural shape for an
**edit → verify → fix** cycle that needs more than one step per round.

```yaml
- id: make_it_pass
  loop:
    until: "steps.test.status == 'passed'"   # minijinja boolean, checked AFTER each iteration
    max: 5                                     # required cap — a loop is always bounded
    steps:
      - id: edit
        provider: claude
        prompt: |
          Make the failing test pass.
          {% if loop.counter > 1 %}Attempt {{ loop.counter }}. Last failure:
          {{ loop.feedback }}{% endif %}
      - id: test
        run: "cargo test"
        depends_on: [edit]
```

- **do-while:** the body always runs at least once; `until` is evaluated *after* each iteration (so
  it can reference an inner step's `status`). Guard the loop's entry with the loop step's own
  `when:` if you need it to sometimes not run at all.
- **Accumulate, not rewind:** workdir edits persist across iterations (unlike `retry`, which
  rewinds), so iteration *N+1* builds on *N*. The cumulative `{{ artifacts.DIFF }}` is refreshed
  inside the body.
- **`loop.*` context** is available to the body (and `until`): `loop.counter` (1-based iteration)
  and `loop.feedback` (the prior iteration's failure, capped). See [Templating](#templating).
- **Outcome:** `until` holding → the loop **passes** with `outputs.iterations` (the count) and
  `outputs.converged: true`; hitting `max` without it (or an `until` error) → the loop **fails**
  (with `iterations`/`converged: false`), and its dependents skip.
- **Body scope:** inner step ids share the one flat namespace and must be **globally unique**
  ([ODIN003](#odin003)); the body is a self-contained sub-DAG — an inner `depends_on` may name only
  a sibling ([ODIN012](#odin012)), while outer data flows in via templates (`{{ steps.<outer>… }}`,
  legal when the loop `depends_on` that outer step). Inner steps may be any kind **except**
  `loop:` ([ODIN040](#odin040)) or `approval:` ([ODIN041](#odin041)).
- A loop must declare a non-blank `until` ([ODIN037](#odin037)), `max >= 1`
  ([ODIN038](#odin038)), and at least one body step ([ODIN039](#odin039)); `gates:`/`judge:` on the
  loop *node* are inert ([ODIN042](#odin042)) — its verification is the body's `until`.

On a durable run, a crash mid-loop **resumes from the last completed iteration** (a per-iteration
workspace snapshot), so completed iterations are not re-run. (Once an inner step *commits*,
snapshots disengage and a crash restarts the loop from the first iteration on the committed work.)
See [`examples/iterate.yaml`](../examples/iterate.yaml).

### Gates

```yaml
- id: implement
  provider: codex
  prompt: "Implement the plan in PLAN."
  gates:
    build: "cargo build --workspace"   # all gates must exit 0,
    test:  "cargo test --workspace"    # else the step Fails (and may retry)
```

### Judge

Score a step's output against criteria with a (usually *different*) provider:

```yaml
- id: review
  provider: claude
  prompt: "Review this diff:\n{{ artifacts.DIFF }}"
  judge:
    provider: codex                    # ODIN005; same-provider judge warns (ODIN021)
    criteria: "Implements PLAN with no regressions or weakened tests."
    threshold: 0.7                      # 0.0..=1.0 (ODIN011); default 0.5
```

The judge provider is prompted to return `{"score": <0.0–1.0>}`; below `threshold` the step
fails.

### Retry

```yaml
retry:
  max: 2                      # additional attempts after the first (0 = no retry)
  backoff: exponential        # `fixed` (default) or `exponential`
  feedback: concise           # `off` (default), `concise`, or `verbose`
  on_fallback_provider: codex # parsed + validated, but INERT in v1 (ODIN023)
```

A step with no `retry:` of its own inherits the workflow [`defaults.retry`](#defaults); a step that
sets one overrides the default wholesale. (Because `retry: { max: 0 }` is indistinguishable from
"unset", a step can't *explicitly* opt out of a default — give it any non-default `retry:` to take
control.)

A plain `retry` re-runs the step *blind* — the next attempt sees exactly the same prompt as
the first. Set **`feedback`** to close the loop into an **implement → verify → redo** cycle:
each attempt after the first re-renders the step's prompt with the prior failure available under
a `retry.*` template root, so the agent can read what went wrong and correct it. The feedback is
the failure's *diagnostic*: a gate's **combined stdout+stderr** (so test-runner output, which most
tools write to stdout, is included), the failing command's stderr, or the judge's verdict.

| `feedback` | What the next attempt sees in `retry.feedback`                                   |
| ---------- | -------------------------------------------------------------------------------- |
| `off`      | nothing — `retry.feedback` is empty (a blind retry)                              |
| `concise`  | the **first non-blank line** of the diagnostic — a brief signal                  |
| `verbose`  | the **full** diagnostic (capped at ~2 KB, most-recent output kept)               |

Prefer **`verbose`** when the agent needs to *act on* the failure (compiler errors, a failing
assertion span across several lines); `concise` is a lightweight signal best paired with output
whose first line is the error. Two notes: `verbose` enlarges the prompt (and so the token cost) of
each retry; and like any free text, `retry.feedback` can carry tool output — interpolate it into a
provider `prompt` (as below), **not** into a `run:`/gate shell command, where untrusted text would
reach a shell unescaped (cf. [ODIN031](#odin031)).

The `retry.*` root resolves in every template position. In a step body it reflects the current
attempt; in a step-level `when:` guard (evaluated once, before any attempt) it is the pre-attempt
state — `retry.attempt == 1`, `retry.feedback` empty.

| Field            | Value                                                            |
| ---------------- | --------------------------------------------------------------- |
| `retry.attempt`  | 1-based attempt number (`1` on the first try, `2` on the first retry, …) |
| `retry.feedback` | the prior attempt's diagnostic, shaped by `feedback` (empty when `off` or on attempt 1) |

```yaml
- id: implement
  provider: claude
  prompt: |
    Fix the failing test.
    {% if retry.attempt > 1 %}
    Your previous attempt failed:
    {{ retry.feedback }}
    {% endif %}
  gates: { test: "cargo test" }
  retry: { max: 3, feedback: verbose }
```

### Artifacts

A lightweight data-flow declaration on top of the shared working directory:

```yaml
artifacts:
  requires: [PLAN]       # must exist before the step runs (ODIN008/ODIN015)
  produces: [PLAN]       # the step is expected to produce these (ODIN007)
```

`DIFF` is **reserved** — the engine captures it automatically (the cumulative git diff of
the run), so a step may not declare `produces: [DIFF]` ([ODIN019](#odin019)) but any step may
reference `{{ artifacts.DIFF }}`.

---

## Parameters

Typed inputs, validated against the caller's `RunInput` at run start:

```yaml
params:
  issue_url:
    type: string         # string (default) | number | bool
    required: true       # default false
    description: "URL of the issue to implement."
  attempts:
    type: number
    default: 3           # used when not supplied (don't combine with required → ODIN022)
```

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `type` | `string` \| `number` \| `bool` | `string` | Expected value type. **Enforced**: a supplied (or default) value of the wrong type fails the run at start with an input error. |
| `required` | bool | `false` | Caller must supply it. |
| `default` | any | — | Value when not supplied. Must match `type` ([ODIN030](#odin030)). |
| `description` | string | — | Human description. |

Reference params in templates as `{{ params.<name> }}` (dot notation — a subscript like
`params["<name>"]` isn't statically checked, [ODIN029](#odin029)). A declared-but-never-
referenced param warns ([ODIN024](#odin024)).

---

## Templating

Prompts, `run` commands, action `with:` values, gate commands, and `when:` expressions are
[minijinja](https://docs.rs/minijinja) templates rendered against a per-step context (judge
`criteria` is reference-checked the same way but passed to the judge model verbatim, **not**
rendered):

| Reference | What it is |
|-----------|-----------|
| `{{ params.<name> }}` | A declared workflow param's value. |
| `{{ steps.<id>.outputs.<key> }}` | A named output of an **upstream** step. Provider and `run:` steps expose `stdout`; an action step exposes only what that action returns (`shell.exec` → `stdout`; `git.commit` → `sha`, `branch`; `git.push` → `branch`, `remote`; `github.open_pr` → `url`, `number`). A `scratch` step additionally exposes `diff`. |
| `{{ steps.<id>.status }}` | An upstream step's outcome as a snake_case string (`passed`, `failed`, `skipped`, …). Use it in a `when:` guard, e.g. `when: "steps.review.status == 'passed'"`. |
| `{{ steps.<id>.exit_code }}` | An upstream step's process exit code, e.g. `when: "steps.build.exit_code == 0"`. |
| `{{ artifacts.DIFF }}` | The cumulative git diff captured so far (vs the run's base commit, refreshed after each passing non-`scratch` step). |
| `{{ trigger.* }}` | The free-form trigger payload (e.g. a webhook event body). |
| `{{ run.id }}` / `{{ run.workflow }}` | This run's id and its workflow name. |
| `{{ retry.attempt }}` / `{{ retry.feedback }}` | This step's 1-based attempt number and the prior attempt's failure (see [Retry](#retry)). Empty on the first attempt. |
| `{{ loop.counter }}` / `{{ loop.feedback }}` | Inside a [`loop:`](#loop-step) body: the 1-based iteration and the prior iteration's failure. `1` / empty outside a loop. |

References are checked statically: an unknown `params`/`steps`/`artifacts` reference, or a
`steps.<id>` that isn't an upstream dependency, is [ODIN017](#odin017); a template that
doesn't compile is [ODIN018](#odin018). (Template-reference checking requires the
`templating` feature, which is on by default.)

---

## Workspaces

Each run gets an isolated working directory, provisioned one of two ways:

```yaml
workspace: { type: worktree }                 # default: a throwaway git worktree per run
# workspace: { type: worktree, base: develop } # cut from a specific branch/ref

workspace:                                     # or: a pool of pre-cloned slots
  type: slot_pool
  pool: 4                                       # number of slots (>= 1 — ODIN016)
  reset: git_clean                              # git_clean (default) | reclone
  base: main                                    # optional base ref
```

- **`worktree`** — `git worktree add` a fresh, detached working tree per run; removed on
  completion. The default.
- **`slot_pool`** — a fixed pool of `pool` checkouts, claimed per run and reset between uses
  (`git_clean` = `git reset --hard && git clean -fdx`; `reclone` = re-clone from origin).

---

## Triggers

Triggers declare *what starts a run*. They are served by the [`odind` daemon](daemon.md);
`odin run` ignores them (it's an explicit, manual run). An empty `triggers` list means
manual-only.

```yaml
triggers:
  - type: manual

  - type: cron
    schedule: "0 3 * * *"        # standard 5-field cron; day-of-week is POSIX (0/7=Sun)

  - type: github_webhook
    events: ["issues.labeled"]   # "<event>" matches any action; "<event>.<action>" is exact
    repo: marlboro-red/odin      # optional owner/repo filter
    params:                      # optional: map run params from the event payload by dot-path
      issue_url: issue.html_url
```

- **`manual`** — fired by `odin run` or an API call. (Future options like an approval gate
  are reserved, which is why it's written as `type: manual` with an object body.)
- **`cron`** — a standard **5-field** cron expression (`min hour dom month dow`), validated
  by [ODIN020](#odin020). Day-of-week is **POSIX** (`0`/`7` = Sunday, `1` = Monday) and times
  are **UTC**. See the [daemon docs](daemon.md#cron-triggers).
- **`github_webhook`** — matched against incoming GitHub deliveries by event/action and an
  optional `repo` filter. The full event is delivered to the run as `trigger.*`; `params`
  extracts specific fields into typed run params by dot-path (so a webhook can satisfy a
  required param). A `params` key not declared in the workflow's `params` warns
  ([ODIN027](#odin027)). See the [daemon docs](daemon.md#webhook-triggers).

---

## Concurrency

By default steps run one at a time. Set `max_parallel: N` to run independent steps
concurrently, up to `N` at once:

```yaml
max_parallel: 3
steps:
  - { id: cand_a, provider: claude,  scratch: true, prompt: "Implement {{ params.task }}" }
  - { id: cand_b, provider: codex,   scratch: true, prompt: "Implement {{ params.task }}" }
  - { id: cand_c, provider: copilot, scratch: true, prompt: "Implement {{ params.task }}" }
  - id: judge
    provider: claude
    depends_on: [cand_a, cand_b, cand_c]
    prompt: |
      Pick the best candidate:
      {{ steps.cand_a.outputs.diff }}
      {{ steps.cand_b.outputs.diff }}
      {{ steps.cand_c.outputs.diff }}
```

The safety rule: all steps in a run share **one** working directory, so a step that mutates
it (any non-`scratch` step) runs **exclusively** — never beside another step. Steps marked
**`scratch: true`** run in their *own* isolated throwaway worktree (cut from the run's base),
so any number of them run **concurrently**. A scratch step's edits never touch the shared
tree; its diff is exposed as `{{ steps.<id>.outputs.diff }}` for a downstream step to
consume. This makes multi-agent fan-out safe without merging concurrent agent edits. Setting
`scratch: true` on an *action* step is inert (its side effects are discarded) and warns
([ODIN028](#odin028)).

---

## Durability & resume

When `durable: true` (the default), the engine checkpoints run state to a SQLite store at
every step boundary, so a crashed or killed run **resumes** where it left off (`odind`
resumes incomplete runs on startup). For durable runs the engine also takes an off-branch
git snapshot of the workspace after each shared-workdir step and restores to it on resume, so
an interrupted step re-applies from a clean tree instead of double-applying its file edits.
(This covers the uncommitted working-tree phase; once a step `git commit`s, git's own commits
are the durable record and snapshotting disengages. See the
[architecture notes](architecture.md).)

---

## Diagnostics catalogue (`ODIN001`–`ODIN044`)

Run `odin validate` to see these. **Errors** make a workflow invalid (it won't run);
**warnings** are runnable but suspicious or inert. Validation collects *all* of them at once.

| Code | Severity | Fires when |
|------|----------|-----------|
| <a id="odin001"></a>ODIN001 | error | The workflow has no steps. |
| <a id="odin002"></a>ODIN002 | error | A step id is empty (or whitespace-only). |
| <a id="odin003"></a>ODIN003 | error | Two steps share the same id. |
| <a id="odin004"></a>ODIN004 | error | A step id isn't a valid identifier / template path segment (start with a letter or `_`; then letters/digits/`_`; no hyphens). |
| <a id="odin005"></a>ODIN005 | error | A `provider`, `judge.provider`, or `retry.on_fallback_provider` names an unregistered provider (with a "did you mean"). |
| <a id="odin006"></a>ODIN006 | error | A provider step has neither `prompt` nor `prompt_file`. |
| <a id="odin007"></a>ODIN007 | error | A step lists the same `produces` artifact twice. |
| <a id="odin008"></a>ODIN008 | error | A step `requires` an artifact that no step produces (`DIFF` is exempt). |
| <a id="odin009"></a>ODIN009 | error | A provider step sets *both* `prompt` and `prompt_file`. |
| <a id="odin010"></a>ODIN010 | error | An action step's `action` is not registered (with a "did you mean"). |
| <a id="odin011"></a>ODIN011 | error | A `judge.threshold` is outside `0.0..=1.0`. |
| <a id="odin012"></a>ODIN012 | error | A `depends_on` entry names an unknown step (with a "did you mean"). |
| <a id="odin013"></a>ODIN013 | error | A step depends on itself. |
| <a id="odin014"></a>ODIN014 | error | The `depends_on` graph has a cycle (the cycle path is shown). |
| <a id="odin015"></a>ODIN015 | error | A required artifact is produced somewhere, but not by an upstream dependency of the requiring step (add it to `depends_on`). |
| <a id="odin016"></a>ODIN016 | error | A `slot_pool` workspace has `pool < 1`. |
| <a id="odin017"></a>ODIN017 | error | A template references an unknown variable, or a `steps.<id>` that isn't an upstream dependency.¹ |
| <a id="odin018"></a>ODIN018 | error | A templated string has a syntax error.¹ |
| <a id="odin019"></a>ODIN019 | error | A step `produces` the reserved `DIFF` artifact. |
| <a id="odin020"></a>ODIN020 | error | A cron `schedule` isn't a valid 5-field expression. |
| <a id="odin021"></a>ODIN021 | **warning** | A step is judged by the *same* provider that produced it. |
| <a id="odin022"></a>ODIN022 | **warning** | A param is both `required` and has a `default` (the default is unreachable). |
| <a id="odin023"></a>ODIN023 | **warning** | `retry.on_fallback_provider` is set but inert in v1. |
| <a id="odin024"></a>ODIN024 | **warning** | A declared param is never referenced in an inline template.¹ (Prompt-file contents aren't scanned.) |
| <a id="odin025"></a>ODIN025 | **warning** | An unknown key at the workflow root (typo, or written for a newer schema). |
| <a id="odin026"></a>ODIN026 | **warning** | `schema_version` minor is newer than this engine supports. |
| <a id="odin027"></a>ODIN027 | **warning** | A `github_webhook` trigger maps a param not declared in `params` (the mapping is inert). |
| <a id="odin028"></a>ODIN028 | **warning** | An *action* step sets `scratch: true` (its side effects are discarded). |
| <a id="odin029"></a>ODIN029 | **warning** | A template accesses a checked root (`params`/`steps`/`artifacts`) with **subscript** syntax (`steps["a"]`); only dot notation is statically checked, so the reference escapes the unknown-ref / upstream checks. |
| <a id="odin030"></a>ODIN030 | error | A param's `default` value does not match its declared `type`. |
| <a id="odin031"></a>ODIN031 | **warning** | An untrusted `trigger.*` value is interpolated into a shell command — a `run:` step, a gate, or `shell.exec`'s `command` — so a webhook payload reaches `sh -c` unescaped (injection risk). Map the fields you trust into typed `params`.¹ |
| <a id="odin032"></a>ODIN032 | error | A workflow with an `approval` gate is not `durable` — a paused gate is persisted and resumed from the store, so it can't be approved without durability. |
| <a id="odin033"></a>ODIN033 | error | A `case:` step declares no `branches` (it could only ever select `else`, or nothing); give it at least one branch. |
| <a id="odin034"></a>ODIN034 | error | Two branches of one `case:` (or a branch and the `else`) share a `label`, so `selected` is ambiguous. |
| <a id="odin035"></a>ODIN035 | error | A `case:` branch has an empty `label`. |
| <a id="odin036"></a>ODIN036 | **warning** | A `case:` selector carries `gates:`/`judge:` — inert (it has no output to check), and a failing gate would flip the always-passing selector to failed and break a merge-back that depends on it. |
| <a id="odin037"></a>ODIN037 | error | A `loop:` step has a blank `until` — no exit condition. |
| <a id="odin038"></a>ODIN038 | error | A `loop:` step's `max` is 0 — it could never run an iteration (use `>= 1`). |
| <a id="odin039"></a>ODIN039 | error | A `loop:` step declares no body `steps`. |
| <a id="odin040"></a>ODIN040 | error | A `loop:` body nests another `loop:` — unsupported (v1). |
| <a id="odin041"></a>ODIN041 | error | A `loop:` body contains an `approval:` gate — unsupported (v1). |
| <a id="odin042"></a>ODIN042 | **warning** | A `loop:` node carries its own `gates:`/`judge:`/`scratch:` — inert; a loop's verification is its `until` over the body. |
| <a id="odin043"></a>ODIN043 | error | A `loop:` body step sets `scratch: true` — unsupported; the body runs sequentially on the shared workdir, so the step's edits would vanish into a throwaway worktree. |
| <a id="odin044"></a>ODIN044 | **warning** | A `durable: true` workflow uses a `slot_pool` workspace — its lease state is in-memory and lost on restart, so a resumed run may race another for its slot; prefer `worktree`. |

¹ ODIN017, ODIN018, ODIN024, ODIN029, and ODIN031 require the `templating` feature (on by default).

A workflow validates **cleanly** when it has zero diagnostics; it's still **runnable** with
warnings (only errors block a run). See [`examples/fix-flaky-test.yaml`](../examples/fix-flaky-test.yaml)
for a workflow that intentionally trips two documented warnings ([ODIN023](#odin023) — an inert
`on_fallback_provider`; and [ODIN044](#odin044) — pairing `durable` with a `slot_pool` workspace).
