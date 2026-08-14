use crate::backend::{Backend, StartResult};
use crate::executor::{HostExecutor, HostOutput};
use crate::model::{DesiredService, HostTarget, ObservedService, RuntimeState};
use anyhow::{Context, Result, bail};
use std::time::Duration;

const BOOTSTRAP_WINDOW: &str = "__distrun_bootstrap";
const LOCK_SESSION_PREFIX: &str = "__distrun_lock_";
const LOG_ROOT: &str = ".local/state/distrun/logs";
const LOG_DRAIN_MAX_POLLS: usize = 20;
const LOG_DRAIN_POLL_SECONDS: &str = "0.05";
const LOG_DRAIN_GRACE_SECONDS: &str = "1";
const STOP_OPERATION_OVERHEAD: Duration = Duration::from_secs(5);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SESSION_PREFIX: &str = "distrun/";
const PLACEHOLDER_COMMAND: &str = "exec sleep 315360000";
const REQUIRE_TMUX: &str = "command -v tmux >/dev/null 2>&1 || { printf '%s\\n' 'tmux is not installed on this host' >&2; exit 127; }";
const PANE_LIST_FORMAT: &str = "#{session_name}|#{window_id}|#{pane_id}|#{@distrun_pane_id}|#{pane_active}|#{@distrun_service}|#{@distrun_runtime_id}|#{@distrun_ready}|#{pane_dead}";
const IDENTITY_FORMAT: &str =
    "#{session_name}|#{window_id}|#{pane_id}|#{@distrun_service}|#{@distrun_runtime_id}";

#[derive(Debug)]
pub struct TmuxBackend<E> {
    executor: E,
}

impl<E> TmuxBackend<E> {
    pub fn new(executor: E) -> Self {
        Self { executor }
    }
}

impl<E> TmuxBackend<E>
where
    E: HostExecutor,
{
    pub fn follow_logs(
        &self,
        host: &HostTarget,
        service: &ObservedService,
        tail: usize,
    ) -> Result<()> {
        self.executor
            .stream(host, &follow_logs_command(service, tail)?)
    }

    fn capture_logs(
        &self,
        host: &HostTarget,
        service: &ObservedService,
        tail: usize,
        timeout: Duration,
    ) -> Result<String> {
        if tail == 0 {
            return Ok(String::new());
        }
        let (command, persistent) = logs_command(service, tail)?;
        let output = self
            .executor
            .run_with_timeout(host, &command, timeout)?
            .stdout;
        if persistent {
            Ok(output)
        } else {
            Ok(last_log_lines(&output, tail))
        }
    }

    fn list_project_windows(
        &self,
        host: &HostTarget,
        project: &str,
        timeout: Option<Duration>,
    ) -> Result<Vec<ObservedService>> {
        let command = pane_inventory_command();
        let output = self.run_observation_command(host, &command, timeout)?;
        let services = parse_pane_lines(&host.name, &output.stdout)?;

        Ok(services
            .into_iter()
            .filter(|service| service.project == project)
            .collect())
    }

    fn list_all_windows(
        &self,
        host: &HostTarget,
        timeout: Duration,
    ) -> Result<Vec<ObservedService>> {
        let command = pane_inventory_command();
        let output = self.executor.run_with_timeout(host, &command, timeout)?;
        parse_pane_lines(&host.name, &output.stdout)
    }

    fn run_observation_command(
        &self,
        host: &HostTarget,
        command: &str,
        timeout: Option<Duration>,
    ) -> Result<HostOutput> {
        match timeout {
            Some(timeout) => self.executor.run_with_timeout(host, command, timeout),
            None => self.executor.run(host, command),
        }
    }
}

