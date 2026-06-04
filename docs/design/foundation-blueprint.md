No async deps present. On Rust 1.85, native `async fn` in traits is stable but **not object-safe** for `dyn` dispatch — and the registry/engine needs `Arc<dyn Provider>`. All three lenses independently converged on `async-trait`. I'll lock that decision. I have everything needed to synthesize.

# ODIN FOUNDATION — Authoritative Implementation Blueprint (M0 + M1 + Trait Surface)

> **⚠️ Historical design record.** This is the blueprint the foundation was *built from*,
> not a description of the shipped code. A few things changed during implementation — for
> the **authoritative** validation catalogue and module behavior, see
> [`../architecture.md`](../architecture.md) and the source. Known deviations from §6
> below:
> - "Prompt on an action/run step" is a **parse-time error** (in `StepKind::from_discriminants`),
>   not a diagnostic. The §6 row for it was dropped and the codes renumbered: unknown-root-field
>   is **ODIN025**, newer-schema-minor is **ODIN026**. Since then the live catalogue has grown
>   to **28 codes** — **ODIN027** (`github_webhook` maps an undeclared param) and **ODIN028**
>   (`scratch: true` on an action step) were added with later features. See the
>   [workflow reference](../workflow-reference.md) for the authoritative, current catalogue.
> - In the §6 orchestration sketch: `ODIN024` (unused param) is emitted by
>   `context::refs::check` (templating-gated), not `rules::params` (which emits only ODIN022);
>   and the cycle check is `rules::cycles` (calling `graph::find_cycle`), not `graph::cycles`.

This is the single source of truth. It merges the **extensibility**, **integration**, and **simplicity** blueprints, resolving every conflict in favor of **correctness** and **integration ergonomics**, keeping only *cheap* forward-compat seams, and cutting speculative machinery. It is written against the **actual scaffold** (`resolver=3`, `edition=2024`, `rust-version=1.85`, `serde_yaml_ng`, `indexmap`+serde, `minijinja`, `thiserror 2`, `uuid v4`, `chrono`; lints `unsafe_code=forbid`, `missing_docs=warn`, `unreachable_pub=warn`, clippy pedantic).

---

## 0. Resolved cross-cutting decisions (the rulings every section obeys)

These are the conflicts between the three lenses, decided once:

