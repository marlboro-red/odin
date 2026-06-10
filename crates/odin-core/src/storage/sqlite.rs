//! A single-file SQLite [`Store`] for durable, crash-resumable run state.
//!
//! Each run is one row in `runs` holding the whole [`RunState`] as a JSON blob plus a
//! denormalized `status` column for cheap incomplete-run queries; the append-only audit
//! log lives in `events`. rusqlite is synchronous, so each async method briefly locks the
//! connection and runs the (sub-millisecond, local) query inline.

use std::path::Path;

use async_trait::async_trait;
use chrono::Utc;
use indexmap::IndexMap;
use rusqlite::{Connection, OptionalExtension as _, params, params_from_iter};
use tokio::sync::Mutex;

use crate::api::RunStatus;
use crate::error::StoreError;
use crate::ids::RunId;
use crate::traits::{
    PrunePolicy, PruneReport, PrunedCount, RunEvent, RunState, RunStatusCount, Store, StoreMetrics,
};

/// The baseline schema (migration to v1). Idempotent (`IF NOT EXISTS`), so it also adopts a
/// pre-versioning database that already has the tables.
const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS runs (
    run_id     TEXT PRIMARY KEY,
    workflow   TEXT NOT NULL,
    status     TEXT NOT NULL,
    state      TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS runs_status ON runs(status);
CREATE TABLE IF NOT EXISTS events (
    run_id TEXT NOT NULL,
    seq    INTEGER NOT NULL,
    event  TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
);
";

/// A composite index on `(workflow, status)` so the `/metrics` aggregate (`GROUP BY workflow,
/// status`) streams from the index in group order instead of scanning every row and building a
/// temp B-tree. The pre-existing `runs_status` index (on `status` alone) can't serve a grouping
/// that leads with `workflow`.
const SCHEMA_V2_METRICS_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS runs_workflow_status ON runs(workflow, status);";

/// A persistent tally of pruned terminal runs per `(workflow, status)`. Retention deletes
/// `runs` rows, which would make the `odin_runs_total` counter (a live `COUNT(*)`) DECREASE —
/// invalid for a Prometheus counter. `prune` instead UPSERTs the about-to-be-deleted counts
/// here *before* deleting, so `metrics()` can report `live COUNT(*) + pruned tally`: a finished
/// run adds to the live count, a pruned run moves that unit live→tally, the sum never falls.
/// It holds only terminal statuses (the prune predicate guarantees it), so the non-terminal
/// gauges are unaffected.
const SCHEMA_V3_PRUNED_COUNTS: &str = "
CREATE TABLE IF NOT EXISTS pruned_counts (
    workflow TEXT NOT NULL,
    status   TEXT NOT NULL,
    pruned   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workflow, status)
);
";

/// An index on `updated_at` (descending) serving the hot `recent()` list query
/// (`ORDER BY updated_at DESC, run_id DESC LIMIT ?`) — the dashboard poll, `odin status --watch`,
/// and `odin list`. Without it that sort builds a temp B-tree over every row on each call; with it
/// the rows stream from the index in already-sorted order and the `LIMIT` stops early.
const SCHEMA_V4_UPDATED_AT_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS runs_updated_at ON runs(updated_at DESC, run_id DESC);";

/// A tiny signal table for cross-process cancellation: `odin cancel <id>` (a separate process from
/// the daemon executing the run) inserts the run id here, and the engine's per-run watcher polls it
/// and fires the run's cancel token. A separate `IF NOT EXISTS` table (rather than a `runs` column)
/// keeps the migration idempotent and the transient signal out of the durable run-state blob.
const SCHEMA_V5_CANCEL_REQUESTS: &str = "
CREATE TABLE IF NOT EXISTS cancel_requests (
    run_id TEXT PRIMARY KEY
);
";

/// Ordered migrations tracked by SQLite's `PRAGMA user_version`. The entry at index `i`
/// upgrades the database from version `i` to `i + 1` (so `MIGRATIONS.len()` is the current
/// version). **Append** new migrations; never edit or reorder a released one — an in-place
/// edit would not re-run on a database already at that version.
const MIGRATIONS: &[&str] = &[
    SCHEMA_V1,
    SCHEMA_V2_METRICS_INDEX,
    SCHEMA_V3_PRUNED_COUNTS,
    SCHEMA_V4_UPDATED_AT_INDEX,
    SCHEMA_V5_CANCEL_REQUESTS,
];

