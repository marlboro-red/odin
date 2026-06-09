# Odin

**Get a deeper code review than any single agent can give.** Odin fans a panel of coding agents
out over your repo or PR — *in parallel, across providers (Claude Code, Codex, Copilot CLI)* — and
synthesizes them with a cross-provider judge into one ranked report. It's built on a durable,
crash-resumable workflow engine, so the same machinery also drives unattended issue→PR work on cron
and GitHub webhooks.

The sharpest thing it does today: a second opinion that a single `claude` call or a GitHub Action
can't — several reviewers, each in its own isolated worktree, on different models, reconciled by a
judge.

## See it in 30 seconds

No agent CLI or authentication required — `--mock` makes the provider steps echo their prompts so
the whole pipeline runs offline:

```sh
cargo build                                            # Rust 1.85+, edition 2024

# Zero-dependency hello-world — watch the engine provision a worktree and walk a DAG:
cargo run -p odin-cli -- run examples/quickstart.yaml --repo . --no-store

# The wedge — a parallel, cross-provider, judged review of THIS repo, written to REVIEW.md
# (offline via --mock; drop it once the agent CLIs are installed for a real review):
cargo run -p odin-cli -- run examples/deep-review.yaml --repo . --no-store --mock \
  --param out="$PWD/REVIEW.md"
```

[`examples/deep-review.yaml`](examples/deep-review.yaml) is four concurrent `scratch:` reviewers
(correctness / robustness / concurrency / security) → a cross-provider lead reviewer that
synthesizes a ranked P0/P1/P2 report. [`-codex`](examples/deep-review-codex.yaml) swaps the judge;
[`branching-review.yaml`](examples/branching-review.yaml) classifies the codebase's top concern and
deep-dives *only that one* via a `case:` branch.

## Prerequisites

- **Install:** prebuilt `odin` / `odind` binaries (Linux / macOS / Windows · x86_64 + arm64) are
  attached to each [tagged release](../../releases); or build from source with Rust 1.85+
  (`cargo build`).
- **A git repo** to run against (`--repo .`).
- **Agent CLIs** for `provider:` steps — whichever you reference: `claude` (Claude Code),
  `codex` (OpenAI Codex), `copilot` (GitHub Copilot CLI) — installed, on `PATH`, and
  **authenticated**. Or run with **`--mock`** to skip them entirely.
- **`gh`** (authenticated) to read/post on PRs — the `repo` scope for the PR-opening / commenting
  flows (`issue-to-pr`, `adversarial-review`); read-only for `local-review` (it only diffs a PR).

## It's also a general workflow engine

Review is the wedge, not the whole tool. The same engine runs durable, unattended software work —
plan → implement → self-review → open a PR — checkpointed so a crashed run resumes where it left
off, served on cron schedules and signed GitHub webhooks by the `odind` daemon:

```yaml
name: issue-to-pr
durable: true                      # checkpointed & crash-resumable
workspace: { type: worktree }      # or { type: slot_pool, pool: 4 }
params:
  issue_url: { type: string, required: true }
steps:
  - id: plan
    provider: claude
    prompt: "Read {{ params.issue_url }} and write a plan to plan.md."
    artifacts: { produces: [PLAN] }
  - id: implement                  # gated on a green build + tests
    provider: codex
    prompt: "Implement the plan in PLAN."
    depends_on: [plan]
    artifacts: { requires: [PLAN] }
    gates: { build: "cargo build --workspace", test: "cargo test --workspace" }
  - id: review                     # judged by a *different* provider
    provider: claude
    prompt: "Review this diff:\n{{ artifacts.DIFF }}"
    depends_on: [implement]
    judge: { provider: codex, criteria: "Implements PLAN, no regressions.", threshold: 0.7 }
  - id: open_pr                    # built-in action, runs only if review passed
    action: github.open_pr
    with: { title: "Implement {{ params.issue_url }}" }
    depends_on: [review]
    when: "steps.review.status == 'passed'"
```

Run a directory of workflows on their triggers (the webhook server starts only if a workflow
declares one):

```sh
ODIN_WEBHOOK_SECRET=… cargo run -p odin-daemon -- \
  --workflows examples --repo . --webhook-addr 127.0.0.1:9292
```