impl<E> Backend for TmuxBackend<E>
where
    E: HostExecutor,
{
    fn list(&self, host: &HostTarget, project: &str) -> Result<Vec<ObservedService>> {
        self.list_project_windows(host, project, None)
    }

    fn list_with_timeout(
        &self,
        host: &HostTarget,
        project: &str,
        timeout: Duration,
    ) -> Result<Vec<ObservedService>> {
        self.list_project_windows(host, project, Some(timeout))
    }

    fn list_all_with_timeout(
        &self,
        host: &HostTarget,
        timeout: Duration,
    ) -> Result<Vec<ObservedService>> {
        self.list_all_windows(host, timeout)
    }

    fn start(&self, host: &HostTarget, service: &DesiredService) -> Result<StartResult> {
        let output = self.executor.run(host, &start_command(service)?)?;
        match output.stdout.lines().last() {
            Some("started") => Ok(StartResult::Started),
            Some("already-present") => Ok(StartResult::AlreadyPresent),
            _ => bail!(
                "tmux did not report a start result for service `{}`",
                service.name
            ),
        }
    }

    fn stop_service(
        &self,
        host: &HostTarget,
        service: &ObservedService,
        timeout: Duration,
    ) -> Result<()> {
        self.executor
            .run_with_timeout(
                host,
                &stop_service_command(service, timeout)?,
                stop_operation_timeout(timeout)?,
            )
            .context("service stop failed")?;
        Ok(())
    }

    fn stop_project(&self, host: &HostTarget, project: &str, timeout: Duration) -> Result<()> {
        self.executor
            .run_with_timeout(
                host,
                &stop_project_command(project, timeout)?,
                stop_operation_timeout(timeout)?,
            )
            .context("project stop failed")?;
        Ok(())
    }

    fn logs_with_timeout(
        &self,
        host: &HostTarget,
        service: &ObservedService,
        tail: usize,
        timeout: Duration,
    ) -> Result<String> {
        self.capture_logs(host, service, tail, timeout)
    }
}

fn start_command(service: &DesiredService) -> Result<String> {
    validate_component("project", &service.project)?;
    validate_component("service", &service.name)?;

    let session = session_name(&service.project);
    let session_target = exact_session_target(&service.project);
    let project_dir_ref = format!("{LOG_ROOT}/{}", service.project);
    let run_template_ref = format!("{project_dir_ref}/{}.XXXXXX", service.name);
    let find_existing = find_service_windows(&session_target, &service.name);
    let ensure_session = ensure_session_command(&session);
    let configure_window = format!(
        "tmux set-window-option -t \"$window_id\" automatic-rename off && \
         tmux set-window-option -t \"$window_id\" allow-rename off && \
         tmux set-window-option -t \"$window_id\" remain-on-exit on && \
         tmux set-window-option -t \"$window_id\" @distrun_service {} && \
         tmux set-window-option -t \"$window_id\" @distrun_pane_id \"$pane_id\" && \
         tmux set-window-option -t \"$window_id\" @distrun_runtime_id \"$runtime_id\" && \
         tmux set-window-option -t \"$window_id\" @distrun_ready 0 && \
         tmux rename-window -t \"$window_id\" {}",
        sh_quote(&service.name),
        sh_quote(&service.name),
    );
    let new_window = tmux(&[
        "new-window",
        "-dP",
        "-F",
        "#{window_id}|#{pane_id}",
        "-t",
        &session_target,
        "-n",
        &service.name,
        PLACEHOLDER_COMMAND,
    ]);
    let service_script = sh_quote(&service_command(service));
    let body = format!(
        "set -e; \
         start_complete=0; window_id=; pane_id=; run_dir=; \
         cleanup_start() {{ \
           if [ \"$start_complete\" != 1 ]; then \
             [ -z \"$window_id\" ] || tmux kill-window -t \"$window_id\" 2>/dev/null || true; \
             [ -z \"$run_dir\" ] || rm -rf \"$run_dir\" 2>/dev/null || true; \
           fi; \
           cleanup_lock; \
         }}; \
         trap cleanup_start EXIT; \
         {ensure_session}; \
         existing=; \
         for candidate_window in $({find_existing}); do \
           managed_pane_id=$(tmux show-options -wqv -t \"$candidate_window\" @distrun_pane_id); \
           if [ -z \"$managed_pane_id\" ]; then \
             [ -n \"$existing\" ] || existing=\"$candidate_window\"; \
           else \
             managed_runtime_id=$(tmux show-options -wqv -t \"$candidate_window\" @distrun_runtime_id); \
             managed_ready=$(tmux show-options -wqv -t \"$candidate_window\" @distrun_ready); \
             managed_identity=$(tmux display-message -p -t \"$managed_pane_id\" '#{{window_id}}|#{{pane_id}}' 2>/dev/null || true); \
             if [ \"$managed_identity\" = \"$candidate_window|$managed_pane_id\" ] && \
                {{ [ -z \"$managed_runtime_id\" ] || [ \"$managed_ready\" = 1 ]; }}; then \
               [ -n \"$existing\" ] || existing=\"$candidate_window\"; \
             else \
               tmux kill-window -t \"$candidate_window\"; \
             fi; \
           fi; \
         done; \
         if [ -n \"$existing\" ]; then \
           start_complete=1; printf '%s\\n' already-present; exit 0; \
         fi; \
         project_dir=\"$HOME/{project_dir_ref}\"; \
         mkdir -p \"$project_dir\"; \
         run_dir=$(mktemp -d \"$HOME/{run_template_ref}\"); \
         runtime_id=${{run_dir##*.}}; \
         log_ref={log_ref_prefix}\"$runtime_id\"/pty.log; \
         log_path=\"$HOME/$log_ref\"; \
         umask 077; : > \"$log_path\"; \
         created=$({new_window}); \
         window_id=${{created%%|*}}; pane_id=${{created#*|}}; \
         [ -n \"$window_id\" ] && [ -n \"$pane_id\" ] && [ \"$window_id\" != \"$pane_id\" ]; \
         {configure_window}; \
         pipe_command='exec cat >> \"$HOME/'\"$log_ref\"'\"'; \
         tmux pipe-pane -O -t \"$pane_id\" \"$pipe_command\"; \
         [ \"$(tmux display-message -p -t \"$pane_id\" '#{{pane_pipe}}')\" = 1 ]; \
         tmux respawn-pane -k -t \"$pane_id\" {service_script}; \
         tmux set-window-option -t \"$window_id\" @distrun_ready 1; \
         start_complete=1; printf '%s\\n' started",
        log_ref_prefix = sh_quote(&format!("{project_dir_ref}/{}.", service.name)),
    );

    with_project_lock(&service.project, &body)
}

