//! Scaffold-time **templating** for `odin recipe new`.
//!
//! A recipe opts in with a leading `# odin:template … # end` comment block declaring its
//! variables; the body then uses `@@VAR@@` placeholders. `@@` is chosen for **zero overlap** with
//! the engine's run-time `{{ … }}` templates, shell heredocs (`<<'EOF'`), shift operators
//! (`a << b`), and shell expansion (`${VAR}`) — so a recipe **without** a header is copied
//! byte-for-byte, and a templated body's run-time `{{ params.x }}` is left untouched.
//!
//! ```text
//! # odin:template
//! #   provider:    { default: claude }
//! #   base_branch: { required: true, description: "branch to diff against" }
//! # end
//! steps:
//!   - id: review
//!     provider: @@provider@@
//!     run: "git diff @@base_branch@@...HEAD"   # run-time {{ params.x }} still works
//! ```

use std::collections::BTreeMap;

use anyhow::{Context as _, bail};

const HEADER_START: &str = "# odin:template";
const HEADER_END: &str = "# end";

/// One declared template variable.
struct VarSpec {
    default: Option<String>,
    required: bool,
    description: Option<String>,
}

/// A read-only view of a declared variable (for prompting and `--explain`).
pub(crate) struct VarInfo<'a> {
    pub name: &'a str,
    pub default: Option<&'a str>,
    pub required: bool,
    pub description: Option<&'a str>,
}

/// A parsed recipe template: its declared variables (in header order) and the body with the
/// `# odin:template` block removed.
pub(crate) struct Template {
    vars: Vec<(String, VarSpec)>,
    body: String,
}

/// Parses a `# odin:template … # end` header in `src`. Returns `Ok(None)` when there is no header
/// — the source is **not** a template and must be copied verbatim.
///
/// # Errors
/// Fails if a header is present but malformed (no `# end`, or a de-commented body that is not a
/// `name: {…}` mapping).
pub(crate) fn parse(src: &str) -> anyhow::Result<Option<Template>> {
    let lines: Vec<&str> = src.lines().collect();
    let Some(start) = lines.iter().position(|l| l.trim() == HEADER_START) else {
        return Ok(None);
    };
    let Some(rel_end) = lines[start + 1..]
        .iter()
        .position(|l| l.trim() == HEADER_END)
    else {
        bail!("`{HEADER_START}` header has no closing `{HEADER_END}` line");
    };
    let end = start + 1 + rel_end;
    let decommented = lines[start + 1..end]
        .iter()
        .map(|l| decomment(l))
        .collect::<Vec<_>>()
        .join("\n");
    let vars = parse_vars(&decommented)?;

    // The instantiated file carries no header — drop the whole block, keep the rest verbatim.
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    kept.extend_from_slice(&lines[..start]);
    kept.extend_from_slice(&lines[end + 1..]);
    let mut body = kept.join("\n");
    if src.ends_with('\n') {
        body.push('\n');
    }
    Ok(Some(Template { vars, body }))
}

/// Strips a leading `#` and one optional following space from a comment line.
fn decomment(line: &str) -> String {
    let t = line.trim_start();
    let rest = t.strip_prefix('#').unwrap_or(t);
    rest.strip_prefix(' ').unwrap_or(rest).to_owned()
}

fn parse_vars(yaml: &str) -> anyhow::Result<Vec<(String, VarSpec)>> {
    let val: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(yaml).context("the `# odin:template` header is not valid YAML")?;
    let map = val
        .as_mapping()
        .context("the `# odin:template` header must map each variable name to a spec")?;
    let mut out = Vec::with_capacity(map.len());
    for (k, v) in map {
        let name = k
            .as_str()
            .context("a template variable name must be a string")?
            .to_owned();
        out.push((name, parse_spec(v)));
    }
    Ok(out)
}

fn parse_spec(v: &serde_yaml_ng::Value) -> VarSpec {
    let map = v.as_mapping();
    let get = |k: &str| map.and_then(|m| m.get(k));
    let default = get("default").and_then(scalar_string);
    // Required defaults to "true when there's no default" — you must supply a value for it.
    let required = get("required")
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(default.is_none());
    let description = get("description").and_then(|v| v.as_str().map(str::to_owned));
    VarSpec {
        default,
        required,
        description,
    }
}

