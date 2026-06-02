# Odin

**A durable, library-first workflow engine that orchestrates autonomous coding-agent
CLIs** — Claude Code, OpenAI Codex, and the GitHub Copilot CLI — to perform
software-engineering work without supervision.

You describe a workflow in YAML (with code-hook escape hatches), pin a provider per
step, and Odin runs it: planning, implementing, self-reviewing, and opening a PR — with
every step checkpointed so a crashed run resumes where it left off.

> **Status: runs end-to-end.** The workflow IR, the full validator (26 diagnostics),
> the templating/context model, the five integration traits, the durable SQLite store,
> the worktree + slot-pool workspaces, the Claude provider adapter, and the executor are
> all implemented, tested, and documented. `odin validate` and `odin run` both work.
> Still to come: the Codex/Copilot provider adapters, built-in actions (`github.open_pr`),
> LLM-as-judge, and the daemon's event triggers.

## Quickstart

```sh
# Build the workspace (Rust 1.85+, edition 2024).
cargo build

# Validate a workflow — reports every problem in one pass, with "did you mean" hints.
cargo run -p odin-cli -- validate examples/issue-to-pr.yaml
# ✓ examples/issue-to-pr.yaml is valid

# Machine-readable diagnostics:
cargo run -p odin-cli -- validate --json examples/fix-flaky-test.yaml

# Run a workflow against a git repo (provisions a worktree, checkpoints to SQLite).
# Steps using `run:` execute for free; `provider:` steps invoke the real agent CLI.
cargo run -p odin-cli -- run path/to/workflow.yaml --repo . --param issue_url=https://...
```

## A workflow at a glance

```yaml
name: issue-to-pr
durable: true                      # checkpointed & crash-resumable
workspace: { type: worktree }      # or { type: slot_pool, pool: 4 }

params:
  issue_url: { type: string, required: true }

steps:
  - id: plan                       # provider step
    provider: claude
    prompt: "Read {{ params.issue_url }} and write a plan to plan.md."
    artifacts: { produces: [PLAN] }

  - id: implement                  # gated on a green build + tests
    provider: codex
    prompt: "Implement the plan in PLAN."
    depends_on: [plan]
    artifacts: { requires: [PLAN] }
    gates:
      build: "cargo build --workspace"
      test:  "cargo test --workspace"

  - id: review                     # judged by a *different* provider
    provider: claude
    prompt: "Review this diff:\n{{ artifacts.DIFF }}"
    depends_on: [implement]
    judge: { provider: codex, criteria: "Implements PLAN, no regressions.", threshold: 0.7 }

  - id: open_pr                    # built-in action, runs only if review passed
    action: github.open_pr
    with: { title: "Implement {{ params.issue_url }}" }
    depends_on: [review]
    when: "steps.review.outputs.passed == true"
```

See [`examples/`](examples/) for fully-annotated workflows, including
[`fix-flaky-test.yaml`](examples/fix-flaky-test.yaml), which exercises every IR feature.

## Why "library-first"?

The engine is the [`odin-core`](crates/odin-core) crate; the `odin` CLI and the (future)
`odind` daemon are thin runners on top. Everything you need to embed Odin in another
program lives in the library:

- **Data in** — [`RunInput`] carries the requirements: typed `params` (validated against
  the workflow) plus a free-form `trigger_payload` for arbitrary event data.
- **Data out** — [`RunSummary`] is plain serializable data: status, per-step results,
  aggregate token/cost usage, and structured `side_effects` (PRs opened, branches
  pushed) for downstream automation.
- **Plug in your own** — five small, object-safe traits are the entire integration
  surface. Implementing a new coding agent is one file:

  | Trait | Responsibility |
  |-------|----------------|
  | `Provider` | invoke a coding-agent CLI |
  | `Workspace` | provision an isolated per-run working directory |
  | `Store` | durably persist run state (crash-resume) |
  | `Action` | perform a named side-effect (`github.open_pr`, …) |
  | `Trigger` | emit run-starting events (manual, webhook, cron) |

### Feature flags

A parse-only embedder (a linter or an editor plugin) pays nothing for the async runtime:

| Feature | Pulls in | Use it for |
|---------|----------|------------|
| `ir` | serde only | parse + validate workflows |
| `templating` | minijinja | render prompts + statically check `{{ refs }}` |
| `runtime` | tokio, async-trait | the five traits, the registry, the engine |
| `mock` | (runtime) | Noop trait impls for downstream tests |
| `full` *(default)* | all of the above | |

```toml
# A linter that only parses and validates:
odin-core = { version = "0.0.1", default-features = false, features = ["ir", "templating"] }
```

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — the layered design, the integration
  surface, the data-flow contracts, and the full `ODIN###` diagnostic catalogue.
- [`docs/design/foundation-blueprint.md`](docs/design/foundation-blueprint.md) — the
  detailed implementation blueprint the foundation was built from.
- `cargo doc --open -p odin-core --all-features` — the API reference.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-features --all-targets   # -D warnings in CI
cargo test  --workspace
```

The workspace forbids `unsafe`, denies warnings in CI, and runs clippy at the `pedantic`
level.

## License

Licensed under the [MIT License](LICENSE).

[`RunInput`]: https://docs.rs/odin-core/latest/odin_core/api/struct.RunInput.html
[`RunSummary`]: https://docs.rs/odin-core/latest/odin_core/api/struct.RunSummary.html
