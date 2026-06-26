use crate::config;
use crate::executor::SystemExecutor;
use crate::model::{
    HostTarget, HostTransport, LOCAL_HOST_NAME, ObservedService, Project, RuntimeState,
    ServiceStatus,
};
use crate::ops;
use crate::tmux::TmuxBackend;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_CONFIG_FILE: &str = "distrun.yml";

#[derive(Debug, Parser)]
#[command(author, version, about = "Run processes anywhere, with SSH + tmux.")]
struct Cli {
    #[arg(short, long, global = true, value_name = "FILE")]
    file: Option<PathBuf>,

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
        #[arg(
            long,
            default_value = "5s",
            value_parser = parse_cli_duration,
            help = "Per-host status observation timeout."
        )]
        timeout: Duration,
        project: Option<String>,
    },
    #[command(about = "List all distrun-managed sessions on configured hosts.")]
    Ps {
        #[arg(long = "host", value_name = "HOST")]
        hosts: Vec<String>,
        #[arg(
            long,
            default_value = "5s",
            value_parser = parse_cli_duration,
            help = "Per-host status observation timeout."
        )]
        timeout: Duration,
    },
    Logs {
        service: String,
        project: Option<String>,
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

pub fn run() -> Result<()> {
    let Cli { file, command } = Cli::parse();
    let file = file.as_deref();
    let backend = TmuxBackend::new(SystemExecutor);

    match command {
        Command::Up { project } => {
            let project = load_required_project(file, project.as_deref())?;
            up(&backend, &project)
        }
        Command::Down { project } => {
            let project = load_project_or_local(file, project.as_deref())?;
            down(&backend, &project)
        }
        Command::Restart { project } => {
            let project = load_required_project(file, project.as_deref())?;
            restart(&backend, &project)
        }
        Command::Status { timeout, project } => {
            let project = load_project_or_local(file, project.as_deref())?;
            status(&backend, &project, timeout)
        }
        Command::Ps { hosts, timeout } => {
            let hosts = load_hosts_or_local(file, hosts)?;
            status_all(&backend, &hosts, timeout)
        }
        Command::Logs {
            service,
            project,
            host,
            tail,
        } => {
            let (project, host) = load_logs_project(file, project.as_deref(), host)?;
            logs(&backend, &project, &service, host.as_deref(), tail)
        }
        Command::Tui { tail, project } => {
            let project = load_project_or_local(file, project.as_deref())?;
            crate::tui::run(project, tail)
        }
    }
}

fn load_required_project(file: Option<&Path>, project: Option<&str>) -> Result<Project> {
    config::load(required_config_file(file), project)
}

fn load_project_or_local(file: Option<&Path>, project: Option<&str>) -> Result<Project> {
    let Some(file) = discovered_config_file(file) else {
        return config::local_project(require_project(project)?);
    };
    config::load(file, project)
}

fn load_logs_project(
    file: Option<&Path>,
    project: Option<&str>,
    host: Option<String>,
) -> Result<(Project, Option<String>)> {
    let Some(file) = discovered_config_file(file) else {
        let host = host.unwrap_or_else(|| LOCAL_HOST_NAME.to_owned());
        let project = config::empty_project(
            require_project(project)?,
            [host_from_cli_value(host.clone())?],
        )?;
        return Ok((project, Some(host)));
    };
    Ok((config::load(file, project)?, host))
}

fn load_hosts_or_local(file: Option<&Path>, hosts: Vec<String>) -> Result<Vec<HostTarget>> {
    if !hosts.is_empty() {
        return hosts.into_iter().map(host_from_cli_value).collect();
    }

    let Some(file) = discovered_config_file(file) else {
        return Ok(vec![config::local_host()]);
    };
    config::load_hosts(file).map(|hosts| hosts.into_values().collect())
}

fn required_config_file<'a>(file: Option<&'a Path>) -> &'a Path {
    file.unwrap_or_else(|| default_config_file())
}

fn discovered_config_file<'a>(file: Option<&'a Path>) -> Option<&'a Path> {
    file.or_else(|| {
        default_config_file()
            .exists()
            .then_some(default_config_file())
    })
}

fn default_config_file() -> &'static Path {
    Path::new(DEFAULT_CONFIG_FILE)
}

fn require_project(project: Option<&str>) -> Result<&str> {
    project.context("missing project name; pass PROJECT or create distrun.yml")
}

fn host_from_cli_value(host: String) -> Result<HostTarget> {
    if host.is_empty() {
        bail!("--host value cannot be empty");
    }

    let transport = if host == LOCAL_HOST_NAME {
        HostTransport::Local
    } else {
        HostTransport::Ssh(host.clone())
    };

    Ok(HostTarget {
        name: host,
        transport,
    })
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

fn status(
    backend: &TmuxBackend<SystemExecutor>,
    project: &Project,
    timeout: Duration,
) -> Result<()> {
    let report = ops::status(backend, project, timeout)?;
    print_statuses(&report.statuses);
    print_unavailable_hosts(&report.unavailable_hosts);
    Ok(())
}

fn status_all<'a>(
    backend: &TmuxBackend<SystemExecutor>,
    hosts: impl IntoIterator<Item = &'a HostTarget>,
    timeout: Duration,
) -> Result<()> {
    let report = ops::status_all_with_timeout(backend, hosts, timeout)?;
    print_all_statuses(&report.observed);
    print_unavailable_hosts(&report.unavailable_hosts);
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

fn print_unavailable_hosts(hosts: &[ops::UnavailableHost]) {
    for host in hosts {
        eprintln!(
            "warning: {} unavailable: {}",
            host.host,
            host.message.trim()
        );
    }
}

fn parse_cli_duration(value: &str) -> std::result::Result<Duration, String> {
    let duration = if let Some(milliseconds) = value.strip_suffix("ms") {
        milliseconds
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| format!("invalid duration `{value}`"))?
    } else if let Some(seconds) = value.strip_suffix('s') {
        seconds
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("invalid duration `{value}`"))?
    } else {
        value
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("invalid duration `{value}`; use seconds like `5s`"))?
    };

    if duration.is_zero() {
        return Err("duration must be greater than zero".to_owned());
    }

    Ok(duration)
}