/// The string form of a YAML scalar (string/number/bool), for a `default:` value.
fn scalar_string(v: &serde_yaml_ng::Value) -> Option<String> {
    match v {
        serde_yaml_ng::Value::String(s) => Some(s.clone()),
        serde_yaml_ng::Value::Bool(b) => Some(b.to_string()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Template {
    /// The declared variable names, in header order.
    pub(crate) fn var_names(&self) -> Vec<&str> {
        self.vars.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// A read-only view of every declared variable, in header order.
    pub(crate) fn vars(&self) -> impl Iterator<Item = VarInfo<'_>> {
        self.vars.iter().map(|(name, spec)| VarInfo {
            name,
            default: spec.default.as_deref(),
            required: spec.required,
            description: spec.description.as_deref(),
        })
    }

    /// The required variables with no value yet — neither a `--set` entry nor a declared default —
    /// i.e. the ones an interactive prompt should ask for.
    pub(crate) fn missing_required(&self, set: &BTreeMap<String, String>) -> Vec<VarInfo<'_>> {
        self.vars()
            .filter(|v| v.required && v.default.is_none() && !set.contains_key(v.name))
            .collect()
    }

    /// Renders the template: fills each `@@VAR@@` with its `--set` value (or declared default),
    /// having already stripped the header, and returns the instantiated body.
    ///
    /// # Errors
    /// Fails if a `--set` key is not declared, a required variable has no value, a body marker is
    /// not declared, or any `@@…@@` remains after substitution.
    pub(crate) fn render(&self, set: &BTreeMap<String, String>) -> anyhow::Result<String> {
        for k in set.keys() {
            if !self.vars.iter().any(|(n, _)| n == k) {
                bail!(
                    "--set {k:?} is not a declared template variable (declared: {})",
                    self.var_names().join(", ")
                );
            }
        }
        let mut values: BTreeMap<&str, String> = BTreeMap::new();
        let mut missing = Vec::new();
        for (name, spec) in &self.vars {
            if let Some(v) = set.get(name) {
                values.insert(name, v.clone());
            } else if let Some(d) = &spec.default {
                values.insert(name, d.clone());
            } else if spec.required {
                missing.push(name.as_str());
            }
        }
        if !missing.is_empty() {
            bail!(
                "missing required template variable(s): {} (pass --set <name>=<value>)",
                missing.join(", ")
            );
        }
        let body = self.substitute(&values)?;
        if body.contains("@@") {
            bail!(
                "a `@@…@@` marker remains after substitution — a malformed or mismatched placeholder"
            );
        }
        Ok(body)
    }

    /// Replaces each well-formed, declared `@@ident@@` with its value (injected **verbatim**, so a
    /// value with YAML-special characters is the template author's responsibility to quote in the
    /// field). A `@@` that doesn't form a clean marker is emitted literally and caught by the
    /// caller's leftover guard.
    fn substitute(&self, values: &BTreeMap<&str, String>) -> anyhow::Result<String> {
        let mut out = String::with_capacity(self.body.len());
        let mut rest = self.body.as_str();
        while let Some(pos) = rest.find("@@") {
            out.push_str(&rest[..pos]);
            let after = &rest[pos + 2..];
            if let Some(ident) = read_ident(after) {
                if after[ident.len()..].starts_with("@@") {
                    if !self.vars.iter().any(|(n, _)| n == ident) {
                        bail!(
                            "template marker @@{ident}@@ in the body is not declared in the `# odin:template` header"
                        );
                    }
                    if let Some(v) = values.get(ident) {
                        out.push_str(v);
                        rest = &after[ident.len() + 2..];
                        continue;
                    }
                }
            }
            out.push_str("@@");
            rest = after;
        }
        out.push_str(rest);
        Ok(out)
    }
}

/// Reads a leading `[A-Za-z_][A-Za-z0-9_]*` identifier from `s`, or `None`.
fn read_ident(s: &str) -> Option<&str> {
    match s.chars().next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return None,
    }
    let end = s
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map_or(s.len(), |(i, _)| i);
    Some(&s[..end])
}

/// Asks the operator for a value — abstracted so tests can inject canned answers.
pub(crate) trait Prompter {
    /// Show `prompt` and return the entered line (trailing newline stripped).
    ///
    /// # Errors
    /// Propagates any I/O error from reading the answer.
    fn ask(&mut self, prompt: &str) -> std::io::Result<String>;
}

/// Reads answers from stdin, writing the prompt to **stderr** so a piped stdout stays clean.
pub(crate) struct StdinPrompter;

impl Prompter for StdinPrompter {
    fn ask(&mut self, prompt: &str) -> std::io::Result<String> {
        use std::io::Write as _;
        eprint!("{prompt}");
        std::io::stderr().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line.trim_end_matches(['\n', '\r']).to_owned())
    }
}

