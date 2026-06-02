//! [`CronTrigger`]: a [`Trigger`](odin_core::Trigger) that fires a workflow on a
//! standard 5-field cron schedule.

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
///    rejects `0`. Both dialects parse day *names* identically, so we rewrite the numeric
///    day-of-week field to names ([`normalize_dow`]) — `0 3 * * 1` then means **Monday**,
///    as the IR documents.
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

/// Rewrites the numeric tokens of a POSIX day-of-week field to day names, so the `cron`
/// crate's Quartz numbering can't silently shift them. Handles single values, comma lists,
/// ranges, and steps (`1-5/2`); `*`, `?`, and names pass through untouched. Out-of-range
/// numbers are left as-is for the crate to reject.
fn normalize_dow(field: &str) -> String {
    field
        .split(',')
        .map(|item| {
            let (base, step) = match item.split_once('/') {
                Some((b, s)) => (b, Some(s)),
                None => (item, None),
            };
            let base = base
                .split('-')
                .map(map_dow_token)
                .collect::<Vec<_>>()
                .join("-");
            match step {
                Some(s) => format!("{base}/{s}"),
                None => base,
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Maps a single POSIX day-of-week number (`0`/`7` = Sunday … `6` = Saturday) to its day
/// name; anything that is not a `0..=7` integer (a name, `*`, `?`) is returned unchanged.
fn map_dow_token(token: &str) -> String {
    const NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
    match token.trim().parse::<u8>() {
        Ok(n) if n <= 7 => NAMES[(n % 7) as usize].to_owned(),
        _ => token.to_owned(),
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
