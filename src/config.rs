use crate::model::{
    DesiredService, HostTarget, HostTransport, LOCAL_HOST_NAME, OnExisting, Project,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default, deserialize_with = "deserialize_path_list")]
    include: Vec<PathBuf>,
    #[serde(
        default,
        rename = "include?",
        deserialize_with = "deserialize_path_list"
    )]
    include_optional: Vec<PathBuf>,
    project: Option<String>,
    #[serde(default)]
    on_existing: Option<RawOnExisting>,
    #[serde(default)]
    hosts: BTreeMap<String, RawHost>,
    #[serde(default)]
    services: BTreeMap<String, RawService>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RawOnExisting {
    #[default]
    Skip,
    Restart,
}

impl From<RawOnExisting> for OnExisting {
    fn from(value: RawOnExisting) -> Self {
        match value {
            RawOnExisting::Skip => Self::Skip,
            RawOnExisting::Restart => Self::Restart,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawHost {
    #[serde(default)]
    ssh: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawService {
    #[serde(default)]
    host: Option<String>,
    cmd: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "deserialize_env_files")]
    env_file: Vec<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    stop_timeout: Option<Duration>,
}

pub fn load(path: &Path, project_override: Option<&str>) -> Result<Project> {
    let mut loader = ConfigLoader::default();
    let raw = loader.load(path, IncludeMode::Required)?;
    normalize(raw, project_override)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IncludeMode {
    Required,
    Optional,
}

#[derive(Debug, Default)]
struct ConfigLoader {
    loaded: BTreeSet<PathBuf>,
    stack: Vec<PathBuf>,
}

impl ConfigLoader {
    fn load(&mut self, path: &Path, mode: IncludeMode) -> Result<RawConfig> {
        let path = match fs::canonicalize(path) {
            Ok(path) => path,
            Err(error) if mode == IncludeMode::Optional && error.kind() == ErrorKind::NotFound => {
                return Ok(RawConfig::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read config {}", path.display()));
            }
        };

        if let Some(index) = self.stack.iter().position(|stacked| stacked == &path) {
            let mut cycle = self.stack[index..]
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>();
            cycle.push(path.display().to_string());
            bail!("include cycle detected: {}", cycle.join(" -> "));
        }

        if !self.loaded.insert(path.clone()) {
            return Ok(RawConfig::default());
        }

        self.stack.push(path.clone());
        let result = self.load_canonical(&path);
        self.stack.pop();
        result
    }

    fn load_canonical(&mut self, path: &Path) -> Result<RawConfig> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let mut raw: RawConfig = serde_saphyr::from_str(&source)
            .with_context(|| format!("failed to parse YAML {}", path.display()))?;
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));

        let includes = std::mem::take(&mut raw.include);
        let optional_includes = std::mem::take(&mut raw.include_optional);

        let mut merged = RawConfig::default();
        for include in includes {
            let include_path = resolve_config_path(base_dir, &include);
            let included = self.load(&include_path, IncludeMode::Required)?;
            merge_raw_config(&mut merged, included, &include_path)?;
        }
        for include in optional_includes {
            let include_path = resolve_config_path(base_dir, &include);
            let included = self.load(&include_path, IncludeMode::Optional)?;
            merge_raw_config(&mut merged, included, &include_path)?;
        }

        resolve_env_files(base_dir, &mut raw);
        merge_raw_config(&mut merged, raw, path)?;
        Ok(merged)
    }
}

fn resolve_config_path(base_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base_dir.join(path)
    }
}

fn resolve_env_files(base_dir: &Path, raw: &mut RawConfig) {
    for service in raw.services.values_mut() {
        for env_file in &mut service.env_file {
            if !env_file.is_absolute() {
                *env_file = base_dir.join(&env_file);
            }
        }
    }
}

fn merge_raw_config(merged: &mut RawConfig, raw: RawConfig, source: &Path) -> Result<()> {
    if raw.project.is_some() {
        merged.project = raw.project;
    }
    if raw.on_existing.is_some() {
        merged.on_existing = raw.on_existing;
    }

    for (name, host) in raw.hosts {
        if merged.hosts.insert(name.clone(), host).is_some() {
            bail!("duplicate host `{name}` while loading {}", source.display());
        }
    }

    for (name, service) in raw.services {
        if merged.services.insert(name.clone(), service).is_some() {
            bail!(
                "duplicate service `{name}` while loading {}",
                source.display()
            );
        }
    }

    Ok(())
}