/// A durable run store backed by a SQLite database.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Opens (creating if needed) a SQLite store at `path`, ensuring the schema exists.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if the database cannot be opened or migrated.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::from_conn(db(Connection::open(path))?, true)
    }

    /// Opens an ephemeral in-memory store (for tests).
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if the database cannot be created.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::from_conn(db(Connection::open_in_memory())?, false)
    }

    fn from_conn(conn: Connection, wal: bool) -> Result<Self, StoreError> {
        // A shared on-disk DB can be opened by a second `odin run`/reader; make writers
        // wait rather than fail with SQLITE_BUSY, and use WAL so readers don't block.
        db(conn.busy_timeout(std::time::Duration::from_secs(5)))?;
        let synchronous = sync_mode();
        if wal {
            let _: String = db(conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0)))?;
            // Choose durability EXPLICITLY rather than relying on the default. NORMAL is the
            // SQLite-recommended setting under WAL: corruption-safe, and the only failure mode
            // is losing the most recent checkpoint(s) on a power loss — which resume re-runs
            // idempotently. An operator who needs zero loss sets `ODIN_SQLITE_SYNCHRONOUS=full`.
            db(conn.pragma_update(None, "synchronous", synchronous))?;
        }
        Self::migrate(&conn)?;
        tracing::debug!(
            schema_version = MIGRATIONS.len(),
            synchronous,
            durable = wal,
            "opened run-state store"
        );
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Brings the database up to the current schema version, applying any pending migrations
    /// atomically. Refuses a database written by a **newer** build rather than operating on a
    /// schema it doesn't understand.
    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        let target = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
        let current: i64 = db(conn.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
        if current > target {
            return Err(StoreError::Backend(format!(
                "run-state database is at schema v{current}, newer than this build supports \
                 (v{target}); upgrade odin to read it"
            )));
        }
        if current == target {
            return Ok(());
        }
        // Apply pending migrations in one transaction so a failure leaves the DB untouched.
        db(conn.execute_batch("BEGIN"))?;
        let applied = (|| -> rusqlite::Result<()> {
            for migration in &MIGRATIONS[usize::try_from(current).unwrap_or(0)..] {
                conn.execute_batch(migration)?;
            }
            // `user_version` takes no bind parameters; `target` is an internal constant.
            conn.pragma_update(None, "user_version", target)
        })();
        match applied {
            Ok(()) => db(conn.execute_batch("COMMIT")),
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(StoreError::Backend(format!("schema migration failed: {e}")))
            }
        }
    }
}

/// The `synchronous` mode for on-disk databases: `NORMAL` (the WAL default) unless the operator
/// opts into `FULL` via `$ODIN_SQLITE_SYNCHRONOUS=full` for zero-loss durability.
fn sync_mode() -> &'static str {
    match std::env::var("ODIN_SQLITE_SYNCHRONOUS") {
        Ok(v) if v.eq_ignore_ascii_case("full") => "FULL",
        _ => "NORMAL",
    }
}

/// Maps a rusqlite error into a [`StoreError`].
fn db<T>(r: rusqlite::Result<T>) -> Result<T, StoreError> {
    r.map_err(|e| StoreError::Backend(e.to_string()))
}

/// Deserializes one run-state blob into `out`, **tolerating a poison row**: a single
/// undeserializable `state` (a row written by a newer schema, or corruption) is logged and
/// skipped rather than failing the whole bulk read — so one bad row can't block crash recovery
/// (`load_incomplete`) of every healthy run, nor break `odin status` / the dashboard.
fn push_state(out: &mut Vec<RunState>, json: &str) {
    match serde_json::from_str(json) {
        Ok(state) => out.push(state),
        Err(e) => tracing::warn!(error = %e, "skipping undeserializable run row"),
    }
}

