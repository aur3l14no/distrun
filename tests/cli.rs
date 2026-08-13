use std::env;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct TestEnv {
    root: PathBuf,
    bin: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let root = env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("create fake bin dir");
        Self { root, bin }
    }

    fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        write_file(&self.root, name, contents)
    }

    fn run(&self, args: &[&str]) -> Output {
        self.execute(args, &[], None)
    }

    fn run_in_root(&self, args: &[&str]) -> Output {
        self.execute(args, &[], Some(&self.root))
    }

    fn run_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        self.execute(args, envs, None)
    }

    fn run_with_env_in_root(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        self.execute(args, envs, Some(&self.root))
    }

    fn run_with_exact_path(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_distrun"))
            .args(args)
            .current_dir(&self.root)
            .env("PATH", &self.bin)
            .env("HOME", &self.root)
            .output()
            .expect("run distrun with isolated PATH")
    }

    fn execute(&self, args: &[&str], envs: &[(&str, &str)], current_dir: Option<&Path>) -> Output {
        let old_path = env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin.clone()];
        paths.extend(env::split_paths(&old_path));

        let mut command = Command::new(env!("CARGO_BIN_EXE_distrun"));
        command
            .args(args)
            .env("PATH", env::join_paths(paths).expect("join PATH"))
            .env("HOME", &self.root)
            .envs(envs.iter().copied());
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }
        command.output().expect("run distrun")
    }
}

#[test]
fn status_allows_config_without_services() {
    let test = TestEnv::new();
    write_down_tmux(&test.bin);
    let config_path = test.write("distrun.yml", "project: demo\n");

    let output = test.run(&["-f", path(&config_path), "status"]);

    assert_success(&output);
    assert_eq!(
        stdout(&output),
        "HOST             SERVICE                  RUNTIME      RELATION     ISSUE\n"
    );
    assert_eq!(stderr(&output), "");
}

#[test]
fn missing_tmux_is_an_unavailable_host_instead_of_missing_runtime_state() {
    let test = TestEnv::new();
    symlink("/bin/sh", test.bin.join("sh")).expect("link shell into isolated PATH");
    let config_path = test.write(
        "distrun.yml",
        "project: demo\nservices:\n  api:\n    cmd: sleep 60\n",
    );

    let status = test.run_with_exact_path(&["-f", path(&config_path), "status", "--timeout", "1s"]);
    assert_failure(&status);
    let status_stdout = stdout(&status);
    assert!(
        status_stdout.contains("api                      unavailable"),
        "{status_stdout}"
    );
    assert!(
        !status_stdout.contains("api                      missing"),
        "{status_stdout}"
    );
    assert!(stderr(&status).contains("tmux is not installed on this host"));

    let list = test.run_with_exact_path(&["list", "--timeout", "1s"]);
    assert_failure(&list);
    assert_eq!(
        stdout(&list),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n"
    );
    assert!(stderr(&list).contains("tmux is not installed on this host"));

    let started = Instant::now();
    let down = test.run_with_exact_path(&["-p", "demo", "down"]);
    assert_failure(&down);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(stderr(&down).contains("tmux is not installed on this host"));
}

#[test]
fn tmux_operational_failures_are_not_treated_as_an_empty_runtime() {
    let test = TestEnv::new();
    write_broken_server_tmux(&test.bin);

    let list = test.run_in_root(&["-p", "demo", "list", "--timeout", "5s"]);
    assert_failure(&list);
    assert_eq!(
        stdout(&list),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n"
    );
    assert!(stderr(&list).contains("tmux socket permission denied"));

    let down = test.run_in_root(&["-p", "demo", "down"]);
    assert_failure(&down);
    assert_eq!(stdout(&down), "");
    let stderr = stderr(&down);
    assert!(stderr.contains("local stop failed"), "{stderr}");
    assert!(stderr.contains("tmux socket permission denied"), "{stderr}");
}

#[test]
fn silent_tmux_failures_are_not_treated_as_absence_or_lock_contention() {
    let test = TestEnv::new();
    let lock_attempts = test.root.join("lock-attempts");
    write_silent_failure_tmux(&test.bin);

    let list = test.run_with_env_in_root(
        &["-p", "demo", "list", "--timeout", "5s"],
        &[("DISTRUN_SILENT_MODE", "query")],
    );
    assert_failure(&list);
    assert!(
        stderr(&list).contains("exit status: 47"),
        "{}",
        stderr(&list)
    );

    let down =
        test.run_with_env_in_root(&["-p", "demo", "down"], &[("DISTRUN_SILENT_MODE", "down")]);
    assert_failure(&down);
    assert!(
        stderr(&down).contains("exit status: 47"),
        "{}",
        stderr(&down)
    );

    let incomplete = test.run_with_env_in_root(
        &["-p", "demo", "list", "--timeout", "5s"],
        &[("DISTRUN_SILENT_MODE", "incomplete")],
    );
    assert_failure(&incomplete);
    assert!(
        stderr(&incomplete).contains("tmux inventory did not complete"),
        "{}",
        stderr(&incomplete)
    );

    let config_path = test.write(
        "distrun.yml",
        "project: demo\nservices:\n  api:\n    cmd: sleep 60\n",
    );
    let up = test.run_with_env(
        &["-f", path(&config_path), "up"],
        &[
            ("DISTRUN_SILENT_MODE", "lock"),
            ("DISTRUN_FAKE_TMUX_LOG", path(&lock_attempts)),
        ],
    );
    assert_failure(&up);
    assert!(stderr(&up).contains("exit status: 47"), "{}", stderr(&up));
    assert_eq!(
        fs::read_to_string(lock_attempts).expect("read lock attempts"),
        "attempt\n"
    );
}

#[test]
fn project_conflicts_with_all_projects_before_loading_host_inventory() {
    let missing = env::temp_dir().join(format!("distrun-missing-{}", unique_id()));
    let output = distrun(&[
        "-p",
        "demo",
        "--hosts-file",
        path(&missing),
        "list",
        "--all-projects",
    ]);

    assert_failure(&output);
    let stderr = stderr(&output);
    assert!(stderr.contains("--all-projects conflicts with --project"));
    assert!(!stderr.contains("failed to read config"));

    let help = distrun(&["list", "--help"]);
    assert_success(&help);
    assert!(stdout(&help).contains("cannot be combined with root --project"));
}

#[test]
fn runtime_project_disables_default_config_discovery() {
    let test = TestEnv::new();
    write_fake_tmux(&test.bin);
    test.write("distrun.yml", "project: old\n");

    let output = test.run_in_root(&["-p", "demo", "list"]);

    assert_success(&output);
    assert_eq!(
        stdout(&output),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         local            demo                     api                      running\n",
        "stderr:\n{}",
        stderr(&output)
    );
    assert_eq!(stderr(&output), "");
}

#[test]
fn default_config_discovery_does_not_search_parent_directories() {
    let test = TestEnv::new();
    write_fake_tmux(&test.bin);
    test.write("distrun.yml", "project: demo\n");
    let child = test.root.join("child");
    fs::create_dir(&child).expect("create child directory");

    let output = test.execute(&["list"], &[], Some(&child));

    assert_stdout(
        &output,
        "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         local            demo                     api                      running\n\
         local            old                      worker                   exited\n",
    );
}

#[test]
fn list_and_aliases_discover_all_local_projects_without_config() {
    let test = TestEnv::new();
    write_fake_tmux(&test.bin);

    let expected = "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         local            demo                     api                      running\n\
         local            old                      worker                   exited\n";
    for command in ["list", "ls", "ps"] {
        let output = test.run_in_root(&[command]);
        assert_stdout(&output, expected);
        assert_eq!(stderr(&output), "");
    }
}

#[test]
fn list_uses_config_project_until_all_projects_is_requested() {
    let test = TestEnv::new();
    write_fake_tmux(&test.bin);
    fs::write(
        test.root.join("distrun.yml"),
        r#"project: demo
services:
  api:
    cmd: sleep 60
"#,
    )
    .expect("write config");

    let current = test.run_in_root(&["list"]);
    assert_success(&current);
    assert_eq!(
        stdout(&current),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         local            demo                     api                      running\n"
    );

    let all = test.run_in_root(&["list", "--all-projects"]);
    assert_success(&all);
    assert_eq!(
        stdout(&all),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         local            demo                     api                      running\n\
         local            old                      worker                   exited\n"
    );
}

#[test]
fn hosts_file_supplies_inventory_and_host_selects_an_alias() {
    let test = TestEnv::new();
    write_fake_tmux(&test.bin);
    let hosts_path = test.write(
        "hosts.yml",
        "project: ignored\nhosts:\n  local: {}\nservices:\n  ignored:\n    env_file: missing.env\n",
    );

    let output = test.run(&["--hosts-file", path(&hosts_path), "--host", "local", "list"]);

    assert_success(&output);
    assert!(stdout(&output).contains("local            demo"));

    let unknown = distrun(&[
        "--hosts-file",
        path(&hosts_path),
        "--host",
        "missing",
        "list",
    ]);
    assert_failure(&unknown);
    assert!(stderr(&unknown).contains("not defined"));
}

