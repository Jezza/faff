//! faff entry point: parse CLI, dispatch to the TUI or the internal report-event hook.

use clap::Parser;
use faff::cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Tui { repo: None }) {
        Command::ReportEvent {
            task,
            event,
            socket,
            db,
        } => faff::cli::report_event(task, &event, &socket, &db),
        // Best-effort like report-event: a hook must never fail the agent.
        Command::SyncMemoryIndex { dir } => {
            if let Err(e) = faff::memory::sync_index(&dir) {
                eprintln!("sync-memory-index: {e:#}");
            }
            Ok(())
        }
        Command::Tui { repo } => faff::tui::run(repo),
    }
}
