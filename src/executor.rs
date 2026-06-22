use crate::model::{HostTarget, HostTransport};
use anyhow::{Context, Result, bail};
use std::process::{Command, Stdio};

#[derive(Debug)]
pub struct HostOutput {
    pub stdout: String,
}

pub trait HostExecutor {
    fn run(&self, host: &HostTarget, host_command: &str) -> Result<HostOutput>;
}

#[derive(Debug, Default)]
pub struct SystemExecutor;

impl HostExecutor for SystemExecutor {
    fn run(&self, host: &HostTarget, host_command: &str) -> Result<HostOutput> {
        let mut command = match &host.transport {
            HostTransport::Local => {
                let mut command = Command::new("sh");
                command.arg("-lc").arg(host_command);
                command
            }
            HostTransport::Ssh(target) => {
                let mut command = Command::new("ssh");
                command.arg(target).arg(host_command);
                command
            }
        };

        let output = command
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to run command for host `{}`", host.name))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if output.status.success() {
            Ok(HostOutput { stdout })
        } else {
            bail!(
                "command failed on host `{}` with status {}: {}",
                host.name,
                output.status,
                stderr.trim()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_local_host_commands_without_ssh() {
        let host = HostTarget {
            name: "local".to_owned(),
            transport: HostTransport::Local,
        };

        let output = SystemExecutor
            .run(&host, "printf local-ok")
            .expect("run local command");

        assert_eq!(output.stdout, "local-ok");
    }
}
