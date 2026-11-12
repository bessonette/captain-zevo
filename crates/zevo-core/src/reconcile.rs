//! Brings the real world in line with the registry: route files in Traefik's
//! directory and project containers attached to the shared network.

use std::path::Path;

use serde::Serialize;

use crate::health::{self, Listening};
use crate::model::{Host, LOCAL_HOST_ID, Project, Settings, Target};
use crate::runtime::{ContainerInfo, Runtime};
use crate::store::Store;
use crate::{Error, Result, certs, envfile, register, setup, traefik};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    WroteRoutes {
        slug: String,
    },
    RemovedOrphanRoutes {
        slug: String,
    },
    CreatedNetwork {
        network: String,
    },
    AttachedTraefik {
        container: String,
    },
    AttachedProject {
        slug: String,
        container: String,
    },
    /// Started the edge Traefik on a remote host.
    StartedEdge {
        host: String,
    },
    /// Wrote a project's routes into a remote host's edge Traefik.
    WroteEdgeRoutes {
        slug: String,
    },
    IssuedCertificate {
        slug: String,
    },
    WroteEnvFile {
        slug: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerState {
    Running,
    Stopped,
    /// No container exists for the project folder.
    Missing,
    /// The runtime could not be queried, or the project is on another host.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectStatus {
    pub slug: String,
    pub container: Option<String>,
    pub state: ContainerState,
    /// Whether the container is on the shared network under the project alias.
    pub attached: bool,
    pub urls: Vec<String>,
    /// Per route, in order: is its app listening where Traefik can reach it?
    pub backends: Vec<Listening>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub actions: Vec<Action>,
    pub projects: Vec<ProjectStatus>,
    pub warnings: Vec<String>,
    /// Devcontainers on this machine that are not registered yet.
    pub discovered: Vec<Discovered>,
}

/// A devcontainer Zevo found but does not manage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Discovered {
    /// Host id; `local` for this machine.
    pub host: String,
    /// The project folder the container was created from.
    pub folder: std::path::PathBuf,
    pub container: String,
    pub running: bool,
}

/// Devcontainers whose folder no registered project uses, one per folder
/// (a running container wins), sorted by folder. `within` limits them to
/// those inside one folder.
pub fn discover(
    host: &str,
    containers: &[ContainerInfo],
    projects: &[Project],
    within: Option<&str>,
) -> Vec<Discovered> {
    let within = within.map(|dir| normalize(Path::new(dir)));
    let registered: Vec<String> = projects
        .iter()
        .filter(|p| p.host == host)
        .map(|p| normalize(&p.path))
        .collect();
    let mut found: std::collections::BTreeMap<String, Discovered> = Default::default();
    for c in containers {
        let Some(folder) = c.labels.get("devcontainer.local_folder") else {
            continue;
        };
        let key = normalize(Path::new(folder));
        if registered.contains(&key) {
            continue;
        }
        if let Some(dir) = &within
            && dir != "/"
            && key != *dir
            && !key.starts_with(&format!("{dir}/"))
        {
            continue;
        }
        let candidate = Discovered {
            host: host.to_string(),
            folder: folder.into(),
            container: c.name.clone(),
            running: c.running,
        };
        match found.get(&key) {
            Some(existing) if existing.running || !candidate.running => {}
            _ => {
                found.insert(key, candidate);
            }
        }
    }
    found.into_values().collect()
}

/// URLs a project is served at.
pub fn urls(project: &Project, settings: &Settings) -> Vec<String> {
    let traefik = settings.traefik.as_ref();
    let tls = traefik.is_some_and(|t| t.tls);
    let scheme = if tls { "https" } else { "http" };
    let port = match traefik.map(|t| t.http_port) {
        None => String::new(),
        Some(443) if tls => String::new(),
        Some(80) if !tls => String::new(),
        Some(port) => format!(":{port}"),
    };
    project
        .routes
        .iter()
        .map(|r| {
            let host = r.hostname(&project.slug, &settings.domain_suffix);
            format!(
                "{scheme}://{host}{port}{}",
                r.path_prefix.as_deref().unwrap_or("")
            )
        })
        .collect()
}

/// Finds the container belonging to a project folder, preferring a running one.
pub fn find_container<'a>(
    project: &Project,
    containers: &'a [ContainerInfo],
) -> Option<&'a ContainerInfo> {
    let folder = normalize(&project.path);
    let label =
        |c: &'a ContainerInfo, key: &str| c.labels.get(key).map(|v| normalize(Path::new(v)));
    let matches = |c: &&'a ContainerInfo| {
        let is_folder = label(c, "devcontainer.local_folder").as_deref() == Some(folder.as_str());
        let service = c.labels.get("com.docker.compose.service");
        match &project.service {
            // Compose projects start several containers; only the named service counts.
            Some(wanted) => {
                service == Some(wanted)
                    && (is_folder
                        || label(c, "com.docker.compose.project.working_dir").is_some_and(|dir| {
                            dir == folder || dir.starts_with(&format!("{folder}/"))
                        }))
            }
            None => is_folder,
        }
    };
    let mut found: Vec<_> = containers.iter().filter(matches).collect();
    found.sort_by_key(|c| !c.running);
    found.into_iter().next()
}