| # | Decision | Ruling | Rationale |
|---|----------|--------|-----------|
| C1 | **Provider naming: closed enum vs string+registry** | **String-keyed via `Registry`** for the *integration surface* (`Provider::id() -> &str`, validator checks against registry), BUT the IR field deserializes through a `ProviderRef` newtype that is just `String`. **No closed `ProviderId` enum.** | Simplicity lens wanted a closed enum for parse-time errors; but the locked decision says providers are third-party-pluggable. A closed enum forks core. We recover the good error message via a validator rule with Levenshtein "did you mean" (ODIN005). Integration ergonomics wins. |
| C2 | **Step kind encoding** | **Hand-written `Deserialize`** on an internally-discriminated `StepKind` enum (simplicity lens's approach), NOT `#[serde(untagged)]`, NOT three raw `Option` fields surfaced to consumers. | Untagged gives unusable errors; raw options make "exactly-one-of" a runtime check. Hand-written deserialize gives precise messages AND makes both/neither unrepresentable downstream. ~30 lines, load-bearing. |
| C3 | **async traits** | **`async-trait` 0.1**, object-safe `Arc<dyn Trait>`. | Native AFIT is not `dyn`-safe on 1.85; registry needs `dyn`. All three lenses agree. |
| C4 | **Store shape: event-sourced vs snapshot** | **Snapshot-primary** (`checkpoint(&RunState)` persists the whole serializable state) **plus** an `append_event` audit log. `RunState` is `Serialize` so a Store persists a blob with zero IR knowledge. NOT pure event-sourcing. | Snapshot is the simplest correct durability contract and the biggest "easy third-party Store" win. The event log is the cheap seam for replay/audit; it is not the source of truth. |
| C5 | **Per-trait error types vs one mega-error** | **Per-trait error enums** (`ProviderError`, `WorkspaceError`, …), each `#[non_exhaustive]` with an `Other(anyhow::Error)` escape hatch; the crate `Error` `#[from]`-wraps them. | A third-party `Provider` impl reads one 4-variant enum, not a 10-variant god-error. Keeps the surface narrow. Adds `anyhow` to deps. |
| C6 | **Durations** | **Hand-rolled `HumanDuration`** parsing `s`/`m`/`h` (+ bare seconds). No `humantime` crate. | YAGNI; agent timeouts are seconds-to-hours. Parse errors surface at deserialize time with good messages. |
| C7 | **`deny_unknown_fields` policy** | **Deny on ALL leaf config structs** (workspace bodies, retry, judge, params, artifacts) AND on the step. **Allow + warn (ODIN018) on the workflow root only** via a second `Value` parse pass. | Simplicity wants deny-everywhere (typo-proofing); extensibility wants allow-on-envelope (forward-compat minor). We split: leaves are closed (typos are errors), the root tolerates unknown keys as a *warning* so a file authored for a newer minor still loads. This is the one place forward-compat earns its keep. |
| C8 | **`schema_version`** | Required-with-default `SchemaVersion{major,minor}` parsed from `"1.0"`. Engine **rejects unknown major**, **warns on newer minor** (ODIN019). | Cheap format-evolution lever; prevents silent misparse. |
| C9 | **Routing/fallback/competition** | **CUT.** `retry.on_fallback_provider` is parsed-but-inert with warning ODIN016. No `ProviderSelector` enum, no `where_`/`ExecutionTarget`, no `with` bag on provider steps beyond what's needed. | Extensibility lens proposed `ProviderSelector` + `ExecutionTarget` seams. These are speculative. A `provider:` string can grow later; we do not pay for the seam now. The inert field keeps today's YAML valid when routing ships. |
| C10 | **`extra: BTreeMap` extension bags** | **CUT** from ctx/outcome structs. Keep `#[non_exhaustive]` on public structs/enums (free) but no extension bags. | The `extra` bags are speculative complexity; `#[non_exhaustive]` already makes additive fields non-breaking. |
| C11 | **Maps** | **`IndexMap`** for every IR/runtime map (gates, params, outputs, artifacts). | Deterministic iteration is load-bearing for reproducible runs and stable diagnostics. Already a dep. |
| C12 | **Diagnostic codes** | **`ODIN###`** string codes on a closed `DiagCode` enum (the brief's requested format). | Stable, documentable, testable. |
| C13 | **Cost type** | `cost_micros: u64` (integer micro-dollars), never `f64`. | Durable money must not drift. |
| C14 | **Feature flags** | `default = ["full"]`; features `ir`, `templating`, `runtime`, `mock`, `full`. `tokio`/`async-trait`/`anyhow` gated behind `runtime`; `minijinja` behind `templating`. | A parse-only embedder (linter/LSP) pays nothing for tokio. Integration ergonomics. |

---

## 1. MODULE LAYOUT

```
odin/
├── Cargo.toml                         # workspace (EXISTS — unchanged)
├── rust-toolchain.toml                # stable (EXISTS)
├── clippy.toml                        # doc idents (EXISTS)
├── .github/workflows/ci.yml           # fmt+clippy+test+doc (EXISTS)
├── crates/
│   ├── odin-core/
│   │   ├── Cargo.toml                  # add async-trait, tokio(rt), anyhow, futures-core; gate features (§ below)
│   │   └── src/
│   │       ├── lib.rs                  # crate root: module decls + curated flat re-exports + version()
│   │       │
│   │       ├── ids.rs                  # newtype IDs: WorkflowId, RunId, StepId, ArtifactName, ParamName, GateName, ProviderRef
│   │       │
│   │       ├── error.rs                # crate Error + Result; per-trait ProviderError/WorkspaceError/StoreError/ActionError/TriggerError
│   │       │
│   │       ├── ir/                     # the Workflow IR — pure serde data, DAG-ready
│   │       │   ├── mod.rs              #   re-exports; Workflow::from_yaml_str / from_yaml_path
│   │       │   ├── workflow.rs         #   Workflow, Metadata, SchemaVersion, Defaults
│   │       │   ├── workspace.rs        #   WorkspaceConfig (worktree | slot_pool), WorktreeConfig, SlotPoolConfig, ResetMode
│   │       │   ├── trigger.rs          #   TriggerDecl (#[non_exhaustive]: manual | github_webhook | cron)
│   │       │   ├── params.rs           #   ParamSpec, ParamType
│   │       │   ├── step.rs             #   Step, StepKind (hand-written Deserialize), ProviderStep/ActionStep/RunStep,
│   │       │   │                       #     Artifacts, JudgeSpec, RetrySpec, Backoff
│   │       │   └── duration.rs         #   HumanDuration newtype (parse "30s"/"5m"/"2h")
│   │       │
│   │       ├── validate/               # semantic validation pass → ValidationReport (collected diagnostics)
│   │       │   ├── mod.rs              #   validate(&Workflow, &Registry) -> ValidationReport; rule orchestration
│   │       │   ├── diagnostic.rs       #   Diagnostic, Severity, DiagCode (ODIN###), Pointer, ValidationReport
│   │       │   ├── rules.rs            #   every numbered rule as fn(&Workflow, &Registry, &mut Vec<Diagnostic>)
│   │       │   └── graph.rs            #   Kahn topo-sort + cycle detection over depends_on (pub topo_order)
│   │       │
│   │       ├── context/                # templating + the run-context shape (minijinja)
│   │       │   ├── mod.rs              #   re-exports
│   │       │   ├── shape.rs            #   ContextShape: statically-legal references derived from the IR
│   │       │   ├── refs.rs             #   extract {{ }} var paths from templates; check against shape (ODIN017)
│   │       │   └── render.rs           #   build_context(), render_template(), eval_when()
│   │       │
│   │       ├── registry.rs             # Registry: name -> Arc<dyn Provider/Workspace/Action/Trigger>; with_builtins(); register_*
│   │       │
│   │       ├── traits/                 # THE integration surface — 5 traits + ctx/outcome structs  [feature = "runtime"]
│   │       │   ├── mod.rs              #   re-exports
│   │       │   ├── provider.rs         #   Provider, InvocationCtx, InvocationOutcome
│   │       │   ├── workspace.rs        #   Workspace, AcquireCtx, WorkspaceHandle
│   │       │   ├── store.rs            #   Store, RunState, StepState, StepStatus, RunEvent
│   │       │   ├── action.rs           #   Action, ActionCtx, ActionOutcome
│   │       │   └── trigger.rs          #   Trigger, TriggerEvent
│   │       │
│   │       ├── usage.rs                # Usage (input/output tokens, cost_micros) — shared by traits + api
│   │       │
│   │       ├── api.rs                  # public IN/OUT contract: RunInput, RunSummary, StepResult, RunStatus, SideEffect
│   │       │
│   │       ├── engine.rs               # Engine trait + EngineBuilder (FROZEN API; impl lands at execution milestone)  [feature = "runtime"]
│   │       │
│   │       └── mock.rs                 # Noop/Mock impls of all 5 traits  [cfg(any(test, feature = "mock"))]
│   │
│   ├── odin-cli/                       # `odin` binary — thin runner (M1: only `validate`)  (EXISTS as stub)
│   │   └── src/
│   │       ├── main.rs                 # clap parse → dispatch (clap added at CLI milestone)
│   │       └── cmd/
│   │           ├── mod.rs
│   │           └── validate.rs         # `odin validate <file>`: load IR, validate, print diagnostics (human + --json)
│   │
│   └── odin-daemon/                    # `odind` binary — STUB only (EXISTS); hosts Triggers post-MVP
│       └── src/main.rs
└── examples/
    └── fix-flaky-test.yaml             # the one IR-exercising example (§8)
```

**Why no `engine/` impl module now:** `engine.rs` holds only the `Engine` *trait* + `EngineBuilder` (frozen public API). The executor is a later consumer of `traits` + `ir`; an empty exec module would invite premature API. The seam is the trait surface itself.

### Cargo.toml additions (`crates/odin-core/Cargo.toml`)

```toml
[dependencies]
chrono = { version = "0.4.44", default-features = false, features = ["clock", "serde"] }
indexmap = { version = "2.14.0", features = ["serde"] }
serde = { version = "1.0.228", features = ["derive"] }
serde_json = "1.0.150"
serde_yaml_ng = "0.10.0"
thiserror = "2.0.18"
uuid = { version = "1.23.2", features = ["v4", "serde"] }

# Optional — gated behind features so a parse-only embedder pays nothing.
minijinja    = { version = "2.20.0", optional = true }
async-trait  = { version = "0.1", optional = true }
anyhow       = { version = "1", optional = true }
tokio        = { version = "1", default-features = false, features = ["rt", "process", "time", "sync", "macros"], optional = true }
futures-core = { version = "0.3", optional = true }

[features]
default    = ["full"]
ir         = []                                              # parse + validate. No async runtime.
templating = ["dep:minijinja"]                               # render + static ref checking
runtime    = ["dep:async-trait", "dep:tokio", "dep:anyhow", "dep:futures-core"]  # the 5 traits + Engine
mock       = ["runtime"]                                     # Noop impls for downstream tests
full       = ["ir", "templating", "runtime"]
```

> Note: the templating module (`context/`) and validator rule ODIN017 require `minijinja`. When `templating` is off, ODIN017 is skipped (the validator still runs every other rule). `validate()` is available under `ir`; ref-checking is conditional.

---

## 2. CORE TYPES

### `ids.rs`

```rust
//! Newtype identifiers. Stringly-typed fields are the #1 source of silent
//! workflow bugs; these make a `StepId` impossible to confuse with an `ArtifactName`.

use std::fmt;
use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[must_use] pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            #[must_use] pub fn as_str(&self) -> &str { &self.0 }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:?})"), &self.0)
            }
        }
        impl From<&str> for $name { fn from(s: &str) -> Self { Self(s.to_owned()) } }
        impl From<String> for $name { fn from(s: String) -> Self { Self(s) } }
    };
}

string_id!(/// Author-assigned, stable id of a workflow (its `name`).
           WorkflowId);
string_id!(/// Author-assigned, stable id of a step. Unique within a workflow.
           StepId);
string_id!(/// Name of a named artifact, e.g. `DIFF`. Convention: UPPER_SNAKE.
           ArtifactName);
string_id!(/// Name of a workflow input parameter.
           ParamName);
string_id!(/// Name of a per-step gate command.
           GateName);
string_id!(/// Reference to a provider by registry key, e.g. "claude".
           /// Resolved against the [`crate::registry::Registry`] at run construction;
           /// validated statically (ODIN005) with a "did you mean" hint.
           ProviderRef);

/// A run instance identifier (UUID v4). Distinct type from [`WorkflowId`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub uuid::Uuid);

impl RunId {
    #[must_use] pub fn new() -> Self { Self(uuid::Uuid::new_v4()) }
}
impl Default for RunId { fn default() -> Self { Self::new() } }
impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
}
```

### `usage.rs`

```rust
//! Token/cost accounting, shared by the trait surface and the public contract.

use serde::{Deserialize, Serialize};

/// Accounting for one or more provider invocations. All fields are best-effort:
/// not every CLI reports usage. Cost is integer micro-dollars to avoid float drift
/// in the durable record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Usage {
    /// Prompt/input tokens, if known.
    pub input_tokens: u64,
    /// Completion/output tokens, if known.
    pub output_tokens: u64,
    /// Cost in USD micro-dollars (1_000_000 = $1.00). Integer: no float drift.
    pub cost_micros: u64,
}

impl Usage {
    /// Fold another usage record into this one (for run-level aggregation).
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cost_micros += other.cost_micros;
    }
}
```

### `error.rs`

```rust
//! Error taxonomy. The crate `Error` is phase-organized; each trait has its OWN
//! small error type so a third-party impl reads one 4-variant enum, not a god-error.

use thiserror::Error;

/// Convenience alias for the crate-level [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// The crate-level error, organized by *phase* (parse → validate → run).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// YAML/serde parse failure (syntax, unknown leaf field, bad enum/duration).
    #[error("failed to parse workflow: {0}")]
    Parse(#[from] serde_yaml_ng::Error),

    /// I/O failure (e.g. reading a workflow file).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The workflow parsed but failed semantic validation. Carries ALL diagnostics.
    #[error("workflow validation failed with {} error(s)", .0.error_count())]
    Validation(crate::validate::ValidationReport),

    /// An unsupported schema major version.
    #[error("unsupported schema_version {found_major} (this engine speaks major {supported_major})")]
    SchemaVersion { found_major: u16, supported_major: u16 },

    /// A name referenced in a workflow had no registered implementation.
    #[error("no {kind} registered under name '{name}'")]
    Unregistered { kind: &'static str, name: String },

    /// A `RunInput` did not satisfy the workflow's declared params.
    #[error("invalid run input: {0}")]
    Input(String),

    /// Template render/eval failure at run time (only with the `templating` feature).
    #[cfg(feature = "templating")]
    #[error("template error in {context}: {source}")]
    Template { context: String, #[source] source: minijinja::Error },

    // ── Plugin failures: each wraps the trait's own error type. ──
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Action(#[from] ActionError),
    #[error(transparent)]
    Trigger(#[from] TriggerError),
}

// ── Per-trait error types. Third parties return THESE. Each is #[non_exhaustive]
//    and ends in an `Other(anyhow::Error)` escape hatch (with the `runtime` feature). ──

/// Error returned by a [`crate::traits::Provider`] invocation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProviderError {
    #[error("provider CLI not found on PATH: {0}")]
    NotFound(String),
    #[error("provider timed out after {0:?}")]
    Timeout(std::time::Duration),
    #[error("provider exited with code {code}: {stderr}")]
    Exited { code: i32, stderr: String },
    #[cfg(feature = "runtime")]
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Error returned by a [`crate::traits::Workspace`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WorkspaceError {
    #[error("git error: {0}")]
    Git(String),
    #[error("no free slot in pool")]
    PoolExhausted,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(feature = "runtime")]
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Error returned by a [`crate::traits::Store`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    #[error("store backend error: {0}")]
    Backend(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[cfg(feature = "runtime")]
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Error returned by a [`crate::traits::Action`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ActionError {
    #[error("unknown action: {0}")]
    Unknown(String),
    #[cfg(feature = "runtime")]
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Error returned by a [`crate::traits::Trigger`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TriggerError {
    #[error("trigger source error: {0}")]
    Source(String),
    #[cfg(feature = "runtime")]
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
```

> **Lint note:** `unsafe_code = "forbid"` is already set workspace-wide; nothing here needs `unsafe`. `missing_docs = "warn"` means every `pub` item needs a doc comment — the blueprint includes them.

---

## 3. WORKFLOW IR

### `ir/duration.rs`

```rust
//! Human-friendly durations: "30s", "5m", "2h", or bare seconds "45".

use std::time::Duration;
use serde::{Deserialize, Serialize};

/// A [`Duration`] written as `"30s"`/`"5m"`/`"2h"` (or bare seconds `"45"`).
/// Parse errors surface at deserialize time with a precise message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    /// Parse a human duration string. Returns a human-readable error on failure.
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (num, unit) = s.split_at(split);
        let n: u64 = num
            .parse()
            .map_err(|_| format!("invalid duration {s:?}: expected a leading number"))?;
        let secs = match unit.trim() {
            "" | "s" => n,
            "m" => n * 60,
            "h" => n * 3600,
            other => return Err(format!("invalid duration unit {other:?} in {s:?} (use s, m, or h)")),
        };
        Ok(Self(Duration::from_secs(secs)))
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{}s", self.0.as_secs()))
    }
}
impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(D::Error::custom)
    }
}
```

### `ir/workflow.rs`

```rust
//! The root workflow type and its metadata.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::ids::{ParamName, WorkflowId};
use super::{duration::HumanDuration, params::ParamSpec, step::{RetrySpec, Step},
            trigger::TriggerDecl, workspace::WorkspaceConfig};

/// The current schema major this engine speaks. Bump only on a breaking IR change.
pub const CURRENT_SCHEMA_MAJOR: u16 = 1;

/// A parsed, **not-yet-validated** workflow definition. Mirrors the YAML 1:1.
///
/// Unknown keys at the root are tolerated (and surfaced as a warning, ODIN018) so a
/// file authored for a newer schema minor still loads; unknown keys in nested config
/// are hard errors.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Workflow {
    /// Schema version of the workflow *file format*. Defaults to the current major.minor.
    #[serde(default)]
    pub schema_version: SchemaVersion,

    /// Stable identity and display name of this workflow.
    pub name: WorkflowId,

    /// Author's semantic version of the workflow content. Opaque to the engine.
    #[serde(default)]
    pub version: Option<String>,

    /// Human description.
    #[serde(default)]
    pub description: Option<String>,

    /// Whether runs of this workflow are checkpointed to the [`crate::traits::Store`].
    #[serde(default = "default_true")]
    pub durable: bool,

    /// How per-run workspaces are provisioned. Defaults to a per-run git worktree.
    #[serde(default)]
    pub workspace: WorkspaceConfig,

    /// Declared triggers. Empty = manual-only. Evaluation of non-manual triggers
    /// is a later milestone; they parse and validate now.
    #[serde(default)]
    pub triggers: Vec<TriggerDecl>,

    /// Input parameter schema, keyed by name. Insertion order preserved.
    #[serde(default)]
    pub params: IndexMap<ParamName, ParamSpec>,

    /// The steps. A DAG via each step's `depends_on`; the first executor walks a
    /// topological order. Non-empty is enforced by validation (ODIN001), not parsing.
    pub steps: Vec<Step>,

    /// Default retry/timeout applied to steps that omit their own. Additive seam.
    #[serde(default)]
    pub defaults: WorkflowDefaults,
}

fn default_true() -> bool { true }

/// Workflow-level defaults applied to steps that don't override them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WorkflowDefaults {
    /// Default per-step timeout.
    #[serde(default)]
    pub timeout: Option<HumanDuration>,
    /// Default retry policy.
    #[serde(default)]
    pub retry: Option<RetrySpec>,
}

/// `MAJOR.MINOR` schema version of the file format. Only `major` gates compatibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SchemaVersion {
    /// Breaking version. The engine refuses majors it does not know.
    pub major: u16,
    /// Additive version. A newer minor loads with a warning (ODIN019).
    pub minor: u16,
}

impl Default for SchemaVersion {
    fn default() -> Self { Self { major: CURRENT_SCHEMA_MAJOR, minor: 0 } }
}
impl TryFrom<String> for SchemaVersion {
    type Error = String;
    fn try_from(s: String) -> std::result::Result<Self, String> {
        let (maj, min) = s.split_once('.').ok_or_else(|| format!("expected MAJOR.MINOR, got {s:?}"))?;
        Ok(Self {
            major: maj.parse().map_err(|_| format!("invalid major in schema_version {s:?}"))?,
            minor: min.parse().map_err(|_| format!("invalid minor in schema_version {s:?}"))?,
        })
    }
}
impl From<SchemaVersion> for String {
    fn from(v: SchemaVersion) -> String { format!("{}.{}", v.major, v.minor) }
}
```

### `ir/workspace.rs`

```rust
//! Pluggable workspace provisioning configuration.

use serde::{Deserialize, Serialize};

/// How a run gets its working directory. Internally tagged on `type`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkspaceConfig {
    /// One throwaway `git worktree` per run. The default.
    Worktree(WorktreeConfig),
    /// A pool of N pre-cloned slots; claim / release / reset between runs.
    SlotPool(SlotPoolConfig),
}

impl Default for WorkspaceConfig {
    fn default() -> Self { Self::Worktree(WorktreeConfig::default()) }
}

/// Configuration for the per-run worktree workspace.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WorktreeConfig {
    /// Base branch/ref the worktree is cut from. Defaults to repo HEAD.
    #[serde(default)]
    pub base: Option<String>,
}

/// Configuration for the slot-pool workspace.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SlotPoolConfig {
    /// Number of clones in the pool. Must be >= 1 (ODIN014).
    pub pool: u16,
    /// How a slot is reset before reuse.
    #[serde(default)]
    pub reset: ResetMode,
    /// Base branch/ref slots are cut from. Defaults to repo HEAD.
    #[serde(default)]
    pub base: Option<String>,
}

/// How a slot pool cleans a slot before reuse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResetMode {
    /// `git reset --hard && git clean -fdx`. Fast; the default.
    #[default]
    GitClean,
    /// Re-clone from origin. Slow; pristine.
    Reclone,
}
```

### `ir/trigger.rs`

```rust
//! Declared triggers. v1 *executes* only `Manual`; others parse & validate now.

use serde::{Deserialize, Serialize};

/// A declared trigger. `#[non_exhaustive]` so new kinds are additive.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TriggerDecl {
    /// Run on explicit user/API request. The only kind wired in v1.
    Manual,
    /// A GitHub webhook event. Declaration parsed now; dispatch in the daemon milestone.
    GithubWebhook(GithubWebhookDecl),
    /// A cron schedule. Declaration parsed now; dispatch in the daemon milestone.
    Cron(CronDecl),
}

/// Declaration of a GitHub webhook trigger.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct GithubWebhookDecl {
    /// Event names, e.g. `["pull_request.opened", "issues.labeled"]`.
    pub events: Vec<String>,
    /// Optional `owner/repo` filter.
    #[serde(default)]
    pub repo: Option<String>,
}

