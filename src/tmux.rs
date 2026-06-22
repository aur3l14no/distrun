use crate::backend::Backend;
use crate::executor::HostExecutor;
use crate::model::{DesiredService, HostTarget, ObservedService, RuntimeState};
use anyhow::{Context, Result};
use std::time::Duration;

const BOOTSTRAP_WINDOW: &str = "__distrun_bootstrap";
const SESSION_PREFIX: &str = "distrun_";
const WINDOW_LIST_FORMAT: &str =
    "#{session_name}|#{@distrun_service}|#{pane_dead}|#{pane_dead_status}";

#[derive(Debug)]
pub struct TmuxBackend<E> {
    executor: E,
}

impl<E> TmuxBackend<E> {
    pub fn new(executor: E) -> Self {
        Self { executor }
    }
}

impl<E> Backend for TmuxBackend<E>
where
    E: HostExecutor,
{
    fn list(&self, host: &HostTarget, project: &str) -> Result<Vec<ObservedService>> {
        let session = session_name(project);
        let command = format!(
            "if {} 2>/dev/null; then {}; fi",
            tmux(&["has-session", "-t", &session]),
            tmux(&["list-windows", "-t", &session, "-F", WINDOW_LIST_FORMAT]),
        );
        let output = self.executor.run(host, &command)?;
        let services = parse_window_lines(&host.name, &output.stdout)?;

        Ok(services
            .into_iter()
            .filter(|service| service.project == project)
            .collect())
    }

    fn list_all(&self, host: &HostTarget) -> Result<Vec<ObservedService>> {
        let command = format!(
            "{} 2>/dev/null || true",
            tmux(&["list-windows", "-a", "-F", WINDOW_LIST_FORMAT]),
        );
        let output = self.executor.run(host, &command)?;

        parse_window_lines(&host.name, &output.stdout)
    }

    fn start(&self, host: &HostTarget, service: &DesiredService) -> Result<()> {
        let session = session_name(&service.project);
        let set_service_name = format!(
            "tmux set-window-option -t \"$window_id\" @distrun_service {}",
            sh_quote(&service.name)
        );
        let command = format!(
            "{} && window_id=$({}) && {} && {}",
            ensure_session(&session),
            tmux(&[
                "new-window",
                "-P",
                "-F",
                "#{window_id}",
                "-d",
                "-t",
                &session,
                "-n",
                &service.name,
                &service_command(service),
            ]),
            set_service_name,
            configure_created_window(&service.name),
        );
        self.executor.run(host, &command)?;
        Ok(())
    }

    fn stop_service(
        &self,
        host: &HostTarget,
        project: &str,
        service: &str,
        timeout: Duration,
    ) -> Result<()> {
        let session = session_name(project);
        let sleep_for = sleep_duration(timeout);
        let find_target = find_service_window(&session, service);
        let command = format!(
            "if {} 2>/dev/null; then \
             target=$({}); \
             if [ -n \"$target\" ]; then \
             if [ \"$(tmux display-message -p -t \"$target\" '#{{pane_dead}}')\" = \"0\" ]; then tmux send-keys -t \"$target\" C-c; sleep {}; fi; \
             tmux kill-window -t \"$target\"; \
             fi; \
             fi",
            tmux(&["has-session", "-t", &session]),
            find_target,
            sleep_for,
        );
        self.executor.run(host, &command)?;
        Ok(())
    }

    fn stop_project(&self, host: &HostTarget, project: &str, timeout: Duration) -> Result<()> {
        let session = session_name(project);
        let sleep_for = sleep_duration(timeout);
        let command = format!(
            "session={}; \
             if tmux has-session -t \"$session\" 2>/dev/null; then \
             tmux list-windows -t \"$session\" -F '#{{window_name}}' | while IFS= read -r window; do \
             [ \"$window\" = {} ] && continue; \
             if [ \"$(tmux display-message -p -t \"$session:$window\" '#{{pane_dead}}')\" = \"0\" ]; then \
             tmux send-keys -t \"$session:$window\" C-c; \
             fi; \
             done; \
             sleep {}; \
             tmux kill-session -t \"$session\"; \
             fi",
            sh_quote(&session),
            sh_quote(BOOTSTRAP_WINDOW),
            sleep_for,
        );
        self.executor.run(host, &command)?;
        Ok(())
    }

    fn logs(&self, host: &HostTarget, project: &str, service: &str, tail: usize) -> Result<String> {
        let session = session_name(project);
        let start = format!("-{}", tail.max(1));
        let find_target = find_service_window(&session, service);
        let capture_pane = format!(
            "tmux capture-pane -p -t \"$target\" -S {}",
            sh_quote(&start)
        );
        let command = format!(
            "if {} 2>/dev/null; then \
             target=$({}); \
             if [ -n \"$target\" ]; then {}; else exit 42; fi; \
             else exit 42; fi",
            tmux(&["has-session", "-t", &session]),
            find_target,
            capture_pane,
        );
        Ok(trim_trailing_blank_lines(
            &self.executor.run(host, &command)?.stdout,
        ))
    }
}

fn parse_window_lines(host: &str, output: &str) -> Result<Vec<ObservedService>> {
    let mut services = Vec::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        if let Some(service) = parse_window_line(host, line)? {
            services.push(service);
        }
    }
    Ok(services)
}

