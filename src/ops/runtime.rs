use super::observe::observe_hosts;
use super::project::{host_for_name, service_stop_timeout};
use super::report::{OperationKind, OperationOutcome as Outcome, OperationReport};
use crate::backend::Backend;
use crate::model::{HostTarget, ObservedService, Project};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

pub(super) fn resolve_runtime_services_with_timeout(
    backend: &(impl Backend + Sync),
    project: &Project,
    selectors: &[String],
    timeout: Duration,
) -> Result<Vec<ObservedService>> {
    if selectors.is_empty() {
        return Ok(Vec::new());
    }

    let selection = observe_runtime_selection(backend, project, selectors, timeout)?;
    if let Some((host, error)) = selection.unavailable.first_key_value() {
        bail!("cannot resolve runtime services because host `{host}` is unavailable: {error}");
    }

    let mut resolved = Vec::new();
    for selector in selection.selectors {
        for service in resolve_runtime_selector(&selection.observed, selector)? {
            if !resolved.contains(service) {
                resolved.push(service.clone());
            }
        }
    }
    Ok(resolved)
}

pub fn stop_runtime_services_with_timeout(
    backend: &(impl Backend + Sync),
    project: &Project,
    selectors: &[String],
    timeout: Duration,
) -> Result<OperationReport> {
    // Resolve the full selection first so an invalid later selector cannot leave
    // the requested operation half-applied.
    let plan = resolve_runtime_mutation_plan(backend, project, selectors, timeout)?;
    for item in &plan {
        if let RuntimeMutationPlanItem::Service(service) = item {
            host_for_name(project, &service.host)?;
        }
    }

    let mut report = OperationReport::new();
    for item in plan {
        match item {
            RuntimeMutationPlanItem::Unavailable {
                host,
                service,
                message,
            } => {
                report.failed(&host, Some(&service), OperationKind::Observe, message);
            }
            RuntimeMutationPlanItem::Service(service) => {
                let host = project
                    .hosts
                    .get(&service.host)
                    .expect("runtime service hosts were validated");
                let timeout = service_stop_timeout(project, &service.host, &service.name);
                match backend.stop_service(host, &service, timeout) {
                    Ok(()) => report.push(Outcome::Stopped {
                        host: service.host,
                        service: Some(service.name),
                    }),
                    Err(error) => report.failed(
                        &service.host,
                        Some(&service.name),
                        OperationKind::Stop,
                        error,
                    ),
                }
            }
        }
    }
    Ok(report)
}

pub fn logs_for_runtime_service_with_timeout(
    backend: &(impl Backend + Sync),
    project: &Project,
    selector: &str,
    tail: usize,
    timeout: Duration,
) -> Result<String> {
    let service =
        resolve_runtime_service_for_logs_with_timeout(backend, project, selector, timeout)?;
    let host = host_for_name(project, &service.host)?;
    backend.logs_with_timeout(host, &service, tail, timeout)
}

pub fn resolve_runtime_service_for_logs_with_timeout(
    backend: &(impl Backend + Sync),
    project: &Project,
    selector: &str,
    timeout: Duration,
) -> Result<ObservedService> {
    let services =
        resolve_runtime_services_with_timeout(backend, project, &[selector.to_owned()], timeout)?;
    let [service] = services.as_slice() else {
        bail!(
            "runtime service `{selector}` has {} instances; logs requires exactly one instance",
            services.len()
        );
    };
    Ok(service.clone())
}

enum RuntimeMutationPlanItem {
    Service(ObservedService),
    Unavailable {
        host: String,
        service: String,
        message: String,
    },
}

struct RuntimeSelection<'a> {
    selectors: Vec<RuntimeSelector<'a>>,
    observed: Vec<ObservedService>,
    unavailable: BTreeMap<String, String>,
}

