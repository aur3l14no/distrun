use crate::model::{
    DesiredService, HostTarget, HostTransport, LOCAL_HOST_NAME, OnExisting, Project,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
struct RawConfig {
    project: Option<String>,
    #[serde(default)]
    on_existing: RawOnExisting,
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
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let raw: RawConfig = serde_saphyr::from_str(&source)
        .with_context(|| format!("failed to parse YAML {}", path.display()))?;
    let base_dir = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    normalize(raw, project_override, base_dir)
}

fn normalize(raw: RawConfig, project_override: Option<&str>, base_dir: &Path) -> Result<Project> {
    let name = project_override
        .map(ToOwned::to_owned)
        .or(raw.project)
        .context("missing project name; set `project:` in distrun.yml or pass --project")?;

    validate_name("project", &name)?;

    if raw.services.is_empty() {
        bail!("config must define at least one service");
    }

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
        let mut env = load_env_files(base_dir, &service.env_file)
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
        on_existing: raw.on_existing.into(),
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

fn load_env_files(base_dir: &Path, env_files: &[PathBuf]) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for env_file in env_files {
        let path = resolve_env_file(base_dir, env_file);
        let file_env = parse_env_file(&path)?;
        env.extend(file_env);
    }
    Ok(env)
}

fn resolve_env_file(base_dir: &Path, env_file: &Path) -> PathBuf {
    if env_file.is_absolute() {
        env_file.to_owned()
    } else {
        base_dir.join(env_file)
    }
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
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawEnvFile {
        One(PathBuf),
        Many(Vec<PathBuf>),
    }

    let value = Option::<RawEnvFile>::deserialize(deserializer)?;
    Ok(match value {
        Some(RawEnvFile::One(path)) => vec![path],
        Some(RawEnvFile::Many(paths)) => paths,
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
    use crate::model::HostTransport;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    fn unique_id() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos()
    }
}
