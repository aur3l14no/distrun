use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn status_allows_config_without_services() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    fs::create_dir_all(&dir).expect("create temp config dir");
    let config_path = dir.join("distrun.yml");
    fs::write(&config_path, "project: demo\n").expect("write config");

    let output = distrun(&["-f", path(&config_path), "status"]);

    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "HOST             SERVICE                  RUNTIME    SPEC\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn status_accepts_positional_project_override() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    fs::create_dir_all(&dir).expect("create temp config dir");
    let config_path = dir.join("distrun.yml");
    fs::write(&config_path, "").expect("write config");

    let output = distrun(&["-f", path(&config_path), "status", "demo"]);

    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "HOST             SERVICE                  RUNTIME    SPEC\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn logs_accepts_positional_project_override() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("create fake bin dir");
    write_logs_tmux(&bin_dir);
    let config_path = dir.join("distrun.yml");
    fs::write(&config_path, "services:\n  api:\n    cmd: sleep 60\n").expect("write config");

    let output = distrun_with_path(&["-f", path(&config_path), "logs", "api", "demo"], &bin_dir);

    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "hello\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn project_commands_use_local_fallback_without_config_file() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("create fake bin dir");

    write_status_tmux(&bin_dir);
    let status = distrun_with_path_in_dir(&["status", "demo"], &bin_dir, &dir);
    assert_success(&status);
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        "HOST             SERVICE                  RUNTIME    SPEC\n\
         local            db                       running    orphan\n"
    );
    assert_eq!(String::from_utf8_lossy(&status.stderr), "");

    write_logs_tmux(&bin_dir);
    let logs = distrun_with_path_in_dir(&["logs", "api", "demo"], &bin_dir, &dir);
    assert_success(&logs);
    assert_eq!(String::from_utf8_lossy(&logs.stdout), "hello\n");
    assert_eq!(String::from_utf8_lossy(&logs.stderr), "");

    write_down_tmux(&bin_dir);
    let down = distrun_with_path_in_dir(&["down", "demo"], &bin_dir, &dir);
    assert_success(&down);
    assert_eq!(String::from_utf8_lossy(&down.stdout), "local stopped\n");
    assert_eq!(String::from_utf8_lossy(&down.stderr), "");
}

#[test]
fn ps_defaults_to_local_host_without_config_file() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("create fake bin dir");
    write_fake_tmux(&bin_dir);

    let output = distrun_with_path_in_dir(&["ps"], &bin_dir, &dir);

    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         local            demo                     api                      running\n\
         local            old                      worker                   exited\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn ps_lists_managed_sessions_from_manual_local_host() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("create fake bin dir");
    write_fake_tmux(&bin_dir);

    let output = distrun_with_path(&["ps", "--host", "local"], &bin_dir);

    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         local            demo                     api                      running\n\
         local            old                      worker                   exited\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn ps_lists_managed_sessions_from_hosts_only_config() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("create fake bin dir");
    write_fake_tmux(&bin_dir);
    let config_path = dir.join("hosts.yml");
    fs::write(
        &config_path,
        r#"hosts:
  local: {}
"#,
    )
    .expect("write hosts config");

    let output = distrun_with_path(&["ps", "-f", path(&config_path)], &bin_dir);

    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         local            demo                     api                      running\n\
         local            old                      worker                   exited\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn status_marks_timed_out_host_unavailable_and_keeps_available_hosts() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("create fake bin dir");
    write_status_tmux(&bin_dir);
    write_slow_ssh(&bin_dir);

    let config_path = dir.join("distrun.yml");
    fs::write(
        &config_path,
        r#"project: demo
hosts:
  edge:
    ssh: edge
services:
  api:
    host: edge
    cmd: sleep 60
  db:
    cmd: sleep 60
"#,
    )
    .expect("write config");

    let started = Instant::now();
    let output = distrun_with_path(&["-f", path(&config_path), "status"], &bin_dir);

    assert_success(&output);
    assert!(started.elapsed() < Duration::from_secs(7));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stdout,
        "HOST             SERVICE                  RUNTIME    SPEC\n\
         edge             api                      -          unavailable\n\
         local            db                       running    in-sync\n",
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("warning: edge unavailable: command timed out"));
}

#[test]
fn status_rejects_removed_all_flag_before_loading_config() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let config_path = dir.join("missing.yml");

    let output = distrun(&["-f", path(&config_path), "status", "--all"]);

    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--all"));
    assert!(!stderr.contains("failed to read config"));
}

#[test]
fn ps_rejects_project_option_before_loading_config() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let config_path = dir.join("missing.yml");

    let output = distrun(&["-f", path(&config_path), "ps", "--project", "demo"]);

    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--project"));
    assert!(!stderr.contains("failed to read config"));
}

#[test]
fn status_rejects_removed_host_flag_before_loading_config() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let config_path = dir.join("missing.yml");

    let output = distrun(&["-f", path(&config_path), "status", "--host", "local"]);

    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--host"));
    assert!(!stderr.contains("failed to read config"));
}

