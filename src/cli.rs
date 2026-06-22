use crate::backend::Backend;
use crate::config;
use crate::executor::SystemExecutor;
use crate::model::{DesiredService, HostTarget, OnExisting, Project, RuntimeState, ServiceStatus};
use crate::reconcile::reconcile_host;
use crate::tmux::TmuxBackend;
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

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
    Status {
        project: Option<String>,
    },
    Logs {
        service: String,
        #[arg(long)]
        host: Option<String>,
        #[arg(long, default_value_t = 80)]
        tail: usize,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let positional_project = match &cli.command {
        Command::Up { project } | Command::Down { project } | Command::Status { project } => {
            project.as_deref()
        }
        Command::Logs { .. } => None,
    };
    let project_override = merge_project_overrides(cli.project.as_deref(), positional_project)?;
    let project = config::load(&cli.file, project_override)?;
    let backend = TmuxBackend::new(SystemExecutor);

    match cli.command {
        Command::Up { .. } => up(&backend, &project),
        Command::Down { .. } => down(&backend, &project),
        Command::Status { .. } => status(&backend, &project),
        Command::Logs {
            service,
            host,
            tail,
        } => logs(&backend, &project, &service, host.as_deref(), tail),
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

fn up(backend: &impl Backend, project: &Project) -> Result<()> {
    for host in project.hosts.values() {
        let desired = desired_for_host(project, &host.name);
        let observed = backend.list(host, &project.name)?;
        let observed_by_name = observed
            .iter()
            .map(|service| (service.name.clone(), service.runtime))
            .collect::<BTreeMap<_, _>>();

        for service in desired.values() {
            match observed_by_name.get(&service.name).copied() {
                None => {
                    backend.start(host, service)?;
                    println!("{} {} started", host.name, service.name);
                }
                Some(RuntimeState::Running) => match project.on_existing {
                    OnExisting::Skip => {
                        println!("{} {} skipped", host.name, service.name);
                    }
                    OnExisting::Restart => {
                        backend.stop_service(
                            host,
                            &project.name,
                            &service.name,
                            service.stop_timeout,
                        )?;
                        backend.start(host, service)?;
                        println!("{} {} restarted", host.name, service.name);
                    }
                },
                Some(RuntimeState::Exited | RuntimeState::Unknown) => {
                    backend.stop_service(
                        host,
                        &project.name,
                        &service.name,
                        service.stop_timeout,
                    )?;
                    backend.start(host, service)?;
                    println!("{} {} started", host.name, service.name);
                }
            }
        }

        for observed in observed {
            if !desired.contains_key(&observed.name) {
                println!(
                    "{} {} orphan {}",
                    host.name,
                    observed.name,
                    observed.runtime.as_str()
                );
            }
        }
    }

    Ok(())
}

fn down(backend: &impl Backend, project: &Project) -> Result<()> {
    for host in project.hosts.values() {
        backend.stop_project(host, &project.name, max_stop_timeout(project, &host.name))?;
        println!("{} stopped", host.name);
    }
    Ok(())
}

fn status(backend: &impl Backend, project: &Project) -> Result<()> {
    let mut statuses = Vec::new();
    // v1 is intentionally stateless: only hosts in the current config are queried.
    // If a host is removed from distrun.yml, leftover processes on that host are
    // undiscoverable until distrun grows a local state file or remote manifest.
    for host in project.hosts.values() {
        let desired = desired_for_host(project, &host.name);
        let observed = backend.list(host, &project.name)?;
        statuses.extend(reconcile_host(&host.name, desired.into_values(), observed));
    }

    print_statuses(&statuses);
    Ok(())
}

fn logs(
    backend: &impl Backend,
    project: &Project,
    service: &str,
    host_name: Option<&str>,
    tail: usize,
) -> Result<()> {
    let host = match host_name {
        Some(host_name) => project
            .hosts
            .get(host_name)
            .ok_or_else(|| anyhow::anyhow!("unknown host `{host_name}`"))?,
        None => host_for_service(project, service)?,
    };
    let logs = backend.logs(host, &project.name, service, tail)?;
    print!("{logs}");
    Ok(())
}

fn desired_for_host(project: &Project, host_name: &str) -> BTreeMap<String, DesiredService> {
    project
        .services
        .iter()
        .filter(|(_, service)| service.host == host_name)
        .map(|(name, service)| (name.clone(), service.clone()))
        .collect()
}

fn host_for_service<'a>(project: &'a Project, service_name: &str) -> Result<&'a HostTarget> {
    let service = project.services.get(service_name).ok_or_else(|| {
        anyhow::anyhow!("service `{service_name}` is not in config; pass --host for orphan logs")
    })?;
    project.hosts.get(&service.host).ok_or_else(|| {
        anyhow::anyhow!(
            "service `{service_name}` references unknown host `{}`",
            service.host
        )
    })
}

fn max_stop_timeout(project: &Project, host_name: &str) -> Duration {
    project
        .services
        .values()
        .filter(|service| service.host == host_name)
        .map(|service| service.stop_timeout)
        .max()
        .unwrap_or(Duration::from_secs(10))
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
