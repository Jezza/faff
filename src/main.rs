//! faf entry point: parse CLI, dispatch to the TUI or the internal report-event hook.

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
        Command::Tui { repo } => faff::tui::run(repo),
    }
}