/// The run's status as the lowercase string stored in the `status` column. Matches the
/// serde representation of [`RunStatus`], so the `load_incomplete` filter is consistent.
fn status_str(status: RunStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

#[async_trait]
impl Store for SqliteStore {
    async fn checkpoint(&self, state: &RunState) -> Result<(), StoreError> {
        let json = serde_json::to_string(state)?;
        let status = status_str(state.status);
        let run_id = state.run_id.to_string();
        let workflow = state.workflow.as_str().to_owned();
        let updated_at = state.updated_at.to_rfc3339();

        let conn = self.conn.lock().await;
        db(conn.execute(
            "INSERT INTO runs(run_id, workflow, status, state, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(run_id) DO UPDATE SET status = ?3, state = ?4, updated_at = ?5",
            params![run_id, workflow, status, json, updated_at],
        ))?;
        Ok(())
    }

    async fn append_event(&self, run_id: RunId, event: &RunEvent) -> Result<(), StoreError> {
        let json = serde_json::to_string(event)?;
        let run_id = run_id.to_string();

        let conn = self.conn.lock().await;
        db(conn.execute(
            "INSERT INTO events(run_id, seq, event)
             VALUES (?1, (SELECT COALESCE(MAX(seq), -1) + 1 FROM events WHERE run_id = ?1), ?2)",
            params![run_id, json],
        ))?;
        Ok(())
    }

    async fn load_incomplete(&self) -> Result<Vec<RunState>, StoreError> {
        // Crash-resumable runs only: terminal states are excluded, and so is `AwaitingApproval`
        // — a run paused at an approval gate is deliberately parked, not crashed, and must NOT
        // be auto-resumed; recording a decision flips it back to `Running` to be picked up.
        // (Derive the strings via status_str so the filter can't drift from the serde repr.)
        let parked = [
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::AwaitingApproval,
        ]
        .map(status_str);
        let conn = self.conn.lock().await;
        let mut stmt = db(conn.prepare(
            "SELECT state FROM runs WHERE status NOT IN (?1, ?2, ?3, ?4) ORDER BY updated_at",
        ))?;
        let rows = db(stmt
            .query_map(params![parked[0], parked[1], parked[2], parked[3]], |row| {
                row.get::<_, String>(0)
            }))?;
        let mut out = Vec::new();
        for row in rows {
            push_state(&mut out, &db(row)?);
        }
        Ok(out)
    }

    async fn recent(&self, limit: usize) -> Result<Vec<RunState>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.conn.lock().await;
        let mut stmt =
            db(conn
                .prepare("SELECT state FROM runs ORDER BY updated_at DESC, run_id DESC LIMIT ?1"))?;
        let rows = db(stmt.query_map(params![limit], |row| row.get::<_, String>(0)))?;
        let mut out = Vec::new();
        for row in rows {
            push_state(&mut out, &db(row)?);
        }
        Ok(out)
    }

    async fn load_run(&self, run_id: RunId) -> Result<Option<RunState>, StoreError> {
        let conn = self.conn.lock().await;
        let run_id = run_id.to_string();
        let mut stmt = db(conn.prepare("SELECT state FROM runs WHERE run_id = ?1"))?;
        let mut rows = db(stmt.query(params![run_id]))?;
        match db(rows.next())? {
            Some(row) => Ok(Some(serde_json::from_str(&db(row.get::<_, String>(0))?)?)),
            None => Ok(None),
        }
    }

    async fn request_cancel(&self, run_id: RunId) -> Result<bool, StoreError> {
        let id = run_id.to_string();
        let terminal = terminal_statuses();
        let in_terminal = placeholders(terminal.len());
        let conn = self.conn.lock().await;
        // Only mark a run that exists and is NOT terminal — a finished run can't be cancelled, and
        // an unknown id should report "not found" rather than leave an orphan signal.
        let cancellable: bool = db(conn.query_row(
            &format!(
                "SELECT 1 FROM runs WHERE run_id = ?1 AND status NOT IN ({in_terminal})"
            ),
            params_from_iter(std::iter::once(id.clone()).chain(terminal)),
            |_| Ok(true),
        )
        .optional())?
        .unwrap_or(false);
        if cancellable {
            db(conn.execute(
                "INSERT OR IGNORE INTO cancel_requests(run_id) VALUES (?1)",
                params![id],
            ))?;
        }
        Ok(cancellable)
    }

    async fn is_cancel_requested(&self, run_id: RunId) -> Result<bool, StoreError> {
        let id = run_id.to_string();
        let conn = self.conn.lock().await;
        Ok(db(conn
            .query_row(
                "SELECT 1 FROM cancel_requests WHERE run_id = ?1",
                params![id],
                |_| Ok(true),
            )
            .optional())?
        .unwrap_or(false))
    }

    async fn claim_awaiting(&self, run_id: RunId) -> Result<bool, StoreError> {
        // Atomic compare-and-swap on the indexed `status` column: only the row that *is*
        // `awaiting_approval` flips to `running`, and `execute` returns the affected-row count, so
        // exactly one of two racing processes gets `1`. The blob's own `status` field still reads
        // `awaiting_approval` until the caller checkpoints the recorded decision — and if the
        // process crashes in that window, resume re-reaches the gate (no decision recorded) and
        // re-parks the run, healing the column. (See `Store::claim_awaiting`.)
        let awaiting = status_str(RunStatus::AwaitingApproval);
        let running = status_str(RunStatus::Running);
        let id = run_id.to_string();
        let conn = self.conn.lock().await;
        let changed = db(conn.execute(
            "UPDATE runs SET status = ?2 WHERE run_id = ?1 AND status = ?3",
            params![id, running, awaiting],
        ))?;
        Ok(changed == 1)
    }

    async fn events(&self, run_id: RunId) -> Result<Vec<RunEvent>, StoreError> {
        let conn = self.conn.lock().await;
        let run_id = run_id.to_string();
        let mut stmt = db(conn.prepare("SELECT event FROM events WHERE run_id = ?1 ORDER BY seq"))?;
        let rows = db(stmt.query_map(params![run_id], |row| row.get::<_, String>(0)))?;
        let mut out = Vec::new();
        for row in rows {
            let json = db(row)?;
            match serde_json::from_str(&json) {
                Ok(event) => out.push(event),
                Err(e) => tracing::warn!(error = %e, "skipping undeserializable event row"),
            }
        }
        Ok(out)
    }

    async fn metrics(&self) -> Result<StoreMetrics, StoreError> {
        // Fold the persistent pruned tally into the live counts so a pruned terminal run still
        // counts toward `odin_runs_total` (which must not decrease). `pruned_counts` holds only
        // terminal statuses, so the non-terminal gauges read `live + 0`. Both inputs are indexed
        // aggregates / a tiny keyed table — no JSON blobs parsed.
        let conn = self.conn.lock().await;
        let mut stmt = db(conn.prepare(
            "SELECT workflow, status, SUM(c) FROM (
                 SELECT workflow, status, COUNT(*) AS c FROM runs GROUP BY workflow, status
                 UNION ALL
                 SELECT workflow, status, pruned AS c FROM pruned_counts
             ) GROUP BY workflow, status",
        ))?;
        let rows = db(stmt.query_map([], |row| {
            Ok(RunStatusCount {
                workflow: row.get::<_, String>(0)?,
                status: row.get::<_, String>(1)?,
                count: row.get::<_, i64>(2)?.max(0).unsigned_abs(),
            })
        }))?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(db(row)?);
        }
        Ok(StoreMetrics { runs })
    }

    async fn prune(&self, policy: &PrunePolicy, dry_run: bool) -> Result<PruneReport, StoreError> {
        // A no-op policy must delete nothing (callers should reject one, but defend here too).
        if policy.is_noop() {
            return Ok(PruneReport {
                dry_run,
                ..PruneReport::default()
            });
        }
        let conn = self.conn.lock().await;
        prune_locked(&conn, policy, dry_run)
    }
}

