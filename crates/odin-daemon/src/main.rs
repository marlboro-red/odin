//! `odind` — the Odin daemon.
//!
//! Placeholder entry point. The daemon hosts long-lived [`Trigger`]s (GitHub
//! webhooks, cron) and dispatches runs into the engine; it is implemented in a
//! post-MVP milestone.
//!
//! [`Trigger`]: odin_core

fn main() {
    println!("odind {} (not yet implemented)", odin_core::version());
}
