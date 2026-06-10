//! `odin cancel`: request cancellation of an in-flight run from another process.
//!
//! The run may be executing inside a separate `odind` daemon, whose in-memory cancel tokens this
//! process can't reach — so cancellation is signalled through the shared run-state store. The
//! engine running the run polls for the signal and stops it (terminally `Cancelled`). Only durable
//! runs (which have a store row) can be cancelled this way.

use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr as _;

use anyhow::Context as _;
use odin_core::{RunId, SqliteStore, Store};
use tokio::runtime::Runtime;

/// Arguments for `odin cancel`.
pub(crate) struct CancelArgs {
    pub run_id: String,
    pub repo: Option<PathBuf>,
    pub db: Option<PathBuf>,
    /// Emit `{"run_id":…,"requested":bool}` on stdout instead of the human line.
    pub json: bool,
}

/// Records a cross-process cancel request for the run.
pub(crate) fn cancel(args: CancelArgs) -> anyhow::Result<ExitCode> {
    let run_id = RunId::from_str(&args.run_id)
        .map_err(|_| anyhow::anyhow!("invalid run id {:?}", args.run_id))?;
    let repo = args.repo.unwrap_or_else(|| PathBuf::from("."));
    let db = args
        .db
        .unwrap_or_else(|| repo.join(".odin").join("state.db"));
    let store = SqliteStore::open(&db).context("opening the run state database")?;
    let runtime = Runtime::new().context("starting the async runtime")?;

    let requested = runtime.block_on(store.request_cancel(run_id))?;
    if args.json {
        println!(
            "{}",
            serde_json::json!({ "run_id": run_id.to_string(), "requested": requested })
        );
        return Ok(if requested {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(2)
        });
    }
    if requested {
        println!(
            "⏹ cancel requested for run {run_id}; if it is running it will stop within a few \
             seconds (terminally cancelled)"
        );
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("✗ no cancellable (non-terminal) run {run_id} found in the store");
        Ok(ExitCode::from(2))
    }
}