fn normalize(raw: RawConfig, project_override: Option<&str>) -> Result<Project> {
    let name = project_override
        .map(ToOwned::to_owned)
        .or(raw.project)
        .context("missing project name; set `project:` in distrun.yml or pass --project")?;

    validate_name("project", &name)?;

    let mut hosts = BTreeMap::new();
    for (host_name, host) in raw.hosts {
        validate_name("host", &host_name)?;
        let transport = normalize_host_transport(&host_name, host)?;
        hosts.insert(
            host_name.clone(),
            HostTarget {
                name: host_name,
                transport,
            },
        );
    }

    let mut services = BTreeMap::new();
    for (service_name, service) in raw.services {
        validate_name("service", &service_name)?;
        let service_host = service.host.unwrap_or_else(|| LOCAL_HOST_NAME.to_owned());

        if service_host == LOCAL_HOST_NAME {
            hosts
                .entry(LOCAL_HOST_NAME.to_owned())
                .or_insert(HostTarget {
                    name: LOCAL_HOST_NAME.to_owned(),
                    transport: HostTransport::Local,
                });
        } else if !hosts.contains_key(&service_host) {
            bail!(
                "service `{}` references unknown host `{}`",
                service_name,
                service_host
            );
        }
        let mut env = load_env_files(&service.env_file)
            .with_context(|| format!("failed to load env_file for service `{service_name}`"))?;
        env.extend(service.env);
        for key in env.keys() {
            validate_env_key(key)?;
        }

        services.insert(
            service_name.clone(),
            DesiredService {
                project: name.clone(),
                name: service_name,
                host: service_host,
                cmd: service.cmd,
                cwd: service.cwd,
                env,
                stop_timeout: service.stop_timeout.unwrap_or(DEFAULT_STOP_TIMEOUT),
            },
        );
    }

    Ok(Project {
        name,
        on_existing: raw.on_existing.unwrap_or_default().into(),
        hosts,
        services,
    })
}

fn normalize_host_transport(host_name: &str, host: RawHost) -> Result<HostTransport> {
    if host_name == LOCAL_HOST_NAME {
        if host.ssh.is_some() {
            bail!("host `{LOCAL_HOST_NAME}` cannot set `ssh`; local transport is fixed");
        }
        return Ok(HostTransport::Local);
    }

    host.ssh
        .map(HostTransport::Ssh)
        .with_context(|| format!("host `{host_name}` must set `ssh`"))
}

fn load_env_files(env_files: &[PathBuf]) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for env_file in env_files {
        let file_env = parse_env_file(env_file)?;
        env.extend(file_env);
    }
    Ok(env)
}

fn parse_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read env file {}", path.display()))?;
    let mut env = BTreeMap::new();
    for (index, line) in source.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (key, value) = line.split_once('=').with_context(|| {
            format!(
                "invalid env file line {} in {}; expected KEY=VALUE",
                index + 1,
                path.display()
            )
        })?;
        let key = key.trim();
        validate_env_key(key)?;
        env.insert(key.to_owned(), value.trim().to_owned());
    }
    Ok(env)
}

fn validate_name(kind: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        Ok(())
    } else {
        bail!("{kind} name `{value}` must contain only ASCII letters, numbers, `_`, or `-`")
    }
}

fn validate_env_key(value: &str) -> Result<()> {
    let mut chars = value.chars();
    let first = chars
        .next()
        .filter(|ch| ch.is_ascii_alphabetic() || *ch == '_');
    let rest_valid = chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');

    if first.is_some() && rest_valid {
        Ok(())
    } else {
        bail!("env key `{value}` must be a valid shell variable name")
    }
}

fn deserialize_optional_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawDuration {
        Seconds(u64),
        Text(String),
    }

    let value = Option::<RawDuration>::deserialize(deserializer)?;
    value
        .map(|value| match value {
            RawDuration::Seconds(seconds) => Ok(Duration::from_secs(seconds)),
            RawDuration::Text(text) => parse_duration(&text).map_err(serde::de::Error::custom),
        })
        .transpose()
}

fn deserialize_env_files<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_path_list(deserializer)
}