fn stop_service_command(service: &ObservedService, timeout: Duration) -> Result<String> {
    validate_component("project", &service.project)?;
    let expected = expected_identity(service);
    let display_identity = display_identity_command(&service.pane_id);
    let cleanup = cleanup_service_log_command(service);
    let wait_for_exit = wait_for_panes_command(&sh_quote(&service.pane_id), timeout)?;
    let body = format!(
        "set -e; \
         if ! current=$({display_identity} 2>/dev/null); then \
           printf '%s\\n' 'failed to verify runtime identity before stopping' >&2; exit 46; \
         fi; \
         if [ \"$current\" != {expected} ]; then \
           printf '%s\\n' 'runtime instance changed before stopping' >&2; exit 42; \
         fi; \
         if ! pane_dead=$(tmux display-message -p -t {} '#{{pane_dead}}' 2>/dev/null); then \
           printf '%s\\n' 'failed to inspect runtime state before stopping' >&2; exit 46; \
         fi; \
         if [ \"$pane_dead\" = 0 ]; then \
           tmux send-keys -t {} C-c; {wait_for_exit}; \
         fi; \
         if ! current=$({display_identity} 2>/dev/null); then \
           printf '%s\\n' 'failed to verify runtime identity after interrupt' >&2; exit 46; \
         fi; \
         if [ \"$current\" != {expected} ]; then \
           printf '%s\\n' 'runtime instance changed while stopping' >&2; exit 42; \
         fi; \
         tmux kill-window -t {}; \
         {cleanup}",
        sh_quote(&service.pane_id),
        sh_quote(&service.pane_id),
        sh_quote(&service.window_id),
        expected = sh_quote(&expected),
    );
    with_project_lock(&service.project, &body)
}

