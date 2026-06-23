use expect_test::{Expect, expect};
use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
#[ignore = "requires scripts/run-docker-tests.sh or DISTRUN_TEST_SSH_* env vars"]
fn manages_remote_services_and_reconciles_config_drift() {
    let ssh = SshTarget::from_env();
    let project = format!("it_{}", unique_id());
    let remote_dir = format!("/tmp/distrun_{project}");
    let local_dir = env::temp_dir().join(format!("distrun-{project}"));
    fs::create_dir_all(&local_dir).expect("create local test dir");
    let parts_dir = local_dir.join("parts");
    fs::create_dir_all(&parts_dir).expect("create config parts dir");
    let config_path = local_dir.join("distrun.yml");
    fs::write(
        parts_dir.join("service.env"),
        "DISTRUN_TEST_SUFFIX=from-env\n",
    )
    .expect("write env file");

    ssh.run(&format!("mkdir -p {remote_dir}"));

    write_config(
        &config_path,
        &ssh,
        &project,
        &remote_dir,
        "skip",
        &[service("api", &remote_dir), service("worker", &remote_dir)],
    );

    assert_success(&distrun(&["-f", path(&config_path), "up"]));
    thread::sleep(Duration::from_secs(2));

    let status = stdout(distrun(&["-f", path(&config_path), "status"]));
    assert_status(
        &status,
        expect![
            [r#"HOST             SERVICE                  RUNTIME    SPEC
test             api                      running    in-sync
test             worker                   running    in-sync
"#]
        ],
    );

    let all_status = stdout(distrun(&["-f", path(&config_path), "ps"]));
    assert_contains(
        &all_status,
        &format!("{:<16} {:<24} {:<24} running", "test", project, "api"),
    );
    assert_contains(
        &all_status,
        &format!("{:<16} {:<24} {:<24} running", "test", project, "worker"),
    );

    let logs = stdout(distrun(&["-f", path(&config_path), "logs", "api"]));
    assert_contains(&logs, "from-env-api-tick");

    let starts_before = ssh.read_u64(&format!("wc -l < {remote_dir}/api-starts"));
    assert_success(&distrun(&["-f", path(&config_path), "up"]));
    thread::sleep(Duration::from_secs(1));
    let starts_after = ssh.read_u64(&format!("wc -l < {remote_dir}/api-starts"));
    assert_eq!(
        starts_before, starts_after,
        "on_existing: skip should not restart a running service"
    );

    let worker_starts_before_restart = ssh.read_u64(&format!("wc -l < {remote_dir}/worker-starts"));
    let restart_output = stdout(distrun(&["-f", path(&config_path), "restart"]));
    expect![[r#"test stopped
test api started
test worker started
"#]]
    .assert_eq(&restart_output);
    thread::sleep(Duration::from_secs(1));
    let starts_after_restart = ssh.read_u64(&format!("wc -l < {remote_dir}/api-starts"));
    let worker_starts_after_restart = ssh.read_u64(&format!("wc -l < {remote_dir}/worker-starts"));
    assert_eq!(
        starts_after + 1,
        starts_after_restart,
        "restart should recreate the project services"
    );
    assert_eq!(
        worker_starts_before_restart + 1,
        worker_starts_after_restart,
        "restart should recreate every configured project service"
    );

    write_config(
        &config_path,
        &ssh,
        &project,
        &remote_dir,
        "skip",
        &[service("api", &remote_dir), service("cron", &remote_dir)],
    );
    let drifted = stdout(distrun(&["-f", path(&config_path), "status"]));
    assert_status(
        &drifted,
        expect![
            [r#"HOST             SERVICE                  RUNTIME    SPEC
test             api                      running    in-sync
test             cron                     -          missing
test             worker                   running    orphan
"#]
        ],
    );

    assert_success(&distrun(&["-f", path(&config_path), "up"]));
    thread::sleep(Duration::from_secs(1));
    let repaired = stdout(distrun(&["-f", path(&config_path), "status"]));
    assert_status(
        &repaired,
        expect![
            [r#"HOST             SERVICE                  RUNTIME    SPEC
test             api                      running    in-sync
test             cron                     running    in-sync
test             worker                   running    orphan
"#]
        ],
    );

    assert_success(&distrun(&["-f", path(&config_path), "down"]));
    let stopped = stdout(distrun(&["-f", path(&config_path), "status"]));
    assert_status(
        &stopped,
        expect![
            [r#"HOST             SERVICE                  RUNTIME    SPEC
test             api                      -          missing
test             cron                     -          missing
"#]
        ],
    );

    ssh.run(&format!("rm -rf {remote_dir}"));
}

#[derive(Clone, Debug)]
struct SshTarget {
    target: String,
}

impl SshTarget {
    fn from_env() -> Self {
        let target = env::var("DISTRUN_TEST_SSH_TARGET").expect("DISTRUN_TEST_SSH_TARGET");

        Self { target }
    }

    fn run(&self, remote_command: &str) -> String {
        stdout(
            Command::new("ssh")
                .arg(&self.target)
                .arg(remote_command)
                .output()
                .expect("run ssh"),
        )
    }

    fn read_u64(&self, remote_command: &str) -> u64 {
        self.run(remote_command)
            .trim()
            .parse()
            .expect("remote command should return u64")
    }
}

fn write_config(
    path: &Path,
    ssh: &SshTarget,
    project: &str,
    remote_dir: &str,
    on_existing: &str,
    services: &[String],
) {
    let services = services.join("\n");
    let root_dir = path.parent().expect("config path should have parent");
    let parts_dir = root_dir.join("parts");
    fs::create_dir_all(&parts_dir).expect("create config parts dir");
    let config = format!(
        r#"project: {project}
on_existing: {on_existing}
include:
  - hosts.yml
  - parts/services.yml
include?: distrun.local.yml
"#,
    );
    let hosts = format!(
        r#"hosts:
  test:
    ssh: {target}
"#,
        target = ssh.target,
    );
    let services = format!(
        r#"services:
{services}
"#,
    );

    fs::write(path, config).expect("write config");
    fs::write(root_dir.join("hosts.yml"), hosts).expect("write hosts config");
    fs::write(parts_dir.join("services.yml"), services).expect("write services config");
    ssh.run(&format!("mkdir -p {remote_dir}"));
}

fn service(name: &str, remote_dir: &str) -> String {
    format!(
        r#"  {name}:
    host: test
    env_file: service.env
    cmd: |
      bash -lc 'echo {name}-start >> {remote_dir}/{name}-starts; while true; do echo ${{DISTRUN_TEST_SUFFIX}}-{name}-tick; sleep 1; done'
    stop_timeout: 1s"#
    )
}

fn distrun(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_distrun"))
        .args(args)
        .output()
        .expect("run distrun")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn stdout(output: Output) -> String {
    assert_success(&output);
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn assert_contains(haystack: &str, needle: &str) {
    assert!(
        haystack.contains(needle),
        "expected to find `{needle}` in:\n{haystack}"
    );
}

fn assert_status(actual: &str, expected: Expect) {
    expected.assert_eq(actual);
}

fn path(path: &Path) -> &str {
    path.to_str().expect("test path must be UTF-8")
}

fn unique_id() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis()
}
