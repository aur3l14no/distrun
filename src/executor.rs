use crate::model::HostTarget;
use anyhow::{Context, Result, bail};
use std::process::Command;

#[derive(Debug)]
pub struct RemoteOutput {
    pub stdout: String,
}

pub trait RemoteExecutor {
    fn run(&self, host: &HostTarget, remote_command: &str) -> Result<RemoteOutput>;
}

#[derive(Debug, Default)]
pub struct SshExecutor;

impl RemoteExecutor for SshExecutor {
    fn run(&self, host: &HostTarget, remote_command: &str) -> Result<RemoteOutput> {
        let mut command = Command::new("ssh");
        command.arg(&host.ssh).arg(remote_command);

        let output = command
            .output()
            .with_context(|| format!("failed to run ssh for host `{}`", host.name))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if output.status.success() {
            Ok(RemoteOutput { stdout })
        } else {
            bail!(
                "remote command failed on host `{}` with status {}: {}",
                host.name,
                output.status,
                stderr.trim()
            )
        }
    }
}