fn deserialize_path_list<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawPathList {
        One(PathBuf),
        Many(Vec<PathBuf>),
    }

    let value = Option::<RawPathList>::deserialize(deserializer)?;
    Ok(match value {
        Some(RawPathList::One(path)) => vec![path],
        Some(RawPathList::Many(paths)) => paths,
        None => Vec::new(),
    })
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    if let Some(milliseconds) = value.strip_suffix("ms") {
        milliseconds
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| format!("invalid duration `{value}`"))
    } else if let Some(seconds) = value.strip_suffix('s') {
        seconds
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("invalid duration `{value}`"))
    } else {
        value
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("invalid duration `{value}`; use seconds like `10s`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HostTransport, OnExisting};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn loads_required_and_optional_includes() {
        let dir = std::env::temp_dir().join(format!("distrun-config-{}", unique_id()));
        let services_dir = dir.join("services");
        fs::create_dir_all(&services_dir).expect("create temp config dir");
        let config_path = dir.join("distrun.yml");
        fs::write(
            dir.join("hosts.yml"),
            r#"hosts:
  web:
    ssh: web-prod
"#,
        )
        .expect("write hosts include");
        fs::write(
            services_dir.join("api.env"),
            r#"TOKEN=from-file
FILE_ONLY=ok
"#,
        )
        .expect("write included env file");
        fs::write(
            services_dir.join("api.yml"),
            r#"include: ../hosts.yml
services:
  api:
    host: web
    cmd: ./api
    env_file: api.env
    env:
      TOKEN: inline
"#,
        )
        .expect("write service include");
        fs::write(
            &config_path,
            r#"include:
  - services/api.yml
include?: missing.local.yml
project: demo
on_existing: restart
services:
  ui:
    cmd: pnpm dev
"#,
        )
        .expect("write root config");

        let project = load(&config_path, None).expect("load config");

        assert_eq!(project.on_existing, OnExisting::Restart);
        assert_eq!(
            project.hosts["web"].transport,
            HostTransport::Ssh("web-prod".to_owned())
        );
        assert_eq!(project.services["ui"].host, "local");
        assert_eq!(project.services["api"].host, "web");
        assert_eq!(project.services["api"].env["TOKEN"], "inline");
        assert_eq!(project.services["api"].env["FILE_ONLY"], "ok");
    }

    #[test]
    fn loads_local_and_ssh_hosts_with_cmd_services() {
        let dir = std::env::temp_dir().join(format!("distrun-config-{}", unique_id()));
        fs::create_dir_all(&dir).expect("create temp config dir");
        let config_path = dir.join("distrun.yml");
        fs::write(
            &config_path,
            r#"project: demo
hosts:
  local: {}
  web:
    ssh: web-prod
services:
  ui:
    cmd: pnpm dev
  watcher:
    host: local
    cmd: cargo watch
  api:
    host: web
    cmd: ./api
"#,
        )
        .expect("write temp config");

        let project = load(&config_path, None).expect("load config");

        assert_eq!(project.hosts["local"].transport, HostTransport::Local);
        assert_eq!(
            project.hosts["web"].transport,
            HostTransport::Ssh("web-prod".to_owned())
        );
        assert_eq!(project.services["ui"].host, "local");
        assert_eq!(project.services["ui"].cmd, "pnpm dev");
        assert_eq!(project.services["watcher"].host, "local");
        assert_eq!(project.services["api"].cmd, "./api");
    }

    #[test]
    fn rejects_ssh_on_explicit_local_host() {
        let dir = std::env::temp_dir().join(format!("distrun-config-{}", unique_id()));
        fs::create_dir_all(&dir).expect("create temp config dir");
        let config_path = dir.join("distrun.yml");
        fs::write(
            &config_path,
            r#"project: demo
hosts:
  local:
    ssh: localhost
services:
  ui:
    cmd: pnpm dev
"#,
        )
        .expect("write temp config");

        let error = load(&config_path, None).expect_err("local host with ssh should fail");

        assert!(error.to_string().contains("local transport is fixed"));
    }

    #[test]
    fn rejects_remote_host_without_ssh() {
        let dir = std::env::temp_dir().join(format!("distrun-config-{}", unique_id()));
        fs::create_dir_all(&dir).expect("create temp config dir");
        let config_path = dir.join("distrun.yml");
        fs::write(
            &config_path,
            r#"project: demo
hosts:
  web: {}
services:
  api:
    host: web
    cmd: ./api
"#,
        )
        .expect("write temp config");

        let error = load(&config_path, None).expect_err("remote host without ssh should fail");

        assert!(error.to_string().contains("must set `ssh`"));
    }

    #[test]
    fn rejects_duplicate_services_across_includes() {
        let dir = std::env::temp_dir().join(format!("distrun-config-{}", unique_id()));
        fs::create_dir_all(&dir).expect("create temp config dir");
        let config_path = dir.join("distrun.yml");
        fs::write(
            dir.join("one.yml"),
            r#"services:
  api:
    cmd: ./one
"#,
        )
        .expect("write first include");
        fs::write(
            dir.join("two.yml"),
            r#"services:
  api:
    cmd: ./two
"#,
        )
        .expect("write second include");
        fs::write(
            &config_path,
            r#"project: demo
include:
  - one.yml
  - two.yml
"#,
        )
        .expect("write root config");

        let error = load(&config_path, None).expect_err("duplicate service should fail");

        assert!(error.to_string().contains("duplicate service `api`"));
    }

    #[test]
    fn rejects_missing_required_include() {
        let dir = std::env::temp_dir().join(format!("distrun-config-{}", unique_id()));
        fs::create_dir_all(&dir).expect("create temp config dir");
        let config_path = dir.join("distrun.yml");
        fs::write(
            &config_path,
            r#"project: demo
include: missing.yml
services:
  ui:
    cmd: pnpm dev
"#,
        )
        .expect("write root config");

        let error = load(&config_path, None).expect_err("missing include should fail");
        let message = error.to_string();

        assert!(message.contains("failed to read config"));
        assert!(message.contains("missing.yml"));
    }

    fn unique_id() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let counter = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        format!("{}-{nanos}-{counter}", std::process::id())
    }
}
