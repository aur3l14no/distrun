use crate::model::{DesiredService, HostTarget, ObservedService};
use anyhow::Result;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartResult {
    Started,
    AlreadyPresent,
}

pub trait Backend {
    fn list(&self, host: &HostTarget, project: &str) -> Result<Vec<ObservedService>>;

    fn list_with_timeout(
        &self,
        host: &HostTarget,
        project: &str,
        timeout: Duration,
    ) -> Result<Vec<ObservedService>>;

    fn list_all_with_timeout(
        &self,
        host: &HostTarget,
        timeout: Duration,
    ) -> Result<Vec<ObservedService>>;

    fn start(&self, host: &HostTarget, service: &DesiredService) -> Result<StartResult>;

    fn stop_service(
        &self,
        host: &HostTarget,
        service: &ObservedService,
        timeout: Duration,
    ) -> Result<()>;

    fn stop_project(&self, host: &HostTarget, project: &str, timeout: Duration) -> Result<()>;

    fn logs_with_timeout(
        &self,
        host: &HostTarget,
        service: &ObservedService,
        tail: usize,
        timeout: Duration,
    ) -> Result<String>;
}
