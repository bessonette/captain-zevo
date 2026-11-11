//! First-run setup: adopt a Traefik that is already running, or start one.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::model::{Settings, TraefikMode, TraefikSettings};
use crate::runtime::{ContainerInfo, Runtime, TraefikSpec};
use crate::store::Store;
use crate::{Error, Result};

pub const MANAGED_CONTAINER: &str = "zevo-traefik";
pub const MANAGED_IMAGE: &str = "traefik:v3";

/// A Traefik container found on the host and what could be learned about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub container: String,
    pub running: bool,
    /// File-provider directory on the host, if one is configured and mounted.
    pub dynamic_dir: Option<PathBuf>,
    /// Entrypoint routes should bind to.
    pub entrypoint: Option<String>,
    /// Host port of that entrypoint.
    pub http_port: Option<u16>,
    /// Whether that entrypoint serves HTTPS.
    pub tls: bool,
    /// Network Traefik already shares with containers, if it has one.
    pub network: Option<String>,
    /// Reasons this Traefik cannot be adopted as is.
    pub problems: Vec<String>,
}

impl Candidate {
    pub fn adoptable(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Lists Traefik containers, running ones first.
pub async fn find_traefik(runtime: &dyn Runtime) -> Result<Vec<Candidate>> {
    let mut found: Vec<Candidate> = runtime
        .containers()
        .await?
        .iter()
        .filter(|c| is_traefik(c))
        .map(inspect)
        .collect();
    found.sort_by_key(|c| (!c.running, c.container.clone()));
    Ok(found)
}

fn is_traefik(c: &ContainerInfo) -> bool {
    let image = c.image.rsplit('/').next().unwrap_or(&c.image);
    image == "traefik" || image.starts_with("traefik:") || image.starts_with("traefik@")
}

/// Static-config values gathered from flags, environment and a mounted config file.
#[derive(Debug, Default)]
struct StaticConfig {
    /// Entrypoint name to the entrypoint it redirects all traffic to.
    redirects: Vec<(String, String)>,
    /// Entrypoints with TLS switched on in the static config.
    tls_entrypoints: Vec<String>,
    /// `providers.docker.network`.
    docker_network: Option<String>,
    file_dir: Option<String>,
    file_single: bool,
    /// (name, container port)
    entrypoints: Vec<(String, u16)>,
    /// A mounted static config file that could not be read from here.
    unreadable: Option<PathBuf>,
}

fn inspect(c: &ContainerInfo) -> Candidate {
    let config = static_config(c);
    let mut problems = Vec::new();
    if !c.running {
        problems.push("the container is not running".to_string());
    }
    // Without the static config nothing below can be trusted, so say only that.
    if let Some(path) = &config.unreadable
        && (config.file_dir.is_none() || config.entrypoints.is_empty())
    {
        problems.push(format!(
            "its config file {} cannot be read from here (Zevo may be running inside a \
             container); run Zevo on the machine that hosts Traefik, or pass --dynamic-dir \
             and --entrypoint",
            path.display()
        ));
        return Candidate {
            container: c.name.clone(),
            running: c.running,
            dynamic_dir: None,
            entrypoint: None,
            http_port: None,
            tls: false,
            network: None,
            problems,
        };
    }

    let dynamic_dir = match &config.file_dir {
        Some(dir) => {
            let host = host_path(c, dir);
            if host.is_none() {
                problems.push(format!(
                    "the file-provider directory {dir} is not mounted from the host"
                ));
            }
            host
        }
        None => {
            problems.push(if config.file_single {
                "the file provider watches a single file; it must watch a directory \
                 (set providers.file.directory)"
                    .to_string()
            } else {
                "the file provider is not enabled; add `--providers.file.directory=<dir>` \
                 and `--providers.file.watch=true`, mount <dir> from the host, and restart Traefik"
                    .to_string()
            });
            None
        }
    };

    // An entrypoint that redirects everything (the usual 80 -> 443 setup) is
    // no place for routes; follow the redirect to where requests are served.
    let mut entry = pick_entrypoint(&config.entrypoints);
    let mut tls = false;
    if let Some((name, _)) = entry
        && let Some((_, to)) = config
            .redirects
            .iter()
            .find(|(from, _)| from.eq_ignore_ascii_case(name))
        && let Some(target) = config
            .entrypoints
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(to))
    {
        entry = Some(target);
        tls = true;
    }
    if let Some((name, port)) = entry {
        tls = tls
            || *port == 443
            || config
                .tls_entrypoints
                .iter()
                .any(|n| n.eq_ignore_ascii_case(name));
    }

    // Prefer the network Traefik's Docker provider is told to use; otherwise
    // the only user-defined network it is on.
    let user_networks: Vec<&String> = c
        .networks
        .keys()
        .filter(|n| !matches!(n.as_str(), "bridge" | "host" | "none"))
        .collect();
    let network = config
        .docker_network
        .clone()
        .filter(|n| c.networks.contains_key(n))
        .or_else(|| match user_networks.as_slice() {
            [only] => Some((*only).clone()),
            _ => None,
        });
    let http_port = entry.and_then(|(_, port)| {
        if c.host_network {
            Some(*port)
        } else {
            c.published_ports.get(port).copied()
        }
    });
    match entry {
        None => problems.push("no HTTP entrypoint found".to_string()),
        Some((name, port)) if http_port.is_none() => problems.push(format!(
            "entrypoint '{name}' (port {port}) is not published on the host"
        )),
        Some(_) => {}
    }

    Candidate {
        container: c.name.clone(),
        running: c.running,
        dynamic_dir,
        entrypoint: entry.map(|(name, _)| name.clone()),
        http_port,
        tls,
        network,
        problems,
    }
}

/// Prefers `web`, then port 80, then the first entrypoint that is not the
/// dashboard (`traefik`) or obviously TLS.
fn pick_entrypoint(entrypoints: &[(String, u16)]) -> Option<&(String, u16)> {
    entrypoints
        .iter()
        .find(|(name, _)| name == "web")
        .or_else(|| entrypoints.iter().find(|(_, port)| *port == 80))
        .or_else(|| {
            entrypoints
                .iter()
                .find(|(name, port)| name != "traefik" && *port != 443)
        })
}

fn static_config(c: &ContainerInfo) -> StaticConfig {
    let mut config = StaticConfig::default();

    // Lowest precedence first: mounted config file, then env, then flags.
    for candidate in [
        "/etc/traefik/traefik.yml",
        "/etc/traefik/traefik.yaml",
        "/traefik.yml",
    ] {
        if let Some((mount_source, path)) = host_mount_path(c, candidate) {
            match std::fs::read_to_string(&path) {
                Ok(text) => apply_yaml(&mut config, &text),
                // The mount is visible but holds no such file: try the next name.
                Err(_) if mount_source.is_dir() => continue,
                Err(_) => config.unreadable = Some(path),
            }
            break;
        }
    }
    for pair in &c.env {
        if let Some((key, value)) = pair.split_once('=')
            && let Some(key) = key.strip_prefix("TRAEFIK_")
        {
            apply_option(
                &mut config,
                &key.to_ascii_lowercase().replace('_', "."),
                value,
            );
        }
    }
    let mut args = c.args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = arg.strip_prefix("--") else {
            continue;
        };
        match flag.split_once('=') {
            Some((key, value)) => apply_option(&mut config, &key.to_ascii_lowercase(), value),
            // `--key value` form.
            None => {
                let key = flag.to_ascii_lowercase();
                if (key.starts_with("providers.file.") || key.starts_with("entrypoints."))
                    && let Some(value) = args.clone().next().filter(|v| !v.starts_with("--"))
                {
                    apply_option(&mut config, &key, value);
                    args.next();
                }
            }
        }
    }
    config
}

