use crate::backend::Backend;
use crate::config;
use crate::executor::SystemExecutor;
use crate::model::{
    HostTarget, HostTransport, LOCAL_HOST_NAME, ObservedService, Project, RuntimeState,
    ServiceStatus,
};
use crate::ops;
use crate::tmux::TmuxBackend;
use anyhow::{Context as AnyhowContext, Result, bail};
use clap::{Parser, Subcommand};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_CONFIG_FILE: &str = "distrun.yml";
const ROOT_OPTIONS_PLACEMENT: &str = "Root options (-f/--file, -p/--project, --hosts-file, --host, and --ssh) must appear before COMMAND. For example: `distrun -p demo list`.";
const CONFIG_COMMAND_CONTEXT: &str = "This command requires a complete configuration. Pass -f/--file FILE before COMMAND, or run it from a directory containing ./distrun.yml. Runtime selectors (-p/--project, --hosts-file, --host, and --ssh) cannot be used.";
const ACTION_COMMAND_CONTEXT: &str = "This command requires a project. Pass -f/--file FILE, run it from a directory containing ./distrun.yml, or pass -p/--project PROJECT before COMMAND. Runtime host selectors also belong before COMMAND.";

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Run processes anywhere, with SSH + tmux.",
    after_help = ROOT_OPTIONS_PLACEMENT
)]
struct Cli {
    #[arg(
        short = 'f',
        long = "file",
        value_name = "FILE",
        conflicts_with_all = ["project", "hosts_file", "hosts", "ssh_targets"],
        help = "Load a complete project configuration."
    )]
    file: Option<PathBuf>,

    #[arg(
        short = 'p',
        long,
        value_name = "PROJECT",
        conflicts_with = "file",
        help = "Select an existing runtime project without loading project configuration."
    )]
    project: Option<String>,

    #[arg(
        long = "hosts-file",
        value_name = "FILE",
        conflicts_with_all = ["file", "ssh_targets"],
        help = "Load runtime host definitions from a YAML hosts inventory."
    )]
    hosts_file: Option<PathBuf>,

    #[arg(
        long = "host",
        value_name = "HOST",
        conflicts_with = "file",
        help = "Select a host alias from --hosts-file, or select local; repeat for more."
    )]
    hosts: Vec<String>,

    #[arg(
        long = "ssh",
        value_name = "TARGET",
        conflicts_with_all = ["file", "hosts_file"],
        help = "Select an ad-hoc SSH target; repeat for more."
    )]
    ssh_targets: Vec<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(
        about = "Ensure configured services are running.",
        override_usage = "distrun [-f FILE] up [SERVICE]...",
        after_help = CONFIG_COMMAND_CONTEXT
    )]
    Up {
        #[arg(value_name = "SERVICE")]
        services: Vec<String>,
    },
    #[command(
        about = "Recreate the whole project or selected configured services.",
        override_usage = "distrun [-f FILE] recreate [SERVICE]...",
        after_help = CONFIG_COMMAND_CONTEXT
    )]
    Recreate {
        #[arg(value_name = "SERVICE")]
        services: Vec<String>,
    },
    #[command(
        about = "Stop the entire project on the selected hosts.",
        override_usage = "distrun [ROOT OPTIONS] down",
        after_help = ACTION_COMMAND_CONTEXT
    )]
    Down,
    #[command(
        about = "Stop observed services, including orphans.",
        override_usage = "distrun [ROOT OPTIONS] stop [OPTIONS] <[HOST/]SERVICE>...",
        after_help = ACTION_COMMAND_CONTEXT
    )]
    Stop {
        #[arg(required = true, value_name = "[HOST/]SERVICE")]
        services: Vec<String>,
        #[arg(
            long,
            default_value = "5s",
            value_parser = parse_cli_duration,
            help = "Per-host service resolution timeout."
        )]
        timeout: Duration,
    },
    #[command(
        about = "Compare configured services with runtime state.",
        override_usage = "distrun [-f FILE] status [OPTIONS]",
        after_help = CONFIG_COMMAND_CONTEXT
    )]
    Status {
        #[arg(
            long,
            default_value = "5s",
            value_parser = parse_cli_duration,
            help = "Per-host status observation timeout."
        )]
        timeout: Duration,
    },
    #[command(
        about = "List observed runtime services in the current context.",
        visible_alias = "ls",
        alias = "ps",
        override_usage = "distrun [ROOT OPTIONS] list [OPTIONS]",
        after_help = ROOT_OPTIONS_PLACEMENT
    )]
    List {
        #[arg(
            long,
            help = "Include every observed project; cannot be combined with root --project."
        )]
        all_projects: bool,
        #[arg(
            long,
            default_value = "5s",
            value_parser = parse_cli_duration,
            help = "Per-host observation timeout."
        )]
        timeout: Duration,
    },
    #[command(
        about = "Print logs for one observed service.",
        override_usage = "distrun [ROOT OPTIONS] logs [OPTIONS] <[HOST/]SERVICE>",
        after_help = ACTION_COMMAND_CONTEXT
    )]
    Logs {
        #[arg(value_name = "[HOST/]SERVICE")]
        service: String,
        #[arg(short = 'f', long, help = "Follow new log output.")]
        follow: bool,
        #[arg(
            short = 'n',
            long,
            default_value_t = 80,
            value_name = "LINES",
            help = "Lines to show from the end of the log."
        )]
        tail: usize,
        #[arg(
            long,
            default_value = "5s",
            value_parser = parse_cli_duration,
            help = "Per-host service resolution timeout; finite log reads also use it."
        )]
        timeout: Duration,
    },
    #[command(
        about = "Open the read-only runtime and log browser.",
        override_usage = "distrun [ROOT OPTIONS] tui [OPTIONS]",
        after_help = ROOT_OPTIONS_PLACEMENT
    )]
    Tui {
        #[arg(
            long,
            help = "Include every observed project; cannot be combined with root --project."
        )]
        all_projects: bool,
        #[arg(
            short = 'n',
            long,
            default_value_t = 80,
            value_name = "LINES",
            help = "Log lines to fetch for the selected service."
        )]
        tail: usize,
        #[arg(
            long,
            default_value = "5s",
            value_parser = parse_cli_duration,
            help = "Per-host refresh and log read timeout."
        )]
        timeout: Duration,
    },
}