fn stop_project_command(project: &str, timeout: Duration) -> Result<String> {
    validate_component("project", project)?;
    let session_name = session_name(project);
    let session_target = exact_session_target(project);
    let inventory = pane_inventory_command();
    let wait_for_exit = wait_for_panes_command("$targets", timeout)?;
    let body = format!(
        "set -e; session_name={}; session_target={}; \
         inventory=$({inventory}); \
         project_inventory=$(printf '%s\\n' \"$inventory\" | awk -F '\\|' -v session=\"$session_name\" '$1 == session'); \
         managed_logs=; \
         if [ -n \"$project_inventory\" ]; then \
           managed_logs=$(printf '%s\\n' \"$project_inventory\" | awk -F '\\|' '$6 != \"\" {{ print $6 \"|\" $7 }}'); \
           targets=$(printf '%s\\n' \"$project_inventory\" | \
             awk -F '\\|' '$6 != \"\" && (($4 != \"\" && $3 == $4) || ($4 == \"\" && $5 == \"1\")) && $9 == \"0\" {{ print $3 }}'); \
           if [ -n \"$targets\" ]; then \
             for pane_id in $targets; do tmux send-keys -t \"$pane_id\" C-c; done; \
             {wait_for_exit}; \
           fi; \
           tmux kill-session -t \"$session_target\"; \
         fi; \
         if [ -n \"$managed_logs\" ]; then \
           printf '%s\\n' \"$managed_logs\" | while IFS='|' read -r service runtime_id; do \
             case \"$service\" in ''|*[!A-Za-z0-9_-]*) continue ;; esac; \
             case \"$runtime_id\" in ??????) ;; *) continue ;; esac; \
             case \"$runtime_id\" in *[!A-Za-z0-9]*) continue ;; esac; \
             rm -rf \"$HOME/{LOG_ROOT}/{project}/$service.$runtime_id\"; \
           done; \
         fi; \
         rmdir \"$HOME/{LOG_ROOT}/{project}\" 2>/dev/null || true",
        sh_quote(&session_name),
        sh_quote(&session_target),
    );
    with_project_lock(project, &body)
}

fn logs_command(service: &ObservedService, tail: usize) -> Result<(String, bool)> {
    let expected = expected_identity(service);

    if let Some(log_ref) = persistent_log_ref(service)? {
        let display_state = display_log_state_command(&service.pane_id);
        let display_pipe = display_log_pipe_command(&service.pane_id);
        let command = format!(
            "current=$({display_identity} 2>/dev/null || true); \
             if [ \"$current\" != {expected} ]; then \
               printf '%s\\n' 'runtime instance changed while resolving logs' >&2; exit 42; \
             fi; \
             state=$({display_state} 2>/dev/null || true); \
             case \"$state\" in \
               1\\|1) \
                 drain_attempt=0; \
                 while [ \"$({display_pipe} 2>/dev/null || true)\" = 1 ] && [ \"$drain_attempt\" -lt {drain_max_polls} ]; do \
                   sleep {drain_poll_seconds}; drain_attempt=$((drain_attempt + 1)); \
                 done; \
                 sleep {drain_grace_seconds} ;; \
               1\\|0|0\\|1) ;; \
               *) printf '%s\\n' 'runtime log stream is not attached; recreate the service' >&2; exit 43 ;; \
             esac; \
             log_ref={log_ref}; log_path=\"$HOME/$log_ref\"; \
             [ -f \"$log_path\" ] || {{ printf '%s\\n' 'runtime log file is missing' >&2; exit 44; }}; \
             tail -n {tail} \"$log_path\"",
            display_identity = display_identity_command(&service.pane_id),
            display_state = display_state,
            display_pipe = display_pipe,
            expected = sh_quote(&expected),
            log_ref = sh_quote(&log_ref),
            drain_max_polls = LOG_DRAIN_MAX_POLLS,
            drain_poll_seconds = LOG_DRAIN_POLL_SECONDS,
            drain_grace_seconds = LOG_DRAIN_GRACE_SECONDS,
        );
        return Ok((command, true));
    }

    let verify = format!(
        "current=$({} 2>/dev/null || true); \
         if [ \"$current\" != {} ]; then \
           printf '%s\\n' 'runtime instance changed while resolving logs' >&2; exit 42; \
         fi",
        display_identity_command(&service.pane_id),
        sh_quote(&expected),
    );
    let start = format!("-{tail}");
    let command = format!(
        "{verify}; {}",
        tmux(&["capture-pane", "-p", "-t", &service.pane_id, "-S", &start,])
    );
    Ok((command, false))
}

