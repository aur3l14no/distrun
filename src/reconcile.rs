use crate::model::{DesiredService, ObservedService, ServiceStatus, SpecState};
use std::collections::{BTreeMap, BTreeSet};

pub fn reconcile_host<'a>(
    host: &str,
    desired: impl Iterator<Item = &'a DesiredService>,
    observed: Vec<ObservedService>,
) -> Vec<ServiceStatus> {
    let desired = desired
        .map(|service| service.name.clone())
        .collect::<BTreeSet<_>>();
    let observed = observed
        .into_iter()
        .map(|service| (service.name.clone(), service))
        .collect::<BTreeMap<_, _>>();
    let names = desired
        .iter()
        .chain(observed.keys())
        .cloned()
        .collect::<BTreeSet<_>>();

    names
        .into_iter()
        .map(|service| match observed.get(&service) {
            Some(observed) if desired.contains(&service) => ServiceStatus {
                host: host.to_owned(),
                service,
                runtime: Some(observed.runtime),
                spec: SpecState::InSync,
            },
            Some(observed) => ServiceStatus {
                host: host.to_owned(),
                service,
                runtime: Some(observed.runtime),
                spec: SpecState::Orphan,
            },
            None => ServiceStatus {
                host: host.to_owned(),
                service,
                runtime: None,
                spec: SpecState::Missing,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DesiredService, RuntimeState};
    use std::collections::BTreeMap;
    use std::time::Duration;

    #[test]
    fn separates_runtime_from_spec_state() {
        let desired = [desired("api"), desired("cron")];
        let observed = vec![
            observed("api", RuntimeState::Running),
            observed("worker", RuntimeState::Running),
        ];

        let statuses = reconcile_host("web", desired.iter(), observed);

        assert_eq!(
            statuses,
            vec![
                ServiceStatus {
                    host: "web".to_owned(),
                    service: "api".to_owned(),
                    runtime: Some(RuntimeState::Running),
                    spec: SpecState::InSync,
                },
                ServiceStatus {
                    host: "web".to_owned(),
                    service: "cron".to_owned(),
                    runtime: None,
                    spec: SpecState::Missing,
                },
                ServiceStatus {
                    host: "web".to_owned(),
                    service: "worker".to_owned(),
                    runtime: Some(RuntimeState::Running),
                    spec: SpecState::Orphan,
                },
            ]
        );
    }

    fn desired(name: &str) -> DesiredService {
        DesiredService {
            project: "demo".to_owned(),
            name: name.to_owned(),
            host: "web".to_owned(),
            cmd: "sleep 60".to_owned(),
            cwd: None,
            env: BTreeMap::new(),
            stop_timeout: Duration::from_secs(1),
        }
    }

    fn observed(name: &str, runtime: RuntimeState) -> ObservedService {
        ObservedService {
            project: "demo".to_owned(),
            host: "web".to_owned(),
            name: name.to_owned(),
            runtime,
        }
    }
}