/// Prompts for each [`Template::missing_required`] variable and folds the (non-empty) answers into
/// `set`. An empty answer is left unset, so [`Template::render`] then reports it as still missing
/// rather than baking in a blank.
///
/// # Errors
/// Propagates a prompter I/O error.
pub(crate) fn fill_interactively(
    tpl: &Template,
    set: &mut BTreeMap<String, String>,
    prompter: &mut dyn Prompter,
) -> std::io::Result<()> {
    for var in tpl.missing_required(set) {
        let prompt = match var.description {
            Some(d) => format!("{} ({d}): ", var.name),
            None => format!("{}: ", var.name),
        };
        let answer = prompter.ask(&prompt)?;
        if !answer.trim().is_empty() {
            set.insert(var.name.to_owned(), answer);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    const TPL: &str = "# odin:template\n\
        #   provider: { default: claude }\n\
        #   base_branch: { required: true }\n\
        # end\n\
        name: src\n\
        steps:\n  - id: r\n    provider: @@provider@@\n    run: \"git diff @@base_branch@@...HEAD\"\n";

    struct CannedPrompter(std::collections::VecDeque<String>);
    impl Prompter for CannedPrompter {
        fn ask(&mut self, _prompt: &str) -> std::io::Result<String> {
            Ok(self.0.pop_front().unwrap_or_default())
        }
    }

    #[test]
    fn header_less_source_is_not_a_template() {
        assert!(
            parse("name: x\nsteps:\n  - {id: a, run: y}\n")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn missing_required_lists_only_unsatisfied_required_vars() {
        let t = parse(TPL).unwrap().unwrap();
        // provider has a default; base_branch is required and unset.
        let names: Vec<&str> = t
            .missing_required(&set(&[]))
            .iter()
            .map(|v| v.name)
            .collect();
        assert_eq!(names, ["base_branch"]);
        // …and once it's supplied, nothing is missing.
        assert!(t.missing_required(&set(&[("base_branch", "x")])).is_empty());
    }

    #[test]
    fn fill_interactively_supplies_missing_then_renders() {
        let t = parse(TPL).unwrap().unwrap();
        let mut s = set(&[]);
        let mut p = CannedPrompter(["develop".to_owned()].into_iter().collect());
        fill_interactively(&t, &mut s, &mut p).unwrap();
        let out = t.render(&s).unwrap();
        assert!(out.contains("git diff develop...HEAD")); // prompted value
        assert!(out.contains("provider: claude")); // default still applies
    }

    #[test]
    fn fill_interactively_empty_answer_leaves_it_missing() {
        let t = parse(TPL).unwrap().unwrap();
        let mut s = set(&[]);
        let mut p = CannedPrompter([String::new()].into_iter().collect());
        fill_interactively(&t, &mut s, &mut p).unwrap();
        assert!(
            t.render(&s)
                .unwrap_err()
                .to_string()
                .contains("missing required")
        );
    }

    #[test]
    fn renders_with_default_and_set() {
        let t = parse(TPL).unwrap().unwrap();
        assert_eq!(t.var_names(), ["provider", "base_branch"]);
        let out = t.render(&set(&[("base_branch", "main")])).unwrap();
        assert!(out.contains("provider: claude")); // default
        assert!(out.contains("git diff main...HEAD")); // --set, injected verbatim
        assert!(!out.contains("# odin:template")); // header stripped
        assert!(!out.contains("@@"));
    }

    #[test]
    fn missing_required_errors() {
        let t = parse(TPL).unwrap().unwrap();
        let err = t.render(&set(&[])).unwrap_err().to_string();
        assert!(
            err.contains("missing required") && err.contains("base_branch"),
            "{err}"
        );
    }

    #[test]
    fn undeclared_set_key_errors() {
        let t = parse(TPL).unwrap().unwrap();
        let err = t
            .render(&set(&[("base_branch", "main"), ("ghost", "x")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--set") && err.contains("ghost"), "{err}");
    }

    #[test]
    fn undeclared_body_marker_errors() {
        let src = "# odin:template\n#   a: { default: x }\n# end\nname: n\nrun: @@a@@ @@b@@\n";
        let t = parse(src).unwrap().unwrap();
        let err = t.render(&set(&[])).unwrap_err().to_string();
        assert!(
            err.contains("@@b@@") && err.contains("not declared"),
            "{err}"
        );
    }

    #[test]
    fn heredoc_and_shift_and_runtime_templates_are_untouched() {
        // None of these are `@@…@@`, and the body has no header → copied verbatim.
        let src = "name: x\nsteps:\n  - id: a\n    run: |\n      cat <<'EOF'\n      x\n      EOF\n      echo $((1<<2))\n    when: \"{{ params.go }}\"\n";
        assert!(parse(src).unwrap().is_none());
    }

    #[test]
    fn stray_double_at_in_a_template_body_is_caught() {
        // A templated body that contains a literal `@@` (not a valid marker) is rejected.
        let src =
            "# odin:template\n#   a: { default: x }\n# end\nname: n\nrun: \"echo @@a@@ then @@\"\n";
        let t = parse(src).unwrap().unwrap();
        assert!(
            t.render(&set(&[]))
                .unwrap_err()
                .to_string()
                .contains("remains after substitution")
        );
    }
}