#[derive(Debug)]
enum Context {
    Config(PathBuf),
    Runtime {
        project: Option<String>,
        hosts: Vec<HostTarget>,
    },
    None,
}

#[derive(Debug)]
struct InventoryScope {
    hosts: Vec<HostTarget>,
    project_filter: Option<String>,
}

pub fn run() -> Result<()> {
    let Cli {
        file,
        project,
        hosts_file,
        hosts,
        ssh_targets,
        command,
    } = Cli::parse();
    validate_command_context(project.as_deref(), &command)?;
    let context = resolve_context(file, project, hosts_file, hosts, ssh_targets)?;
    let backend = TmuxBackend::new(SystemExecutor);

    match command {
        Command::Up { services } => {
            let project = require_config(&context, "up")?;
            print_operation_report(ops::up_selected(&backend, &project, &services)?)
        }
        Command::Recreate { services } => {
            let project = require_config(&context, "recreate")?;
            print_operation_report(ops::recreate(&backend, &project, &services)?)
        }
        Command::Down => {
            let project = action_project(&context)?;
            print_operation_report(ops::down(&backend, &project)?)
        }
        Command::Stop { services, timeout } => {
            let project = action_project(&context)?;
            print_operation_report(ops::stop_runtime_services_with_timeout(
                &backend, &project, &services, timeout,
            )?)
        }
        Command::Status { timeout } => {
            let project = require_config(&context, "status")?;
            status(&backend, &project, timeout)
        }
        Command::List {
            all_projects,
            timeout,
        } => {
            let scope = inventory_scope(&context, all_projects)?;
            list(&backend, &scope, timeout)
        }
        Command::Logs {
            service,
            follow,
            tail,
            timeout,
        } => {
            let project = action_project(&context)?;
            logs(&backend, &project, &service, follow, tail, timeout)
        }
        Command::Tui {
            all_projects,
            tail,
            timeout,
        } => {
            let scope = inventory_scope(&context, all_projects)?;
            crate::tui::run(scope.hosts, scope.project_filter, tail, timeout)
        }
    }
}

fn validate_command_context(project: Option<&str>, command: &Command) -> Result<()> {
    let all_projects = matches!(
        command,
        Command::List {
            all_projects: true,
            ..
        } | Command::Tui {
            all_projects: true,
            ..
        }
    );
    if project.is_some() && all_projects {
        bail!("--all-projects conflicts with --project");
    }
    Ok(())
}

