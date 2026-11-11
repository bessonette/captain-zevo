//! Brings the real world in line with the registry: route files in Traefik's
//! directory and project containers attached to the shared network.

use std::path::Path;

use serde::Serialize;

use crate::model::{LOCAL_HOST_ID, Project, Settings, Target};
use crate::runtime::{ContainerInfo, Runtime};
use crate::store::Store;
use crate::{Error, Result, register, setup, traefik};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    WroteRoutes { slug: String },
    RemovedOrphanRoutes { slug: String },
    CreatedNetwork { network: String },
    AttachedTraefik { container: String },
    AttachedProject { slug: String, container: String },
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub actions: Vec<Action>,
    pub projects: Vec<ProjectStatus>,
    pub warnings: Vec<String>,
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

    for project in &projects {
        let mut status = ProjectStatus {
            slug: project.slug.clone(),
            container: None,
            state: ContainerState::Unknown,
            attached: false,
            urls: urls(project, &settings),
        };
        if project.host != LOCAL_HOST_ID {
            if project.routes.iter().any(|r| r.target == Target::Container) {
                report.warnings.push(format!(
                    "{}: on host '{}', which is not managed yet; use host-port routes",
                    project.slug, project.host
                ));
            }
        } else if let (Some(runtime), Some(containers)) = (runtime, &containers) {
            match find_container(project, containers) {
                None => status.state = ContainerState::Missing,
                Some(c) => {
                    status.container = Some(c.name.clone());
                    status.state = if c.running {
                        ContainerState::Running
                    } else {
                        ContainerState::Stopped
                    };
                    status.attached = c.networks.contains_key(&settings.network);
                    if c.running && !status.attached && needs_network(project) {
                        // The alias is a convenience; routes use the container name.
                        let aliases = [project.slug.clone()];
                        match runtime.connect(&c.id, &settings.network, &aliases).await {
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
                }
            }
        }

        // Containers are reached by their own name, which changes on rebuild,
        // so the file follows the container that currently exists.
        match traefik::write_project_for(project, &settings, status.container.as_deref()) {
            Ok(true) => report.actions.push(Action::WroteRoutes {
                slug: project.slug.clone(),
            }),
            Ok(false) => {}
            Err(e) => report.warnings.push(format!("{}: {e}", project.slug)),
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
    Ok(report)
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
    match settings
        .traefik
        .as_ref()
        .and_then(|t| t.container.as_deref())
    {
        Some(container) => {
            match setup::attach_traefik(runtime, container, &settings.network).await {
                Ok(true) => report.actions.push(Action::AttachedTraefik {
                    container: container.to_string(),
                }),
                Ok(false) => {}
                Err(e) => report.warnings.push(e.to_string()),
            }
        }
        None => report.warnings.push(
            "Traefik does not run in a container, so it cannot reach projects by network alias; \
             use host-port routes"
                .to_string(),
        ),
    }
    runtime.containers().await
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
        setup::bootstrap(&store, &rt, Some(80)).await.unwrap();
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

    #[test]
    fn urls_use_https_when_the_entrypoint_does() {
        let settings = Settings {
            traefik: Some(crate::model::TraefikSettings {
                tls: true,
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
