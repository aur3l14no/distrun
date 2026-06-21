mod backend;
mod cli;
mod config;
mod executor;
mod model;
mod reconcile;
mod tmux;

pub fn run() -> anyhow::Result<()> {
    cli::run()
}
