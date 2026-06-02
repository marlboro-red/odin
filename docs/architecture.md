# Odin Architecture

This document describes how Odin is put together: the layers, the integration surface,
the data-flow contracts, the validation catalogue, and the durability model. For the
exhaustive, code-level blueprint the foundation was built from, see
[`design/foundation-blueprint.md`](design/foundation-blueprint.md).

## Principles

1. **Library-first.** `odin-core` is the engine. The `odin` CLI and `odind` daemon are
   thin runners. Anything they can do, an embedder can do.
2. **Durable by default.** A run is a sequence of checkpoints. A crash resumes from the
   last completed step rather than restarting.
3. **Pluggable everything.** Providers, workspaces, stores, actions, and triggers are
   traits resolved through a registry. Built-ins and third-party plugins are equals.
4. **Parse is not validate.** Parsing is fail-fast and structural; validation is a
   separate pass that collects *every* semantic problem at once.
5. **Make illegal states unrepresentable**, and where the type system can't, **fail at
   the earliest, most local point** with a precise message.

## Layers

```
┌──────────────────────────────────────────────────────────────┐
│  Runners     odin (CLI)                 odind (daemon)        │
├──────────────────────────────────────────────────────────────┤
│  Engine      Engine trait · EngineBuilder · run lifecycle     │   runtime
├──────────────────────────────────────────────────────────────┤
│  Pluggable   Provider · Workspace · Store · Action · Trigger  │   runtime
│  surface     + Registry                                       │
├──────────────────────────────────────────────────────────────┤
│  Templating  ContextShape · ref-checking · render/eval        │   templating
├──────────────────────────────────────────────────────────────┤
│  Foundation  Workflow IR · Validator (ODIN###) · api · error  │   ir (always)
└──────────────────────────────────────────────────────────────┘
```

The right column is the Cargo feature that gates each layer, so a parse-only consumer
compiles only the bottom band.

## Crate & module layout

```
crates/
├── odin-core/                     # the engine library — the main deliverable
│   └── src/
│       ├── ids.rs                 # newtype ids (WorkflowId, RunId, StepId, …)
│       ├── usage.rs               # token/cost accounting (integer micro-dollars)
│       ├── error.rs               # crate Error + per-trait error enums
│       ├── api.rs                 # RunInput / RunSummary / RunStatus / StepStatus / SideEffect
│       ├── ir/                    # the Workflow IR (serde types, DAG-ready)
│       ├── validate/              # diagnostics, graph (topo/cycle), rules
│       ├── context/               # templating: shape, ref-checking, render  (templating)
│       ├── traits/                # the five integration traits             (runtime)
│       ├── registry.rs            # name → plugin resolution                (runtime)
│       ├── provider/              # claude / codex / copilot adapters       (runtime)
│       ├── action/                # shell.exec / git.commit / git.push / github.open_pr (runtime)
│       ├── workspace/             # worktree + slot-pool workspaces         (runtime)
│       ├── storage/               # SqliteStore                             (runtime)
│       ├── engine/                # Engine trait + builder + local executor (runtime+templating)
│       └── mock.rs                # Noop trait impls for tests              (mock)
├── odin-cli/                      # `odin` binary: validate / run / list / show / logs
└── odin-daemon/                   # `odin_daemon` lib + `odind` binary: cron + webhooks
```

## The integration surface

Five object-safe (`Arc<dyn _>`, via `async-trait`) traits. Each pairs a cheap identity
accessor (`id`/`kind`/`name`) with one or more required core methods — and, for `Provider`
and `Store`, a couple of defaulted optionals — and exchanges owned, serializable
context/outcome structs so implementations can live in other crates — or, later, other
processes.

