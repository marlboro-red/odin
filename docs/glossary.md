# Glossary

The vocabulary Odin's docs and API use. Terms are grouped by what they describe.

## Workflow & execution

- **Workflow** — a declarative YAML (or programmatic [`Workflow`]) definition of work: typed
  `params`, a `workspace`, ordered `steps`, and optional `triggers`. The unit you `odin run` or a
  daemon serves.
- **Run** — one execution of a workflow. Has a UUID `run_id`, a [status](#statuses), per-step
  results, aggregate usage, and (if `durable`) a persisted checkpoint. Produces a `RunSummary`.
- **Step** — one node in a workflow. Its `kind` is one of: `provider` (invoke an agent CLI), `run`
  (a shell command), `action` (a named built-in side-effect), `case` (branch), `loop` (repeat
  until a condition), or `approval` (a human gate).
- **Gate** — a per-step verification command (`gates:`) that must exit 0 for the step to pass; its
  output feeds `retry.feedback`. (Distinct from an **approval gate**, a human `approval` step.)
- **Judge** — an LLM-as-judge verification: a `provider` scores the step's output, and a
  sub-threshold score fails the step.
- **Scratch** — a `scratch: true` step runs in its own throwaway worktree; its diff is surfaced as
  `outputs.diff` and the worktree is discarded. Used for parallel, merge-free candidates.
- **Trigger** — what starts a run: `manual` (default), `cron` (a schedule), `github_webhook`, or a
  generic `webhook`. The daemon dispatches the event-driven ones.

## Durability

- **Durable** — a `durable: true` workflow checkpoints its run state to the [store](#integration-traits)
  after every step, so a crashed run resumes (`resume_all`) on the next start. A non-durable run is
  in-memory only.
- **Checkpoint** — a persisted snapshot of a run's `RunState` (steps, artifacts, usage, …) written
  at each step boundary for a durable run.
- **Resume** — re-running a durable run from its last checkpoint after a crash/restart, re-executing
  only the unfinished steps (completed steps + their side-effects are seeded from the store).
- **Idempotency** — a durable resume must not re-apply a side-effect a completed step already
  performed; the engine seeds completed steps' side-effects from the store instead of re-running.

## Integration traits

The five pluggable interfaces an embedder can implement (or use the built-ins):

- **Provider** — invokes an agent CLI (`claude` / `codex` / `copilot`) for a `provider:` step.
- **Workspace** — provisions the working directory for a run (`worktree` or `slot_pool`).
- **Store** — persists durable run state and the audit log (`SqliteStore`, or `MemStore` for tests).
- **Action** — a named built-in side-effect for `action:` (`git.commit`, `github.open_pr`, …).
- **Trigger** — a source of `TriggerEvent`s the daemon's supervisor loop drives.

## Statuses

- **Run status** (snake_case on the wire): `pending`, `running`, `awaiting_approval`, `succeeded`,
  `failed`, `cancelled`. The terminal ones are succeeded/failed/cancelled.
- **Step status**: `pending`, `running`, `awaiting_approval`, `passed`, `failed`, `skipped`.

## Surfaces

- **`odin`** — the CLI runner (validate, run, list/show/logs/status, approve/reject/cancel, prune,
  recipe). See [cli.md](cli.md).
- **`odind`** — the daemon: serves cron + webhook triggers, the HTTP approval endpoint, the
  dashboard, and Prometheus `/metrics`. See [daemon.md](daemon.md).
- **Recipe** — a workflow stored by name in the recipe catalog, run as `odin run <name>`.
- **Artifact** — a named file a step produces/requires; the auto-captured git diff is the reserved
  `DIFF` artifact.

[`Workflow`]: ../crates/odin-core/src/ir/workflow.rs
