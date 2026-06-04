# Integration guide

Odin is **library-first**: the `odin` CLI and the `odind` daemon are thin runners over the
`odin-core` crate. This guide shows how to embed Odin in your own program — drive workflows
from code, plug in custom providers/actions/workspaces/triggers, and consume the structured
results.

> The minimal embed example below is also a **compiled doctest** in the crate root
> (`cargo test -p odin-core --doc`), so it can't silently rot. The snippets here mirror it.

## Contents

- [Add the dependency](#add-the-dependency) · [Feature flags](#feature-flags)
- [Run a workflow from code](#run-a-workflow-from-code)
- [Data in / data out](#data-in--data-out)
- [The five integration traits](#the-five-integration-traits)
- [Registering custom plugins](#registering-custom-plugins)
- [Selecting models](#selecting-models)
- [Durability & resume](#durability--resume)
- [Errors](#errors)
- [Embedding the daemon](#embedding-the-daemon)
- [Built-ins reference](#built-ins-reference)

---

## Add the dependency

```toml
[dependencies]
# Odin is not yet published to crates.io — the `odin-core` name there is an UNRELATED crate, so
# `cargo add odin-core` would pull the wrong thing. Depend on git until it's published:
odin-core = { git = "https://github.com/marlboro-red/odin" }   # omitting features → the `full` set
```

`odin-core`'s public API is plain, serializable data plus five small object-safe traits.
Everything you need to embed Odin lives here.

## Feature flags

| Feature | Pulls in | Use it for |
|---------|----------|------------|
| `ir` | serde only | parse + validate workflows |
| `templating` | minijinja | render prompts/conditionals and statically check `{{ refs }}` |
| `runtime` | tokio, async-trait, rusqlite | the five traits, the registry, the durable store |
| `mock` | (`runtime`) | in-memory test doubles (`EchoProvider`, `MemStore`, `NoopAction`, …) for *your* tests |
| `full` *(default)* | `ir` + `templating` + `runtime` (**not** `mock`) | running workflows, the CLI, the daemon |

A parse-only embedder (a linter, an editor plugin) pulls in none of `tokio`:

```toml
odin-core = { git = "https://github.com/marlboro-red/odin", default-features = false, features = ["ir", "templating"] }
```

Note: the `Engine`/`EngineBuilder` require **both** `runtime` and `templating` (they're in
`full`).

---

## Run a workflow from code

```rust
use std::sync::Arc;
use odin_core::{EngineBuilder, RunInput, RunStatus, SqliteStore};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Build an engine over a git repo, with a durable SQLite store for crash-resume.
    let store = SqliteStore::open("runs.db")?;
    let engine = EngineBuilder::new()
        .repo("/path/to/repo")
        .store(Arc::new(store))
        .build()?;

    // Load and run a workflow.
    let workflow = odin_core::Workflow::from_yaml_path("issue-to-pr.yaml")?;
    let input = RunInput::manual().param("issue_url", "https://github.com/o/r/issues/1");
    let summary = engine.run(&workflow, input).await?;

    println!("status: {:?}", summary.status);          // Succeeded / Failed / …
    for step in &summary.steps {
        println!("  {} {:?}", step.id, step.status);
    }
    println!("usage: ${:.4}", summary.usage.cost_usd());
    Ok(())
}
```

`EngineBuilder::new()` seeds the registry with the built-in providers and actions. Without a
`.store(...)`, durable workflows still run — they just aren't checkpointed (no resume).
`Engine::run` first validates the **workflow** and returns `Error::Validation` (carrying every
diagnostic) if it has errors; it then resolves the run's **params** against the workflow's
declared `params`, returning `Error::Input` if a required one is missing.

The `Engine` trait is the whole driving surface:

```rust
#[async_trait]
pub trait Engine: Send + Sync {
    async fn run(&self, workflow: &Workflow, input: RunInput) -> Result<RunSummary>;
    async fn resume_all(&self, workflows: &[Workflow]) -> Result<Vec<RunSummary>>;
    async fn summary(&self, run_id: RunId) -> Result<Option<RunSummary>>;
}
```

---

## Data in / data out

**`RunInput`** carries the requirements into a run — plain, serializable, build it however
you like (deserialize from JSON, construct in code):

```rust
let input = RunInput::manual()
    .param("issue_url", "https://…")     // typed params, validated against the schema
    .param("attempts", 3);
// or carry a free-form event body reachable as `trigger.*` in templates:
let input = RunInput::manual().with_trigger("github_webhook", serde_json::json!({ "issue": { … } }));
```

| Field | Meaning |
|-------|---------|
| `trigger` | which trigger this run corresponds to (default `"manual"`). |
| `params` | typed inputs, validated/coerced against the workflow's param schema. |
| `trigger_payload` | free-form event body, surfaced as `trigger.*` in templates. |
| `idempotency_key` | reserved for **run-level** dedup ("don't start a second run for this key"); not yet acted on. (Distinct from *side-effect* idempotency on resume, which the built-in actions already handle.) |

**`RunSummary`** is the result — pure data, no engine internals or trait objects, safe to
serialize over any transport:

| Field | Meaning |
|-------|---------|
| `run_id` / `workflow` / `status` | run identity and terminal `RunStatus` (`Succeeded`/`Failed`/`Cancelled`/…). |
| `steps` | `Vec<StepResult>` in topological order — each with `status` (`Passed`/`Failed`/`Skipped`), `attempts`, `exit_code`, `outputs`, `gates`, `judge_score`, `usage`, and `error` (the failure reason, if any). |
| `usage` | aggregate `Usage` (`input_tokens`, `output_tokens`, `cost_micros`). |
| `side_effects` | structured outward effects for downstream automation. |
| `diff` | the cumulative git diff captured as the implicit `DIFF` artifact. |
| `error` | the terminal error, iff `status == Failed`. |
| `started_at` / `finished_at` | when the run started, and when it finished (`None` while still running). |

`SideEffect` is a tagged enum — `PullRequest { url, number }`, `Commit { sha, branch }`,
`Push { branch, remote }`, `Comment { url }`, `Artifact { name, path }` — so a caller can,
say, find the PR a run opened without scraping logs. `Usage.cost_micros` is integer
micro-dollars (no float drift); `cost_usd()` is display-only.

---

## The five integration traits

Implementing a new coding agent, workspace strategy, persistence backend, side-effect, or
event source is **one trait, usually one file**. All five are object-safe (used as
`Arc<dyn Trait>`), `Send + Sync`, implemented via `#[async_trait]`, and exchange owned,
mostly-serializable context/outcome structs so impls can live in *your* crate.

> **Three things to know before you start.** (1) Annotate every `impl` with the re-exported
> `async_trait` macro, so you neither add nor version-match the `async-trait` crate (a
> mismatched version otherwise surfaces as a cryptic `E0195`). Either fully qualify it,
> `#[odin_core::async_trait]`, or `use odin_core::async_trait;` once and write `#[async_trait]`
> — the examples below use the latter. `anyhow` and `serde_json` are re-exported the same way
> (`odin_core::anyhow`, `odin_core::serde_json`) — handy because trait errors wrap
> `anyhow::Error` and the API exchanges `serde_json::Value`. (2) Import the traits **and** their
> context/outcome structs from the crate root — `use odin_core::{Provider, InvocationCtx,
> InvocationOutcome, Action, ActionCtx, ActionOutcome, Workspace, WorkspaceHandle, …};` — they're
> all re-exported there. Several of these structs are `#[non_exhaustive]`, so build
> them with their constructors (`InvocationOutcome::success`, `ActionOutcome::success().with_*`,
> `SideEffect::pull_request`/`comment`/…, `WorkspaceHandle::new`, `TriggerEvent::new`), not struct
> literals. (3) A trait method returns the plain `Result<T, ThatTraitsError>` shown in its
> signature (e.g. `Result<InvocationOutcome, ProviderError>`) — i.e. `std::result::Result`,
> **not** the crate's own `odin_core::Result<T>` alias (which is `Result<T, odin_core::Error>`);
> writing the latter in an `impl` is an `E0053` type mismatch. A complete, compiled
> custom-Provider + custom-Action example lives at
> [`crates/odin-core/examples/custom_plugin.rs`](../crates/odin-core/examples/custom_plugin.rs)
> (`cargo run -p odin-core --example custom_plugin`).

### `Provider` — invoke a coding-agent CLI

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderRef;                                    // registry key, e.g. "claude"
    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError>;
    async fn version(&self) -> Option<String> { None }             // optional
    async fn health_check(&self) -> Result<(), ProviderError> { Ok(()) }  // optional
}
```

`invoke` receives an `InvocationCtx { step_id, workdir, prompt, inputs, timeout, cancel }`
(the prompt is already rendered; `inputs` maps required artifacts to on-disk paths) and
returns an `InvocationOutcome`. Build it with `InvocationOutcome::success(stdout)` or
`::failure(exit_code)`, chaining `.with_stderr(..)` / `.with_output(k, v)` / `.with_usage(..)`
/ `.with_produced(name, path)` (it's `#[non_exhaustive]`). The `outputs` map is exposed to
later steps as `steps.<id>.outputs.*`. A provider must **not** touch the store or git — the
engine owns durability and DIFF capture.

```rust
struct EchoProvider;

#[async_trait]
impl Provider for EchoProvider {
    fn id(&self) -> ProviderRef { ProviderRef::new("echo") }
    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError> {
        Ok(InvocationOutcome::success(ctx.prompt.unwrap_or_default()))
    }
}
```

`ctx.cancel` is a clonable `CancelToken`; honor it for long-running work (await
`cancel.cancelled()` or check `is_cancelled()`).

### `Workspace` — provision an isolated working directory

```rust
#[async_trait]
pub trait Workspace: Send + Sync {
    fn kind(&self) -> &str;                                                  // "worktree" | "slot_pool" | …
    async fn acquire(&self, ctx: AcquireCtx) -> Result<WorkspaceHandle, WorkspaceError>;
    async fn release(&self, handle: WorkspaceHandle) -> Result<(), WorkspaceError>;  // idempotent
}
```

`acquire(AcquireCtx { run_id, config })` returns a handle built with `WorkspaceHandle::new(run_id,
path, branch, token)` — `path` should be absolute, `token` is your opaque reclaim handle. The
handle is `Serialize` because the engine persists it in `RunState` to reattach on resume. The
built-ins are `WorktreeWorkspace` (a throwaway `git worktree` per run) and `SlotPoolWorkspace`
(a pool of reusable clones).

> **Selection caveat.** A workflow's `workspace.type` is a *closed* set today — `worktree` or
> `slot_pool`. Registering a custom `Workspace` under one of those `kind()`s (via
> `register_workspace`) **overrides** that built-in (last-writer-wins), but introducing a
> brand-new `type:` string from YAML is not yet wired. So you can swap the implementation of a
> built-in workspace kind, but not add a third selectable kind without an engine change.

### `Store` — durable, crash-resumable persistence

```rust
#[async_trait]
pub trait Store: Send + Sync {
    async fn checkpoint(&self, state: &RunState) -> Result<(), StoreError>;     // atomic, at step boundaries
    async fn append_event(&self, run_id: RunId, event: &RunEvent) -> Result<(), StoreError>;
    async fn load_incomplete(&self) -> Result<Vec<RunState>, StoreError>;       // crash-recovery entry
    async fn load_run(&self, run_id: RunId) -> Result<Option<RunState>, StoreError>;
    async fn recent(&self, limit: usize) -> Result<Vec<RunState>, StoreError> { Ok(vec![]) }  // optional
    async fn events(&self, run_id: RunId) -> Result<Vec<RunEvent>, StoreError> { Ok(vec![]) } // optional
}
```

`RunState` is `Serialize`, so a backend can persist it as one opaque blob — implement
`checkpoint`/`load_incomplete`/`load_run` and you have crash-resume. (`RunState`/`RunEvent`/
`StepState` are `#[non_exhaustive]` and have no public constructor: a store *round-trips* them
via serde, it doesn't fabricate them — which is all a durable backend needs.) The ships-with backend is
[`SqliteStore`] (`open(path)` or `open_in_memory()`); `MemStore` (the `mock` feature) is an
in-memory one for tests. `RunEvent` is the audit log: `RunStarted`, `StepStarted`,
`GateResult`, `JudgeResult`, `StepFinished`, `RunFinished`.

### `Action` — a named side-effect

```rust
#[async_trait]
pub trait Action: Send + Sync {
    fn name(&self) -> &str;                                          // the name authors reference in `action:`
    async fn run(&self, ctx: ActionCtx) -> Result<ActionOutcome, ActionError>;
}
```

`run(ActionCtx { step_id, workdir, args })` — `args` are the step's templated `with:` values —
returns an `ActionOutcome`. Build it with `ActionOutcome::success().with_output(k, v)` /
`.with_side_effect(e)`; a `SideEffect` (constructed via `SideEffect::pull_request`/`comment`/
`commit`/`push`/`artifact`) records an outward effect in the run summary. Built-ins:
`shell.exec`, `git.commit`, `git.push`, `github.open_pr` (the last is idempotent — it
reattaches to an existing open PR on the head branch instead of duplicating).

### <a id="trigger"></a>`Trigger` — a source of run-starting events

```rust
#[async_trait]
pub trait Trigger: Send + Sync {
    fn kind(&self) -> &str;                                                       // "manual" | "cron" | "github_webhook" | …
    async fn next_event(&mut self) -> Result<Option<TriggerEvent>, TriggerError>; // &mut self; cancel-safe
}
```

`next_event` blocks until the next event, or returns `Ok(None)` when the source is exhausted
(manual = one event then `None`; cron/webhook never end). It's the one trait method taking
`&mut self`. A `TriggerEvent` carries the `source`, the target `workflow`, and a `RunInput` —
build it with `TriggerEvent::new(source, workflow, input)`. The daemon drives one task per
trigger; see [embedding the daemon](#embedding-the-daemon).

---

## Registering custom plugins

The `Registry` holds your providers, actions, workspaces, and triggers, keyed by their
`id()`/`name()`/`kind()`. Add yours through the builder:

```rust
let mut builder = EngineBuilder::new()           // seeds the built-ins
    .repo("/path/to/repo")
    .store(Arc::new(SqliteStore::open("runs.db")?));
builder.registry_mut()
    .register_provider(Arc::new(MyProvider))      // resolves `provider: my-agent`
    .register_action(Arc::new(MyAction))          // resolves `action: my.thing`
    .register_workspace(Arc::new(MyWorkspace));   // overrides the built-in `kind()` it returns
let engine = builder.build()?;
```

> `registry_mut()` takes `&mut self` while `repo`/`store` take `self` by value — so bind the
> builder to a `let mut`, call the value-setters first, then `registry_mut()`, then `build()`.

The validator recognizes your plugins too: `Registry::known_names()` gives a `KnownNames`
(provider + action names) you pass to `validate`, so a workflow referencing your custom
provider validates cleanly:

```rust
let report = odin_core::validate(&workflow, &registry.known_names());
```

---

## Selecting models

The built-in providers are thin wrappers around the `claude` / `codex` / `copilot` CLIs.
Odin passes **no** credentials and, by default, **no** model — it invokes the CLI with a
fixed argument vector and lets the CLI use whatever it is logged into and configured for.
There are two ways to control which model runs.

> The fixed argument vector also sets each provider's **default autonomy**: `codex` runs
> `--sandbox workspace-write --skip-git-repo-check`, `copilot` runs `--allow-all` (tools,
> paths, **and network**), and `claude` adds nothing. `with_extra_args` *replaces* those
> flags, so use it to tighten (e.g. `codex … --sandbox read-only`) or loosen — see the
> autonomy table in [architecture.md](architecture.md#security--trust-boundaries).

**Globally, via the CLI's own config (no code).** Because the child process inherits your
environment, the model the CLI is configured to use is the model that runs. Set it where the
CLI looks — e.g. `export ANTHROPIC_MODEL=…` for Claude Code, codex's `~/.codex/config.toml`,
or copilot's config — before launching `odin`/`odind`. This applies to every step that uses
that CLI.

**Per provider, via the builder (`with_model`).** Each built-in exposes `with_model`, which
appends `--model <name>` to every invocation. The model is a separate field, so the pin
survives a later `with_extra_args` — but note `with_extra_args` *replaces* (does not append
to) the sandbox/permission defaults, so re-supply those in your args if you call it. Set the
model through `with_model` only — don't *also* put `--model` in `with_extra_args`, or the CLI
receives two `--model` flags:

```rust
use odin_core::{ClaudeProvider, CodexProvider};

ClaudeProvider::new().with_model("claude-opus-4-8");   // claude -p … --model claude-opus-4-8
CodexProvider::new().with_model("gpt-5.2-codex");      // codex exec … --model gpt-5.2-codex <prompt>
```

**Mixing models in one workflow (`with_id` + `with_model`).** A provider is registered under
its `id()`. Give each instance a distinct id and register several, then target them from
steps by name:

```rust
use std::sync::Arc;
use odin_core::{EngineBuilder, ClaudeProvider, SqliteStore};

let mut builder = EngineBuilder::new()
    .repo("/path/to/repo")
    .store(Arc::new(SqliteStore::open("runs.db")?));
builder.registry_mut()
    .register_provider(Arc::new(
        ClaudeProvider::new().with_id("planner").with_model("claude-opus-4-8"),
    ))
    .register_provider(Arc::new(
        ClaudeProvider::new().with_id("reviewer").with_model("claude-sonnet-4-6"),
    ));
let engine = builder.build()?;
```

```yaml
steps:
  - { id: plan,   provider: planner,  prompt: "..." }
  - { id: review, provider: reviewer, prompt: "...", depends_on: [plan] }
```

Reusing a built-in id (`"claude"`) **replaces** that built-in (last writer wins), so
`register_provider(Arc::new(ClaudeProvider::new().with_model("…")))` re-pins the default
`provider: claude` without adding a new name.

> **Validation caveat.** A custom id like `planner` is known to the engine you build —
> `Engine::run` validates against the live registry, so the run is fine — but the standalone
> `odin validate` CLI only knows the three built-in names and will report
> [ODIN005](workflow-reference.md#odin005) ("unknown provider") for `provider: planner`. To
> validate such a workflow in your own tooling, pass your registry's names:
> `odin_core::validate(&workflow, &registry.known_names())`.

---

## Durability & resume

With a store and `durable: true` workflows, the engine checkpoints `RunState` at every step
boundary. To recover after a crash, call `resume_all` on startup with the workflows you serve:

```rust
let resumed = engine.resume_all(&workflows).await?;   // resumes incomplete runs from the store
```

Recovery is per-run (one run's failure doesn't abort the others). A run whose workspace path
is gone is failed cleanly; a run targeting a workflow you no longer pass is skipped. For
durable runs the engine also snapshots the workspace off-branch and restores it on resume so
an interrupted step re-applies from a clean tree — see the
[architecture notes](architecture.md). Each step's **side effects** are persisted
(`StepState.side_effects`) and reconstructed into the resumed summary, so a crash never drops a
PR/commit/push from the record; and the built-in side-effecting actions are idempotent across
their non-atomic boundary (`github.open_pr` reattaches to an existing open PR rather than
duplicating; `git.push` of pushed commits is a no-op). A **custom** `Action` that creates an
external resource should do the same (query-before-create) to be resume-safe.

---

## Errors

`odin_core::Result<T>` is `Result<T, Error>`. `Error` is a `#[non_exhaustive]` enum organized
by phase — `Parse`, `Io`, `Validation(ValidationReport)`, `SchemaVersion`, `Input`,
`Unregistered`, `Template`, `Unimplemented` — plus a transparent wrapper per trait
(`Provider`/`Workspace`/`Store`/`Action`/`Trigger`Error). Each trait error has an
`Other(#[from] anyhow::Error)` variant, so a custom impl can wrap arbitrary errors:

```rust
async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError> {
    let out = call_my_agent(&ctx).await.map_err(anyhow::Error::from)?;  // → ProviderError::Other
    Ok(InvocationOutcome::success(out))
}
```

`Error::Validation` carries the full `ValidationReport`; `report.into_result()` is `Err` iff
there are *error*-severity diagnostics (warnings alone pass).

---

## Embedding the daemon

The `odin-daemon` crate turns events into runs. Compose a [`Daemon`] (the supervisor loop +
concurrency + graceful drain) with cron triggers (derived from workflows) and a
[`WebhookServer`], all sharing one shutdown token. Embedding it adds two dependencies
beyond `odin-core`:

```toml
[dependencies]
odin-daemon = "0.0.1"
tokio = { version = "1", features = ["full"] }   # for the runtime + tokio::join!
```

```rust
use std::sync::Arc;
use odin_core::ir::TriggerDecl;
use odin_daemon::{Daemon, WebhookServer};

// `engine: Arc<dyn Engine>` and `workflows: Vec<Workflow>` from the steps above;
// `secret: String` is your GitHub webhook HMAC secret. (This body is inside an
// `async fn` returning `anyhow::Result<()>`.)

// 1. Subscribe every `github_webhook` trigger to a shared HTTP server — BEFORE the
//    workflows move into the daemon. `subscribe` returns the pull-side `Trigger` to register.
let mut server = WebhookServer::new("127.0.0.1:9292".parse()?, Some(secret));
let mut webhook_triggers = Vec::new();
for workflow in &workflows {
    for decl in &workflow.triggers {
        if let TriggerDecl::GithubWebhook(github) = decl {
            webhook_triggers.push(server.subscribe(github, workflow.name.clone()));
        }
    }
}

// 2. Build the daemon (this moves `workflows`). Cron triggers are derived from each
//    workflow's `triggers:`; tune concurrency, then register the webhook triggers.
let mut daemon = Daemon::from_workflows(engine, workflows)?.with_max_concurrent_runs(8);
for trigger in webhook_triggers {
    daemon.add_trigger(Box::new(trigger));
}

// 3. Cancel this token (e.g. on ctrl-c) to stop accepting events and drain in-flight runs.
let shutdown = daemon.cancellation_token();

// 4. Drive the supervisor loop and the HTTP server together; both end on shutdown.
//    `tokio::join!` yields one `Result` per task — propagate them, don't drop them.
let bound = server.bind().await?;
let (daemon_res, server_res) = tokio::join!(daemon.run(), bound.serve(shutdown));
daemon_res.and(server_res)?;
```

To add an *entirely new* event source (a message queue, a poller), implement
[`Trigger`](#trigger) and `daemon.add_trigger(Box::new(MyTrigger))`. The daemon resumes
incomplete runs on startup, dispatches up to `max_concurrent_runs` at once, logs (never
crashes on) a failing run, and drains in-flight runs on shutdown.

---

## Built-ins reference

| Kind | Built-ins (`odin_core::…`) |
|------|----------------------------|
| Providers | `ClaudeProvider`, `CodexProvider`, `CopilotProvider` — each `::new()` plus `.with_id(..)` / `.with_model(..)` / `.with_program(..)` / `.with_extra_args(..)` (see [Selecting models](#selecting-models)) |
| Actions | `ShellExec` → `shell.exec`, `GitCommit` → `git.commit`, `GitPush` → `git.push`, `OpenPr` → `github.open_pr` |
| Workspaces | `WorktreeWorkspace::new(repo)`, `SlotPoolWorkspace::new(repo, pool_dir, size, reset)` |
| Store | `SqliteStore::open(path)`, `SqliteStore::open_in_memory()` |
| Mocks (`mock` feature) | under `odin_core::mock::` — `EchoProvider`, `TmpWorkspace`, `MemStore`, `NoopAction`, `ScriptedTrigger` |

For the full type-level API, run `cargo doc --open -p odin-core --all-features`.

[`SqliteStore`]: https://docs.rs/odin-core/latest/odin_core/storage/struct.SqliteStore.html
[`Daemon`]: https://docs.rs/odin-daemon/latest/odin_daemon/struct.Daemon.html
[`WebhookServer`]: https://docs.rs/odin-daemon/latest/odin_daemon/struct.WebhookServer.html
