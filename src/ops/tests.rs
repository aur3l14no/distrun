use super::project::DEFAULT_STOP_TIMEOUT;
use super::report::{OperationKind, OperationOutcome};
use super::runtime::{
    logs_for_runtime_service_with_timeout, resolve_runtime_services_with_timeout,
    stop_runtime_services_with_timeout,
};
use super::*;
use crate::backend::{Backend, StartResult};
use crate::model::{
    DesiredService, HostTarget, HostTransport, ObservedService, OnExisting, Project, RuntimeState,
};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    List {
        host: String,
        project: String,
    },
    Start {
        host: String,
        service: String,
    },
    StopService {
        host: String,
        service: String,
        window_id: String,
        timeout: Duration,
    },
    StopProject {
        host: String,
        project: String,
    },
}

#[derive(Default)]
struct FakeBackend {
    observed: Mutex<BTreeMap<String, Vec<ObservedService>>>,
    calls: Mutex<Vec<Call>>,
    timeout_calls: Mutex<Vec<(String, Duration)>>,
    unavailable_hosts: BTreeSet<String>,
    failed_starts: BTreeSet<(String, String)>,
    failed_service_stops: BTreeSet<String>,
    failed_project_stops: BTreeSet<String>,
    observation_delay: Duration,
    active_observations: AtomicUsize,
    peak_observations: AtomicUsize,
}

impl FakeBackend {
    fn with_observed(services: impl IntoIterator<Item = ObservedService>) -> Self {
        let mut observed = BTreeMap::<String, Vec<ObservedService>>::new();
        for service in services {
            observed
                .entry(service.host.clone())
                .or_default()
                .push(service);
        }
        Self {
            observed: Mutex::new(observed),
            ..Self::default()
        }
    }

    fn with_timed_observation(
        services: impl IntoIterator<Item = ObservedService>,
        unavailable_hosts: impl IntoIterator<Item = &'static str>,
        observation_delay: Duration,
    ) -> Self {
        let mut backend = Self::with_observed(services);
        backend.unavailable_hosts = unavailable_hosts.into_iter().map(str::to_owned).collect();
        backend.observation_delay = observation_delay;
        backend
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls lock").clone()
    }

    fn record(&self, call: Call) {
        self.calls.lock().expect("calls lock").push(call);
    }

    fn observed(&self, host: &str) -> Vec<ObservedService> {
        self.observed
            .lock()
            .expect("observed lock")
            .get(host)
            .cloned()
            .unwrap_or_default()
    }

    fn timeout_calls(&self) -> Vec<(String, Duration)> {
        self.timeout_calls
            .lock()
            .expect("timeout calls lock")
            .clone()
    }

    fn peak_observations(&self) -> usize {
        self.peak_observations.load(Ordering::SeqCst)
    }
}

impl Backend for FakeBackend {
    fn list(&self, host: &HostTarget, project: &str) -> Result<Vec<ObservedService>> {
        self.record(Call::List {
            host: host.name.clone(),
            project: project.to_owned(),
        });
        Ok(self
            .observed(host.name.as_str())
            .into_iter()
            .filter(|service| service.project == project)
            .collect())
    }

    fn list_with_timeout(
        &self,
        host: &HostTarget,
        project: &str,
        timeout: Duration,
    ) -> Result<Vec<ObservedService>> {
        self.timeout_calls
            .lock()
            .expect("timeout calls lock")
            .push((host.name.clone(), timeout));
        let active = self.active_observations.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_observations.fetch_max(active, Ordering::SeqCst);
        thread::sleep(self.observation_delay);
        self.active_observations.fetch_sub(1, Ordering::SeqCst);

        if self.unavailable_hosts.contains(&host.name) {
            bail!("simulated timeout on host `{}`", host.name);
        }
        self.list(host, project)
    }

    fn list_all_with_timeout(
        &self,
        host: &HostTarget,
        _timeout: Duration,
    ) -> Result<Vec<ObservedService>> {
        Ok(self.observed(&host.name))
    }

