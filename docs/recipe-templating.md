# Scaffolding & templating recipes

`odin recipe new` scaffolds a new workflow from an existing recipe, a [bundled starter](cli.md#odin-recipe-subcommand),
or a file — so you start from something that already works instead of a blank file. Optionally, a
source can declare **template variables** you fill in at scaffold time.

There are **two distinct templating layers**, and they don't collide:

| | Scaffold-time (`@@VAR@@`) | Run-time (`{{ … }}`) |
|---|---|---|
| Filled by | `odin recipe new --set` (once, when you scaffold) | the engine, on every run |
| Sees | values you pass on the command line | `params.*`, `trigger.*`, `steps.*.outputs.*` |
| Result | **baked into** the new file as a constant | stays dynamic in the file |
| Delimiter | `@@name@@` | `{{ expr }}` |

`@@…@@` was picked for **zero overlap** with the run-time `{{ }}` templates, shell heredocs
(`<<'EOF'`), shift operators (`a << b`), and shell expansion (`${VAR}`) — so a templated recipe's
run-time `{{ params.x }}` passes straight through untouched.

## Scaffolding (no templating)

```sh
odin recipe new my-review --from adversarial-review     # copy a starter → ./my-review.yaml
odin recipe new my-flow   --from ./some/workflow.yaml    # …or from any file
odin recipe new prod      --from issue-to-pr --catalog   # …installed into the catalog (run by name)
```

`new` rewrites the new file's `name:` to `<name>` and prints a `validate`/`run` hint. Destinations:

| Flag | Writes to |
|------|-----------|
| *(default)* | `./<name>.yaml` |
| `--out <PATH>` | a `.yaml`/`.yml` file, or a directory (writes `<name>.yaml` inside) |
| `--catalog` | the recipe catalog as `<name>` (then `odin run <name>`) |
| `--stdout` | stdout (nothing written; provenance goes to stderr, so it pipes cleanly) |

`new` refuses to overwrite an existing destination without `--force`.

## Templating

A source becomes a **template** by declaring a `# odin:template … # end` comment header, then
using `@@name@@` placeholders in the body:

```yaml
# odin:template
#   provider:    { default: claude }
#   base_branch: { required: true, description: "branch to diff the PR against" }
# end
name: review-template
workspace: { type: worktree }
steps:
  - id: diff
    run: "git diff @@base_branch@@...HEAD"
  - id: review
    provider: @@provider@@
    prompt: "Review the diff above."
    depends_on: [diff]
    when: "{{ params.dry_run }}"     # run-time templating is untouched
```

Each variable maps to a small spec:

- `default: <value>` — used when `--set` doesn't supply one. A variable **with** a default is
  optional; one **without** is required.
- `required: true|false` — force a variable required (or not), overriding the default-based rule.
- `description: "<text>"` — documentation (shown in prompts and `--explain`).

A worked example ships at [`examples/templated-pr-review.yaml`](../examples/templated-pr-review.yaml).

Scaffold it, filling the variables:

```sh
odin recipe new pr-review --from ./review-template.yaml --set base_branch=main
#   provider defaults to claude; base_branch is required → must be --set
```

The result has the header **stripped**, `@@base_branch@@` replaced with `main`, `@@provider@@`
with `claude`, and the run-time `{{ params.dry_run }}` left exactly as written.

### Preview & prompts

**`--explain`** prints what would be filled — the scaffold-time `@@VAR@@` values (and where each
comes from: `set` / `default` / `required`) and the run-time `{{ params.* }}` the result still
expects — and **writes nothing**:

```sh
$ odin recipe new pr-review --from ./review-template.yaml --set base_branch=main --explain
recipe 'pr-review' from file ./review-template.yaml

Scaffold-time variables (@@VAR@@), baked in now:
  @@provider@@ = claude  [default]  # agent CLI to review with
  @@base_branch@@ = main  [set]     # branch to diff against

Run-time params ({{ params.* }}), supplied per run:
  report  (required)
```

**Interactive prompts** — when stdout is a terminal (or you pass `--interactive`), a required
variable left unset is **prompted for** (showing its description). A non-interactive run (a script,
a pipe, CI) keeps the hard "missing required variable" error instead of hanging.

### Rules & guard-rails

- Only a file with a `# odin:template` header is ever substituted; **any other source is copied
  byte-for-byte**. Passing `--set` to a header-less source is an error (so a typo doesn't silently
  no-op).
- Values are injected **verbatim**. Put a placeholder where a value goes; if a value may contain
  YAML-special characters, quote the field in the template (`field: "@@var@@"`).
- A `@@…@@` whose name isn't declared, a missing required variable, an undeclared `--set` key, or a
  leftover `@@…@@` after substitution each fail with a clear message. After substitution the new
  file is re-parsed, so a value that breaks the YAML structure is caught immediately — but run
  `odin validate <new-file>` for the full rule-level checks before relying on it.

See the [`odin recipe`](cli.md#odin-recipe-subcommand) reference for every flag, and
[getting started](getting-started.md#run-by-name-the-recipe-catalog) for the catalog basics.