fn apply_option(config: &mut StaticConfig, key: &str, value: &str) {
    if key == "providers.file.directory" {
        config.file_dir = Some(value.to_string());
    } else if key == "providers.file.filename" {
        config.file_single = true;
    } else if key == "providers.docker.network" {
        config.docker_network = Some(value.to_string());
    } else if let Some(rest) = key.strip_prefix("entrypoints.") {
        if let Some(name) = rest.strip_suffix(".address") {
            if let Some(port) = address_port(value) {
                set_entrypoint(config, name, port);
            }
        } else if let Some(name) = rest.strip_suffix(".http.redirections.entrypoint.to") {
            config.redirects.push((name.to_string(), value.to_string()));
        } else if let Some((name, _)) = rest.split_once(".http.tls") {
            config.tls_entrypoints.push(name.to_string());
        }
    }
}

fn set_entrypoint(config: &mut StaticConfig, name: &str, port: u16) {
    match config
        .entrypoints
        .iter_mut()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
    {
        Some(existing) => existing.1 = port,
        None => config.entrypoints.push((name.to_string(), port)),
    }
}

/// Port from an entrypoint address such as `:80`, `0.0.0.0:8080` or `:80/tcp`.
fn address_port(address: &str) -> Option<u16> {
    let address = address.split('/').next()?;
    address.rsplit(':').next()?.parse().ok()
}

fn apply_yaml(config: &mut StaticConfig, text: &str) {
    let Ok(doc) = serde_norway::from_str::<serde_norway::Value>(text) else {
        return;
    };
    let file = doc.get("providers").and_then(|p| p.get("file"));
    if let Some(dir) = file
        .and_then(|f| f.get("directory"))
        .and_then(|d| d.as_str())
    {
        config.file_dir = Some(dir.to_string());
    }
    if file.and_then(|f| f.get("filename")).is_some() {
        config.file_single = true;
    }
    if let Some(entrypoints) = doc.get("entryPoints").and_then(|e| e.as_mapping()) {
        for (name, value) in entrypoints {
            if let (Some(name), Some(port)) = (
                name.as_str(),
                value
                    .get("address")
                    .and_then(|a| a.as_str())
                    .and_then(address_port),
            ) {
                set_entrypoint(config, name, port);
            }
            let http = value.get("http");
            if let (Some(name), Some(to)) = (
                name.as_str(),
                http.and_then(|h| h.get("redirections"))
                    .and_then(|r| r.get("entryPoint"))
                    .and_then(|e| e.get("to"))
                    .and_then(|t| t.as_str()),
            ) {
                config.redirects.push((name.to_string(), to.to_string()));
            }
            if let (Some(name), Some(_)) = (name.as_str(), http.and_then(|h| h.get("tls"))) {
                config.tls_entrypoints.push(name.to_string());
            }
        }
    }
    if let Some(network) = doc
        .get("providers")
        .and_then(|p| p.get("docker"))
        .and_then(|d| d.get("network"))
        .and_then(|n| n.as_str())
    {
        config.docker_network = Some(network.to_string());
    }
}

/// Maps a path inside the container to the host through its bind mounts.
fn host_path(c: &ContainerInfo, container_path: &str) -> Option<PathBuf> {
    host_mount_path(c, container_path).map(|(_, path)| path)
}

/// Like [`host_path`], also returning the host side of the mount used.
fn host_mount_path(c: &ContainerInfo, container_path: &str) -> Option<(PathBuf, PathBuf)> {
    let wanted = Path::new(container_path);
    c.mounts
        .iter()
        .filter(|m| !m.source.is_empty())
        .filter_map(|m| {
            let rest = wanted.strip_prefix(&m.destination).ok()?;
            let source = PathBuf::from(&m.source);
            // Joining an empty remainder would add a trailing slash.
            let path = if rest.as_os_str().is_empty() {
                source.clone()
            } else {
                source.join(rest)
            };
            Some((m.destination.len(), source, path))
        })
        // The most specific mount wins.
        .max_by_key(|(len, _, _)| *len)
        .map(|(_, source, path)| (source, path))
}

/// Records an existing Traefik in settings and joins it to the Zevo network.
pub async fn adopt(
    store: &Store,
    runtime: &dyn Runtime,
    candidate: &Candidate,
) -> Result<Settings> {
    if let Some(problem) = candidate.problems.first() {
        return Err(Error::Other(format!(
            "cannot adopt Traefik container '{}': {problem}",
            candidate.container
        )));
    }
    let (Some(dynamic_dir), Some(entrypoint), Some(http_port)) = (
        &candidate.dynamic_dir,
        &candidate.entrypoint,
        candidate.http_port,
    ) else {
        return Err(Error::Other("incomplete Traefik candidate".into()));
    };
    if !dynamic_dir.is_dir() {
        return Err(Error::Other(format!(
            "Traefik's route directory {} is not visible from here; \
             pass its location with --dynamic-dir",
            dynamic_dir.display()
        )));
    }

    let mut settings = store.settings()?;
    // Reuse the network Traefik already shares with containers, so nothing
    // that works today has to be reconnected.
    if let Some(network) = &candidate.network {
        settings.network = network.clone();
    }
    runtime.ensure_network(&settings.network).await?;
    attach_traefik(runtime, &candidate.container, &settings.network).await?;
    settings.traefik = Some(TraefikSettings {
        tls: candidate.tls,
        mode: TraefikMode::Adopted,
        container: Some(candidate.container.clone()),
        dynamic_dir: dynamic_dir.clone(),
        entrypoint: entrypoint.clone(),
        http_port,
    });
    store.save_settings(&settings)?;
    Ok(settings)
}

/// Records a Traefik that Zevo cannot inspect (for example a host binary).
/// Projects behind it need host-port routes.
pub fn adopt_manual(
    store: &Store,
    dynamic_dir: &Path,
    entrypoint: &str,
    http_port: u16,
    tls: bool,
) -> Result<Settings> {
    if !dynamic_dir.is_dir() {
        return Err(Error::Other(format!(
            "{} is not a directory",
            dynamic_dir.display()
        )));
    }
    let mut settings = store.settings()?;
    settings.traefik = Some(TraefikSettings {
        tls,
        mode: TraefikMode::Adopted,
        container: None,
        dynamic_dir: dynamic_dir.to_path_buf(),
        entrypoint: entrypoint.to_string(),
        http_port,
    });
    store.save_settings(&settings)?;
    Ok(settings)
}

/// Starts the `zevo-traefik` container. With `http_port` unset, uses 80 or the
/// first free port from 8080 up.
pub async fn bootstrap(
    store: &Store,
    runtime: &dyn Runtime,
    http_port: Option<u16>,
) -> Result<Settings> {
    let mut settings = store.settings()?;
    let http_port = match http_port {
        Some(port) => port,
        None => std::iter::once(80)
            .chain(8080..8100)
            .find(|port| !port_in_use(*port))
            .ok_or_else(|| Error::Other("no free HTTP port found; pass --port".into()))?,
    };
    let dynamic_dir = store.managed_dynamic_dir();
    std::fs::create_dir_all(&dynamic_dir)
        .map_err(|e| Error::io(format!("creating {}", dynamic_dir.display()), e))?;

    runtime.ensure_network(&settings.network).await?;
    runtime
        .run_traefik(&TraefikSpec {
            container: MANAGED_CONTAINER.to_string(),
            image: MANAGED_IMAGE.to_string(),
            network: settings.network.clone(),
            dynamic_dir: dynamic_dir.clone(),
            http_port,
        })
        .await?;

    settings.traefik = Some(TraefikSettings {
        tls: false,
        mode: TraefikMode::Managed,
        container: Some(MANAGED_CONTAINER.to_string()),
        dynamic_dir,
        entrypoint: "web".to_string(),
        http_port,
    });
    store.save_settings(&settings)?;
    Ok(settings)
}

/// Connects the Traefik container to the network unless it already is.
/// Returns true if it connected it.
pub(crate) async fn attach_traefik(
    runtime: &dyn Runtime,
    container: &str,
    network: &str,
) -> Result<bool> {
    let containers = runtime.containers().await?;
    let Some(info) = containers.iter().find(|c| c.name == container) else {
        return Err(Error::Runtime(format!(
            "Traefik container '{container}' not found"
        )));
    };
    if info.host_network || info.networks.contains_key(network) {
        return Ok(false);
    }
    runtime.connect(&info.id, network, &[]).await?;
    Ok(true)
}