#[test]
fn up_expands_service_interpolation_from_env_file_and_defaults() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let bin_dir = dir.join("bin");
    let workspace_dir = dir.join("workspace");
    fs::create_dir_all(&bin_dir).expect("create fake bin dir");
    fs::create_dir_all(&workspace_dir).expect("create workspace dir");
    let log_path = dir.join("tmux.log");
    write_recording_tmux(&bin_dir);

    fs::write(
        dir.join("service.env"),
        format!("SERVICE_HOST=local\nWORKSPACE={}\n", path(&workspace_dir)),
    )
    .expect("write env file");
    let config_path = dir.join("distrun.yml");
    fs::write(
        &config_path,
        r#"project: demo
services:
  api:
    host: ${SERVICE_HOST:-local}
    cmd: printf %s ${RUN_ROOT:-/tmp/run}
    cwd: ${WORKSPACE:-/tmp}
    env_file: service.env
    env:
      RUN_ROOT: ${WORKSPACE:-/tmp}/run
  fallback:
    cmd: printf %s ${MISSING:-/tmp}
    cwd: ${MISSING:-/tmp}
"#,
    )
    .expect("write config");

    let output = distrun_with_path_and_env(
        &["-f", path(&config_path), "up"],
        &bin_dir,
        &[("DISTRUN_FAKE_TMUX_LOG", path(&log_path))],
    );

    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "local api started\nlocal fallback started\n"
    );
    let tmux_log = fs::read_to_string(log_path).expect("read tmux log");
    assert!(tmux_log.contains(&format!("cd '{}'", path(&workspace_dir))));
    assert!(tmux_log.contains(&format!("RUN_ROOT='{}/run'", path(&workspace_dir))));
    assert!(tmux_log.contains(&format!(
        "exec sh -lc 'printf %s {}/run'",
        path(&workspace_dir)
    )));
    assert!(tmux_log.contains("cd '/tmp'"));
    assert!(!tmux_log.contains("${WORKSPACE"));
    assert!(!tmux_log.contains("${RUN_ROOT"));
}

#[test]
fn up_uses_service_env_interpolation_priority() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("create fake bin dir");
    let log_path = dir.join("tmux.log");
    write_recording_tmux(&bin_dir);

    fs::write(
        dir.join("service.env"),
        "FROM_FILE=from-file\nOVERRIDE=from-file\n",
    )
    .expect("write env file");
    let self_parent_key = format!("DISTRUN_TEST_SELF_PARENT_{}", unique_id());
    let self_default_key = format!("DISTRUN_TEST_SELF_DEFAULT_{}", unique_id());
    let config_path = dir.join("distrun.yml");
    fs::write(
        &config_path,
        format!(
            r#"project: demo
services:
  api:
    cmd: printf '%s %s %s %s %s' ${{FROM_FILE}} ${{OVERRIDE}} ${{FROM_OTHER}} ${{{self_parent_key}}} ${{{self_default_key}}}
    env_file: service.env
    env:
      OVERRIDE: inline
      FROM_OTHER: ${{OVERRIDE}}
      {self_parent_key}: ${{{self_parent_key}:-default-parent}}
      {self_default_key}: ${{{self_default_key}:-default-value}}
"#
        ),
    )
    .expect("write config");

    let output = distrun_with_path_and_env(
        &["-f", path(&config_path), "up"],
        &bin_dir,
        &[
            ("DISTRUN_FAKE_TMUX_LOG", path(&log_path)),
            ("OVERRIDE", "from-parent"),
            (&self_parent_key, "from-parent"),
        ],
    );

    assert_success(&output);
    let tmux_log = fs::read_to_string(log_path).expect("read tmux log");
    assert!(tmux_log.contains("FROM_FILE='from-file'"));
    assert!(tmux_log.contains("OVERRIDE='inline'"));
    assert!(tmux_log.contains("FROM_OTHER='inline'"));
    assert!(tmux_log.contains(&format!("{self_parent_key}='from-parent'")));
    assert!(tmux_log.contains(&format!("{self_default_key}='default-value'")));
    assert!(tmux_log.contains("from-file inline inline from-parent default-value"));
    assert!(!tmux_log.contains("${"));
}

#[test]
fn tui_help_is_available() {
    let output = distrun(&["tui", "--help"]);

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Open the interactive service dashboard"));
    assert!(stdout.contains("--tail"));
}

#[test]
fn tui_requires_interactive_terminal() {
    let dir = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    fs::create_dir_all(&dir).expect("create temp config dir");
    let config_path = dir.join("distrun.yml");
    fs::write(&config_path, "project: demo\n").expect("write config");

    let output = distrun(&["-f", path(&config_path), "tui"]);

    assert_failure(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("distrun tui requires an interactive terminal")
    );
}