/// Declaration of a cron trigger.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CronDecl {
    /// Standard 5-field cron expression. Validity is checked at validate-time (ODIN015).
    pub schedule: String,
}
```

### `ir/params.rs`

```rust
//! Workflow input parameter schema.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single declared input parameter. The validator checks `RunInput.params`
/// against these at run start.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ParamSpec {
    /// Value type. Defaults to `string`.
    #[serde(rename = "type", default)]
    pub ty: ParamType,
    /// Whether the caller must supply this param.
    #[serde(default)]
    pub required: bool,
    /// Default value when not supplied. Contradicts `required: true` (ODIN013, warning).
    #[serde(default)]
    pub default: Option<Value>,
    /// Human description.
    #[serde(default)]
    pub description: Option<String>,
}

/// Minimal parameter value type. Richer typing is a future seam (add variants).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ParamType {
    /// A UTF-8 string (the default).
    #[default]
    String,
    /// A JSON number.
    Number,
    /// A boolean.
    Bool,
}
```

### `ir/step.rs` — the crown jewel

```rust
//! Steps and the exactly-one-of-kind discriminated union.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{ArtifactName, GateName, ProviderRef, StepId};
use super::duration::HumanDuration;

/// One node in the workflow DAG.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Step {
    /// Stable, author-assigned id. Unique & non-empty (ODIN002/ODIN003) and a valid
    /// template path segment (ODIN004).
    pub id: StepId,

    /// Exactly one of provider/action/run. Flattened so YAML reads naturally
    /// (`provider: claude` sits directly on the step).
    #[serde(flatten)]
    pub kind: StepKind,

    /// Named artifact data-flow on top of the shared workdir.
    #[serde(default)]
    pub artifacts: Artifacts,

    /// Named gate commands: name → shell command. All must exit 0. Deterministic order.
    #[serde(default)]
    pub gates: IndexMap<GateName, String>,

    /// Optional LLM-as-judge over this step's output.
    #[serde(default)]
    pub judge: Option<JudgeSpec>,

    /// Retry policy. Defaults to no retries.
    #[serde(default)]
    pub retry: RetrySpec,

    /// Wall-clock timeout for the step body. Defaults to the workflow default / none.
    #[serde(default)]
    pub timeout: Option<HumanDuration>,

    /// Minijinja boolean expression; the step is skipped when it evaluates false.
    #[serde(default)]
    pub when: Option<String>,

    /// DAG edges: ids of steps that must complete before this one.
    #[serde(default)]
    pub depends_on: Vec<StepId>,
}

/// The body of a step. EXACTLY ONE variant is present in valid YAML — enforced by a
/// hand-written [`Deserialize`] that yields a precise error for both "none" and "more
/// than one", instead of serde's opaque untagged-enum failure.
#[derive(Clone, Debug)]
pub enum StepKind {
    /// Invoke a pinned coding-agent provider with a prompt.
    Provider(ProviderStep),
    /// Run a registered in-process [`crate::traits::Action`].
    Action(ActionStep),
    /// Shell out to an external command (code hook, any language).
    Run(RunStep),
}

/// Provider-invocation step body.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderStep {
    /// Registry key of the provider to invoke (e.g. "claude"). Validated (ODIN005).
    pub provider: ProviderRef,
    /// Inline prompt template (minijinja). Mutually exclusive with `prompt_file` (ODIN009).
    pub prompt: Option<String>,
    /// Path to a prompt template file. Mutually exclusive with `prompt`.
    pub prompt_file: Option<String>,
}

/// Built-in-action step body.
#[derive(Clone, Debug, Serialize)]
pub struct ActionStep {
    /// Registry key of the action (e.g. "github.open_pr"). Validated (ODIN010).
    pub action: String,
    /// Free-form, templated arguments passed to the action.
    pub with: IndexMap<String, Value>,
}

/// Run-hook step body.
#[derive(Clone, Debug, Serialize)]
pub struct RunStep {
    /// Shell command line. Runs in the step's workdir.
    pub run: String,
}

// ── Hand-written Deserialize for the exactly-one-of-kind discriminant ──

#[derive(Deserialize)]
struct StepKindRaw {
    #[serde(default)] provider: Option<ProviderRef>,
    #[serde(default)] prompt: Option<String>,
    #[serde(default)] prompt_file: Option<String>,
    #[serde(default)] action: Option<String>,
    #[serde(default)] with: Option<IndexMap<String, Value>>,
    #[serde(default)] run: Option<String>,
}

