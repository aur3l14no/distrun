use super::project::{
    desired_for_host, host_for_name, max_stop_timeout, selected_configured_services,
    validate_desired_hosts,
};
use super::report::{OperationKind, OperationOutcome as Outcome, OperationReport};
use crate::backend::{Backend, StartResult};
use crate::model::{
    DesiredService, HostTarget, ObservedService, OnExisting, Project, RuntimeState,
};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

pub fn up_selected(
    backend: &impl Backend,
    project: &Project,
    service_names: &[String],
) -> Result<OperationReport> {
    let selected = selected_configured_services(project, service_names)?;
    validate_desired_hosts(project, selected.as_ref())?;
    let mut report = OperationReport::new();

    for host in project.hosts.values() {
        let configured = desired_for_host(project, &host.name);
        let desired = configured
            .iter()
            .map(|(name, service)| (*name, *service))
            .filter(|(name, _)| {
                selected
                    .as_ref()
                    .is_none_or(|selected| selected.contains(name))
            })
            .collect::<BTreeMap<_, _>>();
        if selected.is_some() && desired.is_empty() {
            continue;
        }

        let observed = match backend.list(host, &project.name) {
            Ok(observed) => observed,
            Err(error) => {
                report.failed(&host.name, None, OperationKind::Observe, error);
                continue;
            }
        };
        let mut observed_by_name = BTreeMap::<&str, Vec<&ObservedService>>::new();
        for service in &observed {
            observed_by_name
                .entry(service.name.as_str())
                .or_default()
                .push(service);
        }

        for service in desired.values().copied() {
            match observed_by_name
                .get(service.name.as_str())
                .map(Vec::as_slice)
            {
                None => match backend.start(host, service) {
                    Ok(StartResult::Started) => report.push(Outcome::Started {
                        host: host.name.clone(),
                        service: service.name.clone(),
                    }),
                    Ok(StartResult::AlreadyPresent) => report.push(Outcome::Skipped {
                        host: host.name.clone(),
                        service: service.name.clone(),
                    }),
                    Err(error) => {
                        report.failed(&host.name, Some(&service.name), OperationKind::Start, error)
                    }
                },
                Some([observed]) if observed.runtime == RuntimeState::Running => {
                    match project.on_existing {
                        OnExisting::Skip => report.push(Outcome::Skipped {
                            host: host.name.clone(),
                            service: service.name.clone(),
                        }),
                        OnExisting::Restart => {
                            restart_for_up(&mut report, backend, host, observed, service, true);
                        }
                    }
                }
                Some([observed]) => {
                    restart_for_up(&mut report, backend, host, observed, service, false);
                }
                Some(duplicates) => {
                    report.failed(
                        &host.name,
                        Some(&service.name),
                        OperationKind::Observe,
                        anyhow::anyhow!(
                            "found {} runtime instances; run `distrun stop {}/{}` or recreate the service to repair it",
                            duplicates.len(),
                            host.name,
                            service.name
                        ),
                    );
                }
            }
        }

        for observed in observed {
            if !configured.contains_key(observed.name.as_str()) {
                report.push(Outcome::Orphan {
                    host: host.name.clone(),
                    service: observed.name,
                    runtime: observed.runtime,
                });
            }
        }
    }

    Ok(report)
}

pub fn recreate(
    backend: &impl Backend,
    project: &Project,
    service_names: &[String],
) -> Result<OperationReport> {
    let selected = selected_configured_services(project, service_names)?;
    validate_desired_hosts(project, selected.as_ref())?;

    match selected {
        Some(selected) => recreate_selected(backend, project, &selected),
        None => Ok(recreate_all(backend, project)),
    }
}

pub fn down(backend: &impl Backend, project: &Project) -> Result<OperationReport> {
    validate_desired_hosts(project, None)?;
    let mut report = OperationReport::new();

    for host in project.hosts.values() {
        match backend.stop_project(host, &project.name, max_stop_timeout(project, &host.name)) {
            Ok(()) => report.push(Outcome::Stopped {
                host: host.name.clone(),
                service: None,
            }),
            Err(error) => report.failed(&host.name, None, OperationKind::Stop, error),
        }
    }

    Ok(report)
}

