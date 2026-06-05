//! The `odin recipe` subcommands: inspect the workflow recipe catalog (run-by-name).
//!
//! `list` shows what's in the catalog, `show` prints a recipe's YAML, and `path` prints its
//! resolved filesystem path (for scripting). The catalog directory itself is resolved by
//! [`crate::catalog`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use odin_core::Workflow;

use crate::{catalog, scaffold};

/// `odin recipe list [--tag <T>]` — list the recipes in the catalog (name + description + tags),
/// optionally filtered to those carrying `<T>`, or as JSON.
pub(crate) fn list(
    recipes_dir: Option<&Path>,
    tag: Option<&str>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let dir = catalog::dir(recipes_dir)?;
    let all = catalog::list(&dir)?;
    // Tags are normalized to lowercase by the IR, so match against a lowercased filter.
    let tag_lc = tag.map(str::to_ascii_lowercase);
    let shown: Vec<&catalog::Recipe> = all
        .iter()
        .filter(|r| {
            tag_lc
                .as_ref()
                .is_none_or(|t| r.tags.iter().any(|rt| rt == t))
        })
        .collect();

    if json {
        let arr: Vec<_> = shown
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "path": r.path,
                    "workflow_name": r.workflow_name,
                    "description": r.description,
                    "tags": r.tags,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(ExitCode::SUCCESS);
    }

    print_human_list(&dir, all.is_empty(), &shown, tag);
    Ok(ExitCode::SUCCESS)
}

/// Renders the human (non-JSON) recipe listing: the empty-catalog hint, the no-match-for-`--tag`
/// line, or a name/description/tags table over `shown`.
fn print_human_list(dir: &Path, all_empty: bool, shown: &[&catalog::Recipe], tag: Option<&str>) {
    if all_empty {
        println!("no recipes in {}", dir.display());
        println!(
            "  run `odin recipe init` to add the bundled starters, or `odin recipe add <file>`."
        );
        return;
    }
    if shown.is_empty() {
        println!(
            "no recipes tagged {:?} in {}",
            tag.unwrap_or(""),
            dir.display()
        );
        return;
    }
    println!("recipes in {}:\n", dir.display());
    let width = shown.iter().map(|r| r.name.len()).max().unwrap_or(0);
    for r in shown {
        let tags = if r.tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", r.tags.join(", "))
        };
        match (&r.workflow_name, &r.description) {
            (None, _) => println!("  {:<width$}  (does not parse as a workflow)", r.name),
            (Some(_), Some(desc)) => println!("  {:<width$}  {desc}{tags}", r.name),
            (Some(_), None) => println!("  {:<width$}{tags}", r.name),
        }
    }
}

