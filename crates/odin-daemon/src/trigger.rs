//! [`CronTrigger`]: a [`Trigger`](odin_core::Trigger) that fires a workflow on a
//! standard 5-field cron schedule.

use std::collections::BTreeSet;
use std::str::FromStr as _;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cron::Schedule;
use odin_core::error::TriggerError;
use odin_core::traits::{Trigger, TriggerEvent};
use odin_core::{RunInput, WorkflowId};

/// Fires `workflow` every time a standard 5-field cron expression elapses.
///
/// `next_event` sleeps until the next occurrence, then yields a [`TriggerEvent`] tagged
/// `trigger = "cron"`. A cron run carries no params, so it suits param-less workflows
/// (nightly maintenance, scheduled audits); a schedule pointed at a workflow with
/// required params will surface a validation error at dispatch time.
///
/// # Time zone
/// Schedules are evaluated in **UTC**, not the server's local time — deterministic and
/// free of daylight-saving gaps/repeats, matching hosted cron (e.g. GitHub Actions). A
/// `"0 3 * * *"` schedule therefore fires at 03:00 UTC.
pub struct CronTrigger {
    schedule: Schedule,
    workflow: WorkflowId,
    source: String,
}

impl CronTrigger {
    /// Builds a trigger from a standard 5-field cron expression
    /// (`minute hour day-of-month month day-of-week`, e.g. `"0 3 * * 1"`).
    ///
    /// # Errors
    /// Returns a [`TriggerError`] if `expr` is not a valid 5-field cron expression.
    pub fn new(expr: impl AsRef<str>, workflow: WorkflowId) -> Result<Self, TriggerError> {
        let expr = expr.as_ref();
        let schedule = parse_5field(expr)
            .map_err(|e| TriggerError::Source(format!("invalid cron {expr:?}: {e}")))?;
        Ok(Self {
            schedule,
            workflow,
            source: format!("cron:{expr}"),
        })
    }

    /// The next instant this schedule fires strictly after `after`, or `None` if the
    /// schedule has no future occurrence. Pure — the scheduling logic, free of any sleep.
    #[must_use]
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.schedule.after(&after).next()
    }
}

/// Parses a standard 5-field cron expression into a [`Schedule`], bridging two gaps
/// between POSIX cron and the `cron` crate's Quartz dialect:
///
/// 1. **Seconds.** The crate expects a leading seconds field (6–7 fields); standard cron
///    has none, so we require exactly 5 fields and prepend `0` (fire at second 0).
/// 2. **Day-of-week numbering.** POSIX uses `0`/`7` = Sunday, `1` = Monday … `6` =
///    Saturday; the crate uses Quartz numbering (`1` = Sunday … `7` = Saturday) and
///    rejects `0`. We rewrite the numeric day-of-week field into the crate's Quartz domain
///    ([`normalize_dow`]) — `0 3 * * 1` then means **Monday**, as the IR documents.
fn parse_5field(expr: &str) -> Result<Schedule, String> {
    let fields = expr.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 {
        return Err(format!("expected 5 fields, found {}", fields.len()));
    }
    let dow = normalize_dow(fields[4]);
    let normalized = format!(
        "0 {} {} {} {} {dow}",
        fields[0], fields[1], fields[2], fields[3]
    );
    Schedule::from_str(&normalized).map_err(|e| e.to_string())
}