fn follow_logs_command(service: &ObservedService, tail: usize) -> Result<String> {
    let Some(log_ref) = persistent_log_ref(service)? else {
        bail!(
            "runtime service `{}/{}` has no persistent transcript; recreate it before following logs",
            service.host,
            service.name
        );
    };
    let expected = expected_identity(service);
    let display_identity = display_identity_command(&service.pane_id);
    let display_state = display_log_state_command(&service.pane_id);
    let display_pipe = display_log_pipe_command(&service.pane_id);

    // tmux exposes whether the pipe is open, but no durable "all bytes flushed"
    // event. Keep the drain bounded so a broken pipe cannot hang log following.
    Ok(format!(
        "current=$({display_identity} 2>/dev/null || true); \
         if [ \"$current\" != {expected} ]; then \
           printf '%s\\n' 'runtime instance changed while resolving logs' >&2; exit 42; \
         fi; \
         log_ref={log_ref}; log_path=\"$HOME/$log_ref\"; \
         [ -f \"$log_path\" ] || {{ printf '%s\\n' 'runtime log file is missing' >&2; exit 44; }}; \
         state=$({display_state} 2>/dev/null || true); \
         case \"$state\" in \
           *\\|1) exec tail -n {tail} \"$log_path\" ;; \
           1\\|0) ;; \
           *) printf '%s\\n' 'runtime log stream is not attached; recreate the service' >&2; exit 43 ;; \
         esac; \
         follow_pid=; \
         cleanup_follow() {{ \
           [ -z \"$follow_pid\" ] || kill \"$follow_pid\" 2>/dev/null || true; \
           [ -z \"$follow_pid\" ] || wait \"$follow_pid\" 2>/dev/null || true; \
         }}; \
         trap cleanup_follow EXIT; \
         trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM; \
         tail -n {tail} -F \"$log_path\" & follow_pid=$!; \
         while kill -0 \"$follow_pid\" 2>/dev/null; do \
           current=$({display_identity} 2>/dev/null || true); \
           [ \"$current\" = {expected} ] || break; \
           state=$({display_state} 2>/dev/null || true); \
           case \"$state\" in \
             *\\|1) \
               drain_attempt=0; \
               while [ \"$({display_pipe} 2>/dev/null || true)\" = 1 ] && [ \"$drain_attempt\" -lt {drain_max_polls} ]; do \
                 sleep {drain_poll_seconds}; drain_attempt=$((drain_attempt + 1)); \
               done; \
               sleep {drain_grace_seconds}; break ;; \
           esac; \
           sleep 0.1; \
         done; \
         if kill -0 \"$follow_pid\" 2>/dev/null; then exit 0; fi; \
         wait \"$follow_pid\"; follow_status=$?; follow_pid=; exit \"$follow_status\"",
        expected = sh_quote(&expected),
        log_ref = sh_quote(&log_ref),
        drain_max_polls = LOG_DRAIN_MAX_POLLS,
        drain_poll_seconds = LOG_DRAIN_POLL_SECONDS,
        drain_grace_seconds = LOG_DRAIN_GRACE_SECONDS,
    ))
}

fn with_project_lock(project: &str, body: &str) -> Result<String> {
    validate_component("project", project)?;
    let lock_session = format!("{LOCK_SESSION_PREFIX}{project}");
    let lock_target = format!("={lock_session}");
    Ok(format!(
        "{REQUIRE_TMUX}; lock_session={}; lock_target={}; owner_pid=$$; \
         lock_monitor=\"while kill -0 $owner_pid 2>/dev/null; do sleep 0.2; done\"; \
         while :; do \
           if lock_error=$(tmux new-session -d -s \"$lock_session\" -n lock \"$lock_monitor\" 2>&1 >/dev/null); then \
             break; \
           else \
             lock_status=$?; \
           fi; \
           case \"$lock_error\" in \
             *\"duplicate session:\"*) sleep 0.05 ;; \
             *) printf '%s\\n' \"$lock_error\" >&2; exit \"$lock_status\" ;; \
           esac; \
         done; \
         cleanup_lock() {{ tmux kill-session -t \"$lock_target\" 2>/dev/null || true; }}; \
         trap cleanup_lock EXIT; \
         trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM; \
         {body}",
        sh_quote(&lock_session),
        sh_quote(&lock_target),
    ))
}

