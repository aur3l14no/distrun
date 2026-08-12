use crate::model::{ObservedService, RuntimeState, ServiceStatus};
use std::fmt::Display;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    Observe,
    Start,
    Stop,
}

impl OperationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationOutcome {
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
    Recreated {
        host: String,
        service: String,
    },
    Stopped {
        host: String,
        service: Option<String>,
    },
    Orphan {
        host: String,
        service: String,
        runtime: RuntimeState,
    },
    Failed {
        host: String,
        service: Option<String>,
        operation: OperationKind,
        message: String,
    },
}

impl OperationOutcome {
    pub fn line(&self) -> String {
        match self {
            Self::Started { host, service } => format!("{host} {service} started"),
            Self::Skipped { host, service } => format!("{host} {service} skipped"),
            Self::Restarted { host, service } => format!("{host} {service} restarted"),
            Self::Recreated { host, service } => format!("{host} {service} recreated"),
            Self::Stopped { host, service } => target(host, service.as_deref(), "stopped"),
            Self::Orphan {
                host,
                service,
                runtime,
            } => format!("{host} {service} orphan {}", runtime.as_str()),
            Self::Failed {
                host,
                service,
                operation,
                message,
            } => target(
                host,
                service.as_deref(),
                &format!("{} failed: {message}", operation.as_str()),
            ),
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

fn target(host: &str, service: Option<&str>, suffix: &str) -> String {
    match service {
        Some(service) => format!("{host} {service} {suffix}"),
        None => format!("{host} {suffix}"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationReport {
    pub outcomes: Vec<OperationOutcome>,
}

impl OperationReport {
    pub(super) fn new() -> Self {
        Self {
            outcomes: Vec::new(),
        }
    }

    pub(super) fn push(&mut self, outcome: OperationOutcome) {
        self.outcomes.push(outcome);
    }

    pub(super) fn failed(
        &mut self,
        host: &str,
        service: Option<&str>,
        operation: OperationKind,
        error: impl Display,
    ) {
        self.push(OperationOutcome::Failed {
            host: host.to_owned(),
            service: service.map(str::to_owned),
            operation,
            message: format!("{error:#}"),
        });
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
    pub statuses: Vec<ServiceStatus>,
    pub unavailable_hosts: Vec<UnavailableHost>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusAllReport {
    pub observed: Vec<ObservedService>,
    pub unavailable_hosts: Vec<UnavailableHost>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnavailableHost {
    pub host: String,
    pub message: String,
}
