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
    // tmux IDs are stable for the lifetime of this concrete runtime. Commands
    // must use these IDs after resolving a user-facing service selector.
    pub window_id: String,
    pub pane_id: String,
    // Older distrun windows do not carry a runtime ID. They remain observable
    // and use capture-pane as a read-only log fallback.
    pub runtime_id: Option<String>,
    pub runtime: RuntimeState,
}

impl ObservedService {
    pub fn logical_key(&self) -> (&str, &str, &str) {
        (&self.host, &self.project, &self.name)
    }

    pub fn instance_label(&self) -> &str {
        self.runtime_id.as_deref().unwrap_or(&self.window_id)
    }
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
pub enum RuntimeStatus {
    Running,
    Exited,
    Unknown,
    Missing,
    Unavailable,
}

impl RuntimeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Unknown => "unknown",
            Self::Missing => "missing",
            Self::Unavailable => "unavailable",
        }
    }
}

impl From<RuntimeState> for RuntimeStatus {
    fn from(runtime: RuntimeState) -> Self {
        match runtime {
            RuntimeState::Running => Self::Running,
            RuntimeState::Exited => Self::Exited,
            RuntimeState::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigRelation {
    Configured,
    Orphan,
}

impl ConfigRelation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Orphan => "orphan",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusIssue {
    Duplicate { instance: String },
}

impl StatusIssue {
    pub fn label(&self) -> String {
        match self {
            Self::Duplicate { instance } => format!("duplicate:{instance}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceStatus {
    pub host: String,
    pub service: String,
    pub runtime: RuntimeStatus,
    pub relation: ConfigRelation,
    pub issue: Option<StatusIssue>,
}