impl<'de> Deserialize<'de> for StepKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let r = StepKindRaw::deserialize(d)?;
        let discriminants = [r.provider.is_some(), r.action.is_some(), r.run.is_some()];
        match discriminants.iter().filter(|b| **b).count() {
            0 => Err(D::Error::custom(
                "step must declare exactly one of `provider:`, `action:`, or `run:` (found none)",
            )),
            1 if r.provider.is_some() => Ok(StepKind::Provider(ProviderStep {
                provider: r.provider.unwrap(),
                prompt: r.prompt,
                prompt_file: r.prompt_file,
            })),
            1 if r.action.is_some() => Ok(StepKind::Action(ActionStep {
                action: r.action.unwrap(),
                with: r.with.unwrap_or_default(),
            })),
            1 => Ok(StepKind::Run(RunStep { run: r.run.unwrap() })),
            _ => Err(D::Error::custom(
                "step declares more than one of `provider:`, `action:`, `run:` — choose exactly one",
            )),
        }
    }
}

impl Serialize for StepKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        // Re-flatten into the raw shape on serialize so round-trips are faithful.
        use serde::ser::SerializeMap as _;
        let mut m = s.serialize_map(None)?;
        match self {
            StepKind::Provider(p) => {
                m.serialize_entry("provider", &p.provider)?;
                if let Some(x) = &p.prompt { m.serialize_entry("prompt", x)?; }
                if let Some(x) = &p.prompt_file { m.serialize_entry("prompt_file", x)?; }
            }
            StepKind::Action(a) => {
                m.serialize_entry("action", &a.action)?;
                if !a.with.is_empty() { m.serialize_entry("with", &a.with)?; }
            }
            StepKind::Run(r) => { m.serialize_entry("run", &r.run)?; }
        }
        m.end()
    }
}

/// Artifact data-flow declared by a step.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Artifacts {
    /// Named artifacts this step needs present before it runs.
    #[serde(default)]
    pub requires: Vec<ArtifactName>,
    /// Named artifacts this step is expected to produce. `DIFF` is engine-auto-captured.
    #[serde(default)]
    pub produces: Vec<ArtifactName>,
}

impl Artifacts {
    /// True if neither requires nor produces anything.
    #[must_use] pub fn is_empty(&self) -> bool {
        self.requires.is_empty() && self.produces.is_empty()
    }
}

/// LLM-as-judge configuration for a step.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct JudgeSpec {
    /// Provider used as the judge (a registry key). Validated (ODIN005).
    pub provider: ProviderRef,
    /// Natural-language criteria the output must satisfy.
    pub criteria: String,
    /// Pass threshold in `0.0..=1.0` (ODIN011). Defaults to 0.5.
    #[serde(default = "default_threshold")]
    pub threshold: f32,
}
fn default_threshold() -> f32 { 0.5 }

/// Retry policy for a step.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RetrySpec {
    /// Max *additional* attempts after the first. 0 = no retry (default).
    #[serde(default)]
    pub max: u8,
    /// Backoff strategy between attempts.
    #[serde(default)]
    pub backoff: Backoff,
    /// Provider to switch to on final failure. **Inert in v1** (routing is a later
    /// layer); parsed and validated so workflows are forward-compatible (ODIN016).
    #[serde(default)]
    pub on_fallback_provider: Option<ProviderRef>,
}

/// Backoff strategy between retry attempts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Backoff {
    /// Constant delay between attempts. The default.
    #[default]
    Fixed,
    /// Exponentially increasing delay.
    Exponential,
}
```

### `ir/mod.rs`

```rust
//! The Workflow intermediate representation: serde-deserializable types mirroring YAML.

pub mod duration;
pub mod params;
pub mod step;
pub mod trigger;
pub mod workflow;
pub mod workspace;

pub use duration::HumanDuration;
pub use params::{ParamSpec, ParamType};
pub use step::{Artifacts, ActionStep, Backoff, JudgeSpec, ProviderStep, RetrySpec, RunStep, Step, StepKind};
pub use trigger::{CronDecl, GithubWebhookDecl, TriggerDecl};
pub use workflow::{CURRENT_SCHEMA_MAJOR, SchemaVersion, Workflow, WorkflowDefaults};
pub use workspace::{ResetMode, SlotPoolConfig, WorkspaceConfig, WorktreeConfig};

use std::path::Path;
use crate::error::{Error, Result};

impl Workflow {
    /// Parse a workflow from a YAML string. **Parse only** — call
    /// [`crate::validate::validate`] afterward for semantic checks. The two phases
    /// are deliberately separate: parsing is fail-fast (one error), validation
    /// collects all diagnostics.
    ///
    /// Rejects an unsupported schema major before returning (so a v2 file fails
    /// loudly rather than mis-binding to v1 fields).
    pub fn from_yaml_str(src: &str) -> Result<Self> {
        let wf: Workflow = serde_yaml_ng::from_str(src)?;
        if wf.schema_version.major != CURRENT_SCHEMA_MAJOR {
            return Err(Error::SchemaVersion {
                found_major: wf.schema_version.major,
                supported_major: CURRENT_SCHEMA_MAJOR,
            });
        }
        Ok(wf)
    }

    /// Parse a workflow from a file path.
    pub fn from_yaml_path(path: impl AsRef<Path>) -> Result<Self> {
        let src = std::fs::read_to_string(path)?;
        Self::from_yaml_str(&src)
    }
}
```

> **Forward-compat note:** `Step` carries `provider`/`action`/`run` only inside the *private* `StepKindRaw`; consumers always see the resolved `StepKind` enum. A 4th kind (e.g. `sub_workflow`) is a new variant + one match arm in the hand-written deserialize — no churn at call sites because `StepKind` is matched exhaustively in one place (the executor) and the IR types are `#[non_exhaustive]`.

---

## 4. FIVE TRAITS (`traits/`, feature `runtime`)

Design contract for all five: **object-safe** (`Arc<dyn Trait>`), **one required method** plus defaulted optionals, **owned/serializable ctx-outcome structs** so impls can live in other crates (and later other processes), **per-trait error types**, and a `tokio_util`-free `CancelToken` newtype so the public signatures don't leak the dep.

> Add `tokio-util = { version = "0.7", features = ["rt"], optional = true }` under `runtime` for the real `CancellationToken` behind `CancelToken`.

### `traits/provider.rs`

```rust
//! The [`Provider`] trait: invoke an autonomous coding-agent CLI for a step.

use async_trait::async_trait;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};

use crate::error::ProviderError;
use crate::ids::{ArtifactName, ProviderRef, StepId};
use crate::usage::Usage;

/// An autonomous coding-agent CLI Odin can invoke for a `provider:` step.
///
/// Implement this in one file: you receive an [`InvocationCtx`] (rendered prompt,
/// workdir, inputs) and return an [`InvocationOutcome`] (exit code, captured output,
/// usage, produced artifacts). The engine owns durability and DIFF capture; a
/// provider must not touch the [`crate::traits::Store`] or git.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Registry key this provider answers to (e.g. "claude"). Must match `provider:` pins.
    fn id(&self) -> ProviderRef;

    /// Run the agent against the workspace. The ONE required method.
    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError>;

    /// Best-effort CLI version string, recorded in run state for reproducibility.
    /// Default: `None`.
    async fn version(&self) -> Option<String> { None }

    /// Cheap readiness probe (CLI installed & authed?). Default: assume OK.
    async fn health_check(&self) -> Result<(), ProviderError> { Ok(()) }
}

/// Everything a provider needs for one invocation. Owned so it crosses crate/process
/// boundaries; the prompt is already fully rendered by the engine.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct InvocationCtx {
    /// The step being run.
    pub step_id: StepId,
    /// Working directory (the acquired workspace path).
    pub workdir: PathBuf,
    /// Fully-rendered prompt. `None` only for prompt-from-artifact steps.
    pub prompt: Option<String>,
    /// Required artifacts, resolved to on-disk paths.
    pub inputs: IndexMap<ArtifactName, PathBuf>,
    /// Per-step timeout; the provider should self-limit, the engine hard-kills.
    pub timeout: Option<std::time::Duration>,
    /// Fires on run cancel/timeout. Honor it and return promptly.
    pub cancel: CancelToken,
}

impl InvocationCtx {
    /// The workdir as a `&Path`.
    #[must_use] pub fn workdir(&self) -> &Path { &self.workdir }
}

/// What a provider produced.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct InvocationOutcome {
    /// Process exit code (0 = success by convention).
    pub exit_code: i32,
    /// Captured stdout (the agent's textual result; may be read by a judge).
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Structured outputs exposed to later steps as `steps.<id>.outputs.*`.
    pub outputs: IndexMap<String, serde_json::Value>,
    /// Token/cost usage, if the CLI reports it.
    pub usage: Option<Usage>,
    /// Artifacts explicitly written (name → path). The engine still auto-captures `DIFF`.
    pub produced: IndexMap<ArtifactName, PathBuf>,
}

impl InvocationOutcome {
    /// Convenience constructor for trivial/mock providers: exit 0, given stdout.
    #[must_use] pub fn success(stdout: impl Into<String>) -> Self {
        Self { exit_code: 0, stdout: stdout.into(), stderr: String::new(),
               outputs: IndexMap::new(), usage: Some(Usage::default()), produced: IndexMap::new() }
    }
}

/// A clonable cancellation handle wrapping `tokio_util::sync::CancellationToken`,
/// so the public signature does not leak the dependency.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(#[cfg(feature = "runtime")] pub(crate) tokio_util::sync::CancellationToken);

impl CancelToken {
    /// True once cancellation has been requested.
    #[must_use] pub fn is_cancelled(&self) -> bool { self.0.is_cancelled() }
    /// Resolves when cancellation is requested.
    pub async fn cancelled(&self) { self.0.cancelled().await }
}
```

**Mock:** `EchoProvider(ProviderRef)` — `invoke` returns `InvocationOutcome::success(prompt)`; ~8 lines.

### `traits/workspace.rs`