fn distrun(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_distrun"))
        .args(args)
        .output()
        .expect("run distrun")
}

fn distrun_with_path(args: &[&str], bin_dir: &Path) -> Output {
    distrun_with_path_and_env(args, bin_dir, &[])
}

fn distrun_with_path_in_dir(args: &[&str], bin_dir: &Path, current_dir: &Path) -> Output {
    distrun_with_path_env_and_dir(args, bin_dir, &[], Some(current_dir))
}

fn distrun_with_path_and_env(args: &[&str], bin_dir: &Path, envs: &[(&str, &str)]) -> Output {
    distrun_with_path_env_and_dir(args, bin_dir, envs, None)
}

fn distrun_with_path_env_and_dir(
    args: &[&str],
    bin_dir: &Path,
    envs: &[(&str, &str)],
    current_dir: Option<&Path>,
) -> Output {
    let old_path = env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin_dir.to_path_buf()];
    paths.extend(env::split_paths(&old_path));
    let path = env::join_paths(paths).expect("join PATH");

    let mut command = Command::new(env!("CARGO_BIN_EXE_distrun"));
    command.args(args).env("PATH", path);
    for (key, value) in envs {
        command.env(key, value);
    }
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    command.output().expect("run distrun")
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

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "command should fail\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn path(path: &Path) -> &str {
    path.to_str().expect("test path must be UTF-8")
}

fn write_fake_tmux(bin_dir: &Path) {
    let tmux = bin_dir.join("tmux");
    fs::write(
        &tmux,
        r#"#!/bin/sh
if [ "$1" = "list-windows" ] && [ "$2" = "-a" ]; then
    printf '%s\n' \
        'distrun_demo|api|0|0' \
        'distrun_old|worker|1|0' \
        'manual|ignored|0|0' \
        'distrun_demo||0|0'
    exit 0
fi
exit 1
"#,
    )
    .expect("write fake tmux");
    let mut permissions = fs::metadata(&tmux)
        .expect("read fake tmux metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).expect("chmod fake tmux");
}

fn write_status_tmux(bin_dir: &Path) {
    let tmux = bin_dir.join("tmux");
    fs::write(
        &tmux,
        r#"#!/bin/sh
if [ "$1" = "has-session" ]; then
    exit 0
fi
if [ "$1" = "list-windows" ] && [ "$2" = "-t" ]; then
    printf '%s\n' 'distrun_demo|db|0|0'
    exit 0
fi
exit 1
"#,
    )
    .expect("write fake tmux");
    let mut permissions = fs::metadata(&tmux)
        .expect("read fake tmux metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).expect("chmod fake tmux");
}

fn write_logs_tmux(bin_dir: &Path) {
    let tmux = bin_dir.join("tmux");
    fs::write(
        &tmux,
        r#"#!/bin/sh
if [ "$1" = "has-session" ]; then
    exit 0
fi
if [ "$1" = "list-windows" ]; then
    printf '%s\n' '%1|api'
    exit 0
fi
if [ "$1" = "capture-pane" ]; then
    printf '%s\n\n' 'hello'
    exit 0
fi
exit 1
"#,
    )
    .expect("write fake tmux");
    let mut permissions = fs::metadata(&tmux)
        .expect("read fake tmux metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).expect("chmod fake tmux");
}

fn write_slow_ssh(bin_dir: &Path) {
    let ssh = bin_dir.join("ssh");
    fs::write(
        &ssh,
        r#"#!/bin/sh
sleep 10
"#,
    )
    .expect("write fake ssh");
    let mut permissions = fs::metadata(&ssh)
        .expect("read fake ssh metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&ssh, permissions).expect("chmod fake ssh");
}

fn write_down_tmux(bin_dir: &Path) {
    let tmux = bin_dir.join("tmux");
    fs::write(
        &tmux,
        r#"#!/bin/sh
if [ "$1" = "has-session" ]; then
    exit 1
fi
exit 0
"#,
    )
    .expect("write fake tmux");
    let mut permissions = fs::metadata(&tmux)
        .expect("read fake tmux metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).expect("chmod fake tmux");
}

fn write_recording_tmux(bin_dir: &Path) {
    let tmux = bin_dir.join("tmux");
    fs::write(
        &tmux,
        r#"#!/bin/sh
case "$1" in
    has-session)
        exit 1
        ;;
    display-message)
        printf '%%0\n'
        exit 0
        ;;
    new-window)
        printf '%s\n' "$*" >> "$DISTRUN_FAKE_TMUX_LOG"
        printf '%%1\n'
        exit 0
        ;;
    new-session|set-window-option|rename-window)
        exit 0
        ;;
esac
exit 0
"#,
    )
    .expect("write recording tmux");
    let mut permissions = fs::metadata(&tmux)
        .expect("read recording tmux metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).expect("chmod recording tmux");
}

fn unique_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos();
    let counter = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}_{}", std::process::id(), nanos, counter)
}
