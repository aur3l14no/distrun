use crate::model::{
    ConfigRelation, DesiredService, ObservedService, RuntimeStatus, ServiceStatus, StatusIssue,
};
use std::collections::{BTreeMap, BTreeSet};

pub fn reconcile_host<'a>(
    host: &str,
    desired: impl Iterator<Item = &'a DesiredService>,
    observed: Vec<ObservedService>,
) -> Vec<ServiceStatus> {
    let desired = desired
        .map(|service| service.name.clone())
        .collect::<BTreeSet<_>>();
    let mut observed_by_name = BTreeMap::<String, Vec<ObservedService>>::new();
    for service in observed {
        observed_by_name
            .entry(service.name.clone())
            .or_default()
            .push(service);
    }
    let names = desired
        .iter()
        .chain(observed_by_name.keys())
        .cloned()
        .collect::<BTreeSet<_>>();

    names
        .into_iter()
        .flat_map(|service| match observed_by_name.get(&service) {
            Some(instances) => instances
                .iter()
                .map(|observed| ServiceStatus {
                    host: host.to_owned(),
                    service: service.clone(),
                    runtime: observed.runtime.into(),
                    relation: if desired.contains(&service) {
                        ConfigRelation::Configured
                    } else {
                        ConfigRelation::Orphan
                    },
                    issue: (instances.len() > 1).then(|| StatusIssue::Duplicate {
                        instance: observed.instance_label().to_owned(),
                    }),
                })
                .collect::<Vec<_>>(),
            None => vec![ServiceStatus {
                host: host.to_owned(),
                service,
                runtime: RuntimeStatus::Missing,
                relation: ConfigRelation::Configured,
                issue: None,
            }],
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
    fn separates_runtime_from_config_relation() {
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
                    runtime: RuntimeStatus::Running,
                    relation: ConfigRelation::Configured,
                    issue: None,
                },
                ServiceStatus {
                    host: "web".to_owned(),
                    service: "cron".to_owned(),
                    runtime: RuntimeStatus::Missing,
                    relation: ConfigRelation::Configured,
                    issue: None,
                },
                ServiceStatus {
                    host: "web".to_owned(),
                    service: "worker".to_owned(),
                    runtime: RuntimeStatus::Running,
                    relation: ConfigRelation::Orphan,
                    issue: None,
                },
            ]
        );
    }

    #[test]
    fn exposes_additional_same_name_runtimes_as_duplicates() {
        let desired = [desired("api")];
        let observed = vec![
            observed_with_id("api", "@1", RuntimeState::Running),
            observed_with_id("api", "@2", RuntimeState::Exited),
        ];

        let statuses = reconcile_host("web", desired.iter(), observed);

        assert_eq!(
            statuses,
            vec![
                ServiceStatus {
                    host: "web".to_owned(),
                    service: "api".to_owned(),
                    runtime: RuntimeStatus::Running,
                    relation: ConfigRelation::Configured,
                    issue: Some(StatusIssue::Duplicate {
                        instance: "@1".to_owned(),
                    }),
                },
                ServiceStatus {
                    host: "web".to_owned(),
                    service: "api".to_owned(),
                    runtime: RuntimeStatus::Exited,
                    relation: ConfigRelation::Configured,
                    issue: Some(StatusIssue::Duplicate {
                        instance: "@2".to_owned(),
                    }),
                },
            ]
        );
    }

    #[test]
    fn distinguishes_orphans_from_duplicate_orphans() {
        let observed = vec![
            observed_with_id("worker", "@1", RuntimeState::Running),
            observed_with_id("worker", "@2", RuntimeState::Exited),
        ];

        let statuses = reconcile_host("web", std::iter::empty(), observed);

        assert_eq!(statuses[0].relation, ConfigRelation::Orphan);
        assert_eq!(statuses[1].relation, ConfigRelation::Orphan);
        assert_eq!(
            statuses[0].issue,
            Some(StatusIssue::Duplicate {
                instance: "@1".to_owned(),
            })
        );
        assert_eq!(
            statuses[1].issue,
            Some(StatusIssue::Duplicate {
                instance: "@2".to_owned(),
            })
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
        observed_with_id(name, &format!("@{name}"), runtime)
    }

    fn observed_with_id(name: &str, window_id: &str, runtime: RuntimeState) -> ObservedService {
        ObservedService {
            project: "demo".to_owned(),
            host: "web".to_owned(),
            name: name.to_owned(),
            window_id: window_id.to_owned(),
            pane_id: format!("%{name}"),
            runtime_id: None,
            runtime,
        }
    }
}
