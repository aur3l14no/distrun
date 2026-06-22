mod backend;
mod cli;
mod config;
mod executor;
mod model;
mod ops;
mod reconcile;
mod tmux;
mod tui;

pub fn run() -> anyhow::Result<()> {
    cli::run()
}