/// Rewrites a POSIX day-of-week field into the `cron` crate's Quartz numeric domain
/// (`1` = Sunday … `7` = Saturday) by *expanding* lists/ranges/steps into an explicit set of
/// numbers (POSIX `0`/`7` = Sunday … `6` = Saturday). Expanding — rather than rewriting to day
/// names or to a single Quartz range — sidesteps two crate limitations at once: it never emits
/// a *named* range, and it never emits a *backwards* range, both of which the crate rejects.
/// That matters for extremely common POSIX expressions: `1-7` ("every day"), and any range
/// ending on Sunday (`4-7`, `5-7`, `6-7`, `6-0`), all of which the old name-rewrite turned
/// into crate-rejected `MON-SUN`-style ranges. `*`/`?` pass through; anything that doesn't
/// parse as a POSIX day-of-week is returned unchanged for the crate to reject with its own error.
fn normalize_dow(field: &str) -> String {
    let f = field.trim();
    if f == "*" || f == "?" {
        return f.to_owned();
    }
    let mut quartz: BTreeSet<u8> = BTreeSet::new();
    for item in f.split(',') {
        match expand_dow_item(item) {
            // POSIX day `p` (0=Sun..6=Sat) → Quartz (1=Sun..7=Sat).
            Some(days) => quartz.extend(days.into_iter().map(|p| p + 1)),
            None => return field.to_owned(),
        }
    }
    if quartz.is_empty() {
        return field.to_owned();
    }
    quartz
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Expands one comma-item of a POSIX day-of-week field (`a`, `a-b`, `*`, each with an optional
/// `/step`) into the set of POSIX day numbers (`0..=6`, Sunday..Saturday) it selects. Supports
/// wrap-around ranges (`6-0` = Sat,Sun). Returns `None` if it is not parseable POSIX.
///
/// The expansion works in a `0..=7` space — where **both** `0` and `7` denote Sunday — and
/// folds `7 → 0` only *after* expanding ranges/steps. That keeps `0-7` and `1-7` spanning the
/// whole week (rather than collapsing to Sunday once `7` folds to `0`), and makes a bare
/// `n/step` range to `7` so it picks up Sunday (vixie semantics: `1/2` = Mon,Wed,Fri,Sun).
fn expand_dow_item(item: &str) -> Option<BTreeSet<u8>> {
    let (base, step) = match item.split_once('/') {
        Some((b, s)) => (b.trim(), Some(s.trim().parse::<u8>().ok()?)),
        None => (item.trim(), None),
    };
    let step = step.unwrap_or(1);
    if step == 0 {
        return None;
    }
    // The ordered day sequence the base selects, in `0..=7` space (before the step + fold).
    let seq: Vec<u8> = if base == "*" {
        (0..=6).collect()
    } else if let Some((a, b)) = base.split_once('-') {
        let (lo, hi) = (dow_token(a)?, dow_token(b)?);
        if lo <= hi {
            (lo..=hi).collect()
        } else {
            // Wrap-around range (e.g. `6-0` = Sat..Sun): `lo..=7` then `0..=hi`.
            (lo..=7).chain(0..=hi).collect()
        }
    } else {
        let n = dow_token(base)?;
        // A bare `n` is just `n`; `n/step` ranges `n..=7` (so it can include Sunday).
        if step == 1 {
            vec![n]
        } else {
            (n..=7).collect()
        }
    };
    // Apply the step over the ordered sequence, then fold `7 → 0` (both are Sunday).
    Some(
        seq.into_iter()
            .step_by(step as usize)
            .map(|d| d % 7)
            .collect(),
    )
}

/// Maps a single POSIX day-of-week token — a digit `0..=7` or a 3-letter English day name
/// (case-insensitive) — to a day index in `0..=7`, where both `0` and `7` are Sunday. The `7`
/// is deliberately **not** folded to `0` here so range/step expansion can span the full week;
/// the fold happens in [`expand_dow_item`] after expansion. `None` if it isn't a day token.
fn dow_token(token: &str) -> Option<u8> {
    let t = token.trim();
    if let Ok(n) = t.parse::<u8>() {
        return (n <= 7).then_some(n);
    }
    match t.to_ascii_uppercase().as_str() {
        "SUN" => Some(0),
        "MON" => Some(1),
        "TUE" => Some(2),
        "WED" => Some(3),
        "THU" => Some(4),
        "FRI" => Some(5),
        "SAT" => Some(6),
        _ => None,
    }
}

#[async_trait]
impl Trigger for CronTrigger {
    // The trait fixes the return type to `&str`; the literal cannot be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn kind(&self) -> &str {
        "cron"
    }

    async fn next_event(&mut self) -> Result<Option<TriggerEvent>, TriggerError> {
        let now = Utc::now();
        let Some(next) = self.next_after(now) else {
            // No future occurrence: the source is exhausted (cannot happen for a 5-field
            // expression, which has no year and always recurs, but handled for safety).
            return Ok(None);
        };
        // `to_std` fails only on a negative span; `next` is strictly after `now`, but guard
        // anyway so a clock adjustment can't panic — a zero wait just re-evaluates at once.
        let wait = (next - now).to_std().unwrap_or(Duration::ZERO);
        tokio::time::sleep(wait).await;

        let mut input = RunInput::manual();
        input.trigger = String::from("cron");
        Ok(Some(TriggerEvent::new(
            self.source.clone(),
            self.workflow.clone(),
            input,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::CronTrigger;
    use chrono::{TimeZone as _, Utc};
    use odin_core::WorkflowId;

    fn wf() -> WorkflowId {
        WorkflowId::new("nightly")
    }

    #[test]
    fn rejects_non_5field_expressions() {
        // Too few, too many, and the cron crate's own 6-field form are all rejected here:
        // CronTrigger's contract is *standard* 5-field cron.
        for bad in ["* * * *", "* * * * * *", "", "not a cron"] {
            assert!(
                CronTrigger::new(bad, wf()).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn accepts_standard_5field_expression() {
        assert!(CronTrigger::new("0 3 * * 1", wf()).is_ok());
        assert!(CronTrigger::new("*/15 * * * *", wf()).is_ok());
    }

    #[test]
    fn next_after_is_the_following_midnight() {
        // Daily at 00:00. From noon on Jan 1, the next fire is midnight starting Jan 2.
        let trigger = CronTrigger::new("0 0 * * *", wf()).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let next = trigger.next_after(base).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap());
    }

    #[test]
    fn next_after_respects_hour_and_weekday() {
        // 03:00 every Monday. From Mon 2026-06-01 12:00, next is Mon 2026-06-08 03:00.
        let trigger = CronTrigger::new("0 3 * * 1", wf()).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let next = trigger.next_after(base).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 6, 8, 3, 0, 0).unwrap());
    }

    #[test]
    fn next_after_is_strictly_after_a_boundary_instant() {
        // Standing exactly on a fire time yields the *next* one, never the same instant.
        let trigger = CronTrigger::new("0 0 * * *", wf()).unwrap();
        let midnight = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let next = trigger.next_after(midnight).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap());
    }

    /// The weekday of the next fire for `expr`, starting from noon Mon 2026-06-01.
    fn next_weekday(expr: &str) -> chrono::Weekday {
        use chrono::Datelike as _;
        let base = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        CronTrigger::new(expr, wf())
            .unwrap()
            .next_after(base)
            .unwrap()
            .weekday()
    }

    #[test]
    fn day_of_week_follows_posix_not_quartz() {
        use chrono::Weekday;
        // The bug this guards: the `cron` crate's raw numbering makes `1` = Sunday. Under
        // POSIX (what the IR promises) `1` = Monday, `0`/`7` = Sunday, `6` = Saturday.
        assert_eq!(next_weekday("0 3 * * 1"), Weekday::Mon, "1 must be Monday");
        assert_eq!(next_weekday("0 3 * * 0"), Weekday::Sun, "0 must be Sunday");
        assert_eq!(next_weekday("0 3 * * 7"), Weekday::Sun, "7 must be Sunday");
        assert_eq!(next_weekday("0 3 * * 5"), Weekday::Fri, "5 must be Friday");
        assert_eq!(
            next_weekday("0 3 * * 6"),
            Weekday::Sat,
            "6 must be Saturday"
        );
    }

    #[test]
    fn posix_dow_ranges_ending_on_sunday_build_and_fire() {
        use chrono::{Datelike as _, Weekday};
        // Regression: every POSIX day-of-week range that ends on Sunday used to be rewritten
        // to a `MON-SUN`-style *named* range and rejected by the cron crate, aborting the
        // whole daemon at startup. They must now build.
        for expr in [
            "0 3 * * 1-7", // every day
            "0 3 * * 4-7",
            "0 3 * * 5-7",
            "0 3 * * 6-7",
            "0 3 * * 6-0", // wrap-around Sat..Sun
        ] {
            assert!(CronTrigger::new(expr, wf()).is_ok(), "{expr} must build");
        }
        // `1-7` is every day: from Wed noon the next fire is Thu 03:00.
        let every = CronTrigger::new("0 3 * * 1-7", wf()).unwrap();
        let wed_noon = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
        assert_eq!(
            every.next_after(wed_noon).unwrap(),
            Utc.with_ymd_and_hms(2026, 6, 4, 3, 0, 0).unwrap()
        );
        // `5-7` = Fri,Sat,Sun — exactly those three weekdays fire over a week.
        let fss = CronTrigger::new("0 3 * * 5-7", wf()).unwrap();
        let mut at = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..3 {
            at = fss.next_after(at).unwrap();
            seen.insert(at.weekday());
        }
        assert_eq!(
            seen,
            [Weekday::Fri, Weekday::Sat, Weekday::Sun]
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
        );
    }

    #[test]
    fn full_week_ranges_and_stepped_singles_include_sunday() {
        use chrono::{Datelike as _, Weekday};
        // `0-7` spans the whole week (both 0 and 7 are Sunday) — must NOT collapse to Sunday.
        let every = CronTrigger::new("0 3 * * 0-7", wf()).unwrap();
        let mut at = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..7 {
            at = every.next_after(at).unwrap();
            seen.insert(at.weekday());
        }
        assert_eq!(seen.len(), 7, "0-7 must fire every weekday, got {seen:?}");
        // `1/2` (vixie: every other day from Monday, wrapping to include Sunday) = Mon,Wed,Fri,Sun.
        let odd = CronTrigger::new("0 3 * * 1/2", wf()).unwrap();
        let mut at = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..4 {
            at = odd.next_after(at).unwrap();
            seen.insert(at.weekday());
        }
        assert_eq!(
            seen,
            [Weekday::Mon, Weekday::Wed, Weekday::Fri, Weekday::Sun]
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
        );
    }

    #[test]
    fn day_of_week_ranges_lists_and_steps_are_posix() {
        use chrono::{Datelike as _, Weekday};
        // `1-5` is Mon–Fri: from noon Monday, the next 03:00 fire is Tuesday.
        assert_eq!(next_weekday("0 3 * * 1-5"), Weekday::Tue);
        // A list `1,3,5` (Mon/Wed/Fri): next fire after noon Monday is Wednesday.
        assert_eq!(next_weekday("0 3 * * 1,3,5"), Weekday::Wed);

        // `1-5` fires on exactly the five weekdays over a week, none on the weekend.
        let trigger = CronTrigger::new("0 3 * * 1-5", wf()).unwrap();
        let mut at = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..5 {
            at = trigger.next_after(at).unwrap();
            seen.insert(at.weekday());
        }
        let expected: std::collections::HashSet<Weekday> = [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
        ]
        .into_iter()
        .collect();
        assert_eq!(seen, expected);
    }
}
