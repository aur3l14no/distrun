use super::project::desired_for_host;
use super::report::{StatusAllReport, StatusReport, UnavailableHost};
use crate::backend::Backend;
use crate::model::{
    ConfigRelation, DesiredService, HostTarget, ObservedService, Project, RuntimeStatus,
    ServiceStatus,
};
use crate::reconcile::reconcile_host;
use anyhow::Result;
use std::thread;
use std::time::Duration;

pub(super) struct HostObservation {
    pub(super) host: String,
    pub(super) result: Result<Vec<ObservedService>, String>,
}

pub fn status(
    backend: &(impl Backend + Sync),
    project: &Project,
    timeout: Duration,
) -> StatusReport {
    let mut statuses = Vec::new();
    let mut unavailable_hosts = Vec::new();
    // distrun is currently stateless: only hosts in the current config are queried.
    // If a host is removed from distrun.yml, leftover processes on that host are
    // undiscoverable until distrun grows a local state file or remote manifest.
    for observation in observe_project_hosts(backend, project, timeout) {
        let desired = desired_for_host(project, &observation.host);
        match observation.result {
            Ok(observed) => statuses.extend(reconcile_host(
                &observation.host,
                desired.into_values(),
                observed,
            )),
            Err(error) => {
                statuses.extend(unavailable_statuses(
                    &observation.host,
                    desired.into_values(),
                ));
                unavailable_hosts.push(UnavailableHost {
                    host: observation.host,
                    message: error,
                });
            }
        }
    }

    StatusReport {
        statuses,
        unavailable_hosts,
    }
}

pub fn status_all_with_timeout<'a>(
    backend: &(impl Backend + Sync),
    hosts: impl IntoIterator<Item = &'a HostTarget>,
    timeout: Duration,
) -> StatusAllReport {
    let mut observed = Vec::new();
    let mut unavailable_hosts = Vec::new();
    for observation in observe_hosts(hosts, |host| backend.list_all_with_timeout(host, timeout)) {
        match observation.result {
            Ok(services) => observed.extend(services),
            Err(error) => unavailable_hosts.push(UnavailableHost {
                host: observation.host,
                message: error,
            }),
        }
    }
    observed.sort_by(|left, right| {
        left.host
            .cmp(&right.host)
            .then_with(|| left.project.cmp(&right.project))
            .then_with(|| left.name.cmp(&right.name))
    });

    StatusAllReport {
        observed,
        unavailable_hosts,
    }
}

pub(super) fn observe_hosts<'a>(
    hosts: impl IntoIterator<Item = &'a HostTarget>,
    observe: impl Fn(&HostTarget) -> Result<Vec<ObservedService>> + Sync,
) -> Vec<HostObservation> {
    let hosts = hosts.into_iter().collect::<Vec<_>>();

    thread::scope(|scope| {
        hosts
            .iter()
            .map(|host| {
                let observe = &observe;
                scope.spawn(move || HostObservation {
                    host: host.name.clone(),
                    result: observe(host).map_err(|error| format!("{error:#}")),
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(observation) => observation,
                Err(payload) => std::panic::resume_unwind(payload),
            })
            .collect()
    })
}

fn observe_project_hosts(
    backend: &(impl Backend + Sync),
    project: &Project,
    timeout: Duration,
) -> Vec<HostObservation> {
    observe_hosts(project.hosts.values(), |host| {
        backend.list_with_timeout(host, &project.name, timeout)
    })
}

fn unavailable_statuses<'a>(
    host: &str,
    desired: impl IntoIterator<Item = &'a DesiredService>,
) -> Vec<ServiceStatus> {
    desired
        .into_iter()
        .map(|service| ServiceStatus {
            host: host.to_owned(),
            service: service.name.clone(),
            runtime: RuntimeStatus::Unavailable,
            relation: ConfigRelation::Configured,
            issue: None,
        })
        .collect()
}
