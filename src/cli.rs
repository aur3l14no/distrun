use crate::config;
use crate::executor::SystemExecutor;
use crate::model::{
    HostTarget, HostTransport, LOCAL_HOST_NAME, ObservedService, Project, RuntimeState,
    ServiceStatus,
};
use crate::ops;
use crate::tmux::TmuxBackend;
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(author, version, about = "Run processes anywhere, with SSH + tmux.")]
struct Cli {
    #[arg(short, long, global = true, default_value = "distrun.yml")]
    file: PathBuf,

    #[arg(short, long, global = true)]
    project: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Up {
        project: Option<String>,
    },
    Down {
        project: Option<String>,
    },
    Restart {
        project: Option<String>,
    },
    Status {
        #[arg(long)]
        all: bool,
        #[arg(long = "host", value_name = "HOST")]
        hosts: Vec<String>,
        project: Option<String>,
    },
    Logs {
        service: String,
        #[arg(long)]
        host: Option<String>,
        #[arg(long, default_value_t = 80)]
        tail: usize,
    },
    #[command(about = "Open the interactive service dashboard.")]
    Tui {
        #[arg(
            long,
            default_value_t = 80,
            help = "Log lines to fetch for the selected service."
        )]
        tail: usize,
        project: Option<String>,
    },
}

impl Command {
    fn project_filter(&self) -> Option<&str> {
        match self {
            Self::Up { project }
            | Self::Down { project }
            | Self::Restart { project }
            | Self::Status { project, .. }
            | Self::Tui { project, .. } => project.as_deref(),
            Self::Logs { .. } => None,
        }
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let backend = TmuxBackend::new(SystemExecutor);

    if let Command::Status {
        all: true,
        hosts,
        project,
    } = &cli.command
    {
        reject_status_all_project(cli.project.as_deref(), project.as_deref())?;
        if !hosts.is_empty() {
            let hosts = manual_hosts(hosts)?;
            return status_all(&backend, &hosts);
        }
    }

    if let Command::Status {
        all: false, hosts, ..
    } = &cli.command
        && !hosts.is_empty()
    {
        bail!("--host can only be used with status --all");
    }

    let project_override =
        merge_project_overrides(cli.project.as_deref(), cli.command.project_filter())?;
    let project = config::load(&cli.file, project_override)?;

    match cli.command {
        Command::Up { .. } => up(&backend, &project),
        Command::Down { .. } => down(&backend, &project),
        Command::Restart { .. } => restart(&backend, &project),
        Command::Status { all, .. } => {
            if all {
                status_all(&backend, project.hosts.values())
            } else {
                status(&backend, &project)
            }
        }
        Command::Logs {
            service,
            host,
            tail,
        } => logs(&backend, &project, &service, host.as_deref(), tail),
        Command::Tui { tail, .. } => crate::tui::run(project, tail),
    }
}

fn merge_project_overrides<'a>(
    global: Option<&'a str>,
    positional: Option<&'a str>,
) -> Result<Option<&'a str>> {
    match (global, positional) {
        (Some(left), Some(right)) if left != right => {
            bail!("project specified twice with different values: `{left}` and `{right}`")
        }
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn reject_status_all_project(global: Option<&str>, positional: Option<&str>) -> Result<()> {
    if global.is_some() || positional.is_some() {
        bail!("status --all cannot be used with a project filter");
    }
    Ok(())
}

fn up(backend: &TmuxBackend<SystemExecutor>, project: &Project) -> Result<()> {
    for event in ops::up(backend, project)? {
        println!("{}", event.line());
    }
    Ok(())
}

fn restart(backend: &TmuxBackend<SystemExecutor>, project: &Project) -> Result<()> {
    let (down_events, up_events) = ops::restart(backend, project)?;
    for event in down_events {
        println!("{} stopped", event.host);
    }
    for event in up_events {
        println!("{}", event.line());
    }
    Ok(())
}

fn down(backend: &TmuxBackend<SystemExecutor>, project: &Project) -> Result<()> {
    for event in ops::down(backend, project)? {
        println!("{} stopped", event.host);
    }
    Ok(())
}

fn status(backend: &TmuxBackend<SystemExecutor>, project: &Project) -> Result<()> {
    let statuses = ops::status(backend, project)?;
    print_statuses(&statuses);
    Ok(())
}

fn status_all<'a>(
    backend: &TmuxBackend<SystemExecutor>,
    hosts: impl IntoIterator<Item = &'a HostTarget>,
) -> Result<()> {
    let observed = ops::status_all(backend, hosts)?;
    print_all_statuses(&observed);
    Ok(())
}

fn logs(
    backend: &TmuxBackend<SystemExecutor>,
    project: &Project,
    service: &str,
    host_name: Option<&str>,
    tail: usize,
) -> Result<()> {
    let logs = ops::logs(backend, project, service, host_name, tail)?;
    print!("{logs}");
    Ok(())
}

fn manual_hosts(hosts: &[String]) -> Result<Vec<HostTarget>> {
    hosts.iter().map(|host| manual_host(host)).collect()
}

fn manual_host(host: &str) -> Result<HostTarget> {
    if host.is_empty() {
        bail!("--host value cannot be empty");
    }

    let transport = if host == LOCAL_HOST_NAME {
        HostTransport::Local
    } else {
        HostTransport::Ssh(host.to_owned())
    };

    Ok(HostTarget {
        name: host.to_owned(),
        transport,
    })
}

fn print_statuses(statuses: &[ServiceStatus]) {
    println!("{:<16} {:<24} {:<10} SPEC", "HOST", "SERVICE", "RUNTIME");
    for status in statuses {
        let runtime = status.runtime.map(RuntimeState::as_str).unwrap_or("-");
        println!(
            "{:<16} {:<24} {:<10} {}",
            status.host,
            status.service,
            runtime,
            status.spec.as_str()
        );
    }
}

fn print_all_statuses(statuses: &[ObservedService]) {
    println!("{:<16} {:<24} {:<24} RUNTIME", "HOST", "PROJECT", "SERVICE");
    for status in statuses {
        println!(
            "{:<16} {:<24} {:<24} {}",
            status.host,
            status.project,
            status.name,
            status.runtime.as_str()
        );
    }
}