    fn start(&self, host: &HostTarget, service: &DesiredService) -> Result<StartResult> {
        self.record(Call::Start {
            host: host.name.clone(),
            service: service.name.clone(),
        });
        if self
            .failed_starts
            .contains(&(host.name.clone(), service.name.clone()))
        {
            bail!(
                "simulated start failure for `{}/{}`",
                host.name,
                service.name
            );
        }
        let mut observed = self.observed.lock().expect("observed lock");
        observed
            .entry(host.name.clone())
            .or_default()
            .push(observed_with_id(
                &host.name,
                &service.name,
                &format!("@started-{}", service.name),
            ));
        Ok(StartResult::Started)
    }

    fn stop_service(
        &self,
        host: &HostTarget,
        service: &ObservedService,
        timeout: Duration,
    ) -> Result<()> {
        self.record(Call::StopService {
            host: host.name.clone(),
            service: service.name.clone(),
            window_id: service.window_id.clone(),
            timeout,
        });
        if self.failed_service_stops.contains(&service.window_id) {
            bail!("simulated stop failure for `{}`", service.window_id);
        }
        if let Some(observed) = self
            .observed
            .lock()
            .expect("observed lock")
            .get_mut(&host.name)
        {
            observed.retain(|item| item.window_id != service.window_id);
        }
        Ok(())
    }

    fn stop_project(&self, host: &HostTarget, project: &str, _timeout: Duration) -> Result<()> {
        self.record(Call::StopProject {
            host: host.name.clone(),
            project: project.to_owned(),
        });
        if self.failed_project_stops.contains(&host.name) {
            bail!("simulated project stop failure on `{}`", host.name);
        }
        if let Some(observed) = self
            .observed
            .lock()
            .expect("observed lock")
            .get_mut(&host.name)
        {
            observed.retain(|item| item.project != project);
        }
        Ok(())
    }

    fn logs_with_timeout(
        &self,
        _host: &HostTarget,
        _service: &ObservedService,
        _tail: usize,
        _timeout: Duration,
    ) -> Result<String> {
        panic!("unexpected log read")
    }
}

#[test]
fn down_reports_each_host_and_continues_after_a_failure() {
    let mut backend = FakeBackend::default();
    backend.failed_project_stops.insert("local".to_owned());
    let mut project = project();
    project.hosts.insert("zeta".to_owned(), remote_host("zeta"));

    let report = down(&backend, &project).expect("down returns a complete report");

    assert_eq!(
        report.outcomes,
        vec![
            stopped("edge", None),
            failed(
                "local",
                None,
                OperationKind::Stop,
                "simulated project stop failure on `local`",
            ),
            stopped("zeta", None),
        ]
    );
    assert_eq!(
        backend
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::StopProject { host, .. } => Some(host),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["edge", "local", "zeta"]
    );
}

#[test]
fn full_recreate_skips_only_the_host_whose_stop_failed() {
    let mut backend = FakeBackend::default();
    backend.failed_project_stops.insert("edge".to_owned());

    let report = recreate(&backend, &project(), &[]).expect("recreate project");

    assert_eq!(
        report.outcomes,
        vec![
            failed(
                "edge",
                None,
                OperationKind::Stop,
                "simulated project stop failure on `edge`",
            ),
            stopped("local", None),
            started("local", "api"),
        ]
    );
    assert_eq!(
        backend
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::StopProject { .. } | Call::Start { .. }))
            .collect::<Vec<_>>(),
        vec![
            Call::StopProject {
                host: "edge".to_owned(),
                project: "demo".to_owned(),
            },
            Call::StopProject {
                host: "local".to_owned(),
                project: "demo".to_owned(),
            },
            Call::Start {
                host: "local".to_owned(),
                service: "api".to_owned(),
            },
        ]
    );
}

#[test]
fn up_reports_a_failed_service_and_starts_later_services() {
    let mut backend = FakeBackend::default();
    backend
        .failed_starts
        .insert(("local".to_owned(), "b".to_owned()));
    let mut project = project();
    project.services.clear();
    for name in ["a", "b", "c"] {
        project
            .services
            .insert(name.to_owned(), desired(name, "local", 1));
    }

    let report = up_selected(&backend, &project, &[]).expect("up returns a complete report");

    assert_eq!(
        report.outcomes,
        vec![
            started("local", "a"),
            failed(
                "local",
                Some("b"),
                OperationKind::Start,
                "simulated start failure for `local/b`",
            ),
            started("local", "c"),
        ]
    );
}