#[test]
fn ssh_target_is_the_exact_runtime_observation_scope() {
    let test = TestEnv::new();
    let ssh_log = test.root.join("ssh.log");
    write_fake_tmux(&test.bin);
    write_recording_ssh(&test.bin);

    let output = test.run_with_env(
        &[
            "-p",
            "demo",
            "--ssh",
            "edge-target",
            "list",
            "--timeout",
            "30s",
        ],
        &[("DISTRUN_FAKE_SSH_LOG", path(&ssh_log))],
    );

    assert_success(&output);
    assert_eq!(
        stdout(&output),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         edge-target      demo                     api                      running\n"
    );
    assert_eq!(
        fs::read_to_string(ssh_log).expect("read ssh log"),
        "-- edge-target\n"
    );
}

#[test]
fn ssh_option_like_target_is_passed_after_an_option_terminator() {
    let test = TestEnv::new();
    let ssh_log = test.root.join("ssh.log");
    write_fake_tmux(&test.bin);
    write_recording_ssh(&test.bin);

    let output = test.run_with_env(
        &["-p", "demo", "--ssh=-V", "list", "--timeout", "30s"],
        &[("DISTRUN_FAKE_SSH_LOG", path(&ssh_log))],
    );

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(ssh_log).expect("read ssh argv log"),
        "-- -V\n"
    );
}

#[test]
fn host_without_inventory_selects_local_by_name() {
    let test = TestEnv::new();
    write_fake_tmux(&test.bin);

    let output = test.run_in_root(&["-p", "demo", "--host", "local", "list"]);

    assert_success(&output);
    assert_eq!(
        stdout(&output),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         local            demo                     api                      running\n",
        "stderr:\n{}",
        stderr(&output)
    );
}

#[test]
fn nonlocal_host_name_requires_an_inventory() {
    let output = distrun(&["-p", "demo", "--host", "edge", "list"]);

    assert_failure(&output);
    let stderr = stderr(&output);
    assert!(stderr.contains("host `edge` requires --hosts-file"));
    assert!(stderr.contains("use --ssh edge"));
}

#[test]
fn empty_hosts_inventory_is_rejected() {
    let dir = TestEnv::new().root;
    let hosts_path = write_file(&dir, "hosts.yml", "hosts: {}\n");

    let output = distrun(&["-p", "demo", "--hosts-file", path(&hosts_path), "list"]);

    assert_failure(&output);
    assert!(stderr(&output).contains("runtime host scope is empty"));
}

#[test]
fn ad_hoc_ssh_targets_do_not_mix_with_a_hosts_inventory() {
    let output = distrun(&[
        "-p",
        "demo",
        "--hosts-file",
        "hosts.yml",
        "--ssh",
        "edge",
        "list",
    ]);

    assert_failure(&output);
    let stderr = stderr(&output);
    assert!(stderr.contains("--hosts-file"));
    assert!(stderr.contains("--ssh"));
    assert!(stderr.contains("cannot be used with"));
}

#[test]
fn subcommand_help_and_errors_show_where_root_options_belong() {
    let help = distrun(&["list", "--help"]);

    assert_success(&help);
    let stdout = stdout(&help);
    assert!(stdout.contains("Usage: distrun [ROOT OPTIONS] list [OPTIONS]"));
    assert!(
        stdout.contains("Root options (-f/--file, -p/--project, --hosts-file, --host, and --ssh)")
    );
    assert!(stdout.contains("must appear before COMMAND"));

    let misplaced = distrun(&["list", "--project", "demo"]);
    assert_failure(&misplaced);
    let stderr = stderr(&misplaced);
    assert!(stderr.contains("Usage: distrun [ROOT OPTIONS] list [OPTIONS]"));
}

#[test]
fn config_only_command_help_explains_its_valid_context() {
    for (command, usage) in [
        ("up", "Usage: distrun [-f FILE] up [SERVICE]..."),
        ("recreate", "Usage: distrun [-f FILE] recreate [SERVICE]..."),
        ("status", "Usage: distrun [-f FILE] status [OPTIONS]"),
    ] {
        let help = distrun(&[command, "--help"]);

        assert_success(&help);
        let stdout = stdout(&help);
        assert!(stdout.contains(usage), "command={command}\n{stdout}");
        assert!(
            stdout.contains("requires a complete configuration"),
            "command={command}\n{stdout}"
        );
        assert!(
            stdout.contains("./distrun.yml"),
            "command={command}\n{stdout}"
        );
    }
}

#[test]
fn action_command_help_explains_its_project_context() {
    for (command, usage) in [
        ("down", "Usage: distrun [ROOT OPTIONS] down"),
        (
            "stop",
            "Usage: distrun [ROOT OPTIONS] stop [OPTIONS] <[HOST/]SERVICE>...",
        ),
        (
            "logs",
            "Usage: distrun [ROOT OPTIONS] logs [OPTIONS] <[HOST/]SERVICE>",
        ),
    ] {
        let help = distrun(&[command, "--help"]);

        assert_success(&help);
        let stdout = stdout(&help);
        assert!(stdout.contains(usage), "command={command}\n{stdout}");
        assert!(
            stdout.contains("requires a project"),
            "command={command}\n{stdout}"
        );
        assert!(
            stdout.contains("-p/--project PROJECT"),
            "command={command}\n{stdout}"
        );
    }
}

#[test]
fn logs_help_explains_timeout_scope() {
    let help = distrun(&["logs", "--help"]);

    assert_success(&help);
    assert!(stdout(&help).contains("service resolution timeout; finite log reads also use it"));
}

#[test]
fn config_host_scope_is_independent_of_explicit_or_implicit_file_selection() {
    let test = TestEnv::new();
    let host_log = test.root.join("hosts.log");
    write_host_scope_probe(&test.bin);
    let config_path = test.write(
        "distrun.yml",
        r#"project: demo
hosts:
  edge:
    ssh: edge-target
services:
  api:
    host: edge
    cmd: sleep 60
"#,
    );

    let explicit = test.run_with_env_in_root(
        &["-f", path(&config_path), "down"],
        &[("DISTRUN_FAKE_HOST_LOG", path(&host_log))],
    );
    let implicit =
        test.run_with_env_in_root(&["down"], &[("DISTRUN_FAKE_HOST_LOG", path(&host_log))]);

    assert_success(&explicit);
    assert_success(&implicit);
    assert_eq!(explicit.stdout, implicit.stdout);
    assert_eq!(stdout(&explicit), "edge stopped\n");
    assert_eq!(
        fs::read_to_string(host_log).expect("read host log"),
        "edge-target\nedge-target\n"
    );
}

#[test]
fn hosts_file_down_uses_only_selected_aliases() {
    let test = TestEnv::new();
    let host_log = test.root.join("hosts.log");
    write_host_scope_probe(&test.bin);
    let hosts_path = test.write(
        "hosts.yml",
        r#"hosts:
  edge:
    ssh: edge-target
  gpu:
    ssh: gpu-target
"#,
    );

    let output = test.run_with_env(
        &[
            "-p",
            "demo",
            "--hosts-file",
            path(&hosts_path),
            "--host",
            "edge",
            "--host",
            "edge",
            "down",
        ],
        &[("DISTRUN_FAKE_HOST_LOG", path(&host_log))],
    );

    assert_stdout(&output, "edge stopped\n");
    assert_eq!(
        fs::read_to_string(host_log).expect("read host log"),
        "edge-target\n"
    );
}

#[test]
fn repeated_ssh_targets_define_an_exact_deduplicated_down_scope() {
    let test = TestEnv::new();
    let host_log = test.root.join("hosts.log");
    write_host_scope_probe(&test.bin);

    let output = test.run_with_env(
        &[
            "-p",
            "demo",
            "--ssh",
            "edge-target",
            "--ssh",
            "edge-target",
            "down",
        ],
        &[("DISTRUN_FAKE_HOST_LOG", path(&host_log))],
    );

    assert_stdout(&output, "edge-target stopped\n");
    assert_eq!(
        fs::read_to_string(host_log).expect("read host log"),
        "edge-target\n"
    );
}

#[test]
fn logs_tail_returns_the_exact_requested_line_count() {
    let test = TestEnv::new();
    write_captured_logs_tmux(&test.bin, "one\ntwo\nthree\n\n\n");

    let two = test.run_in_root(&["-p", "demo", "logs", "api", "-n", "2"]);
    assert_stdout(&two, "two\nthree\n");

    let zero = test.run_in_root(&["-p", "demo", "logs", "api", "-n", "0"]);
    assert_stdout(&zero, "");
}

#[test]
fn finite_logs_reject_a_running_runtime_with_a_detached_transcript_pipe() {
    let test = TestEnv::new();
    write_persistent_logs_tmux(&test.bin, "stale output\n", false, false);

    let output = test.run_in_root(&["-p", "demo", "logs", "api"]);

    assert_failure(&output);
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("runtime log stream is not attached; recreate the service"));
}