fn observe_runtime_selection<'a>(
    backend: &(impl Backend + Sync),
    project: &Project,
    selectors: &'a [String],
    timeout: Duration,
) -> Result<RuntimeSelection<'a>> {
    let selectors = selectors
        .iter()
        .map(|selector| parse_runtime_selector(selector))
        .collect::<Result<Vec<_>>>()?;
    let hosts = runtime_selector_hosts(project, &selectors)?;
    let mut observed = Vec::new();
    let mut unavailable = BTreeMap::new();

    for observation in observe_hosts(hosts, |host| {
        backend.list_with_timeout(host, &project.name, timeout)
    }) {
        match observation.result {
            Ok(services) => observed.extend(services),
            Err(message) => {
                unavailable.insert(observation.host, message);
            }
        }
    }
    observed.sort_by(|left, right| {
        left.host
            .cmp(&right.host)
            .then_with(|| left.name.cmp(&right.name))
    });

    Ok(RuntimeSelection {
        selectors,
        observed,
        unavailable,
    })
}

fn runtime_selector_hosts<'a>(
    project: &'a Project,
    selectors: &[RuntimeSelector<'_>],
) -> Result<Vec<&'a HostTarget>> {
    if selectors.iter().any(|selector| selector.host.is_none()) {
        return Ok(project.hosts.values().collect());
    }

    selectors
        .iter()
        .filter_map(|selector| selector.host)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|host_name| host_for_name(project, host_name))
        .collect()
}

fn resolve_runtime_mutation_plan(
    backend: &(impl Backend + Sync),
    project: &Project,
    selectors: &[String],
    timeout: Duration,
) -> Result<Vec<RuntimeMutationPlanItem>> {
    if selectors.is_empty() {
        return Ok(Vec::new());
    }

    let selection = observe_runtime_selection(backend, project, selectors, timeout)?;

    let mut plan = Vec::new();
    let mut seen_services = Vec::<ObservedService>::new();
    let mut seen_unavailable = BTreeSet::new();
    for selector in selection.selectors {
        let unavailable_hosts = match selector.host {
            Some(host) => selection
                .unavailable
                .get_key_value(host)
                .into_iter()
                .collect::<Vec<_>>(),
            None => selection.unavailable.iter().collect::<Vec<_>>(),
        };
        if !unavailable_hosts.is_empty() {
            for (host, message) in unavailable_hosts {
                if seen_unavailable.insert((host.clone(), selector.service.to_owned())) {
                    plan.push(RuntimeMutationPlanItem::Unavailable {
                        host: host.clone(),
                        service: selector.service.to_owned(),
                        message: format!(
                            "cannot resolve runtime service `{}` because host `{host}` is unavailable: {message}",
                            selector.raw
                        ),
                    });
                }
            }
            continue;
        }

        for service in resolve_runtime_selector(&selection.observed, selector)? {
            if !seen_services.contains(service) {
                seen_services.push(service.clone());
                plan.push(RuntimeMutationPlanItem::Service(service.clone()));
            }
        }
    }

    Ok(plan)
}

#[derive(Clone, Copy, Debug)]
struct RuntimeSelector<'a> {
    raw: &'a str,
    host: Option<&'a str>,
    service: &'a str,
}

fn parse_runtime_selector(selector: &str) -> Result<RuntimeSelector<'_>> {
    if selector.is_empty() {
        bail!("runtime service selector cannot be empty");
    }

    let (host, service) = match selector.split_once('/') {
        Some((host, service))
            if !host.is_empty() && !service.is_empty() && !service.contains('/') =>
        {
            (Some(host), service)
        }
        Some(_) => {
            bail!("invalid runtime service selector `{selector}`; expected `[HOST/]SERVICE`")
        }
        None => (None, selector),
    };

    Ok(RuntimeSelector {
        raw: selector,
        host,
        service,
    })
}

fn resolve_runtime_selector<'a>(
    observed: &'a [ObservedService],
    selector: RuntimeSelector<'_>,
) -> Result<Vec<&'a ObservedService>> {
    let matches = observed
        .iter()
        .filter(|service| {
            service.name == selector.service
                && selector.host.is_none_or(|host| service.host == host)
        })
        .collect::<Vec<_>>();

    if matches.is_empty() {
        bail!("runtime service `{}` was not found", selector.raw);
    }

    let matching_hosts = matches
        .iter()
        .map(|service| service.host.as_str())
        .collect::<BTreeSet<_>>();
    if selector.host.is_none() && matching_hosts.len() > 1 {
        let candidates = matching_hosts
            .into_iter()
            .map(|host| format!("`{host}/{}`", selector.service))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "runtime service `{}` is ambiguous; qualify it as one of: {candidates}",
            selector.raw
        );
    }

    Ok(matches)
}
