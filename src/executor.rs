use crate::model::{HostTarget, HostTransport};
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[derive(Debug)]
pub struct HostOutput {
    pub stdout: String,
}

pub trait HostExecutor {
    fn run(&self, host: &HostTarget, host_command: &str) -> Result<HostOutput>;

    fn run_with_timeout(
        &self,
        host: &HostTarget,
        host_command: &str,
        timeout: Duration,
    ) -> Result<HostOutput> {
        let _ = timeout;
        self.run(host, host_command)
    }
}

#[derive(Debug, Default)]
pub struct SystemExecutor;

impl HostExecutor for SystemExecutor {
    fn run(&self, host: &HostTarget, host_command: &str) -> Result<HostOutput> {
        let output = command_for(host, host_command)
            .output()
            .with_context(|| format!("failed to run command for host `{}`", host.name))?;

        output_result(host, output)
    }

    fn run_with_timeout(
        &self,
        host: &HostTarget,
        host_command: &str,
        timeout: Duration,
    ) -> Result<HostOutput> {
        let mut command = command_for(host, host_command);
        configure_timeout_process(&mut command);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to run command for host `{}`", host.name))?;
        let stdout = child
            .stdout
            .take()
            .with_context(|| format!("missing stdout pipe for host `{}`", host.name))?;
        let stderr = child
            .stderr
            .take()
            .with_context(|| format!("missing stderr pipe for host `{}`", host.name))?;
        let stdout_reader = read_pipe(stdout);
        let stderr_reader = read_pipe(stderr);
        let deadline = Instant::now() + timeout;

        let status = loop {
            if let Some(status) = child
                .try_wait()
                .with_context(|| format!("failed to wait for host `{}`", host.name))?
            {
                break status;
            }

            let now = Instant::now();
            if now >= deadline {
                terminate_child(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                bail!(
                    "command timed out on host `{}` after {}",
                    host.name,
                    format_duration(timeout)
                );
            }

            thread::sleep((deadline - now).min(Duration::from_millis(25)));
        };

        let output = Output {
            status,
            stdout: join_pipe(host, "stdout", stdout_reader)?,
            stderr: join_pipe(host, "stderr", stderr_reader)?,
        };
        output_result(host, output)
    }
}

fn command_for(host: &HostTarget, host_command: &str) -> Command {
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
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn output_result(host: &HostTarget, output: Output) -> Result<HostOutput> {
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

fn read_pipe(mut pipe: impl Read + Send + 'static) -> JoinHandle<std::io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        pipe.read_to_end(&mut output)?;
        Ok(output)
    })
}

fn join_pipe(
    host: &HostTarget,
    name: &str,
    reader: JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("failed to read {name} pipe for host `{}`", host.name))?
        .with_context(|| format!("failed to read {name} pipe for host `{}`", host.name))
}

#[cfg(unix)]
fn configure_timeout_process(command: &mut Command) {
    // Put sh/ssh command trees in their own group so a timeout can clean up descendants too.
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_timeout_process(_command: &mut Command) {}

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let process_group = -(child.id() as i32);
        unsafe {
            libc::kill(process_group, libc::SIGKILL);
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}

fn format_duration(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        return format!("{}s", duration.as_secs());
    }

    format!("{}ms", duration.as_millis())
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

    #[test]
    fn times_out_local_host_commands() {
        let host = HostTarget {
            name: "local".to_owned(),
            transport: HostTransport::Local,
        };

        let started = Instant::now();
        let error = SystemExecutor
            .run_with_timeout(&host, "sleep 5", Duration::from_millis(50))
            .expect_err("command should time out");

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn captures_large_output_while_waiting_with_timeout() {
        let host = HostTarget {
            name: "local".to_owned(),
            transport: HostTransport::Local,
        };

        let output = SystemExecutor
            .run_with_timeout(&host, "yes ok | head -c 1048576", Duration::from_secs(2))
            .expect("large output should not block process exit");

        assert_eq!(output.stdout.len(), 1_048_576);
    }
}