fn normalize(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Runs one reconciliation pass. Route files are written even when the
/// runtime is unavailable; runtime problems are reported as warnings.
pub async fn reconcile(store: &Store, runtime: Option<&dyn Runtime>) -> Result<Report> {
    reconcile_with(store, runtime, &certs::Mkcert).await
}

/// [`reconcile`] with a chosen certificate issuer.
pub async fn reconcile_with(
    store: &Store,
    runtime: Option<&dyn Runtime>,
    issuer: &dyn certs::Issuer,
) -> Result<Report> {
    reconcile_hosts(store, runtime, &RemoteRuntimes::new(), issuer).await
}

/// Connections to remote hosts by host id: a runtime, or why it could not be
/// reached. Hosts missing from the map are treated as not connected.
pub type RemoteRuntimes<'a> =
    std::collections::BTreeMap<String, std::result::Result<&'a dyn Runtime, String>>;

/// Name of the edge Traefik container and its routes volume on remote hosts.
pub const EDGE_CONTAINER: &str = "zevo-edge";
/// Network the edge shares with project containers on a remote host. Always
/// Zevo's own: `settings.network` may be the name of a network this machine's
/// Traefik already had, and a network of that name on another machine belongs
/// to something else.
pub const EDGE_NETWORK: &str = crate::model::DEFAULT_NETWORK;
pub const EDGE_VOLUME: &str = "zevo-edge-routes";

/// What is known about one remote host during a pass.
struct RemoteHost<'a> {
    host: Host,
    runtime: &'a dyn Runtime,
    containers: Vec<ContainerInfo>,
    /// The edge Traefik is running there and can take route files.
    edge_ready: bool,
}