fn resolve_context(
    file: Option<PathBuf>,
    project: Option<String>,
    hosts_file: Option<PathBuf>,
    host_names: Vec<String>,
    ssh_targets: Vec<String>,
) -> Result<Context> {
    if let Some(path) = file.or_else(|| {
        (project.is_none()
            && hosts_file.is_none()
            && host_names.is_empty()
            && ssh_targets.is_empty()
            && default_config_file().exists())
        .then(|| default_config_file().to_path_buf())
    }) {
        return Ok(Context::Config(path));
    }
    if project.is_none() && hosts_file.is_none() && host_names.is_empty() && ssh_targets.is_empty()
    {
        return Ok(Context::None);
    }
    if let Some(name) = project.as_deref() {
        config::empty_project(name, std::iter::empty()).map(|_| ())?;
    }
    let hosts = resolve_runtime_hosts(hosts_file.as_deref(), &host_names, &ssh_targets)?;
    Ok(Context::Runtime { project, hosts })
}

fn resolve_runtime_hosts(
    hosts_file: Option<&Path>,
    host_names: &[String],
    ssh_targets: &[String],
) -> Result<Vec<HostTarget>> {
    if let Some(path) = hosts_file {
        let available = config::load_hosts(path)?;
        if host_names.is_empty() {
            return nonempty_runtime_hosts(
                available.into_values().collect(),
                &format!("{} defines no hosts", path.display()),
            );
        }

        let mut seen = BTreeSet::new();
        let mut selected = Vec::new();
        for name in host_names {
            if !seen.insert(name) {
                continue;
            }
            selected.push(
                available.get(name).cloned().with_context(|| {
                    format!("host `{name}` is not defined in {}", path.display())
                })?,
            );
        }
        return nonempty_runtime_hosts(selected, "no hosts were selected");
    }

    let mut seen = BTreeSet::new();
    let mut hosts = Vec::new();
    for name in host_names {
        if name != LOCAL_HOST_NAME {
            bail!("host `{name}` requires --hosts-file; use --ssh {name} for an ad-hoc SSH target");
        }
        if seen.insert(name.clone()) {
            hosts.push(config::local_host());
        }
    }
    for target in ssh_targets {
        let host = host_from_ssh_target(target)?;
        if seen.insert(host.name.clone()) {
            hosts.push(host);
        }
    }
    if hosts.is_empty() && host_names.is_empty() && ssh_targets.is_empty() {
        hosts.push(config::local_host());
    }
    nonempty_runtime_hosts(hosts, "no hosts were selected")
}

fn require_config(context: &Context, command: &str) -> Result<Project> {
    match context {
        Context::Config(path) => config::load(path),
        Context::Runtime { .. } => bail!("`{command}` requires a configuration file"),
        Context::None => bail!(
            "missing configuration; pass --file <FILE> or create {}",
            DEFAULT_CONFIG_FILE
        ),
    }
}

fn action_project(context: &Context) -> Result<Project> {
    match context {
        Context::Config(path) => config::load(path),
        Context::Runtime {
            project: Some(name),
            hosts,
        } => config::empty_project(name, hosts.clone()),
        Context::Runtime { project: None, .. } => {
            bail!("runtime commands require --project <PROJECT>")
        }
        Context::None => bail!(
            "no project selected; pass --project <PROJECT> or create {}",
            DEFAULT_CONFIG_FILE
        ),
    }
}

fn inventory_scope(context: &Context, all_projects: bool) -> Result<InventoryScope> {
    match context {
        Context::Config(path) => {
            let project = config::load(path)?;
            Ok(InventoryScope {
                hosts: project.hosts.values().cloned().collect(),
                project_filter: (!all_projects).then_some(project.name),
            })
        }
        Context::Runtime { project, hosts } => Ok(InventoryScope {
            hosts: hosts.clone(),
            project_filter: project.clone(),
        }),
        Context::None => Ok(InventoryScope {
            hosts: vec![config::local_host()],
            project_filter: None,
        }),
    }
}

fn default_config_file() -> &'static Path {
    Path::new(DEFAULT_CONFIG_FILE)
}

fn host_from_ssh_target(target: &str) -> Result<HostTarget> {
    if target.is_empty() {
        bail!("--ssh value cannot be empty");
    }
    if target == LOCAL_HOST_NAME {
        bail!("`local` is not an SSH target; use --host local");
    }
    if target.contains('/') {
        bail!("--ssh target `{target}` cannot contain `/`");
    }
    Ok(HostTarget {
        name: target.to_owned(),
        transport: HostTransport::Ssh(target.to_owned()),
    })
}

fn nonempty_runtime_hosts(hosts: Vec<HostTarget>, reason: &str) -> Result<Vec<HostTarget>> {
    if hosts.is_empty() {
        bail!("runtime host scope is empty: {reason}");
    }
    Ok(hosts)
}

