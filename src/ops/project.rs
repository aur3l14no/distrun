use crate::model::{DesiredService, HostTarget, Project};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

pub(super) const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn selected_configured_services<'a>(
    project: &'a Project,
    service_names: &[String],
) -> Result<Option<BTreeSet<&'a str>>> {
    if service_names.is_empty() {
        return Ok(None);
    }

    let mut selected = BTreeSet::new();
    for service_name in service_names {
        let Some((configured_name, _)) = project.services.get_key_value(service_name) else {
            anyhow::bail!("service `{service_name}` is not in config");
        };
        selected.insert(configured_name.as_str());
    }
    Ok(Some(selected))
}

pub(super) fn validate_desired_hosts(
    project: &Project,
    selected: Option<&BTreeSet<&str>>,
) -> Result<()> {
    for service in project
        .services
        .values()
        .filter(|service| selected.is_none_or(|selected| selected.contains(service.name.as_str())))
    {
        host_for_name(project, &service.host)?;
    }
    Ok(())
}

pub(super) fn desired_for_host<'a>(
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

pub(super) fn host_for_name<'a>(project: &'a Project, host_name: &str) -> Result<&'a HostTarget> {
    project
        .hosts
        .get(host_name)
        .ok_or_else(|| anyhow::anyhow!("unknown host `{host_name}`"))
}

pub(super) fn service_stop_timeout(
    project: &Project,
    host_name: &str,
    service_name: &str,
) -> Duration {
    project
        .services
        .get(service_name)
        .filter(|service| service.host == host_name)
        .map(|service| service.stop_timeout)
        .unwrap_or(DEFAULT_STOP_TIMEOUT)
}

pub(super) fn max_stop_timeout(project: &Project, host_name: &str) -> Duration {
    project
        .services
        .values()
        .filter(|service| service.host == host_name)
        .map(|service| service.stop_timeout)
        .max()
        .unwrap_or(DEFAULT_STOP_TIMEOUT)
}
