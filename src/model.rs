use std::collections::BTreeMap;
use std::time::Duration;

pub const LOCAL_HOST_NAME: &str = "local";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub on_existing: OnExisting,
    pub hosts: BTreeMap<String, HostTarget>,
    pub services: BTreeMap<String, DesiredService>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnExisting {
    #[default]
    Skip,
    Restart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostTarget {
    pub name: String,
    pub transport: HostTransport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostTransport {
    Local,
    Ssh(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesiredService {
    pub project: String,
    pub name: String,
    pub host: String,
    pub cmd: String,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub stop_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedService {
    pub project: String,
    pub host: String,
    pub name: String,
    pub runtime: RuntimeState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeState {
    Running,
    Exited,
    Unknown,
}

impl RuntimeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecState {
    InSync,
    Missing,
    Orphan,
    Unavailable,
}

impl SpecState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InSync => "in-sync",
            Self::Missing => "missing",
            Self::Orphan => "orphan",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceStatus {
    pub host: String,
    pub service: String,
    // Runtime state is what the backend observes right now.
    pub runtime: Option<RuntimeState>,
    // Spec state is the desired-vs-observed reconciliation result:
    // missing = + desired only, orphan = - observed only, in-sync = both sides.
    pub spec: SpecState,
}