```rust
//! The [`Workspace`] trait: provision an isolated working directory per run.

use async_trait::async_trait;
use std::path::PathBuf;

use crate::error::WorkspaceError;
use crate::ids::RunId;
use crate::ir::WorkspaceConfig;

/// Provides each run an isolated working directory. v1 impls: worktree & slot-pool.
///
/// Lifecycle: `acquire` → steps run against `handle.path` → `release`.
#[async_trait]
pub trait Workspace: Send + Sync {
    /// Registry key (e.g. "worktree" | "slot_pool").
    fn kind(&self) -> &str;

    /// Claim a workdir for a run. May block/queue if a finite pool is exhausted.
    async fn acquire(&self, ctx: AcquireCtx) -> Result<WorkspaceHandle, WorkspaceError>;

    /// Release/reset a previously acquired workspace. Idempotent.
    async fn release(&self, handle: WorkspaceHandle) -> Result<(), WorkspaceError>;
}

/// What the engine knows when acquiring a workspace.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AcquireCtx {
    /// The run requesting a workspace.
    pub run_id: RunId,
    /// The workflow's declared workspace config.
    pub config: WorkspaceConfig,
}

/// A claimed workspace lease. **Not `Clone`** — a lease has a single owner. Carries
/// the path steps run in plus an impl-private reclaim token (slot index, worktree name).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct WorkspaceHandle {
    /// The run this lease belongs to.
    pub run_id: RunId,
    /// Absolute path steps execute in.
    pub path: PathBuf,
    /// Branch/ref created for this run, if any (folded into the run summary).
    pub branch: Option<String>,
    /// Impl-private reclaim token, opaque to the engine.
    pub token: String,
}
```

> **Resolution of the simplicity-lens "not Clone" vs durability:** `WorkspaceHandle` *is* `Clone + Serialize` here because `RunState` must persist it for crash-resume (C4). Single-ownership of the *lease* is enforced by the engine's lifecycle (acquire→release), not by `!Clone`. This is the correctness-driven merge.

**Mock:** `TmpWorkspace(PathBuf)` — `acquire` hands back a fixed temp path; `release` is a no-op.

### `traits/store.rs`

```rust
//! The [`Store`] trait: durable, crash-resumable persistence of run state.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::api::RunStatus;
use crate::error::StoreError;
use crate::ids::{ArtifactName, RunId, StepId, WorkflowId};

/// Durable persistence for run state. The SQLite impl lands later; the trait is
/// fixed now. The contract is deliberately tiny: **checkpoint** the whole
/// [`RunState`] at step boundaries, **append** events to an audit log, and
/// **load incomplete** runs on startup so they resume.
#[async_trait]
pub trait Store: Send + Sync {
    /// Persist a run-state checkpoint atomically. Called at every step boundary.
    /// A crash mid-call must leave either the old or new state, never partial.
    async fn checkpoint(&self, state: &RunState) -> Result<(), StoreError>;

    /// Append one immutable event to the run's ordered audit log (cheap, frequent).
    async fn append_event(&self, run_id: RunId, event: &RunEvent) -> Result<(), StoreError>;

    /// Load all runs not in a terminal state — the crash-recovery entry point.
    async fn load_incomplete(&self) -> Result<Vec<RunState>, StoreError>;

    /// Load a single run by id (None if unknown).
    async fn load_run(&self, run_id: RunId) -> Result<Option<RunState>, StoreError>;

    /// Read the event log (for replay/inspection). Default: empty (optional capability).
    async fn events(&self, _run_id: RunId) -> Result<Vec<RunEvent>, StoreError> { Ok(Vec::new()) }
}

/// The full durable state of a run — the checkpoint payload. `Serialize` so any
/// Store (SQLite blob, Postgres jsonb, files) persists it with **zero** IR knowledge.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunState {
    /// Run identity.
    pub run_id: RunId,
    /// Which workflow this run executes.
    pub workflow: WorkflowId,
    /// Schema major the workflow declared (reproducibility).
    pub schema_major: u16,
    /// Overall status.
    pub status: RunStatus,
    /// Per-step progress, keyed by step id, in execution order.
    pub steps: IndexMap<StepId, StepState>,
    /// Resolved artifact catalog: name → path relative to the workdir.
    pub artifacts: IndexMap<ArtifactName, String>,
    /// Provider versions actually used (reproducibility).
    pub provider_versions: IndexMap<String, String>,
    /// The inputs the run started with (deterministic resume & audit).
    pub input: crate::api::RunInput,
    /// Workspace lease in use, to reattach on resume.
    pub workspace: Option<crate::traits::workspace::WorkspaceHandle>,
    /// Timestamps.
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Per-step durable progress.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StepState {
    /// Step status.
    pub status: StepStatus,
    /// Attempts so far (>= 1 once started).
    pub attempts: u8,
    /// Last process exit code, if the step ran.
    pub exit_code: Option<i32>,
    /// Named outputs exposed to templating as `steps.<id>.outputs.*`.
    pub outputs: IndexMap<String, serde_json::Value>,
    /// Usage for this step's invocations.
    pub usage: Option<crate::usage::Usage>,
}

/// Lifecycle status of a single step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepStatus {
    /// Not yet started.
    Pending,
    /// Currently executing.
    Running,
    /// Completed successfully (gates/judge passed).
    Passed,
    /// Failed terminally.
    Failed,
    /// Skipped because its `when:` evaluated false or an upstream failed.
    Skipped,
}

/// An immutable audit-log entry. `#[non_exhaustive]` so new event kinds don't break Stores.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunEvent {
    /// The run started.
    RunStarted { at: DateTime<Utc> },
    /// A step started an attempt.
    StepStarted { step: StepId, attempt: u8, at: DateTime<Utc> },
    /// A gate finished.
    GateResult { step: StepId, gate: String, passed: bool, at: DateTime<Utc> },
    /// A judge finished.
    JudgeResult { step: StepId, score: f32, passed: bool, at: DateTime<Utc> },
    /// A step finished an attempt.
    StepFinished { step: StepId, status: StepStatus, exit_code: Option<i32>, at: DateTime<Utc> },
    /// The run finished.
    RunFinished { status: RunStatus, at: DateTime<Utc> },
}
```

**Mock:** `MemStore(Mutex<HashMap<RunId, RunState>>)` — `checkpoint` inserts, `load_incomplete` filters non-terminal; proves the trait works without SQLite. ~15 lines.

### `traits/action.rs`

```rust
//! The [`Action`] trait: a built-in, named side-effect step.

use async_trait::async_trait;
use indexmap::IndexMap;
use std::path::PathBuf;

use crate::error::ActionError;
use crate::ids::StepId;

/// A first-class, reusable side-effect available by name in `action:`
/// (`github.open_pr`, `git.commit`, `shell.exec`). Distinct from a non-deterministic
/// [`Provider`](crate::traits::Provider) and from an arbitrary `run:` hook.
#[async_trait]
pub trait Action: Send + Sync {
    /// The name authors reference in `action:`.
    fn name(&self) -> &str;

    /// Execute the action against the prepared context.
    async fn run(&self, ctx: ActionCtx) -> Result<ActionOutcome, ActionError>;
}

/// Everything an action needs.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ActionCtx {
    /// The step being run.
    pub step_id: StepId,
    /// Working directory.
    pub workdir: PathBuf,
    /// The step's `with:` args, already templated.
    pub args: IndexMap<String, serde_json::Value>,
}

/// What an action produced.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct ActionOutcome {
    /// 0 = success. Mirrors provider/run convention so gate logic is uniform.
    pub exit_code: i32,
    /// Outputs exposed to later steps as `steps.<id>.outputs.*`.
    pub outputs: IndexMap<String, serde_json::Value>,
    /// Externally-visible effects, surfaced in [`crate::api::RunSummary`].
    pub side_effects: Vec<crate::api::SideEffect>,
}
```

**Mock:** `NoopAction(String)` — echoes `args` into `outputs`.

### `traits/trigger.rs`

```rust
//! The [`Trigger`] runtime trait (distinct from `ir::TriggerDecl`, which is config).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::TriggerError;
use crate::ids::WorkflowId;

/// A source of run-starting events. v1 ships only a manual trigger; the daemon
/// hosts long-lived ones (webhook, cron) later. Pull-based so manual (one event then
/// end), cron (timer), and webhook (server-pushed) all fit one shape.
#[async_trait]
pub trait Trigger: Send + Sync {
    /// Stable name ("manual", "github_webhook", "cron").
    fn kind(&self) -> &str;

    /// Block until the next event, or `Ok(None)` when the source is exhausted.
    /// (Manual = one event then `None`; cron/webhook never return `None`.) Cancel-safe.
    async fn next_event(&mut self) -> Result<Option<TriggerEvent>, TriggerError>;
}

/// A fired trigger, ready to become a run. Carries a [`crate::api::RunInput`] so the
/// same run path serves manual and triggered runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TriggerEvent {
    /// Which declared trigger fired.
    pub source: String,
    /// Which workflow to start.
    pub workflow: WorkflowId,
    /// The assembled run input (trigger payload + params).
    pub input: crate::api::RunInput,
}
```

**Mock:** `ScriptedTrigger(VecDeque<TriggerEvent>)` — `next_event` pops the front, then `None`.

### `registry.rs` (the extensibility hub)

```rust
//! The registry: maps string keys to boxed trait objects. Built-ins ship registered;
//! third parties `register_*` with zero core changes. The validator checks names
//! against the live registry.

#![cfg(feature = "runtime")]

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::traits::{Action, Provider, Trigger, Workspace};

/// Resolves provider/workspace/action/trigger names to implementations.
#[derive(Default, Clone)]
pub struct Registry {
    providers: BTreeMap<String, Arc<dyn Provider>>,
    workspaces: BTreeMap<String, Arc<dyn Workspace>>,
    actions: BTreeMap<String, Arc<dyn Action>>,
    triggers: BTreeMap<String, Arc<dyn Trigger>>,
}

impl Registry {
    /// A registry with all built-in providers/workspaces/actions/triggers registered.
    #[must_use] pub fn with_builtins() -> Self { Self::default() /* registers claude/codex/... later */ }

    /// Register a provider under its `id()`.
    pub fn register_provider(&mut self, p: Arc<dyn Provider>) -> &mut Self {
        self.providers.insert(p.id().as_str().to_owned(), p); self
    }
    /// Register an action under its `name()`.
    pub fn register_action(&mut self, a: Arc<dyn Action>) -> &mut Self {
        self.actions.insert(a.name().to_owned(), a); self
    }
    /// Register a workspace under its `kind()`.
    pub fn register_workspace(&mut self, w: Arc<dyn Workspace>) -> &mut Self {
        self.workspaces.insert(w.kind().to_owned(), w); self
    }