#[test]
fn finite_logs_wait_for_an_exited_runtime_transcript_to_drain() {
    let test = TestEnv::new();
    let pipe_state = test.write("pipe-open", "1\n");
    write_draining_logs_tmux(&test.bin);

    let output = test.run_with_env_in_root(
        &["-p", "demo", "logs", "api", "--timeout", "5s"],
        &[("DISTRUN_FAKE_PIPE_STATE", path(&pipe_state))],
    );

    assert_stdout(&output, "initial\nfinal\n");
}

#[test]
fn logs_and_logs_follow_both_reject_duplicate_runtime_instances() {
    let test = TestEnv::new();
    write_duplicate_tmux(&test.bin);

    let logs = test.run_in_root(&["-p", "demo", "logs", "api"]);
    let follow = test.run_in_root(&["-p", "demo", "logs", "api", "--follow"]);

    for output in [logs, follow] {
        assert_failure(&output);
        assert_eq!(stdout(&output), "");
        assert!(
            stderr(&output)
                .contains("runtime service `api` has 2 instances; logs requires exactly one")
        );
    }
}

#[test]
fn logs_follow_streams_repeated_lines_and_stops_when_the_pane_exits() {
    let test = TestEnv::new();
    let state_path = test.root.join("pane-running");
    fs::write(&state_path, "running\n").expect("write pane state");
    write_streaming_logs_tmux(&test.bin);

    let output = test.run_with_env(
        &["-p", "demo", "logs", "api", "-f", "-n", "2"],
        &[("DISTRUN_FAKE_TMUX_STATE", path(&state_path))],
    );

    assert_stdout(&output, "previous\ninitial\nsame\nsame\n");
}

#[test]
fn stop_failure_is_reported_and_preserves_the_runtime_log() {
    assert_failed_operation_preserves_logs(
        &["-p", "demo", "stop", "api"],
        &[],
        "local api stop failed",
    );
}

#[test]
fn exited_keeper_lifecycle_commands_do_not_wait_for_stop_timeout() {
    for args in [
        vec!["-p", "demo", "stop", "api", "--timeout", "5s"],
        vec!["-p", "demo", "down"],
    ] {
        let test = TestEnv::new();
        let tmux_log = test.root.join("tmux.log");
        write_exited_keeper_tmux(&test.bin);

        let started = Instant::now();
        let output = test.run_with_env(&args, &[("DISTRUN_FAKE_TMUX_LOG", path(&tmux_log))]);

        assert_success(&output);
        assert!(started.elapsed() < Duration::from_secs(4), "{args:?}");
        assert!(
            !fs::read_to_string(&tmux_log)
                .expect("read tmux call log")
                .contains("send-keys"),
            "keeper pane must not receive an interrupt: {args:?}"
        );
    }
}

#[test]
fn stop_probe_failure_is_reported_and_preserves_the_runtime_log() {
    assert_failed_operation_preserves_logs(
        &["-p", "demo", "stop", "api"],
        &[("DISTRUN_FAIL_PROBE", "1")],
        "failed to verify runtime identity before stopping",
    );
}

#[test]
fn stop_reports_when_the_resolved_runtime_was_replaced_before_mutation() {
    assert_failed_operation_preserves_logs(
        &["-p", "demo", "stop", "api"],
        &[("DISTRUN_CHANGED_IDENTITY", "1")],
        "runtime instance changed before stopping",
    );
}

#[test]
fn down_failure_is_reported_and_preserves_the_project_logs() {
    assert_failed_operation_preserves_logs(&["-p", "demo", "down"], &[], "local stop failed");
}

fn assert_failed_operation_preserves_logs(
    args: &[&str],
    envs: &[(&str, &str)],
    expected_error: &str,
) {
    let test = TestEnv::new();
    let log_path = write_failing_stop_tmux(&test.bin);

    let output = test.run_with_env_in_root(args, envs);

    assert_failure(&output);
    assert!(stderr(&output).contains(expected_error));
    assert!(log_path.exists(), "failed operation must preserve logs");
}

#[test]
fn down_removes_only_logs_owned_by_the_selected_tmux_server() {
    let test = TestEnv::new();
    write_project_log_cleanup_tmux(&test.bin);

    let project_logs = test.root.join(".local/state/distrun/logs/demo");
    let managed = project_logs.join("api.Ab12Cd");
    let other_server = project_logs.join("api.Zz99Yy");
    fs::create_dir_all(&managed).expect("create managed runtime logs");
    fs::create_dir_all(&other_server).expect("create other tmux server logs");
    fs::write(managed.join("pty.log"), "managed\n").expect("write managed log");
    fs::write(other_server.join("pty.log"), "other server\n").expect("write other server log");

    let output = test.run_in_root(&["-p", "demo", "down"]);

    assert_success(&output);
    assert!(!managed.exists(), "selected runtime logs should be removed");
    assert!(
        other_server.exists(),
        "another tmux server's runtime logs must remain intact"
    );
}

#[test]
fn up_preserves_logs_owned_by_another_tmux_server() {
    let test = TestEnv::new();
    write_recording_tmux(&test.bin);
    fs::write(
        test.root.join("distrun.yml"),
        "project: demo\nservices:\n  api:\n    cmd: sleep 60\n",
    )
    .expect("write config");

    let other_server = test.root.join(".local/state/distrun/logs/demo/api.Zz99Yy");
    fs::create_dir_all(&other_server).expect("create other tmux server logs");
    fs::write(other_server.join("pty.log"), "still running\n").expect("write other server log");
    let tmux_log = test.root.join("tmux.log");

    let output = test.run_with_env_in_root(
        &["up", "api"],
        &[("DISTRUN_FAKE_TMUX_LOG", path(&tmux_log))],
    );

    assert_success(&output);
    assert!(
        other_server.exists(),
        "starting on one tmux server must preserve another server's runtime logs"
    );
}

#[test]
fn up_replaces_a_window_whose_managed_pane_has_disappeared() {
    let test = TestEnv::new();
    let stale_killed = test.root.join("stale-killed");
    write_stale_pane_tmux(&test.bin, StalePane::Missing);
    fs::write(
        test.root.join("distrun.yml"),
        "project: demo\nservices:\n  api:\n    cmd: sleep 60\n",
    )
    .expect("write config");

    let output = test.run_with_env_in_root(
        &["up", "api"],
        &[("DISTRUN_STALE_KILLED", path(&stale_killed))],
    );

    assert_stdout(&output, "local api started\n");
    assert!(
        stale_killed.exists(),
        "up must remove a window whose managed pane no longer exists"
    );
}

#[test]
fn up_replaces_a_managed_window_whose_start_never_became_ready() {
    let test = TestEnv::new();
    let stale_killed = test.root.join("stale-killed");
    write_stale_pane_tmux(&test.bin, StalePane::Unready);
    fs::write(
        test.root.join("distrun.yml"),
        "project: demo\nservices:\n  api:\n    cmd: sleep 60\n",
    )
    .expect("write config");

    let output = test.run_with_env_in_root(
        &["up", "api"],
        &[("DISTRUN_STALE_KILLED", path(&stale_killed))],
    );

    assert_stdout(&output, "local api started\n");
    assert!(
        stale_killed.exists(),
        "an uncommitted managed start must be replaced"
    );
}

#[test]
fn list_skips_malformed_unmanaged_tmux_records() {
    let test = TestEnv::new();
    write_malformed_inventory_tmux(&test.bin);

    let output = test.run_in_root(&["list"]);

    assert_success(&output);
    assert_eq!(
        stdout(&output),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         local            demo                     api                      running\n",
        "stderr:\n{}",
        stderr(&output)
    );
}

#[test]
fn status_marks_every_same_name_runtime_instance_as_duplicate() {
    let test = TestEnv::new();
    write_duplicate_tmux(&test.bin);
    let config_path = test.write(
        "distrun.yml",
        "project: demo\nservices:\n  api:\n    cmd: sleep 60\n",
    );

    let output = test.run(&["-f", path(&config_path), "status"]);

    assert_success(&output);
    let status_stdout = stdout(&output);
    let api_lines = status_stdout
        .lines()
        .filter(|line| line.contains("api"))
        .collect::<Vec<_>>();
    assert_eq!(api_lines.len(), 2, "{status_stdout}");
    assert!(
        api_lines[0].ends_with("configured   duplicate:run1"),
        "{status_stdout}"
    );
    assert!(
        api_lines[1].ends_with("configured   duplicate:run2"),
        "{status_stdout}"
    );

    let list = test.run_in_root(&["-p", "demo", "list"]);
    assert_success(&list);
    let list = stdout(&list);
    assert!(list.contains("api [run1]"), "{list}");
    assert!(list.contains("api [run2]"), "{list}");
}

