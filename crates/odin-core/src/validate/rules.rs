//! The individual validation rules. Each appends zero or more [`Diagnostic`]s; none
//! short-circuits, so one pass surfaces every problem.

use indexmap::{IndexMap, IndexSet};

use super::KnownNames;
use super::diagnostic::{DiagCode, Diagnostic};
use super::graph;
use crate::ids::StepId;
use crate::ir::{StepKind, TriggerDecl, Workflow, WorkspaceConfig};

/// The engine-reserved artifact auto-captured after every step.
const DIFF: &str = "DIFF";

pub(crate) fn step_ptr(i: usize) -> String {
    format!("steps[{i}]")
}

/// ODIN001 — the workflow must declare at least one step.
pub(crate) fn step_list_nonempty(wf: &Workflow, d: &mut Vec<Diagnostic>) {
    if wf.steps.is_empty() {
        d.push(Diagnostic::new(
            DiagCode::NoSteps,
            "steps",
            "workflow has no steps",
        ));
    }
}

/// ODIN002/003/004 — step ids are non-empty, unique, and valid path segments.
pub(crate) fn step_ids(wf: &Workflow, d: &mut Vec<Diagnostic>) {
    let mut first_seen: IndexMap<&str, usize> = IndexMap::new();
    for (i, s) in wf.steps.iter().enumerate() {
        let id = s.id.as_str();
        if id.trim().is_empty() {
            d.push(Diagnostic::new(
                DiagCode::EmptyStepId,
                format!("{}.id", step_ptr(i)),
                format!("step #{i} has an empty id"),
            ));
            continue;
        }
        if !is_valid_id(id) {
            d.push(Diagnostic::new(
                DiagCode::InvalidStepId,
                format!("{}.id", step_ptr(i)),
                format!(
                    "step id {id:?} is invalid: use letters/digits/_/- and start with a letter or _"
                ),
            ));
        }
        if let Some(&j) = first_seen.get(id) {
            d.push(Diagnostic::new(
                DiagCode::DuplicateStepId,
                format!("{}.id", step_ptr(i)),
                format!("duplicate step id {id:?} (first at {})", step_ptr(j)),
            ));
        } else {
            first_seen.insert(id, i);
        }
    }
}

/// ODIN005 — every provider reference names a registered provider.
pub(crate) fn provider_refs(wf: &Workflow, known: &KnownNames<'_>, d: &mut Vec<Diagnostic>) {
    for (i, s) in wf.steps.iter().enumerate() {
        if let StepKind::Provider(p) = &s.kind {
            check_provider(
                p.provider.as_str(),
                known,
                &format!("{}.provider", step_ptr(i)),
                s.id.as_str(),
                d,
            );
        }
        if let Some(j) = &s.judge {
            check_provider(
                j.provider.as_str(),
                known,
                &format!("{}.judge.provider", step_ptr(i)),
                s.id.as_str(),
                d,
            );
        }
        if let Some(fb) = &s.retry.on_fallback_provider {
            check_provider(
                fb.as_str(),
                known,
                &format!("{}.retry.on_fallback_provider", step_ptr(i)),
                s.id.as_str(),
                d,
            );
        }
    }
}

fn check_provider(
    name: &str,
    known: &KnownNames<'_>,
    pointer: &str,
    step: &str,
    d: &mut Vec<Diagnostic>,
) {
    if known.providers.contains(&name) {
        return;
    }
    let mut diag = Diagnostic::new(
        DiagCode::UnknownProvider,
        pointer,
        format!("step {step:?}: unknown provider {name:?}"),
    )
    .with_help(format!("known providers: {}", known.providers.join(", ")));
    if let Some(s) = suggest(name, known.providers.iter().copied()) {
        diag.help = Some(format!(
            "{} (did you mean {s:?}?)",
            diag.help.unwrap_or_default()
        ));
    }
    d.push(diag);
}