#[test]
fn runtime_selector_requires_host_when_observed_name_is_ambiguous() {
    let backend = FakeBackend::with_observed([observed("local", "api"), observed("edge", "api")]);
    let project = project();

    let error = resolve_runtime_services_with_timeout(
        &backend,
        &project,
        &["api".to_owned()],
        TEST_TIMEOUT,
    )
    .expect_err("ambiguous api");
    assert_eq!(
        error.to_string(),
        "runtime service `api` is ambiguous; qualify it as one of: `edge/api`, `local/api`"
    );

    let backend = FakeBackend::with_observed([observed("local", "api"), observed("edge", "api")]);
    let resolved = resolve_runtime_services_with_timeout(
        &backend,
        &project,
        &["edge/api".to_owned()],
        TEST_TIMEOUT,
    )
    .expect("edge api");
    assert_eq!(resolved, vec![observed("edge", "api")]);
    assert_eq!(
        backend.calls(),
        vec![Call::List {
            host: "edge".to_owned(),
            project: "demo".to_owned(),
        }]
    );
    assert_eq!(
        backend.timeout_calls(),
        vec![("edge".to_owned(), TEST_TIMEOUT)]
    );
}

#[test]
fn up_reports_duplicate_configured_instances_without_starting() {
    let backend = FakeBackend::with_observed([
        observed_with_id("local", "api", "@1"),
        observed_with_id("local", "api", "@2"),
    ]);

    let report = up_selected(&backend, &project(), &["api".to_owned()])
        .expect("duplicate is an execution outcome");

    assert!(matches!(
        report.outcomes.as_slice(),
        [OperationOutcome::Failed {
            host,
            service: Some(service),
            operation: OperationKind::Observe,
            message,
        }] if host == "local" && service == "api" && message.contains("found 2 runtime instances")
    ));
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, Call::Start { .. } | Call::StopService { .. }))
    );
}

#[test]
fn stop_qualified_duplicate_stops_every_instance() {
    let backend = FakeBackend::with_observed([
        observed_with_id("local", "api", "@1"),
        observed_with_id("local", "api", "@2"),
    ]);

    let report = stop_runtime_services_with_timeout(
        &backend,
        &project(),
        &["local/api".to_owned()],
        TEST_TIMEOUT,
    )
    .expect("stop duplicate");

    assert_eq!(
        report.outcomes,
        vec![stopped("local", Some("api")), stopped("local", Some("api")),]
    );
    assert!(backend.observed("local").is_empty());
}

#[test]
fn selected_recreate_repairs_duplicate_instances() {
    let backend = FakeBackend::with_observed([
        observed_with_id("local", "api", "@1"),
        observed_with_id("local", "api", "@2"),
    ]);

    let report = recreate(&backend, &project(), &["api".to_owned()]).expect("recreate duplicate");

    assert_eq!(report.outcomes, vec![recreated("local", "api")]);
    let mutation_calls = backend
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::StopService { .. } | Call::Start { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        mutation_calls,
        vec![
            Call::StopService {
                host: "local".to_owned(),
                service: "api".to_owned(),
                window_id: "@1".to_owned(),
                timeout: Duration::from_secs(2),
            },
            Call::StopService {
                host: "local".to_owned(),
                service: "api".to_owned(),
                window_id: "@2".to_owned(),
                timeout: Duration::from_secs(2),
            },
            Call::Start {
                host: "local".to_owned(),
                service: "api".to_owned(),
            },
        ]
    );
}

#[test]
fn selected_recreate_reports_a_failure_and_continues_with_other_hosts() {
    let mut backend = FakeBackend::with_observed([
        observed_with_id("edge", "worker", "@worker"),
        observed_with_id("local", "api", "@api"),
    ]);
    backend.failed_service_stops.insert("@worker".to_owned());

    let report = recreate(
        &backend,
        &project(),
        &["worker".to_owned(), "api".to_owned()],
    )
    .expect("recreate returns a complete report");

    assert_eq!(
        report.outcomes,
        vec![
            failed(
                "edge",
                Some("worker"),
                OperationKind::Stop,
                "simulated stop failure for `@worker`",
            ),
            recreated("local", "api"),
        ]
    );
    assert!(backend.calls().contains(&Call::Start {
        host: "local".to_owned(),
        service: "api".to_owned(),
    }));
}