    /// Look up a provider by name.
    #[must_use] pub fn provider(&self, name: &str) -> Option<&Arc<dyn Provider>> { self.providers.get(name) }
    /// Known provider names (for validation + "did you mean").
    pub fn provider_names(&self) -> impl Iterator<Item = &str> { self.providers.keys().map(String::as_str) }
    /// Look up an action by name.
    #[must_use] pub fn action(&self, name: &str) -> Option<&Arc<dyn Action>> { self.actions.get(name) }
    /// Known action names.
    pub fn action_names(&self) -> impl Iterator<Item = &str> { self.actions.keys().map(String::as_str) }
}
```

> **Validation under `ir`-only builds (no `runtime`):** the validator accepts an optional `&[&str]` of known provider/action names instead of a `&Registry` when the `runtime` feature is off, so a parse-only linter can still check ODIN005/ODIN010 against the built-in set (`["claude","codex","copilot"]`). See §6 signature note.

---

## 5. PUBLIC CONTRACTS (`api.rs`)

```rust
//! The public integration contract: what comes IN to start a run, and what goes OUT.
//! Both are `Serialize + Deserialize` so the boundary is JSON over any transport.

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::ids::{RunId, StepId, WorkflowId};
use crate::traits::store::StepStatus;
use crate::usage::Usage;

/// Requirements coming IN: everything needed to start a run.
///
/// Two channels: typed `params` (validated against the workflow's declared params)
/// and a free-form `trigger` payload (the event verbatim, reachable as `trigger.*`
/// in templates). The split gives type-checking where structure is declared and an
/// escape hatch where it can't be.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunInput {
    /// Which declared trigger this run corresponds to. Defaults to "manual".
    #[serde(default = "default_trigger")]
    pub trigger: String,
    /// Free-form trigger payload, surfaced as `trigger.*`.
    #[serde(default)]
    pub trigger_payload: serde_json::Value,
    /// Param values, by name. Validated & coerced against the workflow's param schema.
    #[serde(default)]
    pub params: IndexMap<String, serde_json::Value>,
    /// Optional caller-supplied idempotency key: re-submitting the same key returns
    /// the existing run instead of starting a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

fn default_trigger() -> String { "manual".into() }

impl RunInput {
    /// Start a manual run.
    #[must_use] pub fn manual() -> Self { Self { trigger: "manual".into(), ..Default::default() } }

    /// Fluent setter for a typed param.
    #[must_use] pub fn param(mut self, k: impl Into<String>, v: impl Into<serde_json::Value>) -> Self {
        self.params.insert(k.into(), v.into()); self
    }
}

/// Results going OUT: the machine-consumable summary of a run. Contains no engine
/// internals or trait objects.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunSummary {
    /// Run identity.
    pub run_id: RunId,
    /// The workflow that ran.
    pub workflow: WorkflowId,
    /// Terminal status.
    pub status: RunStatus,
    /// Per-step results, in execution order.
    pub steps: Vec<StepResult>,
    /// Aggregate usage across all provider/judge invocations.
    pub usage: Usage,
    /// Externally-visible effects (PRs opened, branches pushed) for downstream automation.
    pub side_effects: Vec<SideEffect>,
    /// The git diff captured as the implicit `DIFF` artifact, if any.
    pub diff: Option<String>,
    /// Populated iff `status == Failed`: the terminal error, stringified.
    pub error: Option<String>,
    /// Timestamps.
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Per-step result in a [`RunSummary`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StepResult {
    /// The step id.
    pub id: StepId,
    /// Final status.
    pub status: StepStatus,
    /// Attempts taken.
    pub attempts: u8,
    /// Last exit code.
    pub exit_code: Option<i32>,
    /// Outputs exposed as `steps.<id>.outputs.*`.
    pub outputs: IndexMap<String, serde_json::Value>,
    /// Gate name → passed?.
    pub gates: IndexMap<String, bool>,
    /// Judge score, if a judge ran.
    pub judge_score: Option<f32>,
    /// Usage for this step.
    pub usage: Option<Usage>,
}

/// Lifecycle status of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunStatus {
    /// Created, not started.
    Pending,
    /// Executing.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Failed terminally.
    Failed,
    /// Cancelled (user request, timeout, shutdown).
    Cancelled,
}

impl RunStatus {
    /// True for terminal states.
    #[must_use] pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// A structured, externally-visible effect a run had on the outside world.
/// Internally tagged and `#[non_exhaustive]` so integrators match the kinds they
/// understand and ignore the rest.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SideEffect {
    /// A pull request was opened.
    PullRequest { url: String, number: u64 },
    /// A comment was posted.
    Comment { url: String },
    /// A commit was made.
    Commit { sha: String, branch: Option<String> },
    /// A branch was pushed.
    Push { branch: String, remote: String },
    /// An artifact was written.
    Artifact { name: String, path: String },
}
```

### `engine.rs` (frozen API, impl later — feature `runtime`)

```rust
//! The `Engine` façade: the API embedders drive. The concrete implementation lands
//! at the execution milestone; this trait + builder are FIXED now so the public API
//! does not churn when the executor arrives.

#![cfg(feature = "runtime")]

use std::sync::Arc;

use crate::api::{RunInput, RunSummary};
use crate::error::Result;
use crate::ids::RunId;
use crate::ir::Workflow;
use crate::registry::Registry;
use crate::traits::Store;

/// The thing embedders drive to run workflows.
#[async_trait::async_trait]
pub trait Engine: Send + Sync {
    /// Run a workflow to completion, returning the structured summary. Validates the
    /// input against the workflow's params first; checkpoints if the workflow is durable.
    async fn run(&self, workflow: &Workflow, input: RunInput) -> Result<RunSummary>;

    /// Resume any incomplete runs found in the `Store` (crash recovery).
    async fn resume_all(&self) -> Result<Vec<RunSummary>>;

    /// Fetch the summary of a known run id.
    async fn summary(&self, run_id: RunId) -> Result<Option<RunSummary>>;
}

/// Wires a [`Registry`] of plugins + a [`Store`] into a concrete engine.
#[derive(Default)]
pub struct EngineBuilder {
    registry: Registry,
    store: Option<Arc<dyn Store>>,
}

impl EngineBuilder {
    /// New builder seeded with built-in providers/workspaces/actions.
    #[must_use] pub fn new() -> Self { Self { registry: Registry::with_builtins(), store: None } }
    /// Provide the durable store.
    #[must_use] pub fn store(mut self, s: Arc<dyn Store>) -> Self { self.store = Some(s); self }
    /// Access the registry to register custom plugins.
    pub fn registry_mut(&mut self) -> &mut Registry { &mut self.registry }
    /// Finalize. Errors if required plugins are missing. (Impl: execution milestone.)
    pub fn build(self) -> Result<Arc<dyn Engine>> { unimplemented!("execution milestone") }
}
```

---

## 6. VALIDATION RULES

`validate(wf, known) -> ValidationReport` runs **every** rule and **collects** all diagnostics (never fail-fast). `known` is `&Registry` under `runtime`, or `&KnownNames` (a thin struct of `&[&str]` lists) under `ir`-only builds. Each diagnostic carries a `Severity`, an `ODIN###` `DiagCode`, a structural `Pointer` (`steps[2].depends_on[0]`), a message that names the offender and suggests a fix, and an optional `help`/`did you mean`.

