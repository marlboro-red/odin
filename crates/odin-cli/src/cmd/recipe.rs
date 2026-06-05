//! The `odin recipe` subcommands: inspect the workflow recipe catalog (run-by-name).
//!
//! `list` shows what's in the catalog, `show` prints a recipe's YAML, and `path` prints its
//! resolved filesystem path (for scripting). The catalog directory itself is resolved by
//! [`crate::catalog`].

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use odin_core::Workflow;

use crate::catalog;

/// `odin recipe list` — list the recipes in the catalog (name + description), or as JSON.
pub(crate) fn list(recipes_dir: Option<&Path>, json: bool) -> anyhow::Result<ExitCode> {
    let dir = catalog::dir(recipes_dir)?;
    let recipes = catalog::list(&dir)?;

    if json {
        let arr: Vec<_> = recipes
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "path": r.path,
                    "workflow_name": r.workflow_name,
                    "description": r.description,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(ExitCode::SUCCESS);
    }

    if recipes.is_empty() {
        println!("no recipes in {}", dir.display());
        println!(
            "  run `odin recipe init` to add the bundled starters, or `odin recipe add <file>`."
        );
        return Ok(ExitCode::SUCCESS);
    }

    println!("recipes in {}:\n", dir.display());
    let width = recipes.iter().map(|r| r.name.len()).max().unwrap_or(0);
    for r in &recipes {
        match (&r.workflow_name, &r.description) {
            (None, _) => println!("  {:<width$}  (does not parse as a workflow)", r.name),
            (Some(_), Some(desc)) => println!("  {:<width$}  {desc}", r.name),
            (Some(_), None) => println!("  {:<width$}", r.name),
        }
    }
    Ok(ExitCode::SUCCESS)
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