#[test]
fn logs_reject_duplicate_instances_instead_of_guessing() {
    let backend = FakeBackend::with_observed([
        observed_with_id("local", "api", "@1"),
        observed_with_id("local", "api", "@2"),
    ]);

    let error =
        logs_for_runtime_service_with_timeout(&backend, &project(), "local/api", 10, TEST_TIMEOUT)
            .expect_err("logs must identify one runtime");

    assert_eq!(
        error.to_string(),
        "runtime service `local/api` has 2 instances; logs requires exactly one instance"
    );
}

#[test]
fn bare_runtime_selector_observes_all_hosts_concurrently_with_a_timeout() {
    let backend = FakeBackend::with_timed_observation(
        [observed("local", "api"), observed("edge", "worker")],
        [],
        Duration::from_millis(100),
    );

    let resolved = resolve_runtime_services_with_timeout(
        &backend,
        &project(),
        &["api".to_owned()],
        TEST_TIMEOUT,
    )
    .expect("resolve api");

    assert_eq!(resolved, vec![observed("local", "api")]);
    assert_eq!(backend.peak_observations(), 2);
    let mut timeout_calls = backend.timeout_calls();
    timeout_calls.sort();
    assert_eq!(
        timeout_calls,
        vec![
            ("edge".to_owned(), TEST_TIMEOUT),
            ("local".to_owned(), TEST_TIMEOUT),
        ]
    );
}

#[test]
fn unavailable_hosts_all_report_bare_selector_failures_without_service_mutation() {
    let backend = FakeBackend::with_timed_observation(
        [observed("local", "api")],
        ["edge", "zeta"],
        Duration::ZERO,
    );
    let mut project = project();
    project.hosts.insert("zeta".to_owned(), remote_host("zeta"));

    let report =
        stop_runtime_services_with_timeout(&backend, &project, &["api".to_owned()], TEST_TIMEOUT)
            .expect("transport failures are operation outcomes");

    assert_eq!(report.outcomes.len(), 2);
    for (outcome, expected_host) in report.outcomes.iter().zip(["edge", "zeta"]) {
        assert!(matches!(
            outcome,
            OperationOutcome::Failed {
                host,
                service: Some(service),
                operation: OperationKind::Observe,
                message,
            } if host == expected_host
                && service == "api"
                && message.contains("simulated timeout")
        ));
    }
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, Call::StopService { .. }))
    );
}

#[test]
fn unavailable_qualified_host_does_not_block_another_qualified_stop() {
    let backend =
        FakeBackend::with_timed_observation([observed("local", "api")], ["edge"], Duration::ZERO);

    let report = stop_runtime_services_with_timeout(
        &backend,
        &project(),
        &["local/api".to_owned(), "edge/worker".to_owned()],
        TEST_TIMEOUT,
    )
    .expect("qualified transport failure is an operation outcome");

    assert!(matches!(
        report.outcomes.as_slice(),
        [
            OperationOutcome::Stopped {
                host,
                service: Some(service),
            },
            OperationOutcome::Failed {
                host: failed_host,
                service: Some(failed_service),
                operation: OperationKind::Observe,
                message,
            },
        ] if host == "local"
            && service == "api"
            && failed_host == "edge"
            && failed_service == "worker"
            && message.contains("simulated timeout")
    ));
    assert!(backend.calls().contains(&Call::StopService {
        host: "local".to_owned(),
        service: "api".to_owned(),
        window_id: "@local-api".to_owned(),
        timeout: Duration::from_secs(2),
    }));
}

