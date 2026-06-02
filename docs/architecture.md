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
│  Runners     odin (CLI, today)          odind (daemon, later) │
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
│       ├── engine.rs              # Engine trait + builder (frozen API)     (runtime)
│       └── mock.rs                # Noop trait impls for tests              (mock)
├── odin-cli/                      # `odin` binary (validate implemented)
└── odin-daemon/                   # `odind` binary (stub)
```

## The integration surface

Five object-safe (`Arc<dyn _>`, via `async-trait`) traits. Each has one required method
plus defaulted optionals, and exchanges owned, serializable context/outcome structs so
implementations can live in other crates — or, later, other processes.

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
variant type, not a crate-wide god-error), and each enum ends in a `#[non_exhaustive]`
`Other(anyhow::Error)` escape hatch.

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
named **artifacts** layer explicit handoffs on top. The engine auto-captures the git
diff after each step as the reserved artifact `DIFF`.

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
| ODIN015 | error | a required artifact's producer is an upstream dependency |
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

Structural problems caught at *parse* time (and so not in this table) include unknown
nested fields, invalid durations, and a step with zero or more than one kind.

## Durability

A durable run is checkpointed to the `Store` at every step boundary; `checkpoint` must be
atomic (old-or-new, never partial). On startup the engine calls `load_incomplete` and
resumes each non-terminal run. `RunState` carries enough to resume deterministically: the
original `RunInput`, per-step progress, the resolved artifact catalogue, the workspace
lease, and the provider versions actually used.

## Security & trust boundaries

A workflow is **executable code**: `run:` steps and gate commands are rendered (with
the run's `params` and `trigger_payload` interpolated) and executed via `sh -c`, and
provider steps drive autonomous coding agents. Treat a workflow file and its inputs with
the same trust as a shell script you are about to run.

- **`params` and `trigger_payload` are interpolated into shell commands without
  escaping.** They are surfaced as `params.*` / `trigger.*` and can be referenced from
  `run:` / gate templates. A workflow *author* controls the template, so this is safe for
  author-supplied params — but a `trigger_payload` assembled from an **untrusted source**
  (a webhook) must never be interpolated raw into a `run:`/gate command. The daemon
  milestone, which turns external events into runs, must enforce this boundary (quote/
  escape, or restrict untrusted values to provider prompts only).
- **Run agents in a sandbox.** Provider steps execute real coding-agent CLIs with
  file/shell access in the run's workspace. Per-run worktrees and the slot pool isolate
  the working tree, not the host; run the engine where that blast radius is acceptable.
- **`prompt_file` is contained** under the repository root (absolute paths and `..`
  escapes are rejected), and git invocations use `--` to prevent argument injection from
  config values.

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

## Milestone status

**Done:** workspace scaffold, IR, validator + 28 diagnostics, templating/context model,
the five traits + registry + engine façade + mocks, `odin validate`, examples, full test
suite (clippy-pedantic clean, docs clean).

**Next:** the SQLite `Store`, the worktree and slot-pool `Workspace`s, the `Provider`
adapters (claude/codex/copilot) with robust subprocess management, the step executor
(gates + judge), built-in `Action`s, and the daemon's triggers.

[`RunInput`]: https://docs.rs/odin-core/latest/odin_core/api/struct.RunInput.html
[`RunSummary`]: https://docs.rs/odin-core/latest/odin_core/api/struct.RunSummary.html