/// The terminal run statuses as their stored `status` strings, derived from
/// [`RunStatus::is_terminal`] so the prune predicate can never drift from the enum. The ONLY
/// statuses a run may be pruned in.
fn terminal_statuses() -> Vec<String> {
    [
        RunStatus::Succeeded,
        RunStatus::Failed,
        RunStatus::Cancelled,
        RunStatus::Pending,
        RunStatus::Running,
        RunStatus::AwaitingApproval,
    ]
    .into_iter()
    .filter(|s| s.is_terminal())
    .map(status_str)
    .collect()
}

/// Selects the eligible terminal run ids for `policy`, then (unless `dry_run`) folds their
/// counts into `pruned_counts` and deletes their `runs` + `events` rows — all in one
/// transaction so a crash can't strand events or double-count the tally. Runs under the held
/// connection lock.
#[allow(clippy::too_many_lines)]
fn prune_locked(
    conn: &Connection,
    policy: &PrunePolicy,
    dry_run: bool,
) -> Result<PruneReport, StoreError> {
    let terminal = terminal_statuses();
    let in_terminal = placeholders(terminal.len());

    // Assemble the WHERE clause and its positional binds together so they stay in lockstep. The
    // `status IN (terminal…)` predicate leads EVERY query/branch, so a non-terminal run is
    // structurally unselectable (the safety invariant).
    let mut conds = vec![format!("status IN ({in_terminal})")];
    let mut binds: Vec<String> = terminal.clone();
    if let Some(workflow) = &policy.workflow {
        conds.push("workflow = ?".to_owned());
        binds.push(workflow.as_str().to_owned());
    }
    if let Some(max_age) = policy.max_age {
        // `updated_at` (a terminal run's completion time) is the indexed column; compare RFC3339
        // strings, which sort chronologically since we always write a `+00:00` offset. Use the
        // CHECKED subtraction: an absurdly large `max_age` (past chrono's representable range)
        // would otherwise panic. An un-representable cutoff means "older than ~infinity ago" =
        // nothing qualifies, so the age limit matches no rows (a false predicate).
        match Utc::now().checked_sub_signed(max_age) {
            Some(cutoff) => {
                conds.push("updated_at < ?".to_owned());
                binds.push(cutoff.to_rfc3339());
            }
            None => conds.push("0 = 1".to_owned()),
        }
    }
    if let Some(keep_last) = policy.keep_last {
        // Keep the newest `keep_last` terminal runs PER workflow; the rest are eligible. The
        // subquery is itself terminal-filtered, so a non-terminal run never occupies a kept slot.
        conds.push(format!(
            "run_id IN (SELECT run_id FROM runs r2 WHERE r2.status IN ({in_terminal}) \
             AND r2.workflow = runs.workflow \
             ORDER BY r2.updated_at DESC, r2.run_id DESC LIMIT -1 OFFSET ?)"
        ));
        binds.extend(terminal.iter().cloned());
        binds.push(keep_last.to_string());
    }
    let where_clause = conds.join(" AND ");

    // 1. Select the eligible (run_id, workflow, status). One pass; the JSON blob is never parsed.
    let select = format!("SELECT run_id, workflow, status FROM runs WHERE {where_clause}");
    let mut stmt = db(conn.prepare(&select))?;
    let eligible: Vec<(String, String, String)> = db(db(stmt
        .query_map(params_from_iter(&binds), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        }))?
    .collect::<rusqlite::Result<Vec<_>>>())?;

    // Aggregate per (workflow, status) for the report and the tally upsert.
    let mut per: IndexMap<(String, String), u64> = IndexMap::new();
    let mut run_ids = Vec::with_capacity(eligible.len());
    for (id, workflow, status) in &eligible {
        *per.entry((workflow.clone(), status.clone())).or_default() += 1;
        if let Ok(uuid) = uuid::Uuid::parse_str(id) {
            run_ids.push(RunId(uuid));
        }
    }
    let per_workflow: Vec<PrunedCount> = per
        .iter()
        .map(|((workflow, status), count)| PrunedCount {
            workflow: workflow.clone(),
            status: status.clone(),
            count: *count,
        })
        .collect();
    let runs_pruned = u64::try_from(eligible.len()).unwrap_or(u64::MAX);

    if dry_run || eligible.is_empty() {
        return Ok(PruneReport {
            runs_pruned,
            events_pruned: 0,
            per_workflow,
            run_ids,
            dry_run,
        });
    }

    // 2. Delete (and tally) atomically. The eligible ids drive both deletes.
    let ids: Vec<&String> = eligible.iter().map(|(id, _, _)| id).collect();
    let id_ph = placeholders(ids.len());
    db(conn.execute_batch("BEGIN"))?;
    let applied = (|| -> rusqlite::Result<usize> {
        // Fold counts into the persistent tally BEFORE deleting, so `live + pruned` is preserved.
        for ((workflow, status), count) in &per {
            conn.execute(
                "INSERT INTO pruned_counts(workflow, status, pruned) VALUES (?1, ?2, ?3)
                 ON CONFLICT(workflow, status) DO UPDATE SET pruned = pruned + ?3",
                params![workflow, status, i64::try_from(*count).unwrap_or(i64::MAX)],
            )?;
        }
        // BOTH deletes re-apply the terminal predicate (defence in depth: a selected row that
        // flipped non-terminal between the SELECT and now is left untouched — both its row AND
        // its events — so a surviving run never loses its audit log). `terminal` is permanent
        // today, so this is belt-and-suspenders, but it keeps the two deletes provably symmetric.
        let with_terminal = || ids.iter().map(|s| (*s).clone()).chain(terminal.clone());
        let events_pruned = conn.execute(
            &format!(
                "DELETE FROM events WHERE run_id IN \
                 (SELECT run_id FROM runs WHERE run_id IN ({id_ph}) AND status IN ({in_terminal}))"
            ),
            params_from_iter(with_terminal()),
        )?;
        conn.execute(
            &format!("DELETE FROM runs WHERE run_id IN ({id_ph}) AND status IN ({in_terminal})"),
            params_from_iter(with_terminal()),
        )?;
        // Reclaim any leftover cross-process cancel signals for the pruned runs (harmless if absent).
        conn.execute(
            &format!("DELETE FROM cancel_requests WHERE run_id IN ({id_ph})"),
            params_from_iter(ids.iter().map(|s| (*s).clone())),
        )?;
        Ok(events_pruned)
    })();
    let events_pruned = match applied {
        Ok(n) => {
            db(conn.execute_batch("COMMIT"))?;
            u64::try_from(n).unwrap_or(u64::MAX)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(StoreError::Backend(format!("prune failed: {e}")));
        }
    };

    Ok(PruneReport {
        runs_pruned,
        events_pruned,
        per_workflow,
        run_ids,
        dry_run: false,
    })
}