/// ODIN006/009 — provider steps must have exactly one prompt source.
pub(crate) fn prompts(wf: &Workflow, d: &mut Vec<Diagnostic>) {
    for (i, s) in wf.steps.iter().enumerate() {
        if let StepKind::Provider(p) = &s.kind {
            // A present-but-blank prompt/prompt_file is as good as missing.
            let prompt = p.prompt.as_ref().filter(|t| !t.trim().is_empty());
            let prompt_file = p.prompt_file.as_ref().filter(|t| !t.trim().is_empty());
            match (prompt, prompt_file) {
                (None, None) => d.push(Diagnostic::new(
                    DiagCode::MissingPrompt,
                    step_ptr(i),
                    format!(
                        "provider step {:?} has no prompt; set a non-empty prompt: or prompt_file:",
                        s.id.as_str()
                    ),
                )),
                (Some(_), Some(_)) => d.push(Diagnostic::new(
                    DiagCode::BothPromptAndFile,
                    step_ptr(i),
                    format!(
                        "step {:?} sets both prompt and prompt_file; choose one",
                        s.id.as_str()
                    ),
                )),
                _ => {}
            }
        }
    }
}

/// ODIN010 — every action reference names a registered action.
pub(crate) fn actions(wf: &Workflow, known: &KnownNames<'_>, d: &mut Vec<Diagnostic>) {
    for (i, s) in wf.steps.iter().enumerate() {
        if let StepKind::Action(a) = &s.kind {
            // ODIN028 — an action's effects are discarded with a scratch worktree.
            if s.scratch {
                d.push(Diagnostic::new(
                    DiagCode::ScratchOnAction,
                    format!("{}.scratch", step_ptr(i)),
                    format!(
                        "step {:?}: `scratch: true` on an action step discards its workspace \
                         side effects with the throwaway worktree",
                        s.id.as_str()
                    ),
                ));
            }
            if known.actions.contains(&a.action.as_str()) {
                continue;
            }
            let mut diag = Diagnostic::new(
                DiagCode::UnknownAction,
                format!("{}.action", step_ptr(i)),
                format!("step {:?}: unknown action {:?}", s.id.as_str(), a.action),
            )
            .with_help(format!("known actions: {}", known.actions.join(", ")));
            if let Some(sug) = suggest(&a.action, known.actions.iter().copied()) {
                diag.help = Some(format!(
                    "{} (did you mean {sug:?}?)",
                    diag.help.unwrap_or_default()
                ));
            }
            d.push(diag);
        }
    }
}

/// ODIN011/021 — judge threshold in range; warn if judged by the same provider.
pub(crate) fn judge(wf: &Workflow, d: &mut Vec<Diagnostic>) {
    for (i, s) in wf.steps.iter().enumerate() {
        let Some(j) = &s.judge else { continue };
        if !(0.0..=1.0).contains(&j.threshold) {
            d.push(Diagnostic::new(
                DiagCode::JudgeThresholdRange,
                format!("{}.judge.threshold", step_ptr(i)),
                format!(
                    "step {:?} judge threshold {} out of range 0.0..=1.0",
                    s.id.as_str(),
                    j.threshold
                ),
            ));
        }
        if let StepKind::Provider(p) = &s.kind {
            if p.provider == j.provider {
                d.push(Diagnostic::new(
                    DiagCode::SameProviderJudge,
                    format!("{}.judge.provider", step_ptr(i)),
                    format!(
                        "step {:?} is judged by the same provider ({:?}) it produced — consider an independent judge",
                        s.id.as_str(), j.provider.as_str()
                    ),
                ));
            }
        }
    }
}

