//! A single-file SQLite [`Store`] for durable, crash-resumable run state.
//!
//! Each run is one row in `runs` holding the whole [`RunState`] as a JSON blob plus a
//! denormalized `status` column for cheap incomplete-run queries; the append-only audit
//! log lives in `events`. rusqlite is synchronous, so each async method briefly locks the
//! connection and runs the (sub-millisecond, local) query inline.

use std::path::Path;

use async_trait::async_trait;
use rusqlite::{Connection, params};
use tokio::sync::Mutex;

use crate::api::RunStatus;
use crate::error::StoreError;
use crate::ids::RunId;
use crate::traits::{RunEvent, RunState, Store};

const SCHEMA: &str = "
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
        if wal {
            let _: String = db(conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0)))?;
        }
        db(conn.execute_batch(SCHEMA))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

/// Maps a rusqlite error into a [`StoreError`].
fn db<T>(r: rusqlite::Result<T>) -> Result<T, StoreError> {
    r.map_err(|e| StoreError::Backend(e.to_string()))
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
        // Derive the terminal strings via status_str so the filter can't drift from the
        // serde representation of RunStatus. (Keep this list in sync with is_terminal.)
        let terminal = [
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ]
        .map(status_str);
        let conn = self.conn.lock().await;
        let mut stmt = db(conn.prepare(
            "SELECT state FROM runs WHERE status NOT IN (?1, ?2, ?3) ORDER BY updated_at",
        ))?;
        let rows = db(
            stmt.query_map(params![terminal[0], terminal[1], terminal[2]], |row| {
                row.get::<_, String>(0)
            }),
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&db(row)?)?);
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
            out.push(serde_json::from_str(&db(row)?)?);
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

    async fn events(&self, run_id: RunId) -> Result<Vec<RunEvent>, StoreError> {
        let conn = self.conn.lock().await;
        let run_id = run_id.to_string();
        let mut stmt = db(conn.prepare("SELECT event FROM events WHERE run_id = ?1 ORDER BY seq"))?;
        let rows = db(stmt.query_map(params![run_id], |row| row.get::<_, String>(0)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&db(row)?)?);
        }
        Ok(out)
    }
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
            input: RunInput::manual(),
            workspace: None,
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
}