fn ensure_session_command(session: &str) -> String {
    let target = format!("={session}");
    let new_session = tmux(&[
        "new-session",
        "-dP",
        "-F",
        "#{window_id}",
        "-s",
        session,
        "-n",
        BOOTSTRAP_WINDOW,
        PLACEHOLDER_COMMAND,
    ]);
    format!(
        "if ! {} 2>/dev/null; then \
           if bootstrap_window=$({new_session}); then \
             tmux set-window-option -t \"$bootstrap_window\" automatic-rename off; \
             tmux set-window-option -t \"$bootstrap_window\" allow-rename off; \
             tmux set-window-option -t \"$bootstrap_window\" remain-on-exit on; \
             tmux rename-window -t \"$bootstrap_window\" {}; \
           else \
             {} 2>/dev/null; \
           fi; \
         fi",
        tmux(&["has-session", "-t", &target]),
        sh_quote(BOOTSTRAP_WINDOW),
        tmux(&["has-session", "-t", &target]),
    )
}

fn find_service_windows(session: &str, service: &str) -> String {
    format!(
        "tmux list-windows -t {} -F '#{{window_id}}|#{{@distrun_service}}' | awk -F '\\\\|' -v service={} '$2 == service {{ print $1 }}'",
        sh_quote(session),
        sh_quote(service),
    )
}

fn display_identity_command(pane_id: &str) -> String {
    tmux(&["display-message", "-p", "-t", pane_id, IDENTITY_FORMAT])
}

fn display_log_state_command(pane_id: &str) -> String {
    tmux(&[
        "display-message",
        "-p",
        "-t",
        pane_id,
        "#{pane_pipe}|#{pane_dead}",
    ])
}

fn display_log_pipe_command(pane_id: &str) -> String {
    tmux(&["display-message", "-p", "-t", pane_id, "#{pane_pipe}"])
}

fn pane_inventory_command() -> String {
    let format = format!("#{{W:#{{P:{PANE_LIST_FORMAT}\n}}}}");
    // This server-scoped option is synchronous and needs no pane on tmux 2.9.
    let query = tmux(&[
        "start-server",
        ";",
        "list-sessions",
        "-F",
        &format,
        ";",
        "show-options",
        "-g",
        "exit-empty",
    ]);
    format!(
        "{REQUIRE_TMUX}; output=$({query}) || exit $?; \
         completion='exit-empty on'; inventory=${{output%\"$completion\"}}; \
         if [ \"$inventory\" = \"$output\" ]; then \
           completion='exit-empty off'; inventory=${{output%\"$completion\"}}; \
         fi; \
         if [ \"$inventory\" = \"$output\" ]; then \
           printf '%s\n' 'tmux inventory did not complete' >&2; exit 1; \
         fi; \
         printf '%s' \"$inventory\"",
    )
}

fn expected_identity(service: &ObservedService) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        session_name(&service.project),
        service.window_id,
        service.pane_id,
        service.name,
        service.runtime_id.as_deref().unwrap_or_default(),
    )
}

fn cleanup_service_log_command(service: &ObservedService) -> String {
    let Ok(Some(log_ref)) = persistent_log_ref(service) else {
        return ":".to_owned();
    };
    let Some(run_dir_ref) = log_ref.strip_suffix("/pty.log") else {
        return ":".to_owned();
    };
    format!(
        "rm -rf \"$HOME/{run_dir_ref}\"; rmdir \"$HOME/{LOG_ROOT}/{}\" 2>/dev/null || true",
        service.project
    )
}

fn persistent_log_ref(service: &ObservedService) -> Result<Option<String>> {
    match &service.runtime_id {
        None => Ok(None),
        Some(runtime_id) => {
            validate_component("project", &service.project)?;
            validate_component("service", &service.name)?;
            if runtime_id.len() != 6 || !runtime_id.bytes().all(|byte| byte.is_ascii_alphanumeric())
            {
                bail!("runtime has an invalid id");
            }
            Ok(Some(format!(
                "{LOG_ROOT}/{}/{}.{runtime_id}/pty.log",
                service.project, service.name
            )))
        }
    }
}

fn parse_pane_lines(host: &str, output: &str) -> Result<Vec<ObservedService>> {
    let mut services = Vec::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        if let Some(service) = parse_pane_line(host, line)? {
            services.push(service);
        }
    }
    Ok(services)
}