Workflows can also be kept in a **recipe catalog** and run by name (`odin recipe init`, then
`odin run deep-review …`). See [`examples/`](examples/) for the fully-annotated set:
[`quickstart`](examples/quickstart.yaml) (zero-dep hello-world),
[`deep-review`](examples/deep-review.yaml) / [`-codex`](examples/deep-review-codex.yaml) /
[`branching-review`](examples/branching-review.yaml) (the review wedge),
[`issue-to-pr`](examples/issue-to-pr.yaml) (the canonical PR flow),
[`fix-flaky-test`](examples/fix-flaky-test.yaml) (a kitchen-sink of step/trigger kinds),
[`nightly-maintenance`](examples/nightly-maintenance.yaml) (cron-served),
[`multi-agent-eval`](examples/multi-agent-eval.yaml) (parallel `scratch:` fan-out),
[`gated-deploy`](examples/gated-deploy.yaml) (a human `approval:` gate),
[`self-correct`](examples/self-correct.yaml) (`retry` with feedback),
[`iterate`](examples/iterate.yaml) (a `loop:` until a check passes),
[`triage`](examples/triage.yaml) (`case:` branching),
[`ship-release`](examples/ship-release.yaml) (a `slot_pool` re-clone), and
[`adversarial-review`](examples/adversarial-review.yaml) (**Odin reviewing its own PRs** —
webhook-triggered, parallel reviewers, judge, approval gate).

## Library-first

The engine is the [`odin-core`](crates/odin-core) crate; the `odin` CLI and the `odind` daemon are
thin runners on top. Everything to embed Odin in another program lives in the library: [`RunInput`]
carries typed `params` + a free-form `trigger_payload`; [`RunSummary`] is plain serializable data
(status, per-step results, token/cost usage, structured `side_effects`); and **five small,
object-safe traits are the entire integration surface** — implementing a new coding agent is one
file.

| Trait | Responsibility | | Feature | Pulls in / for |
|-------|----------------|---|---------|----------------|
| `Provider` | invoke a coding-agent CLI | | `ir` | serde — parse + validate |
| `Workspace` | isolate a per-run working dir | | `templating` | minijinja — render + check `{{ refs }}` |
| `Store` | persist run state (crash-resume) | | `runtime` | tokio/rusqlite — the traits + engine |
| `Action` | a named side-effect (`github.open_pr`) | | `mock` | test doubles (`EchoProvider`, `MemStore`) |
| `Trigger` | run-starting events (manual/webhook/cron) | | `full` *(default)* | `ir`+`templating`+`runtime` |

```toml
# A linter that only parses + validates pays nothing for the async runtime.
# Not yet on crates.io (the `odin-core` there is unrelated), so depend on git:
odin-core = { git = "https://github.com/marlboro-red/odin", default-features = false, features = ["ir", "templating"] }
```

See the [integration guide](docs/integration-guide.md) for the full embedding story.

## Documentation

- [Getting started](docs/getting-started.md) — build → validate → run → serve, the recipe catalog.
- [Workflow reference](docs/workflow-reference.md) — every YAML field and all `ODIN###` diagnostics.
- [`odin` CLI](docs/cli.md) and [`odind` daemon](docs/daemon.md) references.
- [Webhook walkthrough](docs/webhook-walkthrough.md) — wire a GitHub webhook end-to-end.
- [Scaffolding & templating](docs/recipe-templating.md) · [Integration guide](docs/integration-guide.md) · [Observability](docs/observability.md) · [Architecture](docs/architecture.md).
- `cargo doc --open -p odin-core --all-features` — the API reference.

## Status

A capable v0.x engine, openly pre-1.0 and not yet validated by external users. Implemented, tested
(~400 tests, clippy-pedantic, `unsafe`-forbidden), and documented: the workflow IR + full validator
(45 diagnostics), the templating/context model, the five traits, the durable SQLite store, worktree
+ slot-pool workspaces, all three provider adapters, the built-in actions, LLM-as-judge + retry, the
concurrent executor (`max_parallel` + `scratch:` fan-out), crash-resume with per-step git snapshots,
`case:`/`loop:` control flow, default step timeouts + run cancellation, the recipe catalog, the
`odin` CLI, and the `odind` daemon (cron + signed webhooks + concurrent dispatch). Known gaps:
codex/copilot dollar-cost reporting (token usage is parsed), operator-facing cost/usage surfaces,
and provider routing/fallback.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-features --all-targets   # -D warnings in CI
cargo test --workspace --all-features                   # matches CI
```

The workspace forbids `unsafe`, denies warnings in CI, and runs clippy at the `pedantic` level.

## License

Licensed under the [MIT License](LICENSE).

[`RunInput`]: https://docs.rs/odin-core/latest/odin_core/api/struct.RunInput.html
[`RunSummary`]: https://docs.rs/odin-core/latest/odin_core/api/struct.RunSummary.html