/// Reconciles local projects and projects on the given remote hosts.
pub async fn reconcile_hosts(
    store: &Store,
    runtime: Option<&dyn Runtime>,
    remotes: &RemoteRuntimes<'_>,
    issuer: &dyn certs::Issuer,
) -> Result<Report> {
    let settings = store.settings()?;
    let Some(traefik_settings) = &settings.traefik else {
        return Err(Error::NotSetUp);
    };
    let mut projects = store.projects()?;
    let mut report = Report::default();

    // Projects that declare their routes in devcontainer.json are re-read first.
    for project in &mut projects {
        match register::sync_declared(store, project) {
            Ok(Some(updated)) => *project = updated,
            Ok(None) => {}
            Err(e) => report.warnings.push(format!("{}: {e}", project.slug)),
        }
    }

    let needs_network = |p: &Project| {
        p.host == LOCAL_HOST_ID && p.routes.iter().any(|r| r.target == Target::Container)
    };
    let containers = match runtime {
        Some(runtime) if projects.iter().any(needs_network) => {
            match prepare_network(runtime, &settings, &mut report).await {
                Ok(containers) => Some(containers),
                Err(e) => {
                    report.warnings.push(e.to_string());
                    None
                }
            }
        }
        Some(runtime) => runtime.containers().await.ok(),
        None => None,
    };

    if let Some(containers) = &containers {
        report.discovered = discover(LOCAL_HOST_ID, containers, &projects, None);
    }
    // Remote hosts: connect-level problems are reported once per host, and a
    // host that is reachable gets its network and edge Traefik prepared.
    let hosts = store.hosts()?;
    let mut remote: std::collections::BTreeMap<String, RemoteHost> = Default::default();
    for host in &hosts {
        let rt = match remotes.get(&host.id) {
            Some(Ok(rt)) => *rt,
            Some(Err(e)) => {
                report
                    .warnings
                    .push(format!("{}: cannot reach the host: {e}", host.name));
                continue;
            }
            None => continue,
        };
        let needs_edge = projects
            .iter()
            .any(|p| p.host == host.id && p.routes.iter().any(|r| r.target == Target::Container));
        let mut edge_ready = false;
        if needs_edge {
            let prepared = async {
                rt.ensure_network(EDGE_NETWORK).await?;
                rt.ensure_edge(&crate::runtime::EdgeSpec {
                    container: EDGE_CONTAINER.to_string(),
                    image: setup::MANAGED_IMAGE.to_string(),
                    network: EDGE_NETWORK.to_string(),
                    volume: EDGE_VOLUME.to_string(),
                    port: host.edge_port,
                })
                .await
            }
            .await;
            match prepared {
                Ok(started) => {
                    edge_ready = true;
                    if started {
                        report.actions.push(Action::StartedEdge {
                            host: host.id.clone(),
                        });
                    }
                }
                Err(e) => report.warnings.push(format!("{}: {e}", host.name)),
            }
        }
        match rt.containers().await {
            Ok(containers) => {
                report.discovered.extend(discover(
                    &host.id,
                    &containers,
                    &projects,
                    host.projects_dir.as_deref(),
                ));
                // Kept even without projects, so files a removed project
                // left in the edge are cleaned up.
                remote.insert(
                    host.id.clone(),
                    RemoteHost {
                        host: host.clone(),
                        runtime: rt,
                        containers,
                        edge_ready,
                    },
                );
            }
            Err(e) => report.warnings.push(format!("{}: {e}", host.name)),
        }
    }

    let mut routable: std::collections::BTreeMap<String, String> = Default::default();
    let mut issuer_ready: Option<std::result::Result<(), String>> = None;
    let mut uncertified: Option<String> = None;
    for project in &projects {
        let mut status = ProjectStatus {
            slug: project.slug.clone(),
            container: None,
            state: ContainerState::Unknown,
            attached: false,
            urls: urls(project, &settings),
            backends: vec![Listening::Unknown; project.routes.len()],
        };
        let known_host = hosts.iter().find(|h| h.id == project.host);
        if project.host == LOCAL_HOST_ID {
            if let (Some(runtime), Some(containers)) = (runtime, &containers) {
                look_at_container(
                    runtime,
                    project,
                    containers,
                    &settings.network,
                    &mut status,
                    &mut report,
                )
                .await;
            }
        } else if let Some(ctx) = remote.get(&project.host) {
            look_at_container(
                ctx.runtime,
                project,
                &ctx.containers,
                EDGE_NETWORK,
                &mut status,
                &mut report,
            )
            .await;
            // The edge Traefik on that host routes to the container. Projects
            // that only use fixed addresses do not involve it, and a host
            // whose edge failed to start was already reported once.
            let uses_edge =
                ctx.edge_ready && project.routes.iter().any(|r| r.target == Target::Container);
            let path = format!(
                "{}/{}",
                crate::runtime::EDGE_ROUTES_DIR,
                traefik::file_name(&project.slug)
            );
            let written = async {
                if !uses_edge {
                    return Ok(false);
                }
                let wanted = traefik::render_edge(project, &settings, status.container.as_deref())?;
                if ctx.runtime.read_file(EDGE_CONTAINER, &path).await? == Some(wanted.clone()) {
                    return Ok(false);
                }
                ctx.runtime
                    .write_file(EDGE_CONTAINER, &path, &wanted)
                    .await?;
                Ok::<bool, Error>(true)
            }
            .await;
            match written {
                Ok(true) => report.actions.push(Action::WroteEdgeRoutes {
                    slug: project.slug.clone(),
                }),
                Ok(false) => {}
                Err(e) => report.warnings.push(format!("{}: {e}", project.slug)),
            }
        } else if known_host.is_none() {
            report.warnings.push(format!(
                "{}: its host '{}' is not registered; add it with `zevo host add`",
                project.slug, project.host
            ));
        }

        // HTTPS entrypoints need a certificate browsers trust for these names.
        if traefik_settings.tls
            && !project.routes.is_empty()
            && !certs::is_current(project, &settings)
        {
            // Settings saved before Zevo recorded where a containerised Traefik
            // reads its files cannot produce a path Traefik can open.
            let path_known =
                traefik_settings.container.is_none() || traefik_settings.container_dir.is_some();
            let ready = if path_known {
                issuer_ready.get_or_insert_with(|| issuer.ready()).clone()
            } else {
                Err(
                    "Open the Traefik page and choose \"Detect again\" (or run `zevo setup`) so \
                     Zevo learns where Traefik reads certificates"
                        .to_string(),
                )
            };
            match &ready {
                Ok(()) => match certs::ensure(project, &settings, issuer) {
                    Ok(true) => report.actions.push(Action::IssuedCertificate {
                        slug: project.slug.clone(),
                    }),
                    Ok(false) => {}
                    Err(e) => report.warnings.push(format!("{}: {e}", project.slug)),
                },
                // Fall back to whatever certificate Traefik already serves.
                Err(_) if certs::existing(&project.slug, traefik_settings).is_some() => {}
                Err(reason) => uncertified = Some(reason.clone()),
            }
        }

        // Containers are reached by their own name, which changes on rebuild,
        // so the file follows the container that currently exists.
        let wrote = match known_host {
            // Remote: this machine's Traefik hands over to the host's edge.
            Some(host) => {
                // Looked up once per host and pass: the name's current IP.
                let address = routable
                    .entry(host.id.clone())
                    .or_insert_with(|| crate::remote::routable_address(&host.address));
                traefik::write_project_via_edge(project, &settings, address, host.edge_port)
            }
            None => traefik::write_project_for(project, &settings, status.container.as_deref()),
        };
        match wrote {
            Ok(true) => report.actions.push(Action::WroteRoutes {
                slug: project.slug.clone(),
            }),
            Ok(false) => {}
            Err(e) => report.warnings.push(format!("{}: {e}", project.slug)),
        }
        if project.host == LOCAL_HOST_ID {
            match envfile::write(project, &settings) {
                Ok(true) => report.actions.push(Action::WroteEnvFile {
                    slug: project.slug.clone(),
                }),
                Ok(false) => {}
                Err(e) => report.warnings.push(format!("{}: {e}", project.slug)),
            }
        }
        report.projects.push(status);
    }

    for slug in traefik::managed_slugs(&traefik_settings.dynamic_dir)? {
        if !projects.iter().any(|p| p.slug == slug)
            && traefik::remove_project(&slug, &traefik_settings.dynamic_dir)?
        {
            report.actions.push(Action::RemovedOrphanRoutes { slug });
        }
    }
    // Route files left in an edge Traefik by projects that are gone.
    for ctx in remote.values() {
        let Ok(files) = ctx
            .runtime
            .list_dir(EDGE_CONTAINER, crate::runtime::EDGE_ROUTES_DIR)
            .await
        else {
            continue;
        };
        for file in files {
            let Some(slug) = traefik::slug_from_name(&file) else {
                continue;
            };
            if !projects
                .iter()
                .any(|p| p.slug == slug && p.host == ctx.host.id)
            {
                let path = format!("{}/{file}", crate::runtime::EDGE_ROUTES_DIR);
                if ctx.runtime.remove_file(EDGE_CONTAINER, &path).await.is_ok() {
                    report.actions.push(Action::RemovedOrphanRoutes {
                        slug: slug.to_string(),
                    });
                }
            }
        }
    }
    if let Some(reason) = uncertified {
        report.warnings.push(format!(
            "Zevo could not create HTTPS certificates, so browsers may warn about project \
             addresses unless your Traefik already has a certificate for them. {reason}"
        ));
    }
    Ok(report)
}