fn restart_for_up(
    report: &mut OperationReport,
    backend: &impl Backend,
    host: &HostTarget,
    observed: &ObservedService,
    service: &DesiredService,
    restarted: bool,
) {
    if let Err(error) = backend.stop_service(host, observed, service.stop_timeout) {
        report.failed(&host.name, Some(&service.name), OperationKind::Stop, error);
        return;
    }

    match backend.start(host, service) {
        Ok(StartResult::Started) => report.push(if restarted {
            Outcome::Restarted {
                host: host.name.clone(),
                service: service.name.clone(),
            }
        } else {
            Outcome::Started {
                host: host.name.clone(),
                service: service.name.clone(),
            }
        }),
        Ok(StartResult::AlreadyPresent) => report.push(Outcome::Skipped {
            host: host.name.clone(),
            service: service.name.clone(),
        }),
        Err(error) => {
            report.push(Outcome::Stopped {
                host: host.name.clone(),
                service: Some(service.name.clone()),
            });
            report.failed(&host.name, Some(&service.name), OperationKind::Start, error);
        }
    }
}

fn recreate_selected(
    backend: &impl Backend,
    project: &Project,
    selected: &BTreeSet<&str>,
) -> Result<OperationReport> {
    let mut services_by_host = BTreeMap::<&str, Vec<&DesiredService>>::new();
    for service in project
        .services
        .values()
        .filter(|service| selected.contains(service.name.as_str()))
    {
        let host = host_for_name(project, &service.host)?;
        services_by_host
            .entry(host.name.as_str())
            .or_default()
            .push(service);
    }

    let mut report = OperationReport::new();
    for (host_name, services) in services_by_host {
        let host = project
            .hosts
            .get(host_name)
            .expect("selected service hosts were validated");
        let observed = match backend.list(host, &project.name) {
            Ok(observed) => observed,
            Err(error) => {
                report.failed(&host.name, None, OperationKind::Observe, error);
                continue;
            }
        };

        for service in services {
            let runtimes = observed
                .iter()
                .filter(|runtime| runtime.name == service.name)
                .collect::<Vec<_>>();
            let mut stopped = 0;
            let mut stop_failed = false;
            for runtime in runtimes {
                match backend.stop_service(host, runtime, service.stop_timeout) {
                    Ok(()) => stopped += 1,
                    Err(error) => {
                        stop_failed = true;
                        report.failed(&host.name, Some(&service.name), OperationKind::Stop, error);
                    }
                }
            }

            if stop_failed {
                record_stopped_recreate_events(&mut report, host, service, stopped);
                continue;
            }

            match backend.start(host, service) {
                Ok(StartResult::Started) => {
                    let outcome = if stopped == 0 {
                        Outcome::Started {
                            host: host.name.clone(),
                            service: service.name.clone(),
                        }
                    } else {
                        Outcome::Recreated {
                            host: host.name.clone(),
                            service: service.name.clone(),
                        }
                    };
                    report.push(outcome);
                }
                Ok(StartResult::AlreadyPresent) => {
                    record_stopped_recreate_events(&mut report, host, service, stopped);
                    report.push(Outcome::Skipped {
                        host: host.name.clone(),
                        service: service.name.clone(),
                    });
                }
                Err(error) => {
                    record_stopped_recreate_events(&mut report, host, service, stopped);
                    report.failed(&host.name, Some(&service.name), OperationKind::Start, error);
                }
            }
        }
    }
    Ok(report)
}

fn record_stopped_recreate_events(
    report: &mut OperationReport,
    host: &HostTarget,
    service: &DesiredService,
    count: usize,
) {
    for _ in 0..count {
        report.push(Outcome::Stopped {
            host: host.name.clone(),
            service: Some(service.name.clone()),
        });
    }
}

fn recreate_all(backend: &impl Backend, project: &Project) -> OperationReport {
    let mut report = OperationReport::new();

    for host in project.hosts.values() {
        if let Err(error) =
            backend.stop_project(host, &project.name, max_stop_timeout(project, &host.name))
        {
            report.failed(&host.name, None, OperationKind::Stop, error);
            continue;
        }
        report.push(Outcome::Stopped {
            host: host.name.clone(),
            service: None,
        });

        for service in desired_for_host(project, &host.name).into_values() {
            match backend.start(host, service) {
                Ok(StartResult::Started) => report.push(Outcome::Started {
                    host: host.name.clone(),
                    service: service.name.clone(),
                }),
                Ok(StartResult::AlreadyPresent) => {
                    report.push(Outcome::Skipped {
                        host: host.name.clone(),
                        service: service.name.clone(),
                    });
                }
                Err(error) => {
                    report.failed(&host.name, Some(&service.name), OperationKind::Start, error);
                }
            }
        }
    }

    report
}