#[test]
fn status_marks_timed_out_host_unavailable_and_keeps_available_hosts() {
    let test = TestEnv::new();
    write_status_tmux(&test.bin);
    write_slow_ssh(&test.bin);

    let config_path = test.write(
        "distrun.yml",
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
    );

    let started = Instant::now();
    let output = test.run(&["-f", path(&config_path), "status"]);

    assert_failure(&output);
    assert!(started.elapsed() < Duration::from_secs(10));
    let stdout = stdout(&output);
    let stderr = stderr(&output);
    assert_eq!(
        stdout,
        "HOST             SERVICE                  RUNTIME      RELATION     ISSUE\n\
         edge             api                      unavailable  configured   -\n\
         local            db                       running      configured   -\n",
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("warning: edge unavailable: command timed out"));
    assert!(stderr.contains("error: 1 host(s) unavailable"));
}

#[test]
fn list_keeps_available_rows_and_fails_when_a_selected_host_is_unavailable() {
    let test = TestEnv::new();
    write_fake_tmux(&test.bin);
    write_failing_ssh(&test.bin);

    let output = test.run_in_root(&[
        "-p",
        "demo",
        "--host",
        "local",
        "--ssh",
        "edge",
        "list",
        "--timeout",
        "5s",
    ]);

    assert_failure(&output);
    assert_eq!(
        stdout(&output),
        "HOST             PROJECT                  SERVICE                  RUNTIME\n\
         local            demo                     api                      running\n",
        "stderr:\n{}",
        stderr(&output)
    );
    let stderr = stderr(&output);
    assert!(stderr.contains("warning: edge unavailable:"));
    assert!(stderr.contains("error: 1 host(s) unavailable"));
}

#[test]
fn unqualified_logs_fails_on_a_slow_host_before_reading_logs() {
    let test = TestEnv::new();
    let tmux_log = test.root.join("tmux.log");
    write_selector_tmux(&test.bin);
    write_slow_ssh(&test.bin);
    let hosts_path = test.write(
        "hosts.yml",
        "hosts:\n  local: {}\n  edge:\n    ssh: edge-target\n",
    );

    let started = Instant::now();
    let output = test.run_with_env(
        &[
            "-p",
            "demo",
            "--hosts-file",
            path(&hosts_path),
            "logs",
            "api",
            "--timeout",
            "1s",
        ],
        &[("DISTRUN_FAKE_TMUX_LOG", path(&tmux_log))],
    );

    assert_failure(&output);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(stdout(&output), "");
    let stderr = stderr(&output);
    assert!(stderr.contains("host `edge` is unavailable"));
    assert!(stderr.contains("command timed out"));
    let tmux_log = fs::read_to_string(tmux_log).unwrap_or_default();
    assert!(!tmux_log.contains("capture-pane"));
}

#[test]
fn follow_on_an_exited_service_falls_back_to_a_finite_log_read() {
    let test = TestEnv::new();
    write_captured_logs_tmux(&test.bin, "final\n\n");

    let output = test.run_in_root(&["-p", "demo", "logs", "api", "--follow", "--timeout", "5s"]);

    assert_stdout(&output, "final\n");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("service `api` is exited; showing available logs without following"),
        "{stderr}"
    );
}

#[test]
fn stop_timeout_is_configurable_for_runtime_resolution() {
    let test = TestEnv::new();
    write_slow_ssh(&test.bin);
    let hosts_path = test.write("hosts.yml", "hosts:\n  edge:\n    ssh: edge-target\n");

    let started = Instant::now();
    let output = test.run(&[
        "-p",
        "demo",
        "--hosts-file",
        path(&hosts_path),
        "stop",
        "edge/api",
        "--timeout",
        "100ms",
    ]);

    assert_failure(&output);
    assert!(started.elapsed() < Duration::from_secs(2));
    let stderr = stderr(&output);
    assert!(stderr.contains("edge api observe failed"));
    assert!(stderr.contains("command timed out"));
}

#[test]
fn complete_config_and_runtime_selectors_are_mutually_exclusive() {
    let dir = TestEnv::new().root;
    let config_path = dir.join("missing.yml");

    let project = distrun(&["-f", path(&config_path), "-p", "demo", "status"]);
    assert_failure(&project);
    let project_stderr = stderr(&project);
    assert!(project_stderr.contains("cannot be used with"));
    assert!(!project_stderr.contains("failed to read config"));

    let host = distrun(&["-f", path(&config_path), "--host", "local", "status"]);
    assert_failure(&host);
    let host_stderr = stderr(&host);
    assert!(host_stderr.contains("--host"));
    assert!(!host_stderr.contains("failed to read config"));

    let ssh = distrun(&["-f", path(&config_path), "--ssh", "edge", "status"]);
    assert_failure(&ssh);
    let ssh_stderr = stderr(&ssh);
    assert!(ssh_stderr.contains("--ssh"));
    assert!(!ssh_stderr.contains("failed to read config"));
}

#[test]
fn config_only_commands_reject_runtime_context() {
    for command in ["up", "recreate", "status"] {
        let output = distrun(&["-p", "demo", command]);
        assert_failure(&output);
        assert!(stderr(&output).contains("requires a configuration file"));
    }
}

#[test]
fn root_file_and_logs_follow_short_flags_coexist() {
    let test = TestEnv::new();
    write_persistent_logs_tmux(&test.bin, "hello\n", true, false);
    let config_path = test.write(
        "distrun.yml",
        "project: demo\nservices:\n  api:\n    cmd: sleep 60\n",
    );

    let output = test.run(&["-f", path(&config_path), "logs", "api", "-f", "-n", "1"]);

    assert_stdout(&output, "hello\n");
}

