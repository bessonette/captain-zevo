//! Everything the UI needs from the outside world, behind one trait so views
//! can run against canned data (`--demo`, tests) as well as the real system.
//!
//! Methods block; call them from a background task, never the UI thread.

use std::path::PathBuf;
use std::sync::Mutex;

use zevo_core::model::{
    DevcontainerKind, LOCAL_HOST_ID, Project, Route, Settings, Target, TraefikMode, TraefikSettings,
};
use zevo_core::reconcile::{self, ContainerState, ProjectStatus};
use zevo_core::runtime::{DockerRuntime, Runtime};
use zevo_core::store::Store;
use zevo_core::traefik::{self, RouterInfo};

/// One project with everything the UI shows about it.
#[derive(Debug, Clone)]
pub struct ProjectView {
    pub project: Project,
    pub status: ProjectStatus,
    /// Traefik config generated for the project; empty before setup.
    pub yaml: String,
}

/// A consistent picture of the system at one moment.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub settings: Settings,
    pub projects: Vec<ProjectView>,
    /// Every router in Traefik's route directory, Zevo's and others'.
    pub routers: Vec<RouterInfo>,
    /// Runtime name and version, or why it is unreachable.
    pub runtime: Result<String, String>,
    pub warnings: Vec<String>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            projects: Vec::new(),
            routers: Vec::new(),
            runtime: Err("not checked yet".to_string()),
            warnings: Vec::new(),
        }
    }
}

impl Snapshot {
    pub fn needs_setup(&self) -> bool {
        self.settings.traefik.is_none()
    }
}

pub trait Backend: Send + Sync + 'static {
    /// Reconciles, then reports the resulting state.
    fn load(&self) -> Result<Snapshot, String>;
    fn remove_project(&self, slug: &str) -> Result<(), String>;
}

/// The real system: the on-disk registry and the local container runtime.
pub struct SystemBackend {
    store: Store,
    tokio: tokio::runtime::Runtime,
}

impl SystemBackend {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            store: Store::open_default().map_err(|e| e.to_string())?,
            tokio: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?,
        })
    }
}