/// ODIN012/013 — `depends_on` targets exist and are not self-references.
pub(crate) fn depends_on(wf: &Workflow, d: &mut Vec<Diagnostic>) {
    let declared: IndexSet<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
    for (i, s) in wf.steps.iter().enumerate() {
        for (k, dep) in s.depends_on.iter().enumerate() {
            let ptr = format!("{}.depends_on[{k}]", step_ptr(i));
            if dep == &s.id {
                d.push(Diagnostic::new(
                    DiagCode::SelfDependency,
                    ptr,
                    format!("step {:?} cannot depend on itself", s.id.as_str()),
                ));
            } else if !declared.contains(dep.as_str()) {
                let mut diag = Diagnostic::new(
                    DiagCode::UnknownDependency,
                    ptr,
                    format!(
                        "step {:?} depends on unknown step {:?}",
                        s.id.as_str(),
                        dep.as_str()
                    ),
                );
                if let Some(sug) = suggest(dep.as_str(), declared.iter().copied()) {
                    diag = diag.with_help(format!("did you mean {sug:?}?"));
                }
                d.push(diag);
            }
        }
    }
}

/// ODIN014 — the dependency graph is acyclic.
pub(crate) fn cycles(wf: &Workflow, d: &mut Vec<Diagnostic>) {
    if let Some(cycle) = graph::find_cycle(wf) {
        let path = cycle
            .iter()
            .map(StepId::as_str)
            .collect::<Vec<_>>()
            .join(" → ");
        d.push(Diagnostic::new(
            DiagCode::DependencyCycle,
            "steps",
            format!("dependency cycle: {path}"),
        ));
    }
}

/// ODIN007/008/015/019 — artifact production/consumption is well-formed and ordered.
pub(crate) fn artifacts(
    wf: &Workflow,
    ancestors: &IndexMap<StepId, IndexSet<StepId>>,
    d: &mut Vec<Diagnostic>,
) {
    // Build the producer index while checking duplicates and the reserved name.
    let mut producers: IndexMap<String, Vec<StepId>> = IndexMap::new();
    for (i, s) in wf.steps.iter().enumerate() {
        let mut seen: IndexSet<&str> = IndexSet::new();
        for a in &s.artifacts.produces {
            let name = a.as_str();
            if name == DIFF {
                d.push(Diagnostic::new(
                    DiagCode::ReservedArtifactDiff,
                    format!("{}.artifacts.produces", step_ptr(i)),
                    format!("{DIFF:?} is auto-captured by the engine; remove it from produces on step {:?}", s.id.as_str()),
                ));
            }
            if !seen.insert(name) {
                d.push(Diagnostic::new(
                    DiagCode::DuplicateProduces,
                    format!("{}.artifacts.produces", step_ptr(i)),
                    format!("step {:?} produces {name:?} more than once", s.id.as_str()),
                ));
            }
            producers
                .entry(name.to_owned())
                .or_default()
                .push(s.id.clone());
        }
    }

    for (i, s) in wf.steps.iter().enumerate() {
        for r in &s.artifacts.requires {
            let name = r.as_str();
            if name == DIFF {
                continue; // built-in, always available after the producing step
            }
            match producers.get(name) {
                None => {
                    let mut diag = Diagnostic::new(
                        DiagCode::UnsatisfiedRequires,
                        format!("{}.artifacts.requires", step_ptr(i)),
                        format!(
                            "step {:?} requires artifact {name:?} which no step produces",
                            s.id.as_str()
                        ),
                    );
                    if !producers.is_empty() {
                        diag = diag.with_help(format!(
                            "produced artifacts: {}",
                            producers.keys().cloned().collect::<Vec<_>>().join(", ")
                        ));
                    }
                    d.push(diag);
                }
                Some(prods) => {
                    let anc = ancestors.get(&s.id);
                    let upstream = prods.iter().any(|p| anc.is_some_and(|set| set.contains(p)));
                    if !upstream {
                        let producer = prods.first().map_or("?", |p| p.as_str());
                        d.push(Diagnostic::new(
                            DiagCode::ArtifactOrdering,
                            format!("{}.artifacts.requires", step_ptr(i)),
                            format!(
                                "step {:?} requires {name:?} but its producer {producer:?} is not an upstream dependency (add it to depends_on)",
                                s.id.as_str()
                            ),
                        ));
                    }
                }
            }
        }
    }
}