| # | Code | Severity | Rule | Message shape |
|---|------|----------|------|---------------|
| 1 | **ODIN001** | Error | `steps` is non-empty | `workflow has no steps` |
| 2 | **ODIN002** | Error | Each `step.id` non-empty after trim | `step #{i} has an empty id` |
| 3 | **ODIN003** | Error | `step.id` unique | `duplicate step id "build" (first at steps[1])` |
| 4 | **ODIN004** | Error | `step.id` matches `^[A-Za-z_][A-Za-z0-9_]*$` (valid template path segment; no hyphens — `a-b` parses as `a - b` in a template) | `step id "fix it" is invalid: use letters/digits/_ and start with a letter or _` |
| 5 | **ODIN005** | Error | Every `ProviderRef` (`provider:`, `judge.provider`, `on_fallback_provider`) is a known provider | `step "fix": unknown provider "claud"` + help `known: claude, codex, copilot` + `did you mean "claude"?` (Levenshtein) |
| 6 | **ODIN006** | Error | Provider step has at least one prompt source OR a documented continuation (else error) | `provider step "x" has no prompt; set prompt: or prompt_file:` |
| 7 | **ODIN007** | Error | A step's `produces` has no duplicate names | `step "x" produces "REPORT" more than once` |
| 8 | **ODIN008** | Error | Every `requires` name is produced by some step or is built-in `DIFF` | `step "review" requires artifact "SPEC" which no step produces` + help listing producers |
| 9 | **ODIN009** | Error | Provider step does not set both `prompt` and `prompt_file` | `step "x" sets both prompt and prompt_file; choose one` |
| 10 | **ODIN010** | Error | Every `action:` name is a known action | `step "open_pr": unknown action "gh.pr"` + did-you-mean |
| 11 | **ODIN011** | Error | `judge.threshold ∈ [0.0, 1.0]` | `step "x" judge threshold 1.5 out of range 0.0..=1.0` |
| 12 | **ODIN012** | Error | Every `depends_on` target exists | `step "test" depends on unknown step "biuld"` + did-you-mean |
| 13 | **ODIN013** | Error | No step depends on itself | `step "x" cannot depend on itself` |
| 14 | **ODIN014** | Error | `depends_on` graph is acyclic | `dependency cycle: a → b → a` (reports one concrete cycle) |
| 15 | **ODIN015** | Error | A `requires` artifact's producer is a transitive `depends_on` ancestor (ordering correctness) | `step "review" requires "PLAN" but its producer "plan" is not an upstream dependency` |
| 16 | **ODIN016** | Error | `slot_pool.pool >= 1` | `workspace pool must be >= 1, got 0` |
| 17 | **ODIN017** | Error | Every `{{ ref }}` in `prompt`/`prompt_file`(loaded)/`when`/`gates`/`judge.criteria`/`with` resolves against the static `ContextShape` (requires `templating`) | `step "x" when: references unknown "steps.plna.outputs.x"; did you mean "plan"?` |
| 18 | **ODIN018** | Error | `when`/template parses as valid minijinja (requires `templating`) | `step "x" when: template syntax error: {detail}` |
| 19 | **ODIN019** | Error | A step `produces` the reserved name `DIFF` | `"DIFF" is auto-captured by the engine; remove it from produces on step "x"` |
| 20 | **ODIN020** | Error | Cron trigger `schedule` is a valid 5-field expression | `trigger cron schedule "* * *" is not a valid 5-field expression` |
| 21 | **ODIN021** | **Warning** | `judge.provider == step.provider` (for provider steps) | `step "x" is judged by the same provider ("claude") it produced — consider an independent judge` |
| 22 | **ODIN022** | **Warning** | Param `required: true` with a `default` | `param "branch" is required but also has a default; the default is unreachable` |
| 23 | **ODIN023** | **Warning** | `retry.on_fallback_provider` set | `on_fallback_provider is declared but routing/fallback is not implemented in v1; this field is inert` |
| 24 | **ODIN024** | **Warning** | A declared param is never referenced in any template | `param "x" is declared but never used` |
| 25 | **ODIN025** | **Warning** | `prompt`/`prompt_file` set on an action/run step (caught by `deny_unknown_fields`? No — caught here since they're not in raw for those kinds) | `step "x" is an action step but sets prompt; prompts apply to provider steps only` |
| 26 | **ODIN026** | **Warning** | Unknown key at the **workflow root** (forward-compat tolerance, C7) | `unknown field "workspce" at root — ignored (typo? or written for a newer schema minor)` |
| 27 | **ODIN027** | **Warning** | `schema_version.minor` newer than engine's | `schema_version 1.3 is newer than this engine's 1.0; unknown features ignored` |

**Not in the table (deliberately upgraded to parse-time errors, better location):** unknown leaf fields (serde `deny_unknown_fields`), invalid durations (`HumanDuration::deserialize`), bad param type, bad workspace `type`, more-than-one / zero step kind (hand-written `StepKind` deserialize). These are *parse* errors (`Error::Parse`), not diagnostics.

```rust
// validate/mod.rs — orchestration sketch
pub fn validate(wf: &Workflow, known: &KnownNames<'_>) -> ValidationReport {
    let mut d = Vec::new();
    rules::step_list_nonempty(wf, &mut d);   // ODIN001
    rules::step_ids(wf, &mut d);             // ODIN002/003/004
    rules::provider_refs(wf, known, &mut d); // ODIN005
    rules::prompts(wf, &mut d);              // ODIN006/009/025
    rules::actions(wf, known, &mut d);       // ODIN010
    rules::judge(wf, &mut d);                // ODIN011/021
    rules::depends_on(wf, &mut d);           // ODIN012/013
    graph::cycles(wf, &mut d);               // ODIN014
    rules::artifacts(wf, &mut d);            // ODIN007/008/015/019
    rules::workspace(wf, &mut d);            // ODIN016
    rules::triggers(wf, &mut d);             // ODIN020
    rules::params(wf, &mut d);               // ODIN022/024
    rules::retry_fallback(wf, &mut d);       // ODIN023
    rules::schema(wf, &mut d);               // ODIN027
    #[cfg(feature = "templating")]
    crate::context::refs::check(wf, &mut d); // ODIN017/018
    // ODIN026 requires a raw-Value reparse; run it in from_yaml_str's caller or here
    // when the raw source is threaded through (see ImplementationOrder step 9).
    ValidationReport { diagnostics: d }
}
```

```rust
// validate/diagnostic.rs — the diagnostic types (abridged; full Pointer impl as in simplicity lens)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity { Warning, Error }

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub enum DiagCode { /* one variant per ODIN### above */ }
impl DiagCode { #[must_use] pub fn as_str(self) -> &'static str { /* "ODIN001".. */ } }

#[derive(Clone, Debug, serde::Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: DiagCode,
    pub message: String,
    pub pointer: String,          // rendered "steps[2].depends_on[0]"
    pub help: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ValidationReport { pub diagnostics: Vec<Diagnostic> }
impl ValidationReport {
    #[must_use] pub fn has_errors(&self) -> bool { self.diagnostics.iter().any(|d| d.severity == Severity::Error) }
    #[must_use] pub fn error_count(&self) -> usize { self.diagnostics.iter().filter(|d| d.severity == Severity::Error).count() }
    pub fn into_result(self) -> crate::error::Result<()> {
        if self.has_errors() { Err(crate::error::Error::Validation(self)) } else { Ok(()) }
    }
}
```

`graph::topo_order(wf) -> Result<Vec<StepId>, Vec<StepId>>` (Kahn; ties broken by declaration order) is `pub` and lives in `validate/graph.rs` — **one** implementation consumed by both the cycle check (ODIN014) and the future executor.

---

## 7. TEMPLATING / CONTEXT MODEL (`context/`, feature `templating`)

### Context shape — what is available WHERE

| Reference | Available in | Source | Statically checked? |
|-----------|--------------|--------|---------------------|
| `params.<name>` | all templates | `RunInput.params` + declared defaults | **Yes** — `<name>` must be a declared param (ODIN017) |
| `trigger.<...>` | all templates | `RunInput.trigger_payload` (free JSON) | **Root only** — `trigger.*` always allowed; children NOT modeled (open webhook payloads) |
| `steps.<id>.outputs.<k>` | a step iff `<id>` ∈ its **transitive `depends_on`** | upstream `StepState.outputs` | **`<id>` checked**: must be a declared step id AND a dependency (ODIN017 / DAG-aware); `<k>` leaf NOT checked (agent decides keys) |
| `steps.<id>.exit_code` | same dependency rule | upstream exit code | `<id>` checked |
| `steps.<id>.status` | same | upstream status | `<id>` checked |
| `artifacts.<NAME>` | a step iff `<NAME>` ∈ its `requires` (or `DIFF`) | run artifact catalog | **`<NAME>` checked** |
| `run.id`, `run.workflow` | all templates | run state | roots checked |

### How the validator checks refs (ODIN017/018)

1. `ContextShape::of(wf)` collects: declared param names, all `produces` names + `DIFF`, all step ids, and (per step) its transitive `depends_on` set computed from the DAG.
2. For each templated string on a step (`prompt`, loaded `prompt_file`, `when`, each gate command, `judge.criteria`, and string values inside `with`), parse with minijinja; on parse failure → **ODIN018**.
3. Walk the AST for variable paths (`Var` + `GetAttr` chains). For each path:
   - First segment must be one of `params|trigger|steps|artifacts|run` (else ODIN017).
   - `params.X` → `X` must be a declared param.
   - `steps.X[...]` → `X` must be a declared step id **and** in this step's transitive `depends_on` (DAG-aware: stays correct under fan-out). Leaf keys under `outputs` are not checked.
   - `artifacts.NAME` → `NAME` ∈ this step's `requires` ∪ `{DIFF}`.
   - `trigger.*` → only the root is validated; children pass.
   - Unknown step/param/artifact → emit ODIN017 with a Levenshtein "did you mean".

### `when:` evaluation semantics (runtime)

`when:` is a minijinja **expression** evaluated against the run context with `UndefinedBehavior::Strict` (an undefined reference is a runtime error, agreeing with the static checker). Evaluate via `Environment::compile_expression(expr).eval(ctx)` and take `value.is_true()` (minijinja's truthiness: `false`/`0`/`""`/`none`/empty-seq are falsy). Empty/absent `when` ⇒ run. This replaces the simplicity lens's string-render hack with minijinja's native expression eval (more correct).

```rust
// context/render.rs — signatures
pub fn build_context(/* &RunState, &Step, &Workflow */) -> minijinja::Value;
pub fn render_template(tpl: &str, ctx: &minijinja::Value, what: &str) -> crate::Result<String>;
pub fn eval_when(expr: &str, ctx: &minijinja::Value) -> crate::Result<bool>; // compile_expression + is_true
```

---

## 8. EXAMPLE WORKFLOW YAML (`examples/fix-flaky-test.yaml`)

```yaml
# Exercises the ENTIRE IR: schema version, metadata, durability, slot-pool workspace
# + reset mode, all three trigger kinds, two param types with required/default, all
# three step kinds (provider/action/run), prompt vs prompt_file, requires/produces +
# built-in DIFF, gates map, judge with a DIFFERENT provider, retry with inert
# fallback, timeouts, when-conditionals, and a fan-in DAG.

schema_version: "1.0"            # major-gated (ODIN; unknown major rejected)
name: fix-flaky-test
version: "0.3.1"                 # author's workflow version (opaque to engine)
description: >
  Diagnose a flaky test from a GitHub issue, propose a fix, verify it, open a PR.
durable: true                    # checkpointed & crash-resumable

workspace:
  type: slot_pool                # non-default workspace variant
  pool: 4                        # ODIN016: must be >= 1
  reset: git_clean               # reset --hard + clean -fdx between runs

triggers:                        # extensible list; only `manual` executes in v1
  - type: manual
  - type: github_webhook
    events: ["issues.labeled"]
    repo: marlboro-red/odin
  - type: cron
    schedule: "0 3 * * 1"        # ODIN020: validated as 5-field cron

params:
  issue_number:
    type: number
    required: true
    description: GitHub issue describing the flake.
  base_branch:
    type: string
    default: "main"

defaults:
  timeout: "30m"
  retry:
    max: 1
    backoff: exponential

steps:
  # ── provider kind: inline prompt, produces an artifact ──
  - id: plan
    provider: claude             # ProviderRef; ODIN005 checks it's registered
    prompt: |
      Read issue #{{ params.issue_number }} on branch {{ params.base_branch }}.
      Produce a one-paragraph root-cause hypothesis and a fix plan.
    artifacts:
      produces: [PLAN]
    timeout: "20m"

  # ── provider kind: prompt_file, requires PLAN, produces DIFF, gates, retry ──
  - id: implement
    provider: codex
    prompt_file: prompts/implement.j2
    depends_on: [plan]
    artifacts:
      requires: [PLAN]           # ODIN008/015: producer `plan` is an upstream dep ✓
      produces: [DIFF]           # NOTE: also auto-captured; listing DIFF triggers ODIN019.
                                 #       (Shown to document the rule; remove in real files.)
    gates:                       # all must exit 0
      typecheck: "cargo check --workspace"
      unit: "cargo test --workspace --quiet"
    retry:
      max: 2
      backoff: exponential
      on_fallback_provider: claude   # ODIN023 warning: inert in v1
    timeout: "30m"

  # ── provider kind: judge with a DIFFERENT provider, when-conditional ──
  - id: review
    provider: claude
    prompt: |
      Review the change for correctness:
      {{ artifacts.DIFF }}
    depends_on: [implement]
    when: "steps.implement.exit_code == 0"   # DAG-reachable ref ✓ (ODIN017)
    judge:
      provider: codex            # != step provider → no ODIN021 warning
      criteria: "The diff fixes the flake without weakening assertions."
      threshold: 0.7             # ODIN011: in [0,1] ✓

  # ── run kind: code hook, fan-in not yet (single dep) ──
  - id: changelog
    run: "scripts/gen-changelog.sh {{ params.issue_number }} > CHANGELOG.md"
    depends_on: [implement]
    timeout: "60s"

  # ── action kind: built-in side-effect, FAN-IN (two parents), conditional ──
  - id: open_pr
    action: github.open_pr       # ODIN010 checks it's registered
    with:
      title: "Fix flaky test (#{{ params.issue_number }})"
      base: "{{ params.base_branch }}"
      body: "{{ steps.review.outputs.summary }}"
    depends_on: [review, changelog]            # genuine DAG fan-in
    when: "steps.review.outputs.passed == true"
```

This single file forces the parser/validator through: schema gate, slot-pool leaf (`deny_unknown_fields`), three trigger kinds, two param types + default, all three step kinds, `prompt` vs `prompt_file`, `requires`/`produces` + built-in `DIFF` (and intentionally demonstrates ODIN019), gates map, cross-provider judge (happy path), retry with inert fallback (ODIN023), timeouts, two `when:` styles, and a fan-in `depends_on` DAG. (To make the file *clean*, delete the `produces: [DIFF]` line — it's left in to exercise ODIN019.)

---

## 9. FLAGGED DECISIONS (escalate; defaults chosen)

- **`ProviderRef` open string vs closed enum (C1).** *Default: open string + registry + ODIN005 "did you mean".* Closing it would make unknown providers a parse error (nicer) but blocks third-party providers — contradicts the locked decision. Confirm we want third-party providers in scope for v1's surface.
- **`deny_unknown_fields` split (C7).** *Default: deny on leaves, warn (ODIN026) at root.* Inverts to strict-everywhere with an opt-in `--lenient` if the team prefers. Recommend warn-by-default + a CLI `--strict` flag for CI.
- **`tokio`/`async-trait` baked into the trait surface (C3, C14).** *Default: commit, gated behind `runtime`.* One-way door once published. The parse/validate path stays runtime-free; confirm before 0.1.0.
- **`when:` truthiness via minijinja `is_true()`.** *Default: minijinja native semantics* (`false`/`0`/`""`/`none`/empty falsy). Confirm this is the intended contract vs a stricter boolean-only rule.
- **`produces` shorthand on a step.** *Default: nested `artifacts.produces` only* (one representation; the flat form is a `deny_unknown_fields` parse error). Authors will want the shorthand — confirm whether to add it as sugar.
- **ODIN015 artifact-ordering strictness.** *Default: named artifacts require a `depends_on` edge; raw shared-workdir files do not.* Confirm the data-flow contract so the rule isn't too strict for the shared-workdir model.
- **`WorkspaceHandle` is `Clone + Serialize` (merge of two lenses).** *Default: yes*, because `RunState` must persist it for resume; single-ownership enforced by lifecycle, not the type. Confirm acceptable.
- **`schema_version` minor concept (ODIN027).** *Default: reserved seam, warn-only.* No minor features exist yet; confirm we want the warning path now.

---

## 10. IMPLEMENTATION ORDER (crate compiles at each step)

Each step leaves the workspace `cargo build`/`cargo test`/`cargo clippy -D warnings` green. Steps 1–9 need only the existing deps + `anyhow`/`async-trait`/`tokio`/`tokio-util` added; gate the async ones behind `runtime`.

1. **`Cargo.toml` features + deps.** Add `[features]` block and optional `minijinja`/`async-trait`/`tokio`/`tokio-util`/`anyhow`/`futures-core`. Build (no new modules yet) → green.
2. **`ids.rs`.** Newtype IDs + `RunId`. Pure, no deps beyond serde/uuid. Add `mod ids;` + re-exports to `lib.rs`. Unit test Display/serde round-trip.
3. **`usage.rs`.** `Usage`. Trivial. Re-export.
4. **`error.rs`.** Crate `Error` + per-trait error enums. The `Validation` variant references `validate::ValidationReport` — temporarily `pub struct ValidationReport;` stub in `validate/diagnostic.rs` (step 7) or feature-gate; simplest: write `validate/diagnostic.rs` *before* `error.rs`. **Reorder: do step 7a (diagnostic types) before error.rs.** Gate `Template` variant behind `templating`, `Other(anyhow)` behind `runtime`.
5. **`ir/duration.rs`.** `HumanDuration` + tests for `30s`/`5m`/`2h`/bad units.
6. **`ir/` data types.** `workspace.rs`, `trigger.rs`, `params.rs`, then `step.rs` (with hand-written `StepKind` deserialize), then `workflow.rs`, then `ir/mod.rs` with `from_yaml_str`/`from_yaml_path`. Add round-trip tests: parse the example YAML, assert structure; assert "two kinds" and "zero kinds" produce precise errors. → green; **the IR is now usable.**
7. **`validate/`.** (a) `diagnostic.rs` (`Diagnostic`/`Severity`/`DiagCode`/`Pointer`/`ValidationReport`) — *do this before/with `error.rs`*. (b) `graph.rs` (`topo_order` + cycle detection). (c) `rules.rs` (all non-template rules). (d) `mod.rs` orchestration. Tests: a fixture per rule asserting its `DiagCode` fires. → green; **`odin validate` is implementable.**
8. **`KnownNames` shim** in `validate` so ODIN005/ODIN010 work without `runtime` (built-in name list `["claude","codex","copilot"]`, `["github.open_pr", ...]`).
9. **`context/`** (feature `templating`). `shape.rs`, `refs.rs` (ODIN017/018), `render.rs` (`build_context`/`render_template`/`eval_when`). Wire `refs::check` into `validate()` under `cfg(templating)`. Add the root-unknown-key warning (ODIN026) by threading the raw `serde_yaml_ng::Value` reparse. → green with `--features templating`.
10. **`traits/`** (feature `runtime`). `provider.rs`, `workspace.rs`, `store.rs`, `action.rs`, `trigger.rs`, `mod.rs`. Needs `api.rs` (step 11) for `RunInput`/`RunStatus`/`SideEffect` referenced by `store.rs`/`trigger.rs` — **write `api.rs` first or in tandem.** → green with `--features runtime`.
11. **`api.rs`.** `RunInput`/`RunSummary`/`StepResult`/`RunStatus`/`SideEffect`. (Available without `runtime`; only references ids/usage/`StepStatus`. Move `StepStatus` to `traits/store.rs` but re-export, OR define `StepStatus` in `api.rs` and have `store.rs` re-export it — **define `StepStatus` in `api.rs`** to avoid a `runtime`→`api` cycle when `api` is built without `runtime`.)
12. **`registry.rs`** (feature `runtime`). `Registry` + `with_builtins()` (empty until providers exist) + `register_*`. Switch `validate()` to accept `&Registry` when `runtime` is on (via a `KnownNames::from(&Registry)`).
13. **`engine.rs`** (feature `runtime`). `Engine` trait + `EngineBuilder` (`build()` = `unimplemented!`). Freezes the public driving API.
14. **`mock.rs`** (`cfg(any(test, feature = "mock"))`). The five Noop impls. Add a test that drives them through the trait objects to prove object-safety and one-file implementability.
15. **`lib.rs` curated re-exports.** Finalize the flat public API (`use odin_core::{Workflow, RunInput, RunSummary, Provider, ...}`); mark internal paths `pub(crate)`/`#[doc(hidden)]`. `cargo doc` clean.
16. **`odin-cli` `validate` command.** `cmd/validate.rs`: load file, `Workflow::from_yaml_str`, `validate`, print diagnostics (human + `--json`), exit non-zero on errors. (clap added here.) `odin-daemon` stays a stub.
17. **`examples/fix-flaky-test.yaml`** committed; add an integration test that parses + validates it and asserts the expected ODIN019/ODIN023 warnings.

**Dependency-ordering corrections folded in:** diagnostic types (7a) precede `error.rs` (4); `api.rs`'s `StepStatus` is the canonical definition (11) to avoid an `api`↔`traits` cycle in non-`runtime` builds; `api.rs` is written alongside `traits/` (10–11). With those, every step compiles.

---

This blueprint is internally consistent and directly implementable: the IR is fully typed and DAG-ready, validation is deterministic and collects all errors with `ODIN###` codes, the five traits are object-safe with per-trait errors and one required method each, the public `RunInput`/`RunSummary` contracts are pure serializable data, and every forward-compat seam kept (`#[non_exhaustive]`, registry, snapshot-Store + event log, inert `on_fallback_provider`, `schema_version`) is cheap, while the speculative ones (`ProviderSelector`, `ExecutionTarget`, `extra` bags, pure event-sourcing) are cut.

Key files to create, all under `/Users/aleph/Desktop/Projects/marlboro-red/odin/`: `crates/odin-core/src/{ids,usage,error,registry,api,engine,mock}.rs`, `crates/odin-core/src/ir/{mod,workflow,workspace,trigger,params,step,duration}.rs`, `crates/odin-core/src/validate/{mod,diagnostic,rules,graph}.rs`, `crates/odin-core/src/context/{mod,shape,refs,render}.rs`, `crates/odin-core/src/traits/{mod,provider,workspace,store,action,trigger}.rs`, `crates/odin-cli/src/cmd/{mod,validate}.rs`, and `examples/fix-flaky-test.yaml`. Modify `crates/odin-core/src/lib.rs` (re-exports) and `crates/odin-core/Cargo.toml` (features + optional deps).