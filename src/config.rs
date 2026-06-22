use crate::model::{
    DesiredService, HostTarget, HostTransport, LOCAL_HOST_NAME, OnExisting, Project,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
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
    on_existing: Option<String>,
    #[serde(default)]
    hosts: BTreeMap<String, RawHost>,
    #[serde(default)]
    services: BTreeMap<String, Value>,
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
    #[serde(default)]
    stop_timeout: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawServiceEnv {
    #[serde(default, deserialize_with = "deserialize_path_list")]
    env_file: Vec<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
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
        let process_env = env::vars().collect::<BTreeMap<_, _>>();

        interpolate_path_list(&mut raw.include, &process_env, "include", path)?;
        interpolate_path_list(&mut raw.include_optional, &process_env, "include?", path)?;

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

        resolve_env_files(base_dir, &mut raw, &process_env)?;
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

fn interpolate_path_list(
    paths: &mut [PathBuf],
    properties: &BTreeMap<String, String>,
    field: &str,
    source: &Path,
) -> Result<()> {
    for path in paths {
        *path = interpolate_path(path, properties).with_context(|| {
            format!(
                "failed to interpolate `{field}` path in {}",
                source.display()
            )
        })?;
    }
    Ok(())
}

fn interpolate_path(path: &Path, properties: &BTreeMap<String, String>) -> Result<PathBuf> {
    let path = path
        .to_str()
        .context("interpolated paths must be valid UTF-8")?;
    interpolate_value(path, properties).map(PathBuf::from)
}

fn resolve_env_files(
    base_dir: &Path,
    raw: &mut RawConfig,
    process_env: &BTreeMap<String, String>,
) -> Result<()> {
    for (service_name, service_value) in &mut raw.services {
        let mut service_env = parse_service_env(service_value, service_name)?;
        for env_file in &mut service_env.env_file {
            *env_file = interpolate_path(env_file, process_env).with_context(|| {
                format!("failed to interpolate `env_file` for service `{service_name}`")
            })?;
            if !env_file.is_absolute() {
                *env_file = base_dir.join(&env_file);
            }
        }
        store_service_env_files(service_value, service_env.env_file, service_name)?;
    }
    Ok(())
}

fn parse_service_env(value: &Value, service_name: &str) -> Result<RawServiceEnv> {
    serde_json::from_value(value.clone())
        .with_context(|| format!("failed to parse env settings for service `{service_name}`"))
}

fn store_service_env_files(
    value: &mut Value,
    env_files: Vec<PathBuf>,
    service_name: &str,
) -> Result<()> {
    let object = value
        .as_object_mut()
        .with_context(|| format!("service `{service_name}` must be a mapping"))?;
    if env_files.is_empty() {
        object.remove("env_file");
    } else {
        object.insert(
            "env_file".to_owned(),
            serde_json::to_value(env_files).context("failed to store resolved env_file paths")?,
        );
    }
    Ok(())
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
    let process_env = env::vars().collect::<BTreeMap<_, _>>();
    let name = normalize_project_name(raw.project, project_override, &process_env)?;
    let on_existing = normalize_on_existing(raw.on_existing, &process_env)?;

    validate_name("project", &name)?;

    let mut hosts = normalize_hosts(raw.hosts, &process_env)?;
    let services = normalize_services(raw.services, &name, &mut hosts, &process_env)?;

    Ok(Project {
        name,
        on_existing,
        hosts,
        services,
    })
}

fn normalize_project_name(
    raw_name: Option<String>,
    project_override: Option<&str>,
    properties: &BTreeMap<String, String>,
) -> Result<String> {
    match project_override {
        Some(name) => Ok(name.to_owned()),
        None => raw_name
            .map(|name| interpolate_config_value("project", &name, properties))
            .transpose()?
            .context("missing project name; set `project:` in distrun.yml or pass --project"),
    }
}

fn normalize_hosts(
    raw_hosts: BTreeMap<String, RawHost>,
    properties: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, HostTarget>> {
    let mut hosts = BTreeMap::new();
    for (host_name, host) in raw_hosts {
        validate_name("host", &host_name)?;
        let transport = normalize_host_transport(&host_name, host, properties)?;
        hosts.insert(
            host_name.clone(),
            HostTarget {
                name: host_name,
                transport,
            },
        );
    }
    Ok(hosts)
}

fn normalize_services(
    raw_services: BTreeMap<String, Value>,
    project_name: &str,
    hosts: &mut BTreeMap<String, HostTarget>,
    process_env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, DesiredService>> {
    let mut services = BTreeMap::new();
    for (service_name, service_value) in raw_services {
        validate_name("service", &service_name)?;

        let service_env = parse_service_env(&service_value, &service_name)?;
        let mut raw_env = load_env_files(&service_env.env_file)
            .with_context(|| format!("failed to load env_file for service `{service_name}`"))?;
        raw_env.extend(service_env.env);
        for key in raw_env.keys() {
            validate_env_key(key)?;
        }

        let resolved_env = interpolate_env(raw_env, process_env, &service_name)?;
        let mut service_properties = process_env.clone();
        service_properties.extend(resolved_env.clone());
        let service = interpolate_service(service_value, &service_properties, &service_name)?;

        let service_host = service.host.unwrap_or_else(|| LOCAL_HOST_NAME.to_owned());

        if service_host == LOCAL_HOST_NAME {
            hosts
                .entry(LOCAL_HOST_NAME.to_owned())
                .or_insert(HostTarget {
                    name: LOCAL_HOST_NAME.to_owned(),
                    transport: HostTransport::Local,
                });
        } else if !hosts.contains_key(&service_host) {
            bail!("service `{service_name}` references unknown host `{service_host}`");
        }

        let cmd = service.cmd;
        let cwd = service.cwd;
        let stop_timeout = service
            .stop_timeout
            .map(|value| parse_duration(&value).map_err(anyhow::Error::msg))
            .transpose()?
            .unwrap_or(DEFAULT_STOP_TIMEOUT);

        services.insert(
            service_name.clone(),
            DesiredService {
                project: project_name.to_owned(),
                name: service_name,
                host: service_host,
                cmd,
                cwd,
                env: resolved_env,
                stop_timeout,
            },
        );
    }
    Ok(services)
}

fn normalize_on_existing(
    value: Option<String>,
    properties: &BTreeMap<String, String>,
) -> Result<OnExisting> {
    let Some(value) = value else {
        return Ok(OnExisting::Skip);
    };
    let value = interpolate_config_value("on_existing", &value, properties)?;

    match value.as_str() {
        "skip" => Ok(OnExisting::Skip),
        "restart" => Ok(OnExisting::Restart),
        _ => bail!("on_existing `{value}` must be `skip` or `restart`"),
    }
}

fn normalize_host_transport(
    host_name: &str,
    host: RawHost,
    properties: &BTreeMap<String, String>,
) -> Result<HostTransport> {
    if host_name == LOCAL_HOST_NAME {
        if host.ssh.is_some() {
            bail!("host `{LOCAL_HOST_NAME}` cannot set `ssh`; local transport is fixed");
        }
        return Ok(HostTransport::Local);
    }

    host.ssh
        .map(|ssh| interpolate_config_value("hosts.*.ssh", &ssh, properties))
        .transpose()?
        .map(HostTransport::Ssh)
        .with_context(|| format!("host `{host_name}` must set `ssh`"))
}

fn load_env_files(env_files: &[PathBuf]) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for env_file in env_files {
        env.extend(parse_env_file(env_file)?);
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

fn interpolate_env(
    raw_env: BTreeMap<String, String>,
    process_env: &BTreeMap<String, String>,
    service_name: &str,
) -> Result<BTreeMap<String, String>> {
    raw_env
        .iter()
        .map(|(key, value)| {
            let mut properties = process_env.clone();
            properties.extend(
                raw_env
                    .iter()
                    .filter(|(other_key, _)| *other_key != key)
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
            interpolate_value(value, &properties)
                .with_context(|| {
                    format!("failed to interpolate `env.{key}` for service `{service_name}`")
                })
                .map(|value| (key.clone(), value))
        })
        .collect()
}

fn interpolate_service(
    mut service: Value,
    properties: &BTreeMap<String, String>,
    service_name: &str,
) -> Result<RawService> {
    let object = service
        .as_object_mut()
        .with_context(|| format!("service `{service_name}` must be a mapping"))?;
    object.remove("env_file");
    object.remove("env");

    let scope = format!("service `{service_name}`");
    interpolate_value_strings(&mut service, properties, &scope)?;
    serde_json::from_value(service)
        .with_context(|| format!("failed to parse service `{service_name}`"))
}

fn interpolate_value_strings(
    value: &mut Value,
    properties: &BTreeMap<String, String>,
    scope: &str,
) -> Result<()> {
    match value {
        Value::String(text) => {
            *text = interpolate_value(text, properties)
                .with_context(|| format!("failed to interpolate {scope}"))?;
        }
        Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                interpolate_value_strings(value, properties, &format!("{scope}[{index}]"))?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                interpolate_value_strings(value, properties, &format!("{scope}.{key}"))?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn interpolate_config_value(
    field: &str,
    value: &str,
    properties: &BTreeMap<String, String>,
) -> Result<String> {
    interpolate_value(value, properties).with_context(|| format!("failed to interpolate `{field}`"))
}

fn interpolate_value(value: &str, properties: &BTreeMap<String, String>) -> Result<String> {
    if !value.contains('$') {
        return Ok(value.to_owned());
    }

    serde_saphyr::from_str_with_options(value, interpolation_options(properties))
        .context("failed to interpolate scalar")
}

fn interpolation_options(properties: &BTreeMap<String, String>) -> serde_saphyr::Options {
    serde_saphyr::options! {
        property_syntax: serde_saphyr::PropertySyntax::BracedOrBare
    }
    .with_properties(
        properties
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>(),
    )
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
    let valid = chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');

    if valid {
        Ok(())
    } else {
        bail!("env key `{value}` must be a valid shell variable name")
    }
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
    use std::env;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn interpolation_uses_env_values_and_default_fallbacks() {
        let env = BTreeMap::from([
            ("EMPTY".to_owned(), String::new()),
            ("WORKSPACE".to_owned(), "/srv/app".to_owned()),
        ]);

        assert_eq!(
            interpolate_value("${WORKSPACE:-/tmp}/api", &env).expect("interpolate workspace"),
            "/srv/app/api"
        );
        assert_eq!(
            interpolate_value("${MISSING:-/tmp}", &env).expect("interpolate missing fallback"),
            "/tmp"
        );
        assert_eq!(
            interpolate_value("${EMPTY:-/tmp}", &env).expect("interpolate empty fallback"),
            "/tmp"
        );
    }

    #[test]
    fn interpolation_uses_serde_saphyr_compose_forms() {
        let env = BTreeMap::from([
            ("EMPTY".to_owned(), String::new()),
            ("SET".to_owned(), "value".to_owned()),
        ]);

        assert_eq!(
            interpolate_value("$SET", &env).expect("bare variable"),
            "value"
        );
        assert_eq!(
            interpolate_value("${MISSING-fallback}", &env).expect("unset fallback"),
            "fallback"
        );
        assert_eq!(
            interpolate_value("${EMPTY-fallback}", &env).expect("empty is set"),
            ""
        );
        assert_eq!(
            interpolate_value("${SET:+replacement}", &env).expect("non-empty replacement"),
            "replacement"
        );
        assert_eq!(
            interpolate_value("${EMPTY+replacement}", &env).expect("set replacement"),
            "replacement"
        );
        assert_eq!(
            interpolate_value("$${SET}", &env).expect("escaped interpolation"),
            "${SET}"
        );

        let message = format!(
            "{:#}",
            interpolate_value("${EMPTY:?must not be empty}", &env).expect_err("required value")
        );
        assert!(message.contains("must not be empty"));
    }

    #[test]
    fn interpolation_rejects_unset_values_without_default() {
        let error =
            interpolate_value("${WORKSPACE}/api", &BTreeMap::new()).expect_err("missing value");
        let message = format!("{error:#}");

        assert!(message.contains("missing property `WORKSPACE`"));
    }

    #[test]
    fn load_interpolates_config_service_paths_and_typed_values() {
        let dir = env::temp_dir().join(format!("distrun-config-{}", unique_id()));
        fs::create_dir_all(&dir).expect("create temp config dir");
        let config_path = dir.join("distrun.yml");
        fs::write(dir.join("service.env"), "TIMEOUT=250ms\n").expect("write env file");
        fs::write(
            &config_path,
            r#"project: ${MISSING_PROJECT:-demo}
on_existing: ${MISSING_ON_EXISTING:-restart}
hosts:
  web:
    ssh: ${MISSING_SSH:-web-prod}
services:
  api:
    host: web
    cmd: ./api
    env_file: ${MISSING_ENV_FILE:-service.env}
    stop_timeout: ${TIMEOUT:-10s}
"#,
        )
        .expect("write temp config");

        let project = load(&config_path, None).expect("load config");

        assert_eq!(project.name, "demo");
        assert_eq!(project.on_existing, OnExisting::Restart);
        assert_eq!(
            project.hosts["web"].transport,
            HostTransport::Ssh("web-prod".to_owned())
        );
        assert_eq!(
            project.services["api"].stop_timeout,
            Duration::from_millis(250)
        );
    }

    #[test]
    fn loads_required_and_optional_includes() {
        let dir = env::temp_dir().join(format!("distrun-config-{}", unique_id()));
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
        let dir = env::temp_dir().join(format!("distrun-config-{}", unique_id()));
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
        let dir = env::temp_dir().join(format!("distrun-config-{}", unique_id()));
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
        let dir = env::temp_dir().join(format!("distrun-config-{}", unique_id()));
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
        let dir = env::temp_dir().join(format!("distrun-config-{}", unique_id()));
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
        let dir = env::temp_dir().join(format!("distrun-config-{}", unique_id()));
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
