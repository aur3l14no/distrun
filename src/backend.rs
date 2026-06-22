use crate::model::{DesiredService, HostTarget, ObservedService};
use anyhow::Result;
use std::time::Duration;

pub trait Backend {
    fn list(&self, host: &HostTarget, project: &str) -> Result<Vec<ObservedService>>;

    fn list_all(&self, host: &HostTarget) -> Result<Vec<ObservedService>>;

    fn start(&self, host: &HostTarget, service: &DesiredService) -> Result<()>;

    fn stop_service(
        &self,
        host: &HostTarget,
        project: &str,
        service: &str,
        timeout: Duration,
    ) -> Result<()>;

    fn stop_project(&self, host: &HostTarget, project: &str, timeout: Duration) -> Result<()>;

    fn logs(&self, host: &HostTarget, project: &str, service: &str, tail: usize) -> Result<String>;
}