```rust
trait Provider {                                   // a coding-agent CLI
    fn id(&self) -> ProviderRef;
    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError>;
    async fn version(&self) -> Option<String> { None }
    async fn health_check(&self) -> Result<(), ProviderError> { Ok(()) }
}

trait Workspace {                                  // an isolated per-run workdir
    fn kind(&self) -> &str;
    async fn acquire(&self, ctx: AcquireCtx) -> Result<WorkspaceHandle, WorkspaceError>;
    async fn release(&self, handle: WorkspaceHandle) -> Result<(), WorkspaceError>;
}

trait Store {                                      // durable, crash-resumable state
    async fn checkpoint(&self, state: &RunState) -> Result<(), StoreError>;
    async fn append_event(&self, run: RunId, event: &RunEvent) -> Result<(), StoreError>;
    async fn load_incomplete(&self) -> Result<Vec<RunState>, StoreError>;
    async fn recent(&self, limit: usize) -> Result<Vec<RunState>, StoreError> { Ok(vec![]) }
    async fn load_run(&self, run: RunId) -> Result<Option<RunState>, StoreError>;
    async fn events(&self, run: RunId) -> Result<Vec<RunEvent>, StoreError> { Ok(vec![]) }
}

trait Action {                                     // a named side-effect
    fn name(&self) -> &str;
    async fn run(&self, ctx: ActionCtx) -> Result<ActionOutcome, ActionError>;
}

trait Trigger {                                    // a source of run-starting events
    fn kind(&self) -> &str;
    async fn next_event(&mut self) -> Result<Option<TriggerEvent>, TriggerError>;
}
```

Each trait returns its **own** small error enum (so an implementor reads a focused 3–4
variant type, not a crate-wide god-error). Each enum is `#[non_exhaustive]` (so adding a
variant is non-breaking) and, under the `runtime` feature, carries an
`Other(anyhow::Error)` escape hatch — a parse-only `ir` build has the enum without that
variant.

The **`Store` contract is snapshot-primary**: `checkpoint` persists the whole
`Serialize`-able `RunState`, so a backend (SQLite blob, Postgres `jsonb`, files) can
persist it with zero knowledge of the IR. `append_event` is a secondary audit log, not
the source of truth.

## Data flow

Three contracts move data through a run:

**1. Run input** — the requirements coming in, as [`RunInput`]:

```jsonc
{
  "trigger": "github_issue",
  "trigger_payload": { "issue": { "number": 42, "url": "..." } },  // free-form, → trigger.*
  "params": { "issue_url": "..." }                                  // typed, validated
}
```