fn parse_pane_line(host: &str, line: &str) -> Result<Option<ObservedService>> {
    let fields = line.split('|').collect::<Vec<_>>();
    let &[
        session,
        window_id,
        pane_id,
        managed_pane_id,
        pane_active,
        name,
        runtime_id,
        ready,
        pane_dead,
    ] = fields.as_slice()
    else {
        return Ok(None);
    };

    let Some(project) = session.strip_prefix(SESSION_PREFIX) else {
        return Ok(None);
    };
    if project.is_empty() || window_id.is_empty() || pane_id.is_empty() || name.is_empty() {
        return Ok(None);
    }
    if (!managed_pane_id.is_empty() && managed_pane_id != pane_id)
        || (managed_pane_id.is_empty() && pane_active != "1")
    {
        return Ok(None);
    }
    if !runtime_id.is_empty() && ready != "1" {
        return Ok(None);
    }

    Ok(Some(ObservedService {
        project: project.to_owned(),
        host: host.to_owned(),
        name: name.to_owned(),
        window_id: window_id.to_owned(),
        pane_id: pane_id.to_owned(),
        runtime_id: (!runtime_id.is_empty()).then(|| runtime_id.to_owned()),
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

fn validate_component(kind: &str, value: &str) -> Result<()> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        Ok(())
    } else {
        bail!("invalid {kind} name `{value}`")
    }
}

fn session_name(project: &str) -> String {
    format!("{SESSION_PREFIX}{project}")
}

fn exact_session_target(project: &str) -> String {
    format!("={}", session_name(project))
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

fn stop_operation_timeout(grace: Duration) -> Result<Duration> {
    grace
        .checked_add(STOP_OPERATION_OVERHEAD)
        .context("stop timeout is too large")
}

fn wait_for_panes_command(targets: &str, timeout: Duration) -> Result<String> {
    let interval_nanos = STOP_POLL_INTERVAL.as_nanos();
    let Ok(polls) = i64::try_from(timeout.as_nanos() / interval_nanos) else {
        bail!("stop timeout is too large");
    };
    let remainder = Duration::from_nanos((timeout.as_nanos() % interval_nanos) as u64);

    Ok(format!(
        "stop_polls={polls}; \
         while :; do \
           stop_running=0; \
           for pane_id in {targets}; do \
             if [ \"$(tmux display-message -p -t \"$pane_id\" '#{{pane_dead}}' 2>/dev/null || true)\" != 1 ]; then \
               stop_running=1; break; \
             fi; \
           done; \
           [ \"$stop_running\" = 1 ] || break; \
           if [ \"$stop_polls\" -eq 0 ]; then sleep {}; break; fi; \
           sleep {}; stop_polls=$((stop_polls - 1)); \
         done",
        sleep_duration(remainder),
        sleep_duration(STOP_POLL_INTERVAL),
    ))
}

fn last_log_lines(output: &str, count: usize) -> String {
    if count == 0 {
        return String::new();
    }
    let trimmed = output.trim_end_matches(|ch: char| ch.is_whitespace());
    let lines = trimmed.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(count);
    if start == lines.len() {
        String::new()
    } else {
        format!("{}\n", lines[start..].join("\n"))
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
    fn start_command_attaches_a_plain_append_only_transcript() {
        let service = desired();
        let command = start_command(&service).expect("build start command");

        assert_valid_shell("start", &command);
        assert!(command.contains("'=distrun/debug_demo'"));
        assert!(command.contains("tmux pipe-pane -O"));
        assert!(command.contains("exec cat >>"));
    }

    #[test]
    fn project_session_targets_require_exact_tmux_matches() {
        assert_eq!(exact_session_target("demo"), "=distrun/demo");

        let stop = stop_project_command("demo", Duration::from_secs(1))
            .expect("build stop project command");
        assert_valid_shell("stop project", &stop);
        assert!(stop.contains("session_name='distrun/demo'"));
        assert!(stop.contains("session_target='=distrun/demo'"));
    }

    #[test]
    fn transcript_log_commands_use_tail() {
        let service = observed(true);
        let (snapshot, persistent) = logs_command(&service, 80).expect("build log command");
        let follow = follow_logs_command(&service, 80).expect("build follow command");

        assert!(persistent);
        assert_valid_shell("snapshot", &snapshot);
        assert_valid_shell("follow", &follow);
        assert!(snapshot.contains("tail -n 80"));
        assert!(follow.contains("tail -n 80 -F"));
    }

    #[test]
    fn sleep_duration_preserves_milliseconds() {
        assert_eq!(sleep_duration(Duration::from_millis(500)), "0.5");
        assert_eq!(sleep_duration(Duration::from_millis(1500)), "1.5");
        assert_eq!(sleep_duration(Duration::from_secs(2)), "2");
    }

    #[test]
    fn legacy_logs_drop_empty_terminal_area_after_output() {
        assert_eq!(
            last_log_lines("api ready\nGET /health 200\n\n\n", 80),
            "api ready\nGET /health 200\n"
        );
        assert_eq!(last_log_lines("one\ntwo\nthree\n\n", 2), "two\nthree\n");
    }

    #[test]
    fn pane_parser_keeps_managed_pane_and_active_legacy_pane() {
        let managed = "distrun/demo|@2|%3|%3|1|api|Ab12Cd|1|0";
        assert_eq!(
            parse_pane_line("web", managed).expect("parse managed pane"),
            Some(observed(true))
        );

        let split = "distrun/demo|@2|%4|%3|0|api|Ab12Cd|1|0";
        assert_eq!(
            parse_pane_line("web", split).expect("parse split pane"),
            None
        );

        let legacy = "distrun/demo|@2|%3||1|api|||0";
        let parsed = parse_pane_line("web", legacy)
            .expect("parse legacy pane")
            .expect("legacy service");
        assert_eq!(parsed.runtime_id, None);
    }

    #[test]
    fn pane_parser_hides_managed_runtimes_until_start_is_ready() {
        let starting = "distrun/demo|@2|%3|%3|1|api|Ab12Cd|0|0";
        assert_eq!(
            parse_pane_line("web", starting).expect("parse starting runtime"),
            None
        );

        let ready = "distrun/demo|@2|%3|%3|1|api|Ab12Cd|1|0";
        assert_eq!(
            parse_pane_line("web", ready).expect("parse ready runtime"),
            Some(observed(true))
        );
    }

    #[test]
    fn pane_parser_uses_dead_pane_as_exited_state() {
        let exited = "distrun/demo|@2|%3|%3|1|api|Ab12Cd|1|1";
        let parsed = parse_pane_line("web", exited)
            .expect("parse managed exited runtime")
            .expect("managed runtime");

        assert_eq!(parsed.runtime, RuntimeState::Exited);
    }

    #[test]
    fn pane_parser_ignores_unmanaged_sessions_and_windows() {
        assert_eq!(
            parse_pane_line("web", "manual|@2|%3|%3|1|api|||0").expect("parse unmanaged session"),
            None
        );
        assert_eq!(
            parse_pane_line("web", "distrun/demo|@2|%3|%3|1|||0").expect("parse unmarked window"),
            None
        );
    }

    #[test]
    fn persistent_log_path_is_derived_from_the_runtime_id() {
        let mut service = observed(true);
        assert_eq!(
            persistent_log_ref(&service).expect("valid log ref"),
            Some(".local/state/distrun/logs/demo/api.Ab12Cd/pty.log".to_owned())
        );

        service.runtime_id = Some("not/a/runtime/id".to_owned());
        assert!(persistent_log_ref(&service).is_err());
    }

    fn assert_valid_shell(name: &str, command: &str) {
        let mut checked_shells = 0;
        for shell in ["sh", "dash"] {
            let status = match std::process::Command::new(shell)
                .args(["-n", "-c", command])
                .status()
            {
                Ok(status) => status,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => panic!("parse {name} command with {shell}: {error}"),
            };
            checked_shells += 1;
            assert!(status.success(), "{shell} rejected the {name} command");
        }
        assert!(checked_shells > 0);
    }

    fn desired() -> DesiredService {
        DesiredService {
            project: "debug_demo".to_owned(),
            name: "api".to_owned(),
            host: "local".to_owned(),
            cmd: "printf 'ready\\n'; sleep 1".to_owned(),
            cwd: None,
            env: BTreeMap::new(),
            stop_timeout: Duration::from_secs(1),
        }
    }

    fn observed(persistent: bool) -> ObservedService {
        ObservedService {
            project: "demo".to_owned(),
            host: "web".to_owned(),
            name: "api".to_owned(),
            window_id: "@2".to_owned(),
            pane_id: "%3".to_owned(),
            runtime_id: persistent.then(|| "Ab12Cd".to_owned()),
            runtime: RuntimeState::Running,
        }
    }
}