#[test]
fn up_expands_service_interpolation_from_env_file_and_defaults() {
    let test = TestEnv::new();
    let workspace_dir = test.root.join("workspace");
    fs::create_dir_all(&workspace_dir).expect("create workspace dir");
    let log_path = test.root.join("tmux.log");
    write_recording_tmux(&test.bin);

    fs::write(
        test.root.join("service.env"),
        format!("SERVICE_HOST=local\nWORKSPACE={}\n", path(&workspace_dir)),
    )
    .expect("write env file");
    let config_path = test.write(
        "distrun.yml",
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
    );

    let output = test.run_with_env(
        &["-f", path(&config_path), "up"],
        &[("DISTRUN_FAKE_TMUX_LOG", path(&log_path))],
    );

    assert_success(&output);
    assert_eq!(
        stdout(&output),
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
    let test = TestEnv::new();
    let log_path = test.root.join("tmux.log");
    write_recording_tmux(&test.bin);

    fs::write(
        test.root.join("service.env"),
        "FROM_FILE=from-file\nOVERRIDE=from-file\n",
    )
    .expect("write env file");
    let self_parent_key = format!("DISTRUN_TEST_SELF_PARENT_{}", unique_id());
    let self_default_key = format!("DISTRUN_TEST_SELF_DEFAULT_{}", unique_id());
    let config_path = test.write(
        "distrun.yml",
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
    );

    let output = test.run_with_env(
        &["-f", path(&config_path), "up"],
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
fn up_only_starts_selected_configured_services() {
    let test = TestEnv::new();
    let log_path = test.root.join("tmux.log");
    write_recording_tmux(&test.bin);
    let config_path = test.write(
        "distrun.yml",
        r#"project: demo
services:
  api:
    cmd: sleep 60
  worker:
    cmd: sleep 60
"#,
    );

    let output = test.run_with_env(
        &["-f", path(&config_path), "up", "api"],
        &[("DISTRUN_FAKE_TMUX_LOG", path(&log_path))],
    );

    assert_stdout(&output, "local api started\n");
    let tmux_log = fs::read_to_string(log_path).expect("read tmux log");
    assert!(tmux_log.contains("-n api"));
    assert!(!tmux_log.contains("-n worker"));
}

#[test]
fn recreate_reports_started_when_the_selected_service_was_missing() {
    let test = TestEnv::new();
    let log_path = test.root.join("tmux.log");
    write_recording_tmux(&test.bin);
    let config_path = test.write(
        "distrun.yml",
        "project: demo\nservices:\n  api:\n    cmd: sleep 60\n",
    );

    let output = test.run_with_env(
        &["-f", path(&config_path), "recreate", "api"],
        &[("DISTRUN_FAKE_TMUX_LOG", path(&log_path))],
    );

    assert_stdout(&output, "local api started\n");
    assert!(
        fs::read_to_string(log_path)
            .expect("read tmux log")
            .contains("-n api")
    );
}

#[test]
fn up_restarts_only_the_selected_running_service_when_configured() {
    let test = TestEnv::new();
    let tmux_log = test.root.join("tmux.log");
    write_lifecycle_tmux(&test.bin);
    let config_path = test.write(
        "distrun.yml",
        r#"project: demo
on_existing: restart
services:
  api:
    cmd: sleep 60
  worker:
    cmd: sleep 60
"#,
    );

    let output = test.run_with_env(
        &["-f", path(&config_path), "up", "api"],
        &[("DISTRUN_FAKE_TMUX_LOG", path(&tmux_log))],
    );

    assert_stdout(
        &output,
        "local api restarted\nlocal old-worker orphan running\n",
    );
    let tmux_log = fs::read_to_string(tmux_log).expect("read tmux log");
    assert!(tmux_log.lines().any(|line| line == "kill-window -t @1"));
    assert!(
        tmux_log
            .lines()
            .any(|line| line.starts_with("new-window ") && line.contains(" -n api "))
    );
    assert!(!tmux_log.lines().any(|line| line == "kill-window -t @2"));
    assert!(!tmux_log.contains(" -n worker "));
}

#[test]
fn up_reports_partial_success_and_continues_after_a_service_failure() {
    let test = TestEnv::new();
    let tmux_log = test.root.join("tmux.log");
    write_recording_tmux(&test.bin);
    let config_path = test.write(
        "distrun.yml",
        r#"project: demo
services:
  alpha:
    cmd: printf alpha; sleep 60
  bravo:
    cmd: printf bravo; sleep 60
  charlie:
    cmd: printf charlie; sleep 60
"#,
    );

    let output = test.run_with_env(
        &["-f", path(&config_path), "up"],
        &[
            ("DISTRUN_FAKE_TMUX_LOG", path(&tmux_log)),
            ("DISTRUN_FAKE_FAIL_SERVICE", "bravo"),
        ],
    );

    assert_failure(&output);
    assert_eq!(
        stdout(&output),
        "local alpha started\nlocal charlie started\n"
    );
    let stderr = stderr(&output);
    assert!(stderr.contains("local bravo start failed"), "{stderr}");
    assert!(stderr.contains("error: 1 operation(s) failed"), "{stderr}");
    let tmux_log = fs::read_to_string(tmux_log).expect("read tmux log");
    let bravo = tmux_log.find("printf bravo").expect("bravo attempted");
    let charlie = tmux_log.find("printf charlie").expect("charlie attempted");
    assert!(charlie > bravo, "later service should still be attempted");
}

#[test]
fn recreate_only_mutates_the_selected_configured_service() {
    let test = TestEnv::new();
    let tmux_log = test.root.join("tmux.log");
    write_lifecycle_tmux(&test.bin);
    let config_path = test.write(
        "distrun.yml",
        r#"project: demo
services:
  api:
    cmd: sleep 60
  worker:
    cmd: sleep 60
"#,
    );

    let output = test.run_with_env(
        &["-f", path(&config_path), "recreate", "api"],
        &[("DISTRUN_FAKE_TMUX_LOG", path(&tmux_log))],
    );

    assert_stdout(&output, "local api recreated\n");
    let tmux_log = fs::read_to_string(tmux_log).expect("read tmux log");
    assert!(tmux_log.lines().any(|line| line == "kill-window -t @1"));
    assert!(
        tmux_log
            .lines()
            .any(|line| line.starts_with("new-window ") && line.contains(" -n api "))
    );
    assert!(!tmux_log.lines().any(|line| line == "kill-window -t @2"));
    assert!(!tmux_log.lines().any(|line| line == "kill-window -t @3"));
    assert!(!tmux_log.contains(" -n worker "));
    assert!(!tmux_log.contains(" -n old-worker "));
}

#[test]
fn stop_can_target_an_observed_orphan_from_config_context() {
    let test = TestEnv::new();
    let tmux_log = test.root.join("tmux.log");
    write_orphan_tmux(&test.bin);
    let config_path = test.write(
        "distrun.yml",
        "project: demo\nservices:\n  api:\n    cmd: sleep 60\n",
    );

    let output = test.run_with_env(
        &["-f", path(&config_path), "stop", "old-worker"],
        &[("DISTRUN_FAKE_TMUX_LOG", path(&tmux_log))],
    );

    assert_stdout(&output, "local old-worker stopped\n");
    assert!(
        fs::read_to_string(tmux_log)
            .expect("read tmux log")
            .contains("kill-window")
    );
}

#[test]
fn stop_resolves_every_selector_before_mutating_runtime() {
    let test = TestEnv::new();
    let tmux_log = test.root.join("tmux.log");
    write_lifecycle_tmux(&test.bin);
    let config_path = test.write("distrun.yml", "project: demo\nhosts:\n  local: {}\n");

    let output = test.run_with_env(
        &["-f", path(&config_path), "stop", "api", "missing"],
        &[("DISTRUN_FAKE_TMUX_LOG", path(&tmux_log))],
    );

    assert_failure(&output);
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("runtime service `missing` was not found"));
    let tmux_log = fs::read_to_string(tmux_log).expect("read tmux log");
    assert!(tmux_log.lines().any(|line| line.starts_with("list-panes ")));
    assert!(!tmux_log.contains("kill-window"));
    assert!(!tmux_log.contains("send-keys"));
}

#[test]
fn qualified_runtime_selector_resolves_an_ambiguous_service() {
    let test = TestEnv::new();
    write_ambiguous_runtime(&test.bin);
    let hosts_path = test.write(
        "hosts.yml",
        "hosts:\n  local: {}\n  edge:\n    ssh: edge-target\n",
    );
    let root = ["-p", "demo", "--hosts-file", path(&hosts_path), "logs"];

    let ambiguous = test.run(&[root[0], root[1], root[2], root[3], root[4], "api"]);
    assert_failure(&ambiguous);
    let stderr = stderr(&ambiguous);
    assert!(stderr.contains("ambiguous"));
    assert!(stderr.contains("`edge/api`"));
    assert!(stderr.contains("`local/api`"));

    let qualified = test.run(&[root[0], root[1], root[2], root[3], root[4], "edge/api"]);
    assert_stdout(&qualified, "edge-target log\n");
}

#[test]
fn explicit_empty_config_has_no_destructive_local_scope() {
    let test = TestEnv::new();
    let state_path = test.root.join("tmux.state");
    fs::write(&state_path, "running\n").expect("write tmux state");
    write_recreate_tmux(&test.bin);
    let config_path = test.write("distrun.yml", "project: demo\n");

    let output = test.run_with_env(
        &["-f", path(&config_path), "recreate"],
        &[("DISTRUN_FAKE_TMUX_STATE", path(&state_path))],
    );

    assert_stdout(&output, "");
    assert!(state_path.exists(), "local project must remain untouched");
}

#[test]
fn tui_help_is_available() {
    let output = distrun(&["tui", "--help"]);

    assert_success(&output);
    let stdout = stdout(&output);
    assert!(stdout.contains("Open the read-only runtime and log browser"));
    assert!(stdout.contains("--all-projects"));
    assert!(stdout.contains("--tail"));
    assert!(stdout.contains("--timeout"));
}

#[test]
fn tui_requires_interactive_terminal() {
    let dir = TestEnv::new().root;
    let config_path = write_file(&dir, "distrun.yml", "project: demo\n");

    let output = distrun(&["-f", path(&config_path), "tui"]);

    assert_failure(&output);
    assert!(stderr(&output).contains("distrun tui requires an interactive terminal"));
}

#[test]
fn real_tmux_up_starts_a_project_on_a_fresh_server() {
    let Some((dir, socket_dir)) = cold_tmux_environment() else {
        return;
    };
    let project = format!("cold_{}", unique_id());
    let config_path = write_file(
        &dir,
        "distrun.yml",
        format!(
            "project: {project}\nservices:\n  api:\n    cmd: sleep 60\n    stop_timeout: 50ms\n"
        ),
    );

    let up = real_distrun_command(&["-f", path(&config_path), "up"], &dir, &socket_dir)
        .output()
        .expect("start project on a fresh tmux server");
    assert_stdout(&up, "local api started\n");

    let present = real_tmux_command(
        &["has-session", "-t", &format!("=distrun/{project}")],
        &dir,
        &socket_dir,
    )
    .output()
    .expect("check newly created project session");
    assert_success(&present);

    let down = real_distrun_command(&["-p", &project, "down"], &dir, &socket_dir)
        .output()
        .expect("clean up cold-start project");
    assert_stdout(&down, "local stopped\n");

    let repeated_down = real_distrun_command(&["-p", &project, "down"], &dir, &socket_dir)
        .output()
        .expect("repeat down without a tmux server");
    assert_stdout(&repeated_down, "local stopped\n");
}

#[test]
fn real_tmux_start_is_atomic_and_transcript_logs_allow_multiple_followers() {
    let Some((dir, socket_dir)) = isolated_tmux_environment() else {
        return;
    };
    let project = format!("real_{}", unique_id());
    let config_path = write_file(
        &dir,
        "distrun.yml",
        format!(
            r#"project: {project}
services:
  api:
    cmd: printf 'first-line\n'; sleep 1; printf 'second-line\n'; sleep 1; printf 'third-line\n'; sleep 1
    stop_timeout: 50ms
"#
        ),
    );

    let first = real_distrun_command(&["-f", path(&config_path), "up", "api"], &dir, &socket_dir)
        .spawn()
        .expect("spawn first up");
    let second = real_distrun_command(&["-f", path(&config_path), "up", "api"], &dir, &socket_dir)
        .spawn()
        .expect("spawn concurrent up");
    let first = first.wait_with_output().expect("wait first up");
    let second = second.wait_with_output().expect("wait concurrent up");

    assert_success(&first);
    assert_success(&second);
    let starts = format!("{}{}", stdout(&first), stdout(&second));
    assert_eq!(starts.matches(" started\n").count(), 1, "{starts}");
    assert_eq!(starts.matches(" skipped\n").count(), 1, "{starts}");

    let list = real_distrun_command(&["-p", &project, "list"], &dir, &socket_dir)
        .output()
        .expect("list real runtime");
    assert_success(&list);
    assert_eq!(
        stdout(&list)
            .lines()
            .filter(|line| line.contains(" api "))
            .count(),
        1
    );

    let initial = real_distrun_command(
        &["-p", &project, "logs", "api", "-n", "10"],
        &dir,
        &socket_dir,
    )
    .output()
    .expect("read first output");
    assert_success(&initial);
    assert!(stdout(&initial).contains("first-line"));

    let follower_args = ["-p", project.as_str(), "logs", "api", "-f", "-n", "0"];
    let first_follower = real_distrun_command(&follower_args, &dir, &socket_dir)
        .spawn()
        .expect("spawn first follower");
    let second_follower = real_distrun_command(&follower_args, &dir, &socket_dir)
        .spawn()
        .expect("spawn second follower");
    let first_follower = first_follower
        .wait_with_output()
        .expect("wait first follower");
    let second_follower = second_follower
        .wait_with_output()
        .expect("wait second follower");

    for follower in [&first_follower, &second_follower] {
        assert_success(follower);
        let logs = stdout(follower);
        assert!(logs.contains("second-line"), "follower logs:\n{logs}");
        assert!(logs.contains("third-line"), "follower logs:\n{logs}");
    }

    let stop = real_distrun_command(&["-p", &project, "stop", "api"], &dir, &socket_dir)
        .output()
        .expect("stop exact runtime instance");
    assert_stdout(&stop, "local api stopped\n");
    assert!(
        !dir.join(format!("{LOG_ROOT_FOR_TESTS}/{project}")).exists(),
        "service stop should remove its managed runtime logs"
    );

    let cleanup = real_distrun_command(&["-f", path(&config_path), "down"], &dir, &socket_dir)
        .output()
        .expect("clean up real runtime");
    assert_success(&cleanup);
}

#[test]
fn real_tmux_project_operations_do_not_match_a_longer_session_prefix() {
    let Some((dir, socket_dir)) = isolated_tmux_environment() else {
        return;
    };
    let base = format!("prefix_{}", unique_id());
    let longer = format!("{base}2");
    let longer_session = format!("distrun/{longer}");
    let create_longer = real_tmux_command(
        &["new-session", "-d", "-s", &longer_session, "sleep 60"],
        &dir,
        &socket_dir,
    )
    .output()
    .expect("create longer-prefix session");
    assert_success(&create_longer);

    let config_path = write_file(
        &dir,
        "prefix.yml",
        format!("project: {base}\nservices:\n  api:\n    cmd: sleep 60\n    stop_timeout: 50ms\n"),
    );
    let up = real_distrun_command(&["-f", path(&config_path), "up"], &dir, &socket_dir)
        .output()
        .expect("start shorter-prefix project");
    assert_success(&up);

    for session in [format!("=distrun/{base}"), format!("={longer_session}")] {
        let present = real_tmux_command(&["has-session", "-t", &session], &dir, &socket_dir)
            .output()
            .expect("check exact session");
        assert_success(&present);
    }

    let down = real_distrun_command(&["-p", &base, "down"], &dir, &socket_dir)
        .output()
        .expect("stop shorter-prefix project");
    assert_success(&down);
    let longer_remains = real_tmux_command(
        &["has-session", "-t", &format!("={longer_session}")],
        &dir,
        &socket_dir,
    )
    .output()
    .expect("check longer-prefix session");
    assert_success(&longer_remains);

    let _ = real_tmux_command(
        &["kill-session", "-t", &format!("={longer_session}")],
        &dir,
        &socket_dir,
    )
    .output();
}

const LOG_ROOT_FOR_TESTS: &str = ".local/state/distrun/logs";

fn real_distrun_command(args: &[&str], home: &Path, tmux_socket_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_distrun"));
    command
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("TMUX_TMPDIR", tmux_socket_dir)
        .env_remove("TMUX")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn real_tmux_command(args: &[&str], home: &Path, tmux_socket_dir: &Path) -> Command {
    let mut command = Command::new("tmux");
    command
        .args(args)
        .env("HOME", home)
        .env("TMUX_TMPDIR", tmux_socket_dir)
        .env_remove("TMUX")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn isolated_tmux_environment() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    if !Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return None;
    }

    let test_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let dir = Path::new("/tmp").join(format!("dt-{}-{test_id}", std::process::id()));
    let socket_dir = dir.join("socket");
    fs::create_dir_all(&socket_dir).expect("create isolated tmux socket dir");
    let probe = real_tmux_command(
        &["new-session", "-d", "-s", "distrun_test_probe", "sleep 60"],
        &dir,
        &socket_dir,
    )
    .output()
    .expect("probe isolated tmux server");
    let reachable = real_tmux_command(
        &["has-session", "-t", "=distrun_test_probe"],
        &dir,
        &socket_dir,
    )
    .output()
    .expect("check isolated tmux server");
    let _ = real_tmux_command(
        &["kill-session", "-t", "=distrun_test_probe"],
        &dir,
        &socket_dir,
    )
    .output();
    (probe.status.success() && reachable.status.success()).then_some((dir, socket_dir))
}

fn cold_tmux_environment() -> Option<(PathBuf, PathBuf)> {
    let (dir, _) = isolated_tmux_environment()?;
    let socket_dir = dir.join("cold-socket");
    fs::create_dir_all(&socket_dir).expect("create cold tmux socket dir");
    Some((dir, socket_dir))
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
        stdout(output),
        stderr(output),
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "command should fail\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output),
    );
}

fn assert_stdout(output: &Output, expected: &str) {
    assert_success(output);
    assert_eq!(stdout(output), expected, "stderr:\n{}", stderr(output));
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("stdout must be UTF-8")
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("stderr must be UTF-8")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("test path must be UTF-8")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_file(root: &Path, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, contents).expect("write test file");
    path
}

fn install_script(bin_dir: &Path, name: &str, contents: &str) {
    let script = bin_dir.join(name);
    fs::write(&script, contents).expect("write fake executable");
    make_executable(&script);
}

fn install_shell_script(bin_dir: &Path, name: &str, body: &str) {
    install_script(bin_dir, name, &format!("#!/bin/sh\n{body}"));
}

fn install_tmux(bin_dir: &Path, body: &str) {
    let mut script = String::from(
        r#"if [ "$1" = start-server ] && [ "$2" = ";" ] && [ "$3" = list-sessions ] && [ "$6" = ";" ] && [ "$7" = show-options ] && [ "$8" = -g ] && [ "$9" = exit-empty ]; then
    trap 'status=$?; trap - 0; if [ "$status" -eq 0 ]; then printf "%s\n" "exit-empty off"; fi; exit "$status"' 0
    set -- list-panes -a -F '#{session_name}|#{window_id}|#{pane_id}|#{@distrun_pane_id}|#{pane_active}|#{@distrun_service}|#{@distrun_runtime_id}|#{@distrun_ready}|#{pane_dead}'
fi
"#,
    );
    script.push_str(body);
    install_shell_script(bin_dir, "tmux", &script);
}

fn install_ssh(bin_dir: &Path, body: &str) {
    install_shell_script(bin_dir, "ssh", body);
}

fn write_transcript(bin_dir: &Path, runtime: &str, contents: &str) -> PathBuf {
    let path = bin_dir
        .parent()
        .expect("fake bin parent")
        .join(LOG_ROOT_FOR_TESTS)
        .join(runtime)
        .join("pty.log");
    fs::create_dir_all(path.parent().expect("transcript parent"))
        .expect("create transcript directory");
    fs::write(&path, contents).expect("write transcript");
    path
}

fn write_fake_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"if [ "$1" = "list-panes" ] && [ "$2" = "-a" ]; then
    printf '%s\n' \
        'distrun/demo|@1|%1||1|api|||0' \
        'distrun/old|@2|%2||1|worker|||1' \
        'manual|@3|%3||1|ignored|||0' \
        'distrun/demo|@4|%4||1|||0'
    exit 0
fi
exit 1
"#,
    );
}