impl Backend for SystemBackend {
    fn load(&self) -> Result<Snapshot, String> {
        self.tokio.block_on(async {
            let settings = self.store.settings().map_err(|e| e.to_string())?;
            let projects = self.store.projects().map_err(|e| e.to_string())?;

            let docker = DockerRuntime::connect().map_err(|e| e.to_string());
            let (docker, runtime) = match docker {
                Ok(docker) => match docker.ping().await {
                    Ok(version) => (Some(docker), Ok(version)),
                    Err(e) => (None, Err(e.to_string())),
                },
                Err(e) => (None, Err(e)),
            };

            let mut snapshot = Snapshot {
                settings,
                runtime,
                ..Snapshot::default()
            };
            let unknown = |p: &Project| ProjectStatus {
                slug: p.slug.clone(),
                container: None,
                state: ContainerState::Unknown,
                attached: false,
                urls: reconcile::urls(p, &snapshot.settings),
            };

            let statuses = match &snapshot.settings.traefik {
                None => projects.iter().map(unknown).collect(),
                Some(traefik_settings) => {
                    let report = reconcile::reconcile(
                        &self.store,
                        docker.as_ref().map(|d| d as &dyn Runtime),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    snapshot.warnings = report.warnings;
                    snapshot.routers =
                        traefik::scan(&traefik_settings.dynamic_dir).map_err(|e| e.to_string())?;
                    report.projects
                }
            };

            snapshot.projects = projects
                .into_iter()
                .map(|project| {
                    let status = statuses
                        .iter()
                        .find(|s| s.slug == project.slug)
                        .cloned()
                        .unwrap_or_else(|| unknown(&project));
                    let yaml = traefik::render(&project, &snapshot.settings).unwrap_or_default();
                    ProjectView {
                        project,
                        status,
                        yaml,
                    }
                })
                .collect();
            Ok(snapshot)
        })
    }

    fn remove_project(&self, slug: &str) -> Result<(), String> {
        let settings = self.store.settings().map_err(|e| e.to_string())?;
        self.store.remove_project(slug).map_err(|e| e.to_string())?;
        if let Some(t) = &settings.traefik {
            traefik::remove_project(slug, &t.dynamic_dir).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// Example data for `--demo`, screenshots and tests. Touches nothing real.
pub struct DemoBackend {
    projects: Mutex<Vec<(Project, ContainerState, Option<&'static str>)>>,
}

impl Default for DemoBackend {
    fn default() -> Self {
        let route = |subdomain: Option<&str>,
                     port: u16,
                     prefix: Option<&str>,
                     address: Option<&str>| Route {
            subdomain: subdomain.map(Into::into),
            port,
            path_prefix: prefix.map(Into::into),
            target: match address {
                Some(address) => Target::HostPort {
                    address: address.into(),
                },
                None => Target::Container,
            },
        };
        let project = |slug: &str,
                       name: &str,
                       host: &str,
                       path: &str,
                       kind,
                       routes: Vec<Route>,
                       tags: &[&str]| Project {
            slug: slug.into(),
            name: name.into(),
            host: host.into(),
            path: PathBuf::from(path),
            kind,
            service: (kind == DevcontainerKind::Compose).then(|| "app".to_string()),
            routes,
            tags: tags.iter().map(|t| t.to_string()).collect(),
        };
        use ContainerState::{Missing, Running, Stopped};
        use DevcontainerKind::{Compose, Dockerfile, Image};
        Self {
            projects: Mutex::new(vec![
                (
                    project(
                        "shop",
                        "Shop storefront",
                        LOCAL_HOST_ID,
                        "/home/dev/code/shop",
                        Compose,
                        vec![
                            route(None, 3000, None, None),
                            route(Some("api"), 8080, Some("/v1"), None),
                        ],
                        &["web"],
                    ),
                    Running,
                    Some("shop-app-1"),
                ),
                (
                    project(
                        "captain-zevo",
                        "Captain Zevo",
                        LOCAL_HOST_ID,
                        "/home/dev/code/captain-zevo",
                        Image,
                        vec![],
                        &["rust"],
                    ),
                    Running,
                    Some("vsc-captain-zevo-4e1"),
                ),
                (
                    project(
                        "docs",
                        "Docs site",
                        LOCAL_HOST_ID,
                        "/home/dev/code/docs",
                        Dockerfile,
                        vec![route(None, 4321, None, None)],
                        &["web"],
                    ),
                    Running,
                    Some("vsc-docs-91c"),
                ),
                (
                    project(
                        "billing",
                        "Billing service",
                        "buildbox",
                        "/srv/dev/billing",
                        Compose,
                        vec![route(None, 9000, None, Some("buildbox.tail"))],
                        &["api"],
                    ),
                    ContainerState::Unknown,
                    None,
                ),
                (
                    project(
                        "ml-lab",
                        "ML lab",
                        LOCAL_HOST_ID,
                        "/home/dev/code/ml-lab",
                        Image,
                        vec![
                            route(None, 8888, None, None),
                            route(Some("board"), 6006, None, None),
                        ],
                        &["python"],
                    ),
                    Stopped,
                    Some("vsc-ml-lab-b07"),
                ),
                (
                    project(
                        "legacy-crm",
                        "Legacy CRM",
                        LOCAL_HOST_ID,
                        "/home/dev/code/legacy-crm",
                        Compose,
                        vec![route(None, 8000, None, None)],
                        &[],
                    ),
                    Missing,
                    None,
                ),
            ]),
        }
    }
}

impl Backend for DemoBackend {
    fn load(&self) -> Result<Snapshot, String> {
        let settings = Settings {
            traefik: Some(TraefikSettings {
                mode: TraefikMode::Adopted,
                container: Some("traefik".into()),
                dynamic_dir: "/home/dev/traefik/dynamic".into(),
                entrypoint: "web".into(),
                http_port: 80,
            }),
            ..Settings::default()
        };
        let projects = self.projects.lock().unwrap();
        let mut routers = Vec::new();
        let views = projects
            .iter()
            .map(|(project, state, container)| {
                for r in &project.routes {
                    routers.push(RouterInfo {
                        file: settings
                            .traefik
                            .as_ref()
                            .unwrap()
                            .dynamic_dir
                            .join(traefik::file_name(&project.slug)),
                        router: format!("zevo-{}", project.slug),
                        hostnames: vec![r.hostname(&project.slug, &settings.domain_suffix)],
                        managed: true,
                        slug: Some(project.slug.clone()),
                    });
                }
                ProjectView {
                    status: ProjectStatus {
                        slug: project.slug.clone(),
                        container: container.map(Into::into),
                        state: state.clone(),
                        // "docs" shows the running-but-unattached case.
                        attached: *state == ContainerState::Running && project.slug != "docs",
                        urls: reconcile::urls(project, &settings),
                    },
                    yaml: traefik::render(project, &settings).unwrap_or_default(),
                    project: project.clone(),
                }
            })
            .collect();
        for (file, router, host) in [
            ("monitoring.yml", "grafana", "grafana.localhost"),
            ("dashboard.yml", "dashboard", "traefik.localhost"),
        ] {
            routers.push(RouterInfo {
                file: PathBuf::from("/home/dev/traefik/dynamic").join(file),
                router: router.into(),
                hostnames: vec![host.into()],
                managed: false,
                slug: None,
            });
        }
        Ok(Snapshot {
            settings,
            projects: views,
            routers,
            runtime: Ok("Docker Engine 29.7 (example)".into()),
            warnings: vec![
                "billing: on host 'buildbox', which is not managed yet; use host-port routes"
                    .into(),
            ],
        })
    }

    fn remove_project(&self, slug: &str) -> Result<(), String> {
        self.projects
            .lock()
            .unwrap()
            .retain(|(p, _, _)| p.slug != slug);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_snapshot_is_consistent() {
        let demo = DemoBackend::default();
        let snapshot = demo.load().unwrap();
        assert!(!snapshot.needs_setup());
        assert_eq!(snapshot.projects.len(), 6);
        let shop = &snapshot.projects[0];
        assert_eq!(
            shop.status.urls,
            ["http://shop.localhost", "http://api.shop.localhost/v1"]
        );
        assert!(
            shop.yaml
                .contains("Host(`api.shop.localhost`) && PathPrefix(`/v1`)")
        );

        demo.remove_project("shop").unwrap();
        assert_eq!(demo.load().unwrap().projects.len(), 5);
    }
}
