//! Template context: what `{{ ... }}` references are available, how they are checked
//! statically, and how they are rendered at run time.
//!
//! Available only with the `templating` feature. The static checker (in [`refs`])
//! feeds diagnostics `ODIN017`/`ODIN018`/`ODIN024` into validation; the renderer
//! (in [`render`]) is used by the executor at run time.
//!
//! ## Context shape — what is available WHERE
//!
//! | Reference | Where | Statically checked |
//! |-----------|-------|--------------------|
//! | `params.<name>` | all templates | `<name>` must be a declared param |
//! | `trigger.<...>` | all templates | root only (open payload) |
//! | `steps.<id>.outputs.<k>` / `.exit_code` / `.status` | a step, iff `<id>` is a transitive dependency | `<id>` must be a declared, upstream step |
//! | `artifacts.<NAME>` | a step, iff `<NAME>` ∈ its `requires` (or `DIFF`) | `<NAME>` checked |
//! | `run.<...>` | all templates | root only |

pub mod refs;
pub mod render;
pub mod shape;

pub use shape::ContextShape;