fn write_broken_server_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"case "$1" in
    new-session)
        case "$*" in *__distrun_lock_demo*) exit 0 ;; esac
        ;;
    kill-session)
        exit 0
        ;;
esac
printf '%s\n' 'tmux socket permission denied' >&2
exit 1
"#,
    );
}

fn write_silent_failure_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"case "$DISTRUN_SILENT_MODE:$1" in
    query:list-panes)
        exit 47
        ;;
    incomplete:list-panes)
        trap - 0
        printf '%s\n' 'tmux socket unavailable' >&2
        exit 0
        ;;
    down:new-session)
        exit 0
        ;;
    down:list-panes)
        exit 47
        ;;
    down:kill-session)
        exit 0
        ;;
    lock:list-panes)
        exit 0
        ;;
    lock:new-session)
        printf '%s\n' attempt >> "$DISTRUN_FAKE_TMUX_LOG"
        if [ "$(wc -l < "$DISTRUN_FAKE_TMUX_LOG")" -gt 1 ]; then
            printf '%s\n' 'lock acquisition retried' >&2
            exit 48
        fi
        exit 47
        ;;
esac
exit 1
"#,
    );
}

fn write_recording_ssh(bin_dir: &Path) {
    install_ssh(
        bin_dir,
        r#"first=$1
target=$1
[ "$target" = -- ] && shift && target=$1
shift
printf '%s %s\n' "$first" "$target" >> "$DISTRUN_FAKE_SSH_LOG"
sh -c "$1"
"#,
    );
}