/// ODIN016 — a slot pool must have at least one slot.
pub(crate) fn workspace(wf: &Workflow, d: &mut Vec<Diagnostic>) {
    if let WorkspaceConfig::SlotPool(p) = &wf.workspace {
        if p.pool < 1 {
            d.push(Diagnostic::new(
                DiagCode::InvalidPoolSize,
                "workspace.pool",
                format!("workspace pool must be >= 1, got {}", p.pool),
            ));
        }
    }
}

/// ODIN020 — cron triggers carry a structurally valid 5-field schedule.
pub(crate) fn triggers(wf: &Workflow, d: &mut Vec<Diagnostic>) {
    for (i, t) in wf.triggers.iter().enumerate() {
        match t {
            TriggerDecl::Cron(c) if !is_valid_cron(&c.schedule) => {
                d.push(Diagnostic::new(
                    DiagCode::InvalidCron,
                    format!("triggers[{i}].schedule"),
                    format!(
                        "trigger cron schedule {:?} is not a valid 5-field expression",
                        c.schedule
                    ),
                ));
            }
            // ODIN027 — a webhook param mapping whose key is not a declared param is inert;
            // the extracted value would go nowhere. Catch the typo at validate time.
            TriggerDecl::GithubWebhook(g) => {
                for name in g.params.keys() {
                    if !wf.params.contains_key(name) {
                        d.push(Diagnostic::new(
                            DiagCode::WebhookParamUndeclared,
                            format!("triggers[{i}].params.{name}"),
                            format!(
                                "webhook maps param {:?}, which is not declared in `params`; \
                                 the mapping is inert",
                                name.as_str()
                            ),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
}

/// ODIN022 — a param should not be both `required` and have a `default`.
/// ODIN030 — a param's `default` must match its declared `type`.
pub(crate) fn params(wf: &Workflow, d: &mut Vec<Diagnostic>) {
    for (name, spec) in &wf.params {
        if spec.required && spec.default.is_some() {
            d.push(Diagnostic::new(
                DiagCode::RequiredWithDefault,
                format!("params.{name}"),
                format!(
                    "param {:?} is required but also has a default; the default is unreachable",
                    name.as_str()
                ),
            ));
        }
        if let Some(default) = &spec.default {
            if !spec.ty.matches(default) {
                d.push(Diagnostic::new(
                    DiagCode::ParamDefaultType,
                    format!("params.{name}.default"),
                    format!(
                        "param {:?} default {default} does not match its declared type {:?}",
                        name.as_str(),
                        spec.ty.name()
                    ),
                ));
            }
        }
    }
}

/// ODIN023 — warn that `on_fallback_provider` is inert in v1.
pub(crate) fn retry_fallback(wf: &Workflow, d: &mut Vec<Diagnostic>) {
    for (i, s) in wf.steps.iter().enumerate() {
        if s.retry.on_fallback_provider.is_some() {
            d.push(Diagnostic::new(
                DiagCode::InertFallbackProvider,
                format!("{}.retry.on_fallback_provider", step_ptr(i)),
                "on_fallback_provider is declared but routing/fallback is not implemented in v1; this field is inert".to_owned(),
            ));
        }
    }
}

/// ODIN026 — warn when the workflow targets a newer schema minor than this engine.
pub(crate) fn schema(wf: &Workflow, d: &mut Vec<Diagnostic>) {
    if wf.schema_version.minor > crate::ir::CURRENT_SCHEMA_MINOR {
        d.push(Diagnostic::new(
            DiagCode::NewerSchemaMinor,
            "schema_version",
            format!(
                "schema_version {}.{} is newer than this engine's {}.{}; unknown features are ignored",
                wf.schema_version.major,
                wf.schema_version.minor,
                crate::ir::CURRENT_SCHEMA_MAJOR,
                crate::ir::CURRENT_SCHEMA_MINOR,
            ),
        ));
    }
}

/// ODIN025 — warn about unknown keys at the workflow root (forward-compat tolerance).
///
/// Needs the raw source because the typed [`Workflow`] silently drops unknown root keys
/// (the root is intentionally not `deny_unknown_fields`).
pub(crate) fn root_unknown_fields(src: &str, d: &mut Vec<Diagnostic>) {
    const KNOWN: &[&str] = &[
        "schema_version",
        "name",
        "version",
        "description",
        "durable",
        "workspace",
        "triggers",
        "params",
        "steps",
        "defaults",
        "max_parallel",
    ];
    let Ok(val) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(src) else {
        return;
    };
    let Some(map) = val.as_mapping() else { return };
    for (k, _) in map {
        if let Some(key) = k.as_str() {
            if !KNOWN.contains(&key) {
                d.push(Diagnostic::new(
                    DiagCode::UnknownRootField,
                    key.to_owned(),
                    format!("unknown field {key:?} at workflow root — ignored (typo, or written for a newer schema minor?)"),
                ));
            }
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn is_valid_id(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Month names the runtime cron parser (the `cron` crate) accepts.
const CRON_MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
/// Day-of-week names the runtime cron parser accepts.
const CRON_DAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// Best-effort structural **and range** check of a standard 5-field cron expression, kept in
/// sync with what the daemon's runtime parser (`odin-daemon`'s `parse_5field`/`normalize_dow`)
/// actually accepts: each field is range-checked, month/day *names* and the Quartz `?`
/// placeholder are accepted, and `*`, lists (`,`), ranges (`-`), and steps (`/`) are supported.
/// It is intentionally not a full cron grammar — its job is to flag, at `odin validate` time,
/// the schedules that would otherwise abort the daemon at startup, without pulling the `cron`
/// crate into the parse-only validation path.
fn is_valid_cron(s: &str) -> bool {
    let fields: Vec<&str> = s.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    // (min, max, names, `?` allowed, wrap-range allowed) for minute, hour, day-of-month,
    // month, day-of-week. Only day-of-week permits a "backwards" range (`6-0` = Sat,Sun): the
    // daemon normalizes those by expansion, whereas the `cron` crate rejects a backwards range
    // in every other field — so accepting one here would be a clean-validate-then-silently-
    // skipped trap. Day-of-week is 0..=7 (POSIX: both 0 and 7 are Sunday).
    let specs: [(u8, u8, &[&str], bool, bool); 5] = [
        (0, 59, &[], false, false),
        (0, 23, &[], false, false),
        (1, 31, &[], true, false),
        (1, 12, &CRON_MONTHS, false, false),
        (0, 7, &CRON_DAYS, true, true),
    ];
    fields.iter().zip(specs).all(|(field, spec)| {
        let (min, max, names, allow_q, allow_wrap) = spec;
        cron_field_ok(field, min, max, names, allow_q, allow_wrap)
    })
}

/// Validates one cron field against its numeric range / allowed names, supporting `*`, comma
/// lists, ranges (`a-b`), and `/step`. A range endpoint may be a digit or an allowed name; a
/// backwards range (`hi < lo`) is rejected unless `allow_wrap` (day-of-week only), matching
/// what the daemon's runtime parser accepts.
fn cron_field_ok(
    field: &str,
    min: u8,
    max: u8,
    names: &[&str],
    allow_q: bool,
    allow_wrap: bool,
) -> bool {
    if field.is_empty() {
        return false;
    }
    // Numeric value of a token (digit in range, or a name's position offset by `min`); `None`
    // if out of range / not a recognized name. Names are ordered from `min` (months start at
    // JAN=1, days at SUN=0).
    let value = |t: &str| -> Option<u8> {
        let t = t.trim();
        if let Ok(n) = t.parse::<u8>() {
            return (min..=max).contains(&n).then_some(n);
        }
        if t.is_empty() {
            return None;
        }
        names
            .iter()
            .position(|name| name.eq_ignore_ascii_case(t))
            .map(|i| min + u8::try_from(i).unwrap_or(u8::MAX))
    };
    field.split(',').all(|item| {
        if item.is_empty() {
            return false;
        }
        let (base, step) = match item.split_once('/') {
            Some((b, s)) => (b, Some(s)),
            None => (item, None),
        };
        // A step, if present, must be a positive integer.
        if let Some(s) = step {
            if !matches!(s.trim().parse::<u32>(), Ok(n) if n > 0) {
                return false;
            }
        }
        match base.trim() {
            "*" => true,
            "?" => allow_q,
            b => match b.split_once('-') {
                Some((lo, hi)) => match (value(lo), value(hi)) {
                    (Some(l), Some(h)) => allow_wrap || l <= h,
                    _ => false,
                },
                None => value(b).is_some(),
            },
        }
    })
}

/// Returns the closest candidate within edit distance 2, for a "did you mean" hint.
pub(crate) fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    candidates
        .map(|c| (levenshtein(name, c), c))
        .filter(|(dist, _)| *dist <= 2)
        .min_by_key(|(dist, _)| *dist)
        .map(|(_, c)| c.to_owned())
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::{is_valid_cron, is_valid_id, levenshtein, suggest};

    #[test]
    fn id_pattern() {
        assert!(is_valid_id("plan_1"));
        assert!(is_valid_id("_x-y"));
        assert!(!is_valid_id("1plan"));
        assert!(!is_valid_id("fix it"));
        assert!(!is_valid_id(""));
    }

    #[test]
    fn cron_shape() {
        assert!(is_valid_cron("0 3 * * 1"));
        assert!(is_valid_cron("*/5 0 * * 1-5"));
        assert!(is_valid_cron("0 3 * * 1-7")); // every day (used to abort the daemon)
        assert!(!is_valid_cron("* * *"));
        assert!(!is_valid_cron("0 3 * * x"));
        // Separator-only fields carry no value and must be rejected.
        assert!(!is_valid_cron("- - - - -"));
        assert!(!is_valid_cron(", , , , ,"));
        // Range-checked per field, kept in sync with the daemon's runtime parser: out-of-range
        // values that would abort the daemon at startup are now rejected at validate time...
        assert!(!is_valid_cron("99 99 * * *"));
        assert!(!is_valid_cron("61 3 * * 1"));
        assert!(!is_valid_cron("0 3 32 13 1"));
        assert!(!is_valid_cron("0 0 0 * *")); // day-of-month 0 is invalid
        assert!(!is_valid_cron("0 3 * * 8")); // day-of-week 8 is invalid
        // ...and names / the Quartz `?` placeholder that the runtime accepts are no longer
        // wrongly flagged.
        assert!(is_valid_cron("0 3 * * MON"));
        assert!(is_valid_cron("0 3 * JAN-MAR 1"));
        assert!(is_valid_cron("0 3 ? * MON-FRI"));
        assert!(is_valid_cron("0 3 * * ?"));
        // Backwards ranges in non-day-of-week fields are rejected — the runtime parser
        // rejects them too, so accepting them would be a clean-validate-then-silently-skipped
        // trap (the dangerous drift direction).
        assert!(!is_valid_cron("5-3 3 * * 1")); // minute
        assert!(!is_valid_cron("0 20-5 * * *")); // hour
        assert!(!is_valid_cron("0 3 31-1 * *")); // day-of-month
        assert!(!is_valid_cron("0 3 * NOV-FEB *")); // month (named, backwards)
        assert!(!is_valid_cron("0 3 * 12-1 *")); // month (numeric, backwards)
        // ...but a backwards *day-of-week* range is a valid POSIX wrap the daemon normalizes.
        assert!(is_valid_cron("0 3 * * 6-0")); // Sat..Sun
        assert!(is_valid_cron("0 3 * * FRI-MON"));
    }

    #[test]
    fn suggestions() {
        assert_eq!(levenshtein("claud", "claude"), 1);
        assert_eq!(
            suggest("claud", ["claude", "codex"].into_iter()).as_deref(),
            Some("claude")
        );
        assert_eq!(suggest("zzzzzz", ["claude"].into_iter()), None);
    }
}