pub(crate) fn port_in_use(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Mount;
    use crate::runtime::fake::FakeRuntime;

    fn traefik(args: &[&str], dir: &Path) -> ContainerInfo {
        ContainerInfo {
            id: "t1".into(),
            name: "traefik".into(),
            image: "docker.io/library/traefik:v3.1".into(),
            running: true,
            args: args.iter().map(|s| s.to_string()).collect(),
            mounts: vec![Mount {
                source: dir.to_string_lossy().into_owned(),
                destination: "/etc/traefik".into(),
            }],
            published_ports: [(80, 80), (8080, 8080)].into(),
            ..ContainerInfo::default()
        }
    }

    #[tokio::test]
    async fn finds_and_adopts_a_flag_configured_traefik() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("dynamic")).unwrap();
        let rt = FakeRuntime::new();
        rt.add_container(ContainerInfo {
            name: "db".into(),
            image: "postgres:17".into(),
            ..ContainerInfo::default()
        });
        rt.add_container(traefik(
            &[
                "traefik",
                "--api.insecure=true",
                "--providers.file.directory=/etc/traefik/dynamic",
                "--entryPoints.traefik.address=:8080",
                "--entryPoints.web.address=:80",
            ],
            tmp.path(),
        ));

        let found = find_traefik(&rt).await.unwrap();
        assert_eq!(found.len(), 1);
        let c = &found[0];
        assert!(c.adoptable(), "{:?}", c.problems);
        assert_eq!(
            c.dynamic_dir.as_deref(),
            Some(tmp.path().join("dynamic").as_path())
        );
        assert_eq!(c.entrypoint.as_deref(), Some("web"));
        assert_eq!(c.http_port, Some(80));

        let store = Store::open(tmp.path().join("cfg"));
        let settings = adopt(&store, &rt, c).await.unwrap();
        assert_eq!(
            settings.traefik.as_ref().unwrap().mode,
            TraefikMode::Adopted
        );
        assert_eq!(store.settings().unwrap(), settings);
        assert!(
            rt.container("traefik")
                .unwrap()
                .networks
                .contains_key("zevo")
        );
    }

    #[tokio::test]
    async fn reads_env_and_mounted_static_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("traefik.yml"),
            "entryPoints:\n  http:\n    address: ':8000'\nproviders:\n  file:\n    directory: /etc/traefik/conf.d\n",
        )
        .unwrap();
        let mut c = traefik(&["traefik"], tmp.path());
        c.published_ports = [(8000, 9000)].into();
        let found = inspect(&c);
        assert_eq!(found.dynamic_dir, Some(tmp.path().join("conf.d")));
        assert_eq!(found.entrypoint.as_deref(), Some("http"));
        assert_eq!(found.http_port, Some(9000));

        // Env overrides the file.
        c.env = vec!["TRAEFIK_PROVIDERS_FILE_DIRECTORY=/etc/traefik/routes".into()];
        assert_eq!(inspect(&c).dynamic_dir, Some(tmp.path().join("routes")));
    }

    #[test]
    fn says_when_the_static_config_is_out_of_reach() {
        let mut c = traefik(&["traefik"], Path::new("/nonexistent/host/dir"));
        c.mounts[0].destination = "/etc/traefik/traefik.yml".into();
        let found = inspect(&c);
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(found.problems[0].contains("cannot be read from here"));
    }

    #[tokio::test]
    async fn follows_the_https_redirect_and_reuses_the_shared_network() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("dynamic")).unwrap();
        std::fs::write(
            tmp.path().join("traefik.yml"),
            "entryPoints:
  web:
    address: ':80'
    http:
      redirections:
        entryPoint:
          to: websecure
          scheme: https
  websecure:
    address: ':443'
providers:
  docker:
    network: proxy
  file:
    directory: /etc/traefik/dynamic
    watch: true
",
        )
        .unwrap();
        let mut c = traefik(&["traefik"], tmp.path());
        c.published_ports = [(80, 80), (443, 443)].into();
        c.networks = [("proxy".to_string(), vec![])].into();
        let rt = FakeRuntime::new();
        rt.ensure_network("proxy").await.unwrap();
        rt.add_container(c);

        let found = find_traefik(&rt).await.unwrap();
        let c = &found[0];
        assert!(c.adoptable(), "{:?}", c.problems);
        assert_eq!(c.entrypoint.as_deref(), Some("websecure"));
        assert_eq!((c.http_port, c.tls), (Some(443), true));
        assert_eq!(c.network.as_deref(), Some("proxy"));

        let store = Store::open(tmp.path().join("cfg"));
        let settings = adopt(&store, &rt, c).await.unwrap();
        assert_eq!(settings.network, "proxy");
        assert!(settings.traefik.unwrap().tls);
        // No second network was created for it.
        assert_eq!(rt.networks(), ["proxy"]);
    }

    #[tokio::test]
    async fn explains_why_a_traefik_cannot_be_adopted() {
        let tmp = tempfile::tempdir().unwrap();
        let rt = FakeRuntime::new();
        let mut c = traefik(
            &[
                "traefik",
                "--providers.docker=true",
                "--entrypoints.web.address=:80",
            ],
            tmp.path(),
        );
        c.published_ports.clear();
        rt.add_container(c);

        let found = find_traefik(&rt).await.unwrap();
        assert!(!found[0].adoptable());
        assert!(
            found[0]
                .problems
                .iter()
                .any(|p| p.contains("file provider is not enabled"))
        );
        assert!(
            found[0]
                .problems
                .iter()
                .any(|p| p.contains("not published"))
        );

        let store = Store::open(tmp.path().join("cfg"));
        assert!(adopt(&store, &rt, &found[0]).await.is_err());
        assert!(store.settings().unwrap().traefik.is_none());
    }

    #[tokio::test]
    async fn bootstrap_starts_a_managed_traefik() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path());
        let rt = FakeRuntime::new();

        let settings = bootstrap(&store, &rt, Some(8088)).await.unwrap();
        let traefik = settings.traefik.unwrap();
        assert_eq!(traefik.mode, TraefikMode::Managed);
        assert_eq!(traefik.http_port, 8088);
        assert!(traefik.dynamic_dir.is_dir());
        assert_eq!(rt.networks(), ["zevo"]);

        // The container it started is itself recognisable and adoptable.
        let found = find_traefik(&rt).await.unwrap();
        assert_eq!(found[0].container, MANAGED_CONTAINER);
        assert!(found[0].adoptable(), "{:?}", found[0].problems);
    }
}
