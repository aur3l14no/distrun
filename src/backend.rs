use crate::model::{DesiredService, HostTarget, ObservedService};
use anyhow::Result;
use std::time::Duration;

pub trait Backend {
    fn list(&self, host: &HostTarget, project: &str) -> Result<Vec<ObservedService>>;

    // NOTE: Status list calls are expected to be lightweight. Backends that can
    // block on external transports should override these timeout variants; the
    // default keeps simple in-memory/test backends from needing timeout plumbing.
    fn list_with_timeout(
        &self,
        host: &HostTarget,
        project: &str,
        _timeout: Duration,
    ) -> Result<Vec<ObservedService>> {
        self.list(host, project)
    }

    fn list_all(&self, host: &HostTarget) -> Result<Vec<ObservedService>>;

    fn list_all_with_timeout(
        &self,
        host: &HostTarget,
        _timeout: Duration,
    ) -> Result<Vec<ObservedService>> {
        self.list_all(host)
    }

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
