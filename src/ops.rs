use crate::backend::Backend;
use crate::model::{
    DesiredService, HostTarget, ObservedService, OnExisting, Project, RuntimeState, ServiceStatus,
};
use crate::reconcile::reconcile_host;
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::time::Duration;

const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpEvent {
    Started {
        host: String,
        service: String,
    },
    Skipped {
        host: String,
        service: String,
    },
    Restarted {
        host: String,
        service: String,
    },
    Orphan {
        host: String,
        service: String,
        runtime: RuntimeState,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownEvent {
    pub host: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceAction {
    Started { host: String, service: String },
    AlreadyRunning { host: String, service: String },
    Stopped { host: String, service: String },
    NotRunning { host: String, service: String },
    Restarted { host: String, service: String },
}

impl UpEvent {
    pub fn line(&self) -> String {
        match self {
            Self::Started { host, service } => format!("{host} {service} started"),
            Self::Skipped { host, service } => format!("{host} {service} skipped"),
            Self::Restarted { host, service } => format!("{host} {service} restarted"),
            Self::Orphan {
                host,
                service,
                runtime,
            } => {
                format!("{host} {service} orphan {}", runtime.as_str())
            }
        }
    }
}

impl ServiceAction {
    pub fn message(&self) -> String {
        match self {
            Self::Started { host, service } => format!("{host} {service} started"),
            Self::AlreadyRunning { host, service } => {
                format!("{host} {service} already running")
            }
            Self::Stopped { host, service } => format!("{host} {service} stopped"),
            Self::NotRunning { host, service } => format!("{host} {service} not running"),
            Self::Restarted { host, service } => format!("{host} {service} restarted"),
        }
    }
}

pub fn up(backend: &impl Backend, project: &Project) -> Result<Vec<UpEvent>> {
    let mut events = Vec::new();

    for host in project.hosts.values() {
        let desired = desired_for_host(project, &host.name);
        let observed = backend.list(host, &project.name)?;
        let observed_by_name = observed
            .iter()
            .map(|service| (service.name.as_str(), service.runtime))
            .collect::<BTreeMap<_, _>>();

        for service in desired.values().copied() {
            match observed_by_name.get(service.name.as_str()).copied() {
                None => {
                    backend.start(host, service)?;
                    events.push(UpEvent::Started {
                        host: host.name.clone(),
                        service: service.name.clone(),
                    });
                }
                Some(RuntimeState::Running) => match project.on_existing {
                    OnExisting::Skip => {
                        events.push(UpEvent::Skipped {
                            host: host.name.clone(),
                            service: service.name.clone(),
                        });
                    }
                    OnExisting::Restart => {
                        backend.stop_service(
                            host,
                            &project.name,
                            &service.name,
                            service.stop_timeout,
                        )?;
                        backend.start(host, service)?;
                        events.push(UpEvent::Restarted {
                            host: host.name.clone(),
                            service: service.name.clone(),
                        });
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
                    events.push(UpEvent::Started {
                        host: host.name.clone(),
                        service: service.name.clone(),
                    });
                }
            }
        }

        for observed in observed {
            if !desired.contains_key(observed.name.as_str()) {
                events.push(UpEvent::Orphan {
                    host: host.name.clone(),
                    service: observed.name,
                    runtime: observed.runtime,
                });
            }
        }
    }

    Ok(events)
}

pub fn restart(
    backend: &impl Backend,
    project: &Project,
) -> Result<(Vec<DownEvent>, Vec<UpEvent>)> {
    let down_events = down(backend, project)?;
    let up_events = up(backend, project)?;
    Ok((down_events, up_events))
}

pub fn down(backend: &impl Backend, project: &Project) -> Result<Vec<DownEvent>> {
    let mut events = Vec::new();

    for host in project.hosts.values() {
        backend.stop_project(host, &project.name, max_stop_timeout(project, &host.name))?;
        events.push(DownEvent {
            host: host.name.clone(),
        });
    }

    Ok(events)
}

pub fn status(backend: &impl Backend, project: &Project) -> Result<Vec<ServiceStatus>> {
    let mut statuses = Vec::new();
    // distrun is currently stateless: only hosts in the current config are queried.
    // If a host is removed from distrun.yml, leftover processes on that host are
    // undiscoverable until distrun grows a local state file or remote manifest.
    for host in project.hosts.values() {
        let desired = desired_for_host(project, &host.name);
        let observed = backend.list(host, &project.name)?;
        statuses.extend(reconcile_host(&host.name, desired.into_values(), observed));
    }

    Ok(statuses)
}

pub fn status_all<'a>(
    backend: &impl Backend,
    hosts: impl IntoIterator<Item = &'a HostTarget>,
) -> Result<Vec<ObservedService>> {
    let mut observed = Vec::new();
    for host in hosts {
        observed.extend(backend.list_all(host)?);
    }
    observed.sort_by(|left, right| {
        left.host
            .cmp(&right.host)
            .then_with(|| left.project.cmp(&right.project))
            .then_with(|| left.name.cmp(&right.name))
    });

    Ok(observed)
}

pub fn logs(
    backend: &impl Backend,
    project: &Project,
    service: &str,
    host_name: Option<&str>,
    tail: usize,
) -> Result<String> {
    let host = match host_name {
        Some(host_name) => host_for_name(project, host_name)?,
        None => host_for_service(project, service)?,
    };
    backend.logs(host, &project.name, service, tail)
}

pub fn logs_for_host(
    backend: &impl Backend,
    project: &Project,
    host_name: &str,
    service: &str,
    tail: usize,
) -> Result<String> {
    let host = host_for_name(project, host_name)?;
    backend.logs(host, &project.name, service, tail)
}

pub fn start_service(
    backend: &impl Backend,
    project: &Project,
    host_name: &str,
    service_name: &str,
) -> Result<ServiceAction> {
    let host = host_for_name(project, host_name)?;
    let service = desired_service_on_host(project, host_name, service_name)?;
    match observed_runtime(backend, host, project, service_name)? {
        None => {
            backend.start(host, service)?;
            Ok(ServiceAction::Started {
                host: host_name.to_owned(),
                service: service_name.to_owned(),
            })
        }
        Some(RuntimeState::Running) => Ok(ServiceAction::AlreadyRunning {
            host: host_name.to_owned(),
            service: service_name.to_owned(),
        }),
        Some(RuntimeState::Exited | RuntimeState::Unknown) => {
            backend.stop_service(host, &project.name, service_name, service.stop_timeout)?;
            backend.start(host, service)?;
            Ok(ServiceAction::Started {
                host: host_name.to_owned(),
                service: service_name.to_owned(),
            })
        }
    }
}

pub fn stop_service(
    backend: &impl Backend,
    project: &Project,
    host_name: &str,
    service_name: &str,
) -> Result<ServiceAction> {
    let host = host_for_name(project, host_name)?;
    if observed_runtime(backend, host, project, service_name)?.is_none() {
        return Ok(ServiceAction::NotRunning {
            host: host_name.to_owned(),
            service: service_name.to_owned(),
        });
    }

    backend.stop_service(
        host,
        &project.name,
        service_name,
        service_stop_timeout(project, host_name, service_name),
    )?;
    Ok(ServiceAction::Stopped {
        host: host_name.to_owned(),
        service: service_name.to_owned(),
    })
}

pub fn restart_service(
    backend: &impl Backend,
    project: &Project,
    host_name: &str,
    service_name: &str,
) -> Result<ServiceAction> {
    let host = host_for_name(project, host_name)?;
    let service = desired_service_on_host(project, host_name, service_name)?;
    backend.stop_service(host, &project.name, service_name, service.stop_timeout)?;
    backend.start(host, service)?;
    Ok(ServiceAction::Restarted {
        host: host_name.to_owned(),
        service: service_name.to_owned(),
    })
}

fn desired_for_host<'a>(
    project: &'a Project,
    host_name: &str,
) -> BTreeMap<&'a str, &'a DesiredService> {
    project
        .services
        .iter()
        .filter(|(_, service)| service.host == host_name)
        .map(|(name, service)| (name.as_str(), service))
        .collect()
}

fn host_for_name<'a>(project: &'a Project, host_name: &str) -> Result<&'a HostTarget> {
    project
        .hosts
        .get(host_name)
        .ok_or_else(|| anyhow::anyhow!("unknown host `{host_name}`"))
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

fn desired_service_on_host<'a>(
    project: &'a Project,
    host_name: &str,
    service_name: &str,
) -> Result<&'a DesiredService> {
    let service = project
        .services
        .get(service_name)
        .ok_or_else(|| anyhow::anyhow!("service `{service_name}` is not in config"))?;

    if service.host != host_name {
        bail!(
            "service `{service_name}` belongs to host `{}`, not `{host_name}`",
            service.host
        );
    }

    Ok(service)
}

fn observed_runtime(
    backend: &impl Backend,
    host: &HostTarget,
    project: &Project,
    service_name: &str,
) -> Result<Option<RuntimeState>> {
    Ok(backend
        .list(host, &project.name)?
        .into_iter()
        .find_map(|service| (service.name == service_name).then_some(service.runtime)))
}

fn service_stop_timeout(project: &Project, host_name: &str, service_name: &str) -> Duration {
    project
        .services
        .get(service_name)
        .filter(|service| service.host == host_name)
        .map(|service| service.stop_timeout)
        .unwrap_or(DEFAULT_STOP_TIMEOUT)
}

fn max_stop_timeout(project: &Project, host_name: &str) -> Duration {
    project
        .services
        .values()
        .filter(|service| service.host == host_name)
        .map(|service| service.stop_timeout)
        .max()
        .unwrap_or(DEFAULT_STOP_TIMEOUT)
}