fn print_operation_report(report: ops::OperationReport) -> Result<()> {
    let mut failures = 0;
    for outcome in report.outcomes {
        if outcome.is_failure() {
            failures += 1;
            eprintln!("{}", outcome.line());
        } else {
            println!("{}", outcome.line());
        }
    }

    if failures > 0 {
        bail!("{failures} operation(s) failed");
    }
    Ok(())
}

fn status(
    backend: &TmuxBackend<SystemExecutor>,
    project: &Project,
    timeout: Duration,
) -> Result<()> {
    let report = ops::status(backend, project, timeout);
    print_statuses(&report.statuses);
    print_unavailable_hosts(&report.unavailable_hosts);
    fail_if_hosts_unavailable(&report.unavailable_hosts)
}

fn list(
    backend: &TmuxBackend<SystemExecutor>,
    scope: &InventoryScope,
    timeout: Duration,
) -> Result<()> {
    let mut report = ops::status_all_with_timeout(backend, &scope.hosts, timeout);
    if let Some(project) = scope.project_filter.as_deref() {
        report.observed.retain(|service| service.project == project);
    }
    print_observed_services(&report.observed);
    print_unavailable_hosts(&report.unavailable_hosts);
    fail_if_hosts_unavailable(&report.unavailable_hosts)
}

fn logs(
    backend: &TmuxBackend<SystemExecutor>,
    project: &Project,
    service: &str,
    follow: bool,
    tail: usize,
    timeout: Duration,
) -> Result<()> {
    if !follow {
        print!(
            "{}",
            ops::logs_for_runtime_service_with_timeout(backend, project, service, tail, timeout,)?
        );
        return Ok(());
    }
    follow_logs(backend, project, service, tail, timeout)
}

fn follow_logs(
    backend: &TmuxBackend<SystemExecutor>,
    project: &Project,
    selector: &str,
    tail: usize,
    timeout: Duration,
) -> Result<()> {
    let target =
        ops::resolve_runtime_service_for_logs_with_timeout(backend, project, selector, timeout)?;
    let host = project
        .hosts
        .get(&target.host)
        .with_context(|| format!("host `{}` is not in the current context", target.host))?;

    if target.runtime != RuntimeState::Running {
        eprintln!(
            "service `{selector}` is {}; showing available logs without following",
            target.runtime.as_str()
        );
        print!(
            "{}",
            backend.logs_with_timeout(host, &target, tail, timeout)?
        );
        return Ok(());
    }

    backend.follow_logs(host, &target, tail)
}

fn print_statuses(statuses: &[ServiceStatus]) {
    println!(
        "{:<16} {:<24} {:<12} {:<12} ISSUE",
        "HOST", "SERVICE", "RUNTIME", "RELATION"
    );
    for status in statuses {
        let issue = status
            .issue
            .as_ref()
            .map(|issue| issue.label())
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "{:<16} {:<24} {:<12} {:<12} {}",
            status.host,
            status.service,
            status.runtime.as_str(),
            status.relation.as_str(),
            issue
        );
    }
}

fn print_observed_services(statuses: &[ObservedService]) {
    let mut counts = BTreeMap::new();
    for status in statuses {
        *counts.entry(status.logical_key()).or_insert(0_usize) += 1;
    }
    println!("{:<16} {:<24} {:<24} RUNTIME", "HOST", "PROJECT", "SERVICE");
    for status in statuses {
        let service = if counts[&status.logical_key()] > 1 {
            format!("{} [{}]", status.name, status.instance_label())
        } else {
            status.name.clone()
        };
        println!(
            "{:<16} {:<24} {:<24} {}",
            status.host,
            status.project,
            service,
            status.runtime.as_str()
        );
    }
}

fn print_unavailable_hosts(hosts: &[ops::UnavailableHost]) {
    for host in hosts {
        eprintln!(
            "warning: {} unavailable: {}",
            host.host,
            host.message.trim()
        );
    }
}

fn fail_if_hosts_unavailable(hosts: &[ops::UnavailableHost]) -> Result<()> {
    if !hosts.is_empty() {
        bail!("{} host(s) unavailable", hosts.len());
    }
    Ok(())
}

fn parse_cli_duration(value: &str) -> std::result::Result<Duration, String> {
    let duration = if let Some(milliseconds) = value.strip_suffix("ms") {
        milliseconds
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| format!("invalid duration `{value}`"))?
    } else if let Some(seconds) = value.strip_suffix('s') {
        seconds
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("invalid duration `{value}`"))?
    } else {
        value
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("invalid duration `{value}`; use seconds like `5s`"))?
    };

    if duration.is_zero() {
        return Err("duration must be greater than zero".to_owned());
    }
    Ok(duration)
}