/// Finds a project's container on one machine, attaches it to the shared
/// network if needed, and checks that its routes' ports are reachable.
async fn look_at_container(
    runtime: &dyn Runtime,
    project: &Project,
    containers: &[ContainerInfo],
    network: &str,
    status: &mut ProjectStatus,
    report: &mut Report,
) {
    let uses_network = project.routes.iter().any(|r| r.target == Target::Container);
    let Some(c) = find_container(project, containers) else {
        status.state = ContainerState::Missing;
        return;
    };
    status.container = Some(c.name.clone());
    status.state = if c.running {
        ContainerState::Running
    } else {
        ContainerState::Stopped
    };
    status.attached = c.networks.contains_key(network);
    if !c.running || !uses_network {
        return;
    }
    if !status.attached {
        // The alias is a convenience; routes use the container name.
        let aliases = [project.slug.clone()];
        match runtime.connect(&c.id, network, &aliases).await {
            Ok(()) => {
                status.attached = true;
                report.actions.push(Action::AttachedProject {
                    slug: project.slug.clone(),
                    container: c.name.clone(),
                });
            }
            Err(e) => report.warnings.push(format!("{}: {e}", project.slug)),
        }
    }
    // Failing to look is not worth a warning; the routes just stay unchecked.
    if let Ok(table) = runtime.exec(&c.id, health::COMMAND).await {
        let ports = health::parse(&table);
        for (route, state) in project.routes.iter().zip(&mut status.backends) {
            if route.target == Target::Container {
                *state = health::port_state(&ports, route.port);
            }
        }
    }
}