#[test]
fn stop_runtime_services_resolves_every_selector_before_stopping_orphans() {
    let backend =
        FakeBackend::with_observed([observed("local", "old-worker"), observed("edge", "worker")]);
    let project = project();

    let error = stop_runtime_services_with_timeout(
        &backend,
        &project,
        &["local/old-worker".to_owned(), "missing".to_owned()],
        TEST_TIMEOUT,
    )
    .expect_err("missing service");
    assert_eq!(error.to_string(), "runtime service `missing` was not found");
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, Call::StopService { .. }))
    );

    let report = stop_runtime_services_with_timeout(
        &backend,
        &project,
        &["local/old-worker".to_owned()],
        TEST_TIMEOUT,
    )
    .expect("stop orphan");
    assert_eq!(report.outcomes, vec![stopped("local", Some("old-worker"))]);
    assert!(backend.calls().contains(&Call::StopService {
        host: "local".to_owned(),
        service: "old-worker".to_owned(),
        window_id: "@local-old-worker".to_owned(),
        timeout: DEFAULT_STOP_TIMEOUT,
    }));
}

#[test]
fn stop_reports_a_failure_and_continues_with_later_instances() {
    let mut backend = FakeBackend::with_observed([
        observed_with_id("local", "old-worker", "@old"),
        observed_with_id("edge", "worker", "@worker"),
    ]);
    backend.failed_service_stops.insert("@old".to_owned());

    let report = stop_runtime_services_with_timeout(
        &backend,
        &project(),
        &["local/old-worker".to_owned(), "edge/worker".to_owned()],
        TEST_TIMEOUT,
    )
    .expect("stop returns a complete report");

    assert_eq!(
        report.outcomes,
        vec![
            failed(
                "local",
                Some("old-worker"),
                OperationKind::Stop,
                "simulated stop failure for `@old`",
            ),
            stopped("edge", Some("worker")),
        ]
    );
    assert!(backend.calls().contains(&Call::StopService {
        host: "edge".to_owned(),
        service: "worker".to_owned(),
        window_id: "@worker".to_owned(),
        timeout: Duration::from_secs(3),
    }));
}

fn project() -> Project {
    Project {
        name: "demo".to_owned(),
        on_existing: OnExisting::Skip,
        hosts: BTreeMap::from([
            ("edge".to_owned(), remote_host("edge")),
            (
                "local".to_owned(),
                HostTarget {
                    name: "local".to_owned(),
                    transport: HostTransport::Local,
                },
            ),
        ]),
        services: BTreeMap::from([
            ("api".to_owned(), desired("api", "local", 2)),
            ("worker".to_owned(), desired("worker", "edge", 3)),
        ]),
    }
}

fn remote_host(name: &str) -> HostTarget {
    HostTarget {
        name: name.to_owned(),
        transport: HostTransport::Ssh(format!("{name}.example")),
    }
}

fn desired(name: &str, host: &str, stop_timeout: u64) -> DesiredService {
    DesiredService {
        project: "demo".to_owned(),
        name: name.to_owned(),
        host: host.to_owned(),
        cmd: name.to_owned(),
        cwd: None,
        env: BTreeMap::new(),
        stop_timeout: Duration::from_secs(stop_timeout),
    }
}

fn observed(host: &str, service: &str) -> ObservedService {
    observed_with_id(host, service, &format!("@{host}-{service}"))
}

fn observed_with_id(host: &str, service: &str, window_id: &str) -> ObservedService {
    ObservedService {
        project: "demo".to_owned(),
        host: host.to_owned(),
        name: service.to_owned(),
        window_id: window_id.to_owned(),
        pane_id: format!("%{window_id}"),
        runtime_id: Some(format!("run-{window_id}")),
        runtime: RuntimeState::Running,
    }
}

fn started(host: &str, service: &str) -> OperationOutcome {
    OperationOutcome::Started {
        host: host.to_owned(),
        service: service.to_owned(),
    }
}

fn recreated(host: &str, service: &str) -> OperationOutcome {
    OperationOutcome::Recreated {
        host: host.to_owned(),
        service: service.to_owned(),
    }
}

fn stopped(host: &str, service: Option<&str>) -> OperationOutcome {
    OperationOutcome::Stopped {
        host: host.to_owned(),
        service: service.map(str::to_owned),
    }
}

fn failed(
    host: &str,
    service: Option<&str>,
    operation: OperationKind,
    message: &str,
) -> OperationOutcome {
    OperationOutcome::Failed {
        host: host.to_owned(),
        service: service.map(str::to_owned),
        operation,
        message: message.to_owned(),
    }
}
