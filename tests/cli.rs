use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn status_allows_config_without_services() {
    let dir = std::env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
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
fn status_all_lists_managed_sessions_from_manual_local_host() {
    let dir = std::env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("create fake bin dir");
    write_fake_tmux(&bin_dir);

    let output = distrun_with_path(&["status", "--all", "--host", "local"], &bin_dir);

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
fn status_all_rejects_project_filter() {
    let output = distrun(&["status", "--all", "demo"]);

    assert_failure(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("status --all cannot be used with a project filter")
    );

    let output = distrun(&["-p", "demo", "status", "--all", "--host", "local"]);

    assert_failure(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("status --all cannot be used with a project filter")
    );
}

#[test]
fn status_host_requires_all_before_loading_config() {
    let dir = std::env::temp_dir().join(format!("distrun-cli-{}", unique_id()));
    let config_path = dir.join("missing.yml");

    let output = distrun(&["-f", path(&config_path), "status", "--host", "local"]);

    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--host can only be used with status --all"));
    assert!(!stderr.contains("failed to read config"));
}

fn distrun(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_distrun"))
        .args(args)
        .output()
        .expect("run distrun")
}

fn distrun_with_path(args: &[&str], bin_dir: &Path) -> Output {
    let old_path = env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin_dir.to_path_buf()];
    paths.extend(env::split_paths(&old_path));
    let path = env::join_paths(paths).expect("join PATH");

    Command::new(env!("CARGO_BIN_EXE_distrun"))
        .args(args)
        .env("PATH", path)
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

fn unique_id() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos()
}