/// Ensures the network exists and Traefik is on it; returns the container list.
async fn prepare_network(
    runtime: &dyn Runtime,
    settings: &Settings,
    report: &mut Report,
) -> Result<Vec<ContainerInfo>> {
    if runtime.ensure_network(&settings.network).await? {
        report.actions.push(Action::CreatedNetwork {
            network: settings.network.clone(),
        });
    }
    let containers = runtime.containers().await?;
    match settings
        .traefik
        .as_ref()
        .and_then(|t| t.container.as_deref())
    {
        Some(container) => {
            match setup::attach_traefik_in(runtime, &containers, container, &settings.network).await
            {
                Ok(true) => report.actions.push(Action::AttachedTraefik {
                    container: container.to_string(),
                }),
                Ok(false) => {}
                Err(e) => report.warnings.push(e.to_string()),
            }
        }
        None => report.warnings.push(
            "Traefik does not run in a container, so it cannot reach projects by container name; \
             use host-port routes"
                .to_string(),
        ),
    }
    // Attaching Traefik changes only Traefik's own entry, which the callers
    // do not read, so the list is still good.
    Ok(containers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DevcontainerKind, Route};
    use crate::runtime::fake::FakeRuntime;

    fn project(slug: &str, path: &str) -> Project {
        Project {
            declared: false,
            address: None,
            env_file: None,
            skip: Vec::new(),
            slug: slug.into(),
            name: slug.into(),
            host: LOCAL_HOST_ID.into(),
            path: path.into(),
            kind: DevcontainerKind::Image,
            service: None,
            routes: vec![Route {
                backend: crate::model::Backend::default(),
                host: None,
                subdomain: None,
                port: 3000,
                path_prefix: None,
                target: Target::Container,
            }],
            tags: vec![],
        }
    }

    fn devcontainer(name: &str, folder: &str) -> ContainerInfo {
        ContainerInfo {
            id: format!("id-{name}"),
            name: name.into(),
            running: true,
            labels: [("devcontainer.local_folder".to_string(), folder.to_string())].into(),
            networks: [("bridge".to_string(), vec![])].into(),
            ..ContainerInfo::default()
        }
    }

    async fn set_up() -> (tempfile::TempDir, Store, FakeRuntime) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path());
        let rt = FakeRuntime::new();
        setup::bootstrap(&store, &rt, Some(80), false)
            .await
            .unwrap();
        (tmp, store, rt)
    }

    #[tokio::test]
    async fn requires_setup() {
        let tmp = tempfile::tempdir().unwrap();
        let err = reconcile(&Store::open(tmp.path()), None).await.unwrap_err();
        assert!(matches!(err, Error::NotSetUp));
    }

    #[tokio::test]
    async fn writes_routes_attaches_and_is_idempotent() {
        let (_tmp, store, rt) = set_up().await;
        store.add_project(&project("shop", "/work/shop")).unwrap();
        rt.add_container(devcontainer("vsc-shop", "/work/shop/"));

        let report = reconcile(&store, Some(&rt)).await.unwrap();
        assert_eq!(
            report.actions,
            [
                Action::AttachedProject {
                    slug: "shop".into(),
                    container: "vsc-shop".into()
                },
                Action::WroteRoutes {
                    slug: "shop".into()
                },
            ]
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.projects[0].state, ContainerState::Running);
        assert!(report.projects[0].attached);
        assert_eq!(report.projects[0].urls, ["http://shop.localhost"]);
        assert_eq!(rt.container("vsc-shop").unwrap().networks["zevo"], ["shop"]);
        assert!(store.managed_dynamic_dir().join("zevo-shop.yml").exists());

        let again = reconcile(&store, Some(&rt)).await.unwrap();
        assert!(again.actions.is_empty(), "{:?}", again.actions);
    }

    #[tokio::test]
    async fn reattaches_after_a_rebuild_and_removes_orphans() {
        let (_tmp, store, rt) = set_up().await;
        store.add_project(&project("shop", "/work/shop")).unwrap();
        rt.add_container(devcontainer("vsc-shop-1", "/work/shop"));
        reconcile(&store, Some(&rt)).await.unwrap();

        // A rebuild replaces the container with a fresh, unattached one.
        rt.remove_container("vsc-shop-1");
        rt.add_container(devcontainer("vsc-shop-2", "/work/shop"));
        let report = reconcile(&store, Some(&rt)).await.unwrap();
        assert_eq!(
            report.actions,
            [
                Action::AttachedProject {
                    slug: "shop".into(),
                    container: "vsc-shop-2".into()
                },
                Action::WroteRoutes {
                    slug: "shop".into()
                },
            ]
        );

        let file =
            std::fs::read_to_string(store.managed_dynamic_dir().join("zevo-shop.yml")).unwrap();
        assert!(file.contains("http://vsc-shop-2:3000"), "{file}");

        store.remove_project("shop").unwrap();
        let report = reconcile(&store, Some(&rt)).await.unwrap();
        assert_eq!(
            report.actions,
            [Action::RemovedOrphanRoutes {
                slug: "shop".into()
            }]
        );
        assert!(!store.managed_dynamic_dir().join("zevo-shop.yml").exists());
    }

    #[tokio::test]
    async fn leaves_a_container_already_on_the_network_alone() {
        let (_tmp, store, rt) = set_up().await;
        store.add_project(&project("shop", "/work/shop")).unwrap();
        let mut c = devcontainer("vsc-shop", "/work/shop");
        c.networks.insert("zevo".into(), vec!["app".into()]);
        rt.add_container(c);

        let report = reconcile(&store, Some(&rt)).await.unwrap();
        // No reconnect: its existing aliases survive, and it counts as attached.
        assert_eq!(rt.container("vsc-shop").unwrap().networks["zevo"], ["app"]);
        assert!(report.projects[0].attached);
        assert_eq!(
            report.actions,
            [Action::WroteRoutes {
                slug: "shop".into()
            }]
        );
    }

    #[tokio::test]
    async fn compose_projects_match_only_their_service() {
        let (_tmp, store, rt) = set_up().await;
        let mut p = project("shop", "/work/shop");
        p.kind = DevcontainerKind::Compose;
        p.service = Some("app".into());
        store.add_project(&p).unwrap();
        for service in ["db", "app"] {
            let mut c = devcontainer(&format!("shop-{service}-1"), "/elsewhere");
            c.labels = [
                (
                    "com.docker.compose.service".to_string(),
                    service.to_string(),
                ),
                (
                    "com.docker.compose.project.working_dir".to_string(),
                    "/work/shop/.devcontainer".to_string(),
                ),
            ]
            .into();
            rt.add_container(c);
        }

        let report = reconcile(&store, Some(&rt)).await.unwrap();
        assert_eq!(report.projects[0].container.as_deref(), Some("shop-app-1"));
        assert!(
            !rt.container("shop-db-1")
                .unwrap()
                .networks
                .contains_key("zevo")
        );
    }

    #[tokio::test]
    async fn still_writes_routes_when_the_runtime_is_down() {
        let (_tmp, store, rt) = set_up().await;
        store.add_project(&project("shop", "/work/shop")).unwrap();
        rt.set_down("cannot connect to the Docker daemon");

        let report = reconcile(&store, Some(&rt)).await.unwrap();
        assert_eq!(
            report.actions,
            [Action::WroteRoutes {
                slug: "shop".into()
            }]
        );
        assert_eq!(report.projects[0].state, ContainerState::Unknown);
        assert!(report.warnings[0].contains("cannot connect"));
    }

    #[tokio::test]
    async fn reports_whether_each_route_is_reachable() {
        let (_tmp, store, rt) = set_up().await;
        let mut p = project("shop", "/work/shop");
        p.routes.push(Route {
            subdomain: Some("api".into()),
            port: 8000,
            ..p.routes[0].clone()
        });
        p.routes.push(Route {
            subdomain: Some("docs".into()),
            port: 4000,
            ..p.routes[0].clone()
        });
        p.routes.push(Route {
            subdomain: Some("far".into()),
            target: Target::HostPort {
                address: "10.0.0.9".into(),
            },
            ..p.routes[0].clone()
        });
        store.add_project(&p).unwrap();
        rt.add_container(devcontainer("vsc-shop", "/work/shop"));

        // Before the container can be inspected, nothing is claimed.
        let report = reconcile(&store, Some(&rt)).await.unwrap();
        assert_eq!(report.projects[0].backends, [Listening::Unknown; 4]);

        rt.set_exec_output(
            "vsc-shop",
            "   0: 00000000:0BB8 00000000:0000 0A\n   1: 0100007F:1F40 00000000:0000 0A\n",
        );
        let report = reconcile(&store, Some(&rt)).await.unwrap();
        assert_eq!(
            report.projects[0].backends,
            [
                Listening::Yes,
                Listening::LoopbackOnly,
                Listening::No,
                Listening::Unknown
            ]
        );
    }

    #[tokio::test]
    async fn discovers_devcontainers_that_are_not_registered() {
        let (_tmp, store, rt) = set_up().await;
        store.add_project(&project("shop", "/work/shop")).unwrap();
        rt.add_container(devcontainer("vsc-shop", "/work/shop"));
        rt.add_container(devcontainer("vsc-blog", "/work/blog/"));
        let mut old = devcontainer("vsc-docs-old", "/work/docs");
        old.running = false;
        rt.add_container(old);
        rt.add_container(devcontainer("vsc-docs-new", "/work/docs"));
        rt.add_container(ContainerInfo {
            name: "postgres".into(),
            running: true,
            ..ContainerInfo::default()
        });

        let report = reconcile(&store, Some(&rt)).await.unwrap();
        let found: Vec<(&str, &str)> = report
            .discovered
            .iter()
            .map(|d| (d.folder.to_str().unwrap(), d.container.as_str()))
            .collect();
        assert_eq!(
            found,
            [("/work/blog/", "vsc-blog"), ("/work/docs", "vsc-docs-new")]
        );
    }

    #[test]
    fn discovery_can_be_limited_to_one_folder() {
        let containers = [
            devcontainer("a", "/home/me/Projects/shop"),
            devcontainer("b", "/home/me/Projects/kindly/app/"),
            devcontainer("c", "/home/me/Projects-old/blog"),
            devcontainer("d", "/srv/other"),
        ];
        let folders = |within| -> Vec<String> {
            discover("box", &containers, &[], within)
                .iter()
                .map(|d| d.folder.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(folders(None).len(), 4);
        assert_eq!(
            folders(Some("/home/me/Projects/")),
            ["/home/me/Projects/kindly/app/", "/home/me/Projects/shop"]
        );
        assert_eq!(folders(Some("/")).len(), 4);
    }

    #[tokio::test]
    async fn remote_projects_go_through_the_hosts_edge_traefik() {
        let (_tmp, store, local) = set_up().await;
        let host = Host::from_ssh("dev@192.168.1.20", Some("buildbox"), None).unwrap();
        store.add_host(&host).unwrap();
        let mut p = project("billing", "/srv/billing");
        p.host = "buildbox".into();
        store.add_project(&p).unwrap();

        let far = FakeRuntime::new();
        far.add_container(devcontainer("vsc-billing", "/srv/billing"));
        far.add_container(devcontainer("vsc-other", "/srv/other"));
        let mut remotes = RemoteRuntimes::new();
        remotes.insert("buildbox".into(), Ok(&far as &dyn Runtime));
        let issuer = certs::FakeIssuer::default();

        let report = reconcile_hosts(&store, Some(&local), &remotes, &issuer)
            .await
            .unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(report.actions.contains(&Action::StartedEdge {
            host: "buildbox".into()
        }));
        assert!(report.actions.contains(&Action::WroteEdgeRoutes {
            slug: "billing".into()
        }));
        assert_eq!(report.projects[0].state, ContainerState::Running);
        assert!(report.projects[0].attached);

        // Remote: the container joined the network and the edge has its routes.
        assert!(
            far.container("vsc-billing")
                .unwrap()
                .networks
                .contains_key("zevo")
        );
        assert!(far.container(EDGE_CONTAINER).unwrap().running);
        let edge = far
            .read_file(EDGE_CONTAINER, "/etc/traefik/dynamic/zevo-billing.yml")
            .await
            .unwrap()
            .unwrap();
        assert!(edge.contains("- url: http://vsc-billing:3000"), "{edge}");
        // Local: this machine's Traefik forwards to the host's edge port.
        let front =
            std::fs::read_to_string(store.managed_dynamic_dir().join("zevo-billing.yml")).unwrap();
        assert!(front.contains("- url: http://192.168.1.20:8480"), "{front}");
        // The other devcontainer there is offered for adding.
        assert_eq!(report.discovered.len(), 1);
        assert_eq!(
            (
                report.discovered[0].host.as_str(),
                report.discovered[0].container.as_str()
            ),
            ("buildbox", "vsc-other")
        );

        // Settled: a second pass changes nothing.
        let again = reconcile_hosts(&store, Some(&local), &remotes, &issuer)
            .await
            .unwrap();
        assert!(again.actions.is_empty(), "{:?}", again.actions);

        // Removing the project cleans up both sides.
        register::remove(&store, "billing").unwrap();
        let report = reconcile_hosts(&store, Some(&local), &remotes, &issuer)
            .await
            .unwrap();
        assert!(report.actions.contains(&Action::RemovedOrphanRoutes {
            slug: "billing".into()
        }));
        assert!(
            !store
                .managed_dynamic_dir()
                .join("zevo-billing.yml")
                .exists()
        );
        assert_eq!(
            far.read_file(EDGE_CONTAINER, "/etc/traefik/dynamic/zevo-billing.yml")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn an_unreachable_host_is_one_warning_and_routes_still_point_at_it() {
        let (_tmp, store, local) = set_up().await;
        store
            .add_host(&Host::from_ssh("dev@192.168.1.20", Some("buildbox"), None).unwrap())
            .unwrap();
        for slug in ["billing", "ledger"] {
            let mut p = project(slug, &format!("/srv/{slug}"));
            p.host = "buildbox".into();
            store.add_project(&p).unwrap();
        }
        let mut remotes = RemoteRuntimes::new();
        remotes.insert("buildbox".into(), Err("connection timed out".into()));

        let report = reconcile_hosts(
            &store,
            Some(&local),
            &remotes,
            &certs::FakeIssuer::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].contains("buildbox") && report.warnings[0].contains("timed out")
        );
        assert!(
            report
                .projects
                .iter()
                .all(|p| p.state == ContainerState::Unknown)
        );
        assert!(
            store
                .managed_dynamic_dir()
                .join("zevo-billing.yml")
                .exists()
        );
    }

    #[tokio::test]
    async fn stopped_and_missing_containers_are_reported_not_attached() {
        let (_tmp, store, rt) = set_up().await;
        store.add_project(&project("shop", "/work/shop")).unwrap();
        store.add_project(&project("blog", "/work/blog")).unwrap();
        let mut c = devcontainer("vsc-shop", "/work/shop");
        c.running = false;
        rt.add_container(c);

        let report = reconcile(&store, Some(&rt)).await.unwrap();
        let state = |slug: &str| {
            report
                .projects
                .iter()
                .find(|p| p.slug == slug)
                .unwrap()
                .state
                .clone()
        };
        assert_eq!(state("shop"), ContainerState::Stopped);
        assert_eq!(state("blog"), ContainerState::Missing);
        assert!(
            !report
                .actions
                .iter()
                .any(|a| matches!(a, Action::AttachedProject { .. }))
        );
    }

    #[tokio::test]
    async fn https_projects_get_a_certificate_listed_in_their_route_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path());
        let rt = FakeRuntime::new();
        setup::bootstrap(&store, &rt, Some(8090), true)
            .await
            .unwrap();
        store.add_project(&project("shop", "/work/shop")).unwrap();
        let issuer = certs::FakeIssuer::default();

        let report = reconcile_with(&store, Some(&rt), &issuer).await.unwrap();
        assert!(report.actions.contains(&Action::IssuedCertificate {
            slug: "shop".into()
        }));
        assert_eq!(
            *issuer.issued.lock().unwrap(),
            [["*.shop.localhost", "shop.localhost"]]
        );
        let file =
            std::fs::read_to_string(store.managed_dynamic_dir().join("zevo-shop.yml")).unwrap();
        assert!(file.contains("tls: {}"), "{file}");
        assert!(
            file.contains("certFile: /etc/traefik/dynamic/zevo-certs/shop.cert.pem"),
            "{file}"
        );
        assert!(
            file.contains("keyFile: /etc/traefik/dynamic/zevo-certs/shop.key.pem"),
            "{file}"
        );

        // Nothing to do the second time.
        let again = reconcile_with(&store, Some(&rt), &issuer).await.unwrap();
        assert!(again.actions.is_empty(), "{:?}", again.actions);

        // Removing the project removes its certificate too.
        register::remove(&store, "shop").unwrap();
        assert_eq!(
            std::fs::read_dir(store.managed_dynamic_dir().join(certs::CERT_DIR))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn without_mkcert_routes_are_still_written_with_one_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path());
        let rt = FakeRuntime::new();
        setup::bootstrap(&store, &rt, Some(8091), true)
            .await
            .unwrap();
        store.add_project(&project("shop", "/work/shop")).unwrap();
        store.add_project(&project("blog", "/work/blog")).unwrap();
        let issuer = certs::FakeIssuer {
            unavailable: Some("mkcert is not installed".into()),
            ..Default::default()
        };

        let report = reconcile_with(&store, Some(&rt), &issuer).await.unwrap();
        assert_eq!(
            report
                .warnings
                .iter()
                .filter(|w| w.contains("mkcert is not installed"))
                .count(),
            1
        );
        let file =
            std::fs::read_to_string(store.managed_dynamic_dir().join("zevo-shop.yml")).unwrap();
        assert!(
            file.contains("tls: {}") && !file.contains("certFile"),
            "{file}"
        );
    }

    #[test]
    fn urls_use_https_when_the_entrypoint_does() {
        let settings = Settings {
            traefik: Some(crate::model::TraefikSettings {
                tls: true,
                container_dir: None,
                mode: crate::model::TraefikMode::Adopted,
                container: None,
                dynamic_dir: "/x".into(),
                entrypoint: "websecure".into(),
                http_port: 443,
            }),
            ..Settings::default()
        };
        assert_eq!(
            urls(&project("shop", "/work/shop"), &settings),
            ["https://shop.localhost"]
        );
    }

    #[test]
    fn urls_include_a_non_default_port_and_prefix() {
        let settings = Settings {
            traefik: Some(crate::model::TraefikSettings {
                tls: false,
                container_dir: None,
                mode: crate::model::TraefikMode::Managed,
                container: None,
                dynamic_dir: "/x".into(),
                entrypoint: "web".into(),
                http_port: 8080,
            }),
            ..Settings::default()
        };
        let mut p = project("shop", "/work/shop");
        p.routes[0].path_prefix = Some("/admin".into());
        assert_eq!(urls(&p, &settings), ["http://shop.localhost:8080/admin"]);
    }
}