fn write_host_scope_probe(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"case "$1" in
    new-session)
        printf '%s\n' "${DISTRUN_FAKE_REMOTE:-local}" >> "$DISTRUN_FAKE_HOST_LOG"
        exit 0
        ;;
    has-session)
        printf '%s\n' "can't find session: distrun/demo" >&2
        exit 1
        ;;
esac
exit 0
"#,
    );

    install_ssh(
        bin_dir,
        r#"target=$1
[ "$target" = -- ] && shift && target=$1
shift
DISTRUN_FAKE_REMOTE="$target" sh -c "$1"
"#,
    );
}

fn write_status_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"if [ "$1" = "has-session" ]; then
    exit 0
fi
if [ "$1" = "list-panes" ]; then
    printf '%s\n' 'distrun/demo|@1|%1||1|db|||0'
    exit 0
fi
exit 1
"#,
    );
}

fn write_persistent_logs_tmux(
    bin_dir: &Path,
    contents: &str,
    pane_dead: bool,
    pipe_attached: bool,
) {
    write_transcript(bin_dir, "demo/api.Ab12Cd", contents);
    let pane_dead = u8::from(pane_dead);
    let pipe_attached = u8::from(pipe_attached);
    install_tmux(
        bin_dir,
        &format!(
            r#"case "$1" in
    has-session) exit 0 ;;
    list-panes)
        printf '%s\n' 'distrun/demo|@1|%1|%1|1|api|Ab12Cd|1|{pane_dead}'
        ;;
    display-message)
        case "$*" in
            *pane_pipe*) printf '%s\n' '{pipe_attached}|{pane_dead}' ;;
            *) printf '%s\n' 'distrun/demo|@1|%1|api|Ab12Cd' ;;
        esac
        ;;
esac
exit 0
"#
        ),
    );
}

fn write_captured_logs_tmux(bin_dir: &Path, capture_output: &str) {
    let capture_path = write_file(
        bin_dir.parent().expect("fake bin parent"),
        "captured-pane.log",
        capture_output,
    );
    install_tmux(
        bin_dir,
        &format!(
            r#"case "$1" in
    has-session) exit 0 ;;
    list-panes) printf '%s\n' 'distrun/demo|@1|%1||1|api|||1' ;;
    display-message) printf '%s\n' 'distrun/demo|@1|%1|api|' ;;
    capture-pane) cat {} ;;
esac
exit 0
"#,
            shell_quote(path(&capture_path))
        ),
    );
}

fn write_duplicate_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"case "$1" in
    has-session)
        exit 0
        ;;
    list-panes)
        printf '%s\n' \
            'distrun/demo|@1|%1|%1|1|api|run1|1|0' \
            'distrun/demo|@2|%2|%2|1|api|run2|1|1'
        exit 0
        ;;
esac
exit 1
"#,
    );
}

fn write_draining_logs_tmux(bin_dir: &Path) {
    write_transcript(bin_dir, "demo/api.Ab12Cd", "initial\n");
    install_tmux(
        bin_dir,
        r#"case "$1" in
    has-session)
        exit 0
        ;;
    list-panes)
        printf '%s\n' 'distrun/demo|@1|%1|%1|1|api|Ab12Cd|1|1'
        ;;
    display-message)
        case "$*" in
            *session_name*)
                printf '%s\n' 'distrun/demo|@1|%1|api|Ab12Cd'
                ;;
            *'pane_pipe}|#{pane_dead}'*)
                if [ -f "$DISTRUN_FAKE_PIPE_STATE" ]; then
                    if [ ! -f "$DISTRUN_FAKE_PIPE_STATE.started" ]; then
                        : > "$DISTRUN_FAKE_PIPE_STATE.started"
                        (
                            sleep 0.1
                            printf 'final\n' >> "$HOME/.local/state/distrun/logs/demo/api.Ab12Cd/pty.log"
                            rm -f "$DISTRUN_FAKE_PIPE_STATE"
                        ) &
                    fi
                    printf '%s\n' '1|1'
                else
                    printf '%s\n' '0|1'
                fi
                ;;
            *pane_pipe*)
                if [ -f "$DISTRUN_FAKE_PIPE_STATE" ]; then printf '1\n'; else printf '0\n'; fi
                ;;
        esac
        ;;
esac
exit 0
"#,
    );
}

fn write_streaming_logs_tmux(bin_dir: &Path) {
    write_transcript(bin_dir, "demo/api.Ab12Cd", "older\nprevious\ninitial\n");
    install_tmux(
        bin_dir,
        r#"case "$1" in
    has-session)
        exit 0
        ;;
    list-panes)
        printf '%s\n' 'distrun/demo|@1|%1|%1|1|api|Ab12Cd|1|0'
        ;;
    display-message)
        case "$*" in
            *'pane_pipe}|#{pane_dead}'*)
                if [ -f "$DISTRUN_FAKE_TMUX_STATE" ]; then
                    printf '%s\n' '1|0'
                    if [ ! -f "$DISTRUN_FAKE_TMUX_STATE.writer" ]; then
                        : > "$DISTRUN_FAKE_TMUX_STATE.writer"
                        (
                            sleep 0.1
                            run_dir="$HOME/.local/state/distrun/logs/demo/api.Ab12Cd"
                            if [ "$DISTRUN_FAKE_DELETE_LOG_DIR" = "1" ]; then
                                rm -rf "$run_dir"
                                rm -f "$DISTRUN_FAKE_TMUX_STATE"
                                exit 0
                            fi
                            printf "same\nsame\n" >> "$run_dir/pty.log"
                            if [ "$DISTRUN_FAKE_KEEP_PANE" != "1" ]; then
                                rm -f "$DISTRUN_FAKE_TMUX_STATE"
                            fi
                        ) >/dev/null 2>&1 &
                    fi
                else
                    printf '%s\n' '0|1'
                fi
                ;;
            *pane_pipe*)
                if [ -f "$DISTRUN_FAKE_TMUX_STATE" ]; then printf '1\n'; else printf '0\n'; fi
                ;;
            *)
                printf '%s\n' 'distrun/demo|@1|%1|api|Ab12Cd'
                ;;
        esac
        ;;
esac
exit 0
"#,
    );
}

fn write_exited_keeper_tmux(bin_dir: &Path) {
    write_transcript(bin_dir, "demo/api.Ab12Cd", "complete\n");

    install_tmux(
        bin_dir,
        r#"printf '%s\n' "$*" >> "$DISTRUN_FAKE_TMUX_LOG"
case "$1" in
    new-session|has-session|kill-window|kill-session)
        exit 0
        ;;
    list-panes)
        case "$*" in
            *session_name*)
                printf '%s\n' 'distrun/demo|@1|%1|%1|1|api|Ab12Cd|1|1'
                ;;
            *)
                printf '%s\n' '%1|%1|1|api|1'
                ;;
        esac
        exit 0
        ;;
    display-message)
        case "$*" in
            *pane_dead*) printf '%s\n' '1' ;;
            *) printf '%s\n' 'distrun/demo|@1|%1|api|Ab12Cd' ;;
        esac
        exit 0
        ;;
    send-keys)
        exit 0
        ;;
esac
exit 1
"#,
    );
}