/// `?,?,…` — `n` SQL placeholders for an `IN (...)` clause.
fn placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::SqliteStore;
    use crate::api::{RunInput, RunStatus};
    use crate::ids::{RunId, WorkflowId};
    use crate::traits::{RunEvent, RunState, Store};
    use chrono::Utc;
    use indexmap::IndexMap;

    fn run_state(status: RunStatus) -> RunState {
        RunState {
            run_id: RunId::new(),
            workflow: WorkflowId::new("w"),
            schema_major: 1,
            status,
            error: None,
            steps: IndexMap::new(),
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual(),
            workspace: None,
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn checkpoint_and_filter_incomplete() {
        let store = SqliteStore::open_in_memory().unwrap();
        let running = run_state(RunStatus::Running);
        let done = run_state(RunStatus::Succeeded);
        let running_id = running.run_id;
        store.checkpoint(&running).await.unwrap();
        store.checkpoint(&done).await.unwrap();

        let incomplete = store.load_incomplete().await.unwrap();
        assert_eq!(incomplete.len(), 1);
        assert_eq!(incomplete[0].run_id, running_id);
        assert!(store.load_run(running_id).await.unwrap().is_some());
        assert!(store.load_run(RunId::new()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn checkpoint_upserts_in_place() {
        let store = SqliteStore::open_in_memory().unwrap();
        let mut state = run_state(RunStatus::Running);
        store.checkpoint(&state).await.unwrap();
        state.status = RunStatus::Succeeded;
        store.checkpoint(&state).await.unwrap();

        assert!(
            store.load_incomplete().await.unwrap().is_empty(),
            "now terminal"
        );
        let loaded = store.load_run(state.run_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, RunStatus::Succeeded);
    }

    #[tokio::test]
    async fn events_round_trip_in_order() {
        let store = SqliteStore::open_in_memory().unwrap();
        let state = run_state(RunStatus::Running);
        store.checkpoint(&state).await.unwrap();
        store
            .append_event(state.run_id, &RunEvent::RunStarted { at: Utc::now() })
            .await
            .unwrap();
        store
            .append_event(
                state.run_id,
                &RunEvent::RunFinished {
                    status: RunStatus::Succeeded,
                    at: Utc::now(),
                },
            )
            .await
            .unwrap();

        let events = store.events(state.run_id).await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], RunEvent::RunStarted { .. }));
        assert!(matches!(events[1], RunEvent::RunFinished { .. }));
    }

    #[tokio::test]
    async fn recent_returns_newest_first_and_respects_limit() {
        let store = SqliteStore::open_in_memory().unwrap();
        let mut older = run_state(RunStatus::Succeeded);
        older.updated_at = Utc::now() - chrono::Duration::seconds(30);
        let newer = run_state(RunStatus::Running);
        store.checkpoint(&older).await.unwrap();
        store.checkpoint(&newer).await.unwrap();

        let recent = store.recent(10).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].run_id, newer.run_id, "newest first");
        assert_eq!(store.recent(1).await.unwrap().len(), 1, "limit respected");
    }

    /// One undeserializable `state` blob (a row from a newer schema, or corruption) must be
    /// skipped, not abort the whole bulk read — otherwise a single bad row blocks crash recovery
    /// of every healthy run and breaks `odin status`.
    #[tokio::test]
    async fn a_poison_row_is_skipped_not_fatal() {
        let store = SqliteStore::open_in_memory().unwrap();
        let good = run_state(RunStatus::Running);
        store.checkpoint(&good).await.unwrap();
        // Inject a garbage blob directly. Its far-future `updated_at` makes it sort FIRST in
        // `recent`, proving the skip happens regardless of position.
        {
            let conn = store.conn.lock().await;
            conn.execute(
                "INSERT INTO runs(run_id, workflow, status, state, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    "poison",
                    "w",
                    "running",
                    "{ this is not valid run state",
                    "2099-01-01T00:00:00+00:00"
                ],
            )
            .unwrap();
        }
        let incomplete = store.load_incomplete().await.unwrap();
        assert_eq!(incomplete.len(), 1, "the healthy run is still recovered");
        assert_eq!(incomplete[0].run_id, good.run_id);
        assert_eq!(
            store.recent(10).await.unwrap().len(),
            1,
            "recent skips the poison row too"
        );
    }

    /// `request_cancel`/`is_cancel_requested` carry a cross-process cancel signal: it marks only a
    /// non-terminal run, and a terminal/unknown run reports "not cancellable".
    #[tokio::test]
    async fn cancel_request_round_trips() {
        let store = SqliteStore::open_in_memory().unwrap();
        let running = run_state(RunStatus::Running);
        store.checkpoint(&running).await.unwrap();
        assert!(
            !store.is_cancel_requested(running.run_id).await.unwrap(),
            "no request yet"
        );
        assert!(
            store.request_cancel(running.run_id).await.unwrap(),
            "a running run is cancellable"
        );
        assert!(
            store.is_cancel_requested(running.run_id).await.unwrap(),
            "the request is recorded"
        );
        assert!(
            !store.request_cancel(RunId::new()).await.unwrap(),
            "an unknown run is not cancellable"
        );
        let done = run_state(RunStatus::Succeeded);
        store.checkpoint(&done).await.unwrap();
        assert!(
            !store.request_cancel(done.run_id).await.unwrap(),
            "a terminal run can't be cancelled"
        );
    }

    /// `claim_awaiting` is a one-winner compare-and-swap: it flips `awaiting_approval` -> `running`
    /// and returns `true` for exactly the caller that won, `false` otherwise — the cross-process
    /// fence against a double-applied approval.
    #[tokio::test]
    async fn claim_awaiting_is_a_one_winner_cas() {
        let store = SqliteStore::open_in_memory().unwrap();
        let awaiting = run_state(RunStatus::AwaitingApproval);
        store.checkpoint(&awaiting).await.unwrap();
        assert!(
            store.claim_awaiting(awaiting.run_id).await.unwrap(),
            "the first claim wins"
        );
        assert!(
            !store.claim_awaiting(awaiting.run_id).await.unwrap(),
            "the second claim loses (the row is now running)"
        );
        assert!(
            !store.claim_awaiting(RunId::new()).await.unwrap(),
            "an unknown run flips nothing"
        );
        let running = run_state(RunStatus::Running);
        store.checkpoint(&running).await.unwrap();
        assert!(
            !store.claim_awaiting(running.run_id).await.unwrap(),
            "a run that isn't awaiting can't be claimed"
        );
    }

    #[tokio::test]
    async fn survives_reopen_for_crash_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let run_id;
        {
            let store = SqliteStore::open(&path).unwrap();
            let state = run_state(RunStatus::Running);
            run_id = state.run_id;
            store.checkpoint(&state).await.unwrap();
        } // store (and its connection) dropped — simulates a crash/restart.

        let store = SqliteStore::open(&path).unwrap();
        let incomplete = store.load_incomplete().await.unwrap();
        assert_eq!(incomplete.len(), 1);
        assert_eq!(incomplete[0].run_id, run_id);
    }

    #[tokio::test]
    async fn recent_query_is_backed_by_an_updated_at_index() {
        let store = SqliteStore::open_in_memory().unwrap();
        let conn = store.conn.lock().await;
        let has_index: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'runs_updated_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_index, "the recent() list query must have an updated_at index");
    }

    #[tokio::test]
    async fn fresh_database_is_at_the_current_schema_version() {
        let store = SqliteStore::open_in_memory().unwrap();
        let conn = store.conn.lock().await;
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, i64::try_from(super::MIGRATIONS.len()).unwrap());
    }

    #[tokio::test]
    async fn rejects_a_database_from_a_newer_build() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        SqliteStore::open(&path).unwrap(); // create at the current version, then close.
        // Simulate a database written by a future odin (schema v999).
        rusqlite::Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 999_i64)
            .unwrap();
        match SqliteStore::open(&path) {
            Err(e) => assert!(
                format!("{e}").contains("newer"),
                "expected a refuse-newer-schema error, got: {e}"
            ),
            Ok(_) => panic!("a newer-schema database must be refused, not opened"),
        }
    }

    #[tokio::test]
    async fn on_disk_store_sets_synchronous_explicitly() {
        // Durability must be chosen, not defaulted: NORMAL (== 1) under WAL.
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("state.db")).unwrap();
        let conn = store.conn.lock().await;
        let sync: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 1, "1 == NORMAL, the WAL-recommended default");
    }

    // ---- retention / prune ----

    use crate::traits::PrunePolicy;
    use chrono::Duration;

    fn at(workflow: &str, status: RunStatus, updated: chrono::DateTime<Utc>) -> RunState {
        let mut s = run_state(status);
        s.workflow = WorkflowId::new(workflow);
        s.updated_at = updated;
        s
    }

    /// The terminal-status string set the prune predicate is built from must be EXACTLY the
    /// terminal `RunStatus` variants — guards against the safety predicate drifting.
    #[test]
    fn terminal_status_set_is_exactly_the_terminal_variants() {
        let mut set = super::terminal_statuses();
        set.sort();
        assert_eq!(set, ["cancelled", "failed", "succeeded"]);
    }

    #[tokio::test]
    async fn prune_deletes_only_terminal_runs() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = Utc::now();
        let mut seeded: Vec<(RunStatus, RunId)> = Vec::new();
        for st in [
            RunStatus::Pending,
            RunStatus::Running,
            RunStatus::AwaitingApproval,
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            let s = at("w", st, now);
            seeded.push((st, s.run_id));
            store.checkpoint(&s).await.unwrap();
        }
        // The most aggressive count policy (keep 0 per workflow) — age-independent.
        let report = store
            .prune(
                &PrunePolicy {
                    keep_last: Some(0),
                    ..PrunePolicy::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            report.runs_pruned, 3,
            "only the 3 terminal runs are prunable"
        );

        for (st, id) in &seeded {
            let present = store.load_run(*id).await.unwrap().is_some();
            assert_eq!(
                present,
                !st.is_terminal(),
                "{st:?} should {} pruning",
                if st.is_terminal() {
                    "NOT survive"
                } else {
                    "survive"
                }
            );
        }
        // load_incomplete is unchanged (pending+running still there; awaiting parked as before).
        assert_eq!(store.load_incomplete().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn prune_never_touches_an_old_awaiting_approval_gate() {
        let store = SqliteStore::open_in_memory().unwrap();
        let ancient = at(
            "w",
            RunStatus::AwaitingApproval,
            Utc::now() - Duration::days(365),
        );
        let id = ancient.run_id;
        store.checkpoint(&ancient).await.unwrap();
        // A year-old paused gate is FAR past any age limit — but it must never be pruned.
        let report = store
            .prune(
                &PrunePolicy {
                    max_age: Some(Duration::days(1)),
                    keep_last: Some(0),
                    ..PrunePolicy::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(report.runs_pruned, 0);
        assert!(
            store.load_run(id).await.unwrap().is_some(),
            "the indefinite-wait gate survives"
        );
    }

    #[tokio::test]
    async fn prune_keep_last_keeps_the_newest_per_workflow() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = Utc::now();
        let mut newest = Vec::new();
        for i in 0..5 {
            let s = at("a", RunStatus::Succeeded, now - Duration::minutes(i));
            if i < 2 {
                newest.push(s.run_id); // i=0,1 are the two most recent
            }
            store.checkpoint(&s).await.unwrap();
        }
        store
            .checkpoint(&at("b", RunStatus::Succeeded, now))
            .await
            .unwrap();

        let report = store
            .prune(
                &PrunePolicy {
                    keep_last: Some(2),
                    ..PrunePolicy::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            report.runs_pruned, 3,
            "a: 5 → keep 2, prune 3; b: 1 → keep all"
        );
        for id in &newest {
            assert!(
                store.load_run(*id).await.unwrap().is_some(),
                "newest-2 of a survive"
            );
        }
    }

    #[tokio::test]
    async fn prune_keeps_odin_runs_total_monotonic_via_the_tally() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = Utc::now();
        for i in 0..3 {
            store
                .checkpoint(&at("a", RunStatus::Succeeded, now - Duration::minutes(i)))
                .await
                .unwrap();
        }
        let total = |m: &crate::traits::StoreMetrics| {
            m.runs
                .iter()
                .find(|r| r.workflow == "a" && r.status == "succeeded")
                .map_or(0, |r| r.count)
        };
        assert_eq!(total(&store.metrics().await.unwrap()), 3);

        // Prune 2 of the 3 — the counter must NOT drop (live 1 + pruned tally 2 == 3).
        store
            .prune(
                &PrunePolicy {
                    keep_last: Some(1),
                    ..PrunePolicy::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            total(&store.metrics().await.unwrap()),
            3,
            "odin_runs_total must stay monotonic across a prune"
        );
    }

    #[tokio::test]
    async fn prune_removes_events_and_dry_run_changes_nothing() {
        let store = SqliteStore::open_in_memory().unwrap();
        let run = at("w", RunStatus::Succeeded, Utc::now());
        let id = run.run_id;
        store.checkpoint(&run).await.unwrap();
        store
            .append_event(id, &RunEvent::RunStarted { at: Utc::now() })
            .await
            .unwrap();
        store
            .append_event(
                id,
                &RunEvent::RunFinished {
                    status: RunStatus::Succeeded,
                    at: Utc::now(),
                },
            )
            .await
            .unwrap();
        let survivor = at("w", RunStatus::Running, Utc::now());
        store.checkpoint(&survivor).await.unwrap();
        store
            .append_event(survivor.run_id, &RunEvent::RunStarted { at: Utc::now() })
            .await
            .unwrap();

        // Dry run: reports the eligible run but deletes nothing.
        let dry = store
            .prune(
                &PrunePolicy {
                    keep_last: Some(0),
                    ..PrunePolicy::default()
                },
                true,
            )
            .await
            .unwrap();
        assert_eq!(dry.runs_pruned, 1);
        assert!(dry.dry_run);
        assert!(
            store.load_run(id).await.unwrap().is_some(),
            "dry run deletes nothing"
        );
        assert_eq!(store.events(id).await.unwrap().len(), 2);

        // Real prune: the terminal run + its events go; the running run + its event remain.
        let report = store
            .prune(
                &PrunePolicy {
                    keep_last: Some(0),
                    ..PrunePolicy::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(report.runs_pruned, 1);
        assert_eq!(report.events_pruned, 2);
        assert!(store.load_run(id).await.unwrap().is_none());
        assert_eq!(
            store.events(id).await.unwrap().len(),
            0,
            "events of a pruned run are gone"
        );
        assert_eq!(
            store.events(survivor.run_id).await.unwrap().len(),
            1,
            "events of a survivor remain"
        );
    }

    #[tokio::test]
    async fn prune_age_based_prunes_old_keeps_recent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let old = at("w", RunStatus::Failed, Utc::now() - Duration::days(90));
        let recent = at("w", RunStatus::Failed, Utc::now() - Duration::hours(1));
        store.checkpoint(&old).await.unwrap();
        store.checkpoint(&recent).await.unwrap();
        let report = store
            .prune(
                &PrunePolicy {
                    max_age: Some(Duration::days(30)),
                    ..PrunePolicy::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(report.runs_pruned, 1);
        assert!(
            store.load_run(old.run_id).await.unwrap().is_none(),
            "90d-old run pruned"
        );
        assert!(
            store.load_run(recent.run_id).await.unwrap().is_some(),
            "1h-old run kept"
        );
    }

    #[tokio::test]
    async fn prune_with_a_noop_policy_deletes_nothing() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .checkpoint(&at("w", RunStatus::Succeeded, Utc::now()))
            .await
            .unwrap();
        let report = store.prune(&PrunePolicy::default(), false).await.unwrap();
        assert_eq!(report.runs_pruned, 0);
        assert_eq!(
            store
                .metrics()
                .await
                .unwrap()
                .runs
                .iter()
                .map(|r| r.count)
                .sum::<u64>(),
            1
        );
    }

    #[tokio::test]
    async fn prune_workflow_filter_scopes_to_one_workflow() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = Utc::now();
        for _ in 0..2 {
            store
                .checkpoint(&at("a", RunStatus::Succeeded, now))
                .await
                .unwrap();
            store
                .checkpoint(&at("b", RunStatus::Succeeded, now))
                .await
                .unwrap();
        }
        // Prune all of workflow "a" only; "b" is untouched.
        let report = store
            .prune(
                &PrunePolicy {
                    keep_last: Some(0),
                    workflow: Some(WorkflowId::new("a")),
                    ..PrunePolicy::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(report.runs_pruned, 2);
        assert!(report.per_workflow.iter().all(|c| c.workflow == "a"));
        let live: u64 = store
            .metrics()
            .await
            .unwrap()
            .runs
            .iter()
            .filter(|r| r.workflow == "b")
            .map(|r| r.count)
            .sum();
        // "b" still has 2 runs live (and the counter for "a" is preserved via the tally).
        assert_eq!(
            store
                .recent(10)
                .await
                .unwrap()
                .iter()
                .filter(|r| r.workflow.as_str() == "b")
                .count(),
            2
        );
        assert_eq!(live, 2);
    }

    #[tokio::test]
    async fn prune_age_and_count_combined_select_the_intersection() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = Utc::now();
        // 5 runs of ages 100d,80d,60d,2d,1d. max_age=30d marks {100,80,60} old; keep_last=2
        // keeps the newest two {1d,2d}. AND ⇒ prune only runs that are BOTH old AND outside the
        // newest 2 ⇒ {100d,80d,60d} (3). The 2d/1d are recent AND kept; 60d is old AND not kept.
        let ages = [100, 80, 60, 2, 1];
        let mut survivors = Vec::new();
        for d in ages {
            let s = at("w", RunStatus::Succeeded, now - Duration::days(d));
            if d <= 2 {
                survivors.push(s.run_id);
            }
            store.checkpoint(&s).await.unwrap();
        }
        let report = store
            .prune(
                &PrunePolicy {
                    max_age: Some(Duration::days(30)),
                    keep_last: Some(2),
                    ..PrunePolicy::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            report.runs_pruned, 3,
            "only the 3 runs that are BOTH old and unkept"
        );
        for id in &survivors {
            assert!(store.load_run(*id).await.unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn prune_does_not_panic_on_an_absurd_age_and_prunes_nothing() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .checkpoint(&at("w", RunStatus::Succeeded, Utc::now()))
            .await
            .unwrap();
        // A max_age past chrono's representable range: the checked cutoff is None ⇒ nothing is
        // "that old" ⇒ prune nothing, no panic.
        let huge = Duration::weeks(520_000 * 52); // ~520k years
        let report = store
            .prune(
                &PrunePolicy {
                    max_age: Some(huge),
                    ..PrunePolicy::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(report.runs_pruned, 0);
    }

    #[tokio::test]
    async fn upgrades_a_v2_database_to_v3_without_data_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        // Create a current DB, seed a run, then simulate a pre-v3 database (drop the v3 table and
        // roll user_version back to 2).
        let store = SqliteStore::open(&path).unwrap();
        let run = run_state(RunStatus::Succeeded);
        let id = run.run_id;
        store.checkpoint(&run).await.unwrap();
        drop(store);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("DROP TABLE pruned_counts;").unwrap();
            conn.pragma_update(None, "user_version", 2_i64).unwrap();
        }
        // Reopen: the v2→v3 migration must re-create pruned_counts and keep the existing run.
        let store = SqliteStore::open(&path).unwrap();
        let v: i64 = {
            let conn = store.conn.lock().await;
            conn.query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(v, i64::try_from(super::MIGRATIONS.len()).unwrap());
        assert!(
            store.load_run(id).await.unwrap().is_some(),
            "the run survived the upgrade"
        );
        // pruned_counts exists again, so metrics + prune work (don't error on the missing table).
        assert_eq!(
            store
                .metrics()
                .await
                .unwrap()
                .runs
                .iter()
                .map(|r| r.count)
                .sum::<u64>(),
            1
        );
        store
            .prune(
                &PrunePolicy {
                    keep_last: Some(0),
                    ..PrunePolicy::default()
                },
                false,
            )
            .await
            .unwrap();
    }
}