/// `odin recipe init` — seed the catalog with the bundled starter recipes. Existing recipes are
/// kept unless `--force`.
pub(crate) fn init(recipes_dir: Option<&Path>, force: bool) -> anyhow::Result<ExitCode> {
    let dir = catalog::dir_create(recipes_dir)?;
    let (wrote, skipped) = catalog::seed(&dir, force)?;
    println!("seeded {} recipe(s) into {}", wrote.len(), dir.display());
    if !skipped.is_empty() {
        println!(
            "  kept {} already present: {} (use --force to overwrite)",
            skipped.len(),
            skipped.join(", ")
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `odin recipe add <file> [--as <name>]` — copy a workflow file into the catalog. The recipe name
/// defaults to the file's stem; refuses to overwrite an existing recipe unless `--force`.
pub(crate) fn add(
    file: &Path,
    as_name: Option<&str>,
    force: bool,
    recipes_dir: Option<&Path>,
) -> anyhow::Result<ExitCode> {
    let body =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    // Validate it's a parseable workflow before adding, so a typo is caught now (not at run time).
    Workflow::from_yaml_str(&body)
        .with_context(|| format!("{} is not a valid workflow", file.display()))?;

    let name = match as_name {
        Some(n) => n.to_owned(),
        None => file
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .with_context(|| {
                format!(
                    "could not derive a recipe name from {}; pass --as <name>",
                    file.display()
                )
            })?,
    };
    if !catalog::is_valid_name(&name) {
        anyhow::bail!("invalid recipe name '{name}' (no path separators, '.', or '..')");
    }

    let dir = catalog::dir_create(recipes_dir)?;
    let dest = dir.join(format!("{name}.yaml"));
    if dest.exists() && !force {
        anyhow::bail!(
            "recipe '{name}' already exists at {} (use --force to overwrite)",
            dest.display()
        );
    }
    std::fs::write(&dest, &body).with_context(|| format!("writing {}", dest.display()))?;
    println!("added recipe '{name}' → {}", dest.display());
    Ok(ExitCode::SUCCESS)
}

/// Parsed arguments for `odin recipe new`.
pub(crate) struct NewArgs {
    pub name: String,
    pub from: String,
    pub set: Vec<String>,
    pub out: Option<PathBuf>,
    pub catalog: bool,
    pub stdout: bool,
    pub recipes_dir: Option<PathBuf>,
    pub force: bool,
}

/// `odin recipe new <name> --from <src> [--set k=v]` — scaffold a new workflow from a recipe,
/// bundled starter, or file. If the source declares a `# odin:template` header, `@@VAR@@`
/// placeholders are filled from `--set` (+ defaults); otherwise the source is copied verbatim. The
/// new file's `name:` is rewritten to `<name>`. Destination: `./<name>.yaml` by default, or `--out`
/// (a file/dir), `--catalog` (install into the catalog), or `--stdout`.
pub(crate) fn new(args: &NewArgs) -> anyhow::Result<ExitCode> {
    if !catalog::is_plain_name(&args.name) {
        anyhow::bail!(
            "invalid recipe name {:?}: use letters, digits, '.', '_', '-' (no spaces or path separators)",
            args.name
        );
    }
    let set = parse_set(&args.set)?;
    let source = catalog::resolve_source(&args.from, args.recipes_dir.as_deref())?;

    // Fill the template if the source declares one; else require that no --set was given.
    let rendered = if let Some(tpl) = scaffold::parse(&source.body)? {
        tpl.render(&set)?
    } else {
        anyhow::ensure!(
            set.is_empty(),
            "source {:?} declares no template variables (no `# odin:template` header), but --set was given",
            args.from
        );
        source.body.clone()
    };
    // Rewrite the name + assert the (now-substituted, valid-YAML) body round-trips.
    let body = catalog::rewrite_workflow_name(&rendered, &args.name)?;

    if args.stdout {
        eprintln!("# recipe {} (from {})", args.name, source.provenance);
        print!("{body}");
        return Ok(ExitCode::SUCCESS);
    }

    let dest = if args.catalog {
        catalog::dir_create(args.recipes_dir.as_deref())?.join(format!("{}.yaml", args.name))
    } else {
        scaffold_dest(&args.name, args.out.as_deref())?
    };
    if dest.exists() && !args.force {
        anyhow::bail!(
            "{} already exists (use --force to overwrite)",
            dest.display()
        );
    }
    std::fs::write(&dest, &body).with_context(|| format!("writing {}", dest.display()))?;
    println!(
        "created '{}' → {} (from {})",
        args.name,
        dest.display(),
        source.provenance
    );
    if args.catalog {
        println!("  run it: odin run {}", args.name);
    } else {
        println!("  next: odin validate {0} && odin run {0}", dest.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// Parses `--set KEY=VALUE` pairs (value may contain `=`; key must be non-empty).
fn parse_set(pairs: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .with_context(|| format!("--set must be KEY=VALUE, got {p:?}"))?;
        if k.is_empty() {
            anyhow::bail!("--set has an empty key: {p:?}");
        }
        out.insert(k.to_owned(), v.to_owned());
    }
    Ok(out)
}

/// Resolves where `recipe new` writes: `./<name>.yaml` by default; with `--out`, a `.yaml`/`.yml`
/// path is taken as the file, anything else as a directory to create and write `<name>.yaml` into.
fn scaffold_dest(name: &str, out: Option<&Path>) -> anyhow::Result<PathBuf> {
    match out {
        None => Ok(PathBuf::from(format!("{name}.yaml"))),
        Some(p) => {
            let is_file = matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml"));
            if is_file {
                Ok(p.to_path_buf())
            } else {
                std::fs::create_dir_all(p)
                    .with_context(|| format!("creating output directory {}", p.display()))?;
                Ok(p.join(format!("{name}.yaml")))
            }
        }
    }
}

/// `odin recipe show <name>` — print the recipe's workflow YAML to stdout (provenance to stderr,
/// so stdout stays a clean, pipeable document).
pub(crate) fn show(name: &str, recipes_dir: Option<&Path>) -> anyhow::Result<ExitCode> {
    let path = locate(name, recipes_dir)?;
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("reading recipe {} at {}", name, path.display()))?;
    eprintln!("# recipe {name} ({})", path.display());
    print!("{body}");
    Ok(ExitCode::SUCCESS)
}

/// `odin recipe path <name>` — print the recipe's resolved filesystem path (for scripting).
pub(crate) fn path(name: &str, recipes_dir: Option<&Path>) -> anyhow::Result<ExitCode> {
    let path = locate(name, recipes_dir)?;
    println!("{}", path.display());
    Ok(ExitCode::SUCCESS)
}

/// Resolves `name` to a recipe path, with a helpful error (listing what *is* available) on a miss.
fn locate(name: &str, recipes_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    let dir = catalog::dir(recipes_dir)?;
    if let Some(path) = catalog::resolve(&dir, name) {
        return Ok(path);
    }
    let available = catalog::list(&dir)?;
    let hint = if available.is_empty() {
        format!("the catalog at {} is empty", dir.display())
    } else {
        let names: Vec<&str> = available.iter().map(|r| r.name.as_str()).collect();
        format!("available recipes: {}", names.join(", "))
    };
    anyhow::bail!("no recipe named '{name}' in {} ({hint})", dir.display())
}