fn write_failing_stop_tmux(bin_dir: &Path) -> std::path::PathBuf {
    let log_path = write_transcript(bin_dir, "demo/api.Ab12Cd", "still needed\n");

    install_tmux(
        bin_dir,
        r#"case "$1" in
    new-session)
        exit 0
        ;;
    has-session)
        exit 0
        ;;
    list-panes)
        case "$*" in
            *session_name*)
                printf '%s\n' 'distrun/demo|@1|%1|%1|1|api|Ab12Cd|1|0'
                ;;
            *)
                printf '%s\n' '%1|%1|1|api|1'
                ;;
        esac
        exit 0
        ;;
    display-message)
        [ "$DISTRUN_FAIL_PROBE" = 1 ] && exit 1
        case "$*" in
            *pane_dead*) printf '1\n' ;;
            *)
                if [ "$DISTRUN_CHANGED_IDENTITY" = 1 ]; then
                    printf '%s\n' 'distrun/demo|@2|%1|api|Ef34Gh'
                else
                    printf '%s\n' 'distrun/demo|@1|%1|api|Ab12Cd'
                fi
                ;;
        esac
        exit 0
        ;;
    kill-window)
        exit 1
        ;;
    kill-session)
        case "$*" in
            *__distrun_lock_demo*) exit 0 ;;
            *) exit 1 ;;
        esac
        ;;
esac
exit 0
"#,
    );
    log_path
}

fn write_project_log_cleanup_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"case "$1" in
    new-session|has-session|kill-session)
        exit 0
        ;;
    list-panes)
        printf '%s\n' 'distrun/demo|@1|%1|%1|1|api|Ab12Cd|1|1'
        exit 0
        ;;
esac
exit 1
"#,
    );
}

enum StalePane {
    Missing,
    Unready,
}

fn write_stale_pane_tmux(bin_dir: &Path, state: StalePane) {
    let (pane, managed_pane, metadata, identity) = match state {
        StalePane::Missing => (
            "distrun/demo|@1|%live|%dead|1|api|Old123|0|0|0",
            "%%dead",
            "",
            "*%dead*) exit 1 ;;",
        ),
        StalePane::Unready => (
            "distrun/demo|@1|%1|%1|1|api|Old123|0|1|0",
            "%%1",
            "*@distrun_runtime_id*) printf 'Old123\\n' ;;\n            *@distrun_ready*) printf '0\\n' ;;",
            "*%1*) printf '%s\\n' '@1|%1' ;;",
        ),
    };
    install_tmux(
        bin_dir,
        &format!(
            r#"case "$1" in
    has-session) exit 0 ;;
    list-panes) printf '%s\n' '{pane}' ;;
    list-windows) printf '%s\n' '@1|api' ;;
    show-options)
        case "$*" in
            *@distrun_pane_id*) printf '{managed_pane}\n' ;;
            {metadata}
        esac
        ;;
    display-message)
        case "$*" in
            {identity}
            *pane_pipe*) printf '1\n' ;;
        esac
        ;;
    kill-window)
        [ "$3" = '@1' ] && : > "$DISTRUN_STALE_KILLED"
        ;;
    new-window) printf '@2|%%2\n' ;;
    pipe-pane|new-session|set-window-option|rename-window|respawn-pane|kill-session) ;;
esac
exit 0
"#
        ),
    );
}

fn write_malformed_inventory_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"if [ "$1" = list-panes ]; then
    printf '%s\n' \
        'distrun/bad|name|@9|%9|%9|1|bad||||0|0' \
        'distrun/demo|@1|%1|%1|1|api||1|0'
fi
exit 0
"#,
    );
}

fn write_orphan_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"case "$1" in
    has-session)
        exit 0
        ;;
    list-panes)
        printf '%s\n' 'distrun/demo|@1|%1||1|old-worker|||0'
        exit 0
        ;;
    display-message)
        case "$*" in
            *session_name*) printf '%s\n' 'distrun/demo|@1|%1|old-worker|' ;;
            *pane_dead*) printf '1\n' ;;
        esac
        exit 0
        ;;
    new-session)
        exit 0
        ;;
    send-keys|kill-window)
        printf '%s\n' "$*" >> "$DISTRUN_FAKE_TMUX_LOG"
        exit 0
        ;;
esac
exit 0
"#,
    );
}

fn write_lifecycle_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"printf '%s\n' "$*" >> "$DISTRUN_FAKE_TMUX_LOG"
case "$1" in
    has-session)
        exit 0
        ;;
    list-panes)
        printf '%s\n' \
            'distrun/demo|@1|%1||1|api|||0' \
            'distrun/demo|@2|%2||1|worker|||0' \
            'distrun/demo|@3|%3||1|old-worker|||0'
        ;;
    list-windows)
        [ -f "$DISTRUN_FAKE_TMUX_LOG.api-stopped" ] || printf '%s\n' '@1|api'
        printf '%s\n' '@2|worker' '@3|old-worker'
        ;;
    display-message)
        case "$*" in
            *pane_pipe*) printf '1\n' ;;
            *pane_dead*) printf '1\n' ;;
            *%1*) printf '%s\n' 'distrun/demo|@1|%1|api|' ;;
            *%2*) printf '%s\n' 'distrun/demo|@2|%2|worker|' ;;
            *%3*) printf '%s\n' 'distrun/demo|@3|%3|old-worker|' ;;
        esac
        ;;
    new-window)
        printf '@4|%%4\n'
        ;;
    kill-window)
        [ "$3" = "@1" ] && : > "$DISTRUN_FAKE_TMUX_LOG.api-stopped"
        ;;
    pipe-pane)
        ;;
    new-session|set-window-option|rename-window|respawn-pane|kill-session)
        ;;
esac
exit 0
"#,
    );
}

fn write_recreate_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"case "$1" in
    has-session)
        if [ -f "$DISTRUN_FAKE_TMUX_STATE" ]; then exit 0; else exit 1; fi
        ;;
    list-windows)
        [ -f "$DISTRUN_FAKE_TMUX_STATE" ] || exit 1
        case "$5" in
            *window_name*) printf '%s\n' 'old-worker' ;;
            *session_name*) printf '%s\n' 'distrun/demo|old-worker|0|0' ;;
            *) printf '%s\n' '%1|old-worker' ;;
        esac
        ;;
    display-message)
        printf '0\n'
        ;;
    send-keys)
        ;;
    kill-session)
        rm -f "$DISTRUN_FAKE_TMUX_STATE"
        ;;
esac
exit 0
"#,
    );
    install_script(bin_dir, "sleep", "#!/bin/sh\nexit 0\n");
}

fn write_ambiguous_runtime(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"case "$1" in
    has-session)
        exit 0
        ;;
    list-panes)
        printf '%s\n' 'distrun/demo|@1|%1||1|api|||0'
        ;;
    display-message)
        printf '%s\n' 'distrun/demo|@1|%1|api|'
        ;;
    capture-pane)
        printf '%s log\n' "${DISTRUN_FAKE_REMOTE:-local}"
        ;;
esac
exit 0
"#,
    );
    install_script(
        bin_dir,
        "ssh",
        "#!/bin/sh\ntarget=$1\n[ \"$target\" = -- ] && shift && target=$1\nshift\nDISTRUN_FAKE_REMOTE=$target sh -c \"$1\"\n",
    );
}

fn write_slow_ssh(bin_dir: &Path) {
    install_ssh(
        bin_dir,
        r#"sleep 10
"#,
    );
}

fn write_failing_ssh(bin_dir: &Path) {
    install_script(
        bin_dir,
        "ssh",
        "#!/bin/sh\nprintf 'edge unavailable\\n' >&2\nexit 1\n",
    );
}

fn write_selector_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"printf '%s\n' "$*" >> "$DISTRUN_FAKE_TMUX_LOG"
case "$1" in
    has-session)
        exit 0
        ;;
    list-panes)
        printf '%s\n' 'distrun/demo|@1|%1||1|api|||0'
        exit 0
        ;;
    capture-pane)
        printf '%s\n' 'must not be read'
        exit 0
        ;;
esac
exit 1
"#,
    );
}

fn write_down_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"if [ "$1" = "has-session" ]; then
    exit 1
fi
exit 0
"#,
    );
}

fn write_recording_tmux(bin_dir: &Path) {
    install_tmux(
        bin_dir,
        r#"case "$1" in
    has-session)
        exit 1
        ;;
    list-panes|list-windows)
        exit 0
        ;;
    display-message)
        case "$*" in
            *pane_pipe*) printf '1\n' ;;
            *) printf '@0\n' ;;
        esac
        exit 0
        ;;
    new-window)
        printf '%s\n' "$*" >> "$DISTRUN_FAKE_TMUX_LOG"
        printf '@1|%%1\n'
        exit 0
        ;;
    new-session)
        case "$*" in *' -F '*) printf '@0\n' ;; esac
        exit 0
        ;;
    pipe-pane)
        exit 0
        ;;
    respawn-pane)
        printf '%s\n' "$*" >> "$DISTRUN_FAKE_TMUX_LOG"
        case "$*" in
            *"$DISTRUN_FAKE_FAIL_SERVICE"*)
                [ -z "$DISTRUN_FAKE_FAIL_SERVICE" ] || exit 1
                ;;
        esac
        exit 0
        ;;
    set-window-option|rename-window|kill-session)
        exit 0
        ;;
esac
exit 0
"#,
    );
}

fn unique_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos();
    let counter = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}_{}", std::process::id(), nanos, counter)
}

fn make_executable(path: &Path) {
    let mut permissions = fs::metadata(path)
        .expect("read fake executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod fake executable");
}