fn parse_window_line(host: &str, line: &str) -> Result<Option<ObservedService>> {
    let mut fields = line.splitn(4, '|');
    let session = fields
        .next()
        .context("tmux list-windows output is missing session name")?;
    let Some(project) = session.strip_prefix(SESSION_PREFIX) else {
        return Ok(None);
    };
    if project.is_empty() {
        return Ok(None);
    }

    let name = fields
        .next()
        .context("tmux list-windows output is missing service name")?;
    let pane_dead = fields.next().unwrap_or("0");

    if name.is_empty() {
        return Ok(None);
    }

    Ok(Some(ObservedService {
        project: project.to_owned(),
        host: host.to_owned(),
        name: name.to_owned(),
        runtime: runtime_state(pane_dead),
    }))
}

fn runtime_state(pane_dead: &str) -> RuntimeState {
    match pane_dead {
        "1" => RuntimeState::Exited,
        "0" => RuntimeState::Running,
        _ => RuntimeState::Unknown,
    }
}

fn ensure_session(session: &str) -> String {
    format!(
        "{} 2>/dev/null || ({} && window_id=$({}) && {})",
        tmux(&["has-session", "-t", session]),
        tmux(&[
            "new-session",
            "-d",
            "-s",
            session,
            "-n",
            BOOTSTRAP_WINDOW,
            "sleep 3650d",
        ]),
        tmux(&[
            "display-message",
            "-p",
            "-t",
            &format!("{session}:0"),
            "#{window_id}",
        ]),
        configure_created_window(BOOTSTRAP_WINDOW),
    )
}

fn configure_created_window(name: &str) -> String {
    format!(
        "{} && {} && {} && tmux rename-window -t \"$window_id\" {}",
        "tmux set-window-option -t \"$window_id\" automatic-rename off",
        "tmux set-window-option -t \"$window_id\" allow-rename off",
        "tmux set-window-option -t \"$window_id\" remain-on-exit on",
        sh_quote(name),
    )
}

fn find_service_window(session: &str, service: &str) -> String {
    format!(
        "tmux list-windows -t {} -F '#{{window_id}}|#{{@distrun_service}}' | awk -F '\\\\|' -v service={} '$2 == service {{ print $1; exit }}'",
        sh_quote(session),
        sh_quote(service),
    )
}

fn service_command(service: &DesiredService) -> String {
    let mut commands = Vec::new();
    if let Some(cwd) = &service.cwd {
        commands.push(format!("cd {}", sh_quote(cwd)));
    }
    if !service.env.is_empty() {
        let mut export = String::from("export");
        for (key, value) in &service.env {
            export.push(' ');
            export.push_str(key);
            export.push('=');
            export.push_str(&sh_quote(value));
        }
        commands.push(export);
    }
    commands.push(format!("exec sh -lc {}", sh_quote(&service.cmd)));
    commands.join(" && ")
}

fn session_name(project: &str) -> String {
    format!("{SESSION_PREFIX}{project}")
}

fn sleep_duration(duration: Duration) -> String {
    if duration.subsec_nanos() == 0 {
        return duration.as_secs().to_string();
    }

    let mut fractional = format!("{:09}", duration.subsec_nanos());
    while fractional.ends_with('0') {
        fractional.pop();
    }
    format!("{}.{}", duration.as_secs(), fractional)
}

fn trim_trailing_blank_lines(output: &str) -> String {
    let trimmed = output.trim_end_matches(|ch: char| ch.is_whitespace());
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

fn tmux(args: &[&str]) -> String {
    let mut words = Vec::with_capacity(args.len() + 1);
    words.push(sh_quote("tmux"));
    words.extend(args.iter().map(|arg| sh_quote(arg)));
    words.join(" ")
}

fn sh_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn shell_quotes_single_quotes() {
        assert_eq!(sh_quote("that's it"), "'that'\"'\"'s it'");
    }

    #[test]
    fn service_command_preserves_shell_command_as_one_script() {
        let service = DesiredService {
            project: "demo".to_owned(),
            name: "api".to_owned(),
            host: "web".to_owned(),
            cmd: "echo $GREETING && sleep 1".to_owned(),
            cwd: Some("/srv/app".to_owned()),
            env: BTreeMap::from([("GREETING".to_owned(), "hello world".to_owned())]),
            stop_timeout: Duration::from_secs(1),
        };

        assert_eq!(
            service_command(&service),
            "cd '/srv/app' && export GREETING='hello world' && exec sh -lc 'echo $GREETING && sleep 1'"
        );
    }

    #[test]
    fn sleep_duration_preserves_milliseconds() {
        assert_eq!(sleep_duration(Duration::from_millis(500)), "0.5");
        assert_eq!(sleep_duration(Duration::from_millis(1500)), "1.5");
        assert_eq!(sleep_duration(Duration::from_secs(2)), "2");
    }

    #[test]
    fn logs_drop_empty_terminal_area_after_output() {
        assert_eq!(
            trim_trailing_blank_lines("api ready\nGET /health 200\n\n\n"),
            "api ready\nGET /health 200\n"
        );
    }

    #[test]
    fn all_window_parser_keeps_only_distrun_service_windows() {
        assert_eq!(
            parse_window_line("web", "distrun_demo|api|0|0").expect("parse managed window"),
            Some(ObservedService {
                project: "demo".to_owned(),
                host: "web".to_owned(),
                name: "api".to_owned(),
                runtime: RuntimeState::Running,
            })
        );
        assert_eq!(
            parse_window_line("web", "manual|api|0|0").expect("parse unmanaged session"),
            None
        );
        assert_eq!(
            parse_window_line("web", "distrun_demo||0|0").expect("parse unmarked window"),
            None
        );
    }
}