**2. Step boundary** — the shared worktree is the primary channel (steps edit files);
named **artifacts** layer explicit handoffs on top. After each **passing, non-`scratch`**
step the engine auto-captures the cumulative git diff (vs the run's base commit) as the
reserved artifact `DIFF`; a `scratch` step's diff stays local to it, exposed as that step's
`outputs.diff`.

```
INPUTS                          STEP                       OUTPUTS
rendered prompt        ┐    ┌───────────┐    ┌─► exit code + stdout/stderr
required artifacts  ───┼───►│ Provider  │────┼─► produced artifacts
workspace path         │    │ / Action  │    ├─► usage (tokens, micro-$)
timeout, cancel        ┘    │ / run:    │    ├─► structured outputs (steps.<id>.outputs.*)
                            └───────────┘    └─► DIFF (auto-captured)
```

Everything in the OUTPUTS column is checkpointed into `RunState` and reachable from later
steps via templating.

**3. Run output** — the results going out, as [`RunSummary`]: status, per-step results,
aggregate `Usage`, structured `side_effects` (PRs, commits, pushes), the captured `DIFF`,
and an error string if it failed. Pure serializable data — no engine internals.

## Templating & context

Prompts, `with:` arguments, gate commands, judge criteria, and `when:` conditionals are
[minijinja](https://docs.rs/minijinja) templates. References are checked **statically**
during validation and evaluated with strict undefined-behavior at run time (a typo is an
error, never a silent empty string).

| Reference | Available | Statically checked |
|-----------|-----------|--------------------|
| `params.<name>` | everywhere | `<name>` must be a declared param |
| `trigger.<…>` | everywhere | root only (open payload) |
| `steps.<id>.outputs.<k>` / `.exit_code` / `.status` | a step, iff `<id>` is a **transitive dependency** | `<id>` must be a declared, upstream step |
| `artifacts.<NAME>` | a step, iff `<NAME>` ∈ its `requires` (or `DIFF`) | `<NAME>` checked |
| `run.<…>` | everywhere | root only |

The dependency-awareness means a `steps.x` reference is only legal if `x` is reachable
through `depends_on`, which stays correct as the DAG fans out.

## Validation catalogue

`validate()` runs every rule and collects all diagnostics; it never short-circuits. Each
maps to a stable `ODIN###` code (serialized as that string). Errors block a run; warnings
do not.

| Code | Sev | Rule |
|------|-----|------|
| ODIN001 | error | workflow has at least one step |
| ODIN002 | error | step id is non-empty |
| ODIN003 | error | step ids are unique |
| ODIN004 | error | step id is a valid template path segment |
| ODIN005 | error | every provider reference is registered (with "did you mean") |
| ODIN006 | error | provider step has a prompt source |
| ODIN007 | error | a step does not produce the same artifact twice |
| ODIN008 | error | every required artifact is produced by some step |
| ODIN009 | error | provider step does not set both `prompt` and `prompt_file` |
| ODIN010 | error | every action reference is registered |
| ODIN011 | error | judge threshold ∈ `0.0..=1.0` |
| ODIN012 | error | `depends_on` targets exist |
| ODIN013 | error | no step depends on itself |
| ODIN014 | error | the dependency graph is acyclic |
| ODIN015 | error | a required artifact's producer must be an upstream dependency of the requiring step |
| ODIN016 | error | a slot pool has `pool >= 1` |
| ODIN017 | error | template references resolve against the context shape |
| ODIN018 | error | templates are syntactically valid |
| ODIN019 | error | no step `produces` the reserved `DIFF` |
| ODIN020 | error | cron schedules are valid 5-field expressions |
| ODIN021 | warning | a step is not judged by the same provider it used |
| ODIN022 | warning | a param is not both `required` and defaulted |
| ODIN023 | warning | `on_fallback_provider` is set (inert in v1) |
| ODIN024 | warning | a declared param is referenced somewhere |
| ODIN025 | warning | unknown field at the workflow root (forward-compat tolerance) |
| ODIN026 | warning | the schema minor is newer than this engine |
| ODIN027 | warning | a `github_webhook` trigger maps a param not declared in `params` (inert) |
| ODIN028 | warning | an `action` step sets `scratch: true` (its side effects are discarded) |

Structural problems caught at *parse* time (and so not in this table) include unknown
nested fields, invalid durations, and a step with zero or more than one kind. The full
per-field catalogue with exact trigger conditions is in the
[workflow reference](workflow-reference.md#diagnostics-catalogue-odin001odin028).

## Concurrency

By default the executor runs one step at a time in dependency order. With `max_parallel: N`
it becomes a bounded ready-set scheduler. The safety rule, given that all steps share one
working directory: a step that mutates the shared tree (any non-`scratch` step) runs
**exclusively** — never beside another step — while `scratch: true` steps run **concurrently**
in their own throwaway worktrees (cut from the run's base). A scratch step's edits never reach
the shared tree; its diff is surfaced as `steps.<id>.outputs.diff` for a downstream step to
consume. This makes multi-agent fan-out safe without merging concurrent agent edits, and a
non-scratch step never races another writer.

The daemon parallelizes a *different* axis: it dispatches independent **runs** concurrently
(bounded by `--max-concurrent-runs`), each in its own isolated workspace.

## Durability

A durable run is checkpointed to the `Store` at every step boundary; `checkpoint` must be
atomic (old-or-new, never partial). On startup the engine calls `load_incomplete` and
resumes each non-terminal run. `RunState` carries enough to resume deterministically: the
original `RunInput`, per-step progress, the resolved artifact catalogue, the workspace
lease, the base commit, and the latest snapshot pointer.

To make resume **idempotent**, a durable run takes an *off-branch* git snapshot of the
workspace after each shared-workdir step (a dangling commit anchored by a per-run ref that's
deleted on completion — it never reaches the workflow's branch or its PR). On resume the
workdir is restored to the last snapshot before re-running, so a step interrupted mid-edit
re-applies from a clean tree instead of double-applying its file changes. This covers the
uncommitted working-tree phase only: once a step `git commit`s, git's own commits are the
durable record and snapshotting disengages (rewinding past a commit would corrupt the run
branch). Side effects *outside* the workspace — a pushed branch, an opened PR — are external
and not covered; design idempotent or `when:`-guarded steps for those.

## Security & trust boundaries

A workflow is **executable code**: `run:` steps and gate commands are rendered (with
the run's `params` and `trigger_payload` interpolated) and executed via `sh -c`, and
provider steps drive autonomous coding agents. Treat a workflow file and its inputs with
the same trust as a shell script you are about to run.

- **`params` and `trigger_payload` are interpolated into shell commands without
  escaping.** They are surfaced as `params.*` / `trigger.*` and can be referenced from
  `run:` / gate templates. A workflow *author* controls the template, so this is safe for
  author-supplied params — but a `trigger_payload` assembled from an **untrusted source**
  (a webhook) must never be interpolated raw into a `run:`/gate command; restrict untrusted
  values to provider prompts, or map only the specific fields you trust into typed params.
  The daemon authenticates webhook deliveries (HMAC-SHA256 over the raw body) and is
  **fail-closed** — it refuses to start a webhook listener without a secret unless
  `--webhook-allow-unsigned` is given — but signature verification proves *origin*, not that
  the payload's contents are safe to interpolate.
- **Run agents in a sandbox.** Provider steps execute real coding-agent CLIs with
  file/shell access in the run's workspace. Per-run worktrees and the slot pool isolate
  the working tree, not the host; run the engine where that blast radius is acceptable.
- **`prompt_file` is contained** under the repository root (absolute paths and `..`
  escapes are rejected). Git is always invoked with a fixed argument vector (never via a
  shell), and config-derived arguments are guarded: `git.push` rejects a remote or branch
  beginning with `-` so it can't be misread as a flag, and diff capture appends a trailing
  `--` to separate revisions from pathspecs.

## Forward-compatibility seams

Kept because they are cheap; everything else was cut as speculative.

- `schema_version: "MAJOR.MINOR"` — unknown **major** is rejected at parse; a newer
  **minor** loads with a warning.
- Every public enum/struct is `#[non_exhaustive]`, so new variants/fields are additive.
- The workflow **root** tolerates unknown keys (warned, ODIN025) so a file written for a
  newer minor still loads; nested config is strict.
- `retry.on_fallback_provider` parses but is inert (warned, ODIN023), so today's YAML
  stays valid when provider routing/fallback ships.
- Providers are string-keyed through the registry rather than a closed enum, so a
  third-party provider needs zero core changes.

## Status

**Implemented & tested:** the workflow IR; the validator (28 diagnostics); the
templating/context model; the five integration traits + registry; the SQLite `Store`; the
worktree and slot-pool `Workspace`s; the `claude`/`codex`/`copilot` `Provider` adapters
(subprocess management, version/health checks, token-usage parsing); the built-in `Action`s
(`shell.exec`, `git.commit`, `git.push`, `github.open_pr`); the executor (dependency order,
gates, LLM-as-judge + retry/backoff, `when:` conditionals, auto-captured `DIFF`); concurrent
step execution (`max_parallel` + isolated `scratch:` steps); durable crash-resume with
per-step off-branch snapshots; the `odin` CLI (`validate` / `run` / `list` / `show` / `logs`);
and the `odind` daemon (cron + signed GitHub webhooks, concurrent run dispatch, graceful
drain). The whole workspace is clippy-pedantic clean with `-D warnings` and `unsafe` forbidden.

**Open refinements:** dollar-cost reporting for codex/copilot (token usage is already parsed;
neither CLI reports a dollar figure); provider routing/fallback (`retry.on_fallback_provider`
parses today but is inert); and engine-level idempotency keys (`RunInput.idempotency_key` is
declared but not yet acted on).

[`RunInput`]: https://docs.rs/odin-core/latest/odin_core/api/struct.RunInput.html
[`RunSummary`]: https://docs.rs/odin-core/latest/odin_core/api/struct.RunSummary.html
