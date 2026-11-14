//! Everything the UI needs from the outside world, behind one trait so views
//! can run against canned data (`--demo`, tests) as well as the real system.
//!
//! Methods block; call them from a background task, never the UI thread.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zevo_core::certs;
use zevo_core::health::Listening;
use zevo_core::ide::{self, Editor};
use zevo_core::lifecycle;
use zevo_core::model::{
    DevcontainerKind, Host, HostDetails, LOCAL_HOST_ID, Project, Route, Settings, Target,
    TraefikMode, TraefikSettings,
};
use zevo_core::plugins::Plugins;
use zevo_core::reconcile::{self, ContainerState, Discovered, ProjectStatus};
use zevo_core::register;
use zevo_core::remote;
use zevo_core::runtime::{DockerRuntime, Runtime};
use zevo_core::setup::{self, Candidate};
use zevo_core::store::Store;
use zevo_core::templates;
use zevo_core::traefik::{self, RouterInfo};

/// One project with everything the UI shows about it.
#[derive(Debug, Clone)]
pub struct ProjectView {
    pub project: Project,
    pub status: ProjectStatus,
    /// Traefik config generated for the project; empty before setup.
    pub yaml: String,
    /// Why the container cannot be built or rebuilt from here, if it cannot.
    pub build_blocker: Option<String>,
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
    /// Devcontainers on this machine that are not registered yet.
    pub discovered: Vec<Discovered>,
    /// Editors installed on this machine, in manifest order.
    pub editors: Vec<Editor>,
    /// Every editor Zevo knows of, installed or not, to say what it looked
    /// for when none is installed.
    pub known_editors: Vec<Editor>,
    /// Starter templates and extras "Start a new project" offers, built-ins
    /// and plugins alike.
    pub templates: Vec<templates::Template>,
    pub extras: Vec<templates::Extra>,
    /// Remote hosts and whether Docker answers on each.
    pub hosts: Vec<HostView>,
}

/// A remote host with its connection status.
#[derive(Debug, Clone)]
pub struct HostView {
    pub host: Host,
    /// Runtime name and version there, or why it cannot be reached.
    pub status: Result<String, String>,
}

impl Snapshot {
    /// The editor "Open in editor" uses: the chosen one if it is installed,
    /// else the first installed one.
    pub fn editor(&self) -> Option<&Editor> {
        ide::preferred(self.settings.editor.as_deref(), &self.editors)
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            projects: Vec::new(),
            routers: Vec::new(),
            runtime: Err("not checked yet".to_string()),
            warnings: Vec::new(),
            discovered: Vec::new(),
            editors: Vec::new(),
            known_editors: Vec::new(),
            templates: Vec::new(),
            extras: Vec::new(),
            hosts: Vec::new(),
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
    /// Inspects a folder on the given host (`local` for this machine) and
    /// proposes a project for it, without saving.
    fn draft(&self, host: &str, folder: &str) -> Result<Project, String>;
    /// Creates a new project folder from a starter template inside `parent`,
    /// with the chosen extras (ids from `Snapshot::extras`), and returns the
    /// folder's path.
    fn create_project(
        &self,
        parent: &str,
        name: &str,
        template: &str,
        extras: &[String],
    ) -> Result<String, String>;
    /// Registers a remote host after checking that Docker answers on it.
    /// `docker_socket` may be empty, in which case Zevo looks for it.
    fn add_host(&self, details: &HostDetails) -> Result<(), String>;
    /// Saves changes to a host, after the same check.
    fn update_host(&self, id: &str, details: &HostDetails) -> Result<(), String>;
    /// Unregisters a host and removes its edge Traefik. `Ok(Some(note))`
    /// when the host was removed but something was left behind there.
    fn remove_host(&self, id: &str) -> Result<Option<String>, String>;
    fn add_project(&self, project: &Project) -> Result<(), String>;
    /// Saves changes to an existing project.
    fn save_project(&self, project: &Project) -> Result<(), String>;
    /// Traefik containers found on this machine, for first-run setup.
    fn traefik_candidates(&self) -> Result<Vec<Candidate>, String>;
    fn adopt(&self, container: &str) -> Result<(), String>;
    /// Starts a Zevo-managed Traefik.
    fn bootstrap(&self) -> Result<(), String>;
    /// Opens the project in the editor, attached to its devcontainer.
    fn open_in_editor(&self, slug: &str, editor: &Editor) -> Result<(), String>;
    /// Remembers which editor "Open in editor" uses, by id.
    fn set_editor(&self, id: &str) -> Result<(), String>;
    /// Starts the project's container, building it if it does not exist.
    /// Building can take minutes.
    fn start(&self, slug: &str) -> Result<(), String>;
    fn stop(&self, slug: &str) -> Result<(), String>;
    /// Rebuilds the container from its devcontainer.json, replacing it.
    fn rebuild(&self, slug: &str) -> Result<(), String>;
    /// The end of the container's output.
    fn logs(&self, slug: &str, lines: usize) -> Result<String, String>;
}

/// The real system: the on-disk registry and the local container runtime.
pub struct SystemBackend {
    store: Store,
    tokio: tokio::runtime::Runtime,
    /// SSH connections to remote hosts, kept open between loads.
    links: Mutex<BTreeMap<String, Link>>,
    /// The connection details each link was opened with, by host id.
    opened_with: Mutex<BTreeMap<String, String>>,
}

/// The state of the connection to one remote host.
enum Link {
    Open(remote::Tunnel, DockerRuntime),
    /// Failed at this time, for this reason. Retried after a pause, so an
    /// unreachable machine does not stall every refresh on an SSH timeout.
    Failed(Instant, String),
}

const RETRY_AFTER: Duration = Duration::from_secs(30);

impl SystemBackend {
    /// Brings the connections in line with the registered hosts: opens new
    /// ones, reopens dead ones, and drops those for removed hosts.
    /// The runtime a project's container lives on. Remote projects reuse the
    /// open connection to their host; if there is none right now (a load has
    /// it, or it failed), a short-lived one is opened and returned so it
    /// lives as long as the runtime is used.
    fn project_runtime(
        &self,
        project: &Project,
    ) -> Result<(Option<remote::Tunnel>, DockerRuntime), String> {
        if project.host == LOCAL_HOST_ID {
            return Ok((None, DockerRuntime::connect().map_err(|e| e.to_string())?));
        }
        if let Some(Link::Open(_, runtime)) = self.links.lock().unwrap().get(&project.host) {
            return Ok((None, runtime.clone()));
        }
        let host = self.store.host(&project.host).map_err(|e| e.to_string())?;
        let tunnel = remote::Tunnel::open(&host).map_err(|e| e.to_string())?;
        let runtime = tunnel.runtime().map_err(|e| e.to_string())?;
        Ok((Some(tunnel), runtime))
    }

    fn refresh_links(
        links: &mut BTreeMap<String, Link>,
        opened_with: &mut BTreeMap<String, String>,
        hosts: &[Host],
    ) {
        // What a connection depends on. A host whose details were changed
        // gets a new one, or Zevo would keep talking to the old machine.
        let key = |host: &Host| {
            format!(
                "{}\n{:?}\n{:?}",
                host.ssh, host.docker_socket, host.docker_command
            )
        };
        links.retain(|id, _| {
            hosts
                .iter()
                .any(|h| &h.id == id && opened_with.get(id) == Some(&key(h)))
        });
        opened_with.retain(|id, _| links.contains_key(id));
        let to_open: Vec<Host> = hosts
            .iter()
            .filter(|host| match links.get_mut(&host.id) {
                Some(Link::Open(tunnel, _)) => !tunnel.alive(),
                Some(Link::Failed(at, _)) => at.elapsed() >= RETRY_AFTER,
                None => true,
            })
            .cloned()
            .collect();
        // Together, so one unreachable machine does not hold up the rest.
        for host in &to_open {
            opened_with.insert(host.id.clone(), key(host));
        }
        for (id, opened) in remote::open_all(&to_open) {
            links.insert(
                id,
                match opened {
                    Ok((tunnel, runtime)) => Link::Open(tunnel, runtime),
                    Err(e) => Link::Failed(Instant::now(), e),
                },
            );
        }
    }
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
            links: Mutex::new(BTreeMap::new()),
            opened_with: Mutex::new(BTreeMap::new()),
        })
    }
}

impl Backend for SystemBackend {
    fn load(&self) -> Result<Snapshot, String> {
        self.tokio.block_on(async {
            let settings = self.store.settings().map_err(|e| e.to_string())?;
            let mut projects = self.store.projects().map_err(|e| e.to_string())?;

            let docker = DockerRuntime::connect().map_err(|e| e.to_string());
            let (docker, runtime) = match docker {
                Ok(docker) => match docker.ping().await {
                    Ok(version) => (Some(docker), Ok(version)),
                    Err(e) => (None, Err(e.to_string())),
                },
                Err(e) => (None, Err(e)),
            };

            // Loads never overlap (the UI runs one at a time), so the
            // connections can be taken out for the length of this one.
            let hosts = self.store.hosts().map_err(|e| e.to_string())?;
            let mut links = std::mem::take(&mut *self.links.lock().unwrap());
            Self::refresh_links(&mut links, &mut self.opened_with.lock().unwrap(), &hosts);
            let mut remotes = reconcile::RemoteRuntimes::new();
            let mut host_views = Vec::new();
            for host in &hosts {
                let status = match links.get(&host.id) {
                    Some(Link::Open(_, rt)) => {
                        remotes.insert(host.id.clone(), Ok(rt as &dyn Runtime));
                        rt.ping().await.map_err(|e| e.to_string())
                    }
                    Some(Link::Failed(_, reason)) => {
                        remotes.insert(host.id.clone(), Err(reason.clone()));
                        Err(reason.clone())
                    }
                    None => Err("not connected".to_string()),
                };
                host_views.push(HostView {
                    host: host.clone(),
                    status,
                });
            }

            let plugins = Plugins::load(self.store.dir());
            let known_editors = plugins.editors;
            let mut snapshot = Snapshot {
                settings,
                runtime,
                editors: ide::available(&known_editors),
                known_editors,
                templates: plugins.templates,
                extras: plugins.extras,
                hosts: host_views,
                ..Snapshot::default()
            };
            let unknown = |p: &Project| ProjectStatus {
                slug: p.slug.clone(),
                container: None,
                state: ContainerState::Unknown,
                attached: false,
                urls: reconcile::urls(p, &snapshot.settings),
                backends: vec![Listening::Unknown; p.routes.len()],
            };

            let statuses = match &snapshot.settings.traefik {
                None => projects.iter().map(unknown).collect(),
                Some(traefik_settings) => {
                    let report = reconcile::reconcile_hosts(
                        &self.store,
                        docker.as_ref().map(|d| d as &dyn Runtime),
                        &remotes,
                        certs::issuer(&Plugins::load(self.store.dir()), self.store.dir()).as_ref(),
                    )
                    .await
                    .map_err(|e| e.to_string());
                    let report = match report {
                        Ok(report) => report,
                        Err(e) => {
                            drop(remotes);
                            *self.links.lock().unwrap() = links;
                            return Err(e);
                        }
                    };
                    // Reconciling may have re-read declared routes from disk.
                    projects = self.store.projects().map_err(|e| e.to_string())?;
                    snapshot.warnings = report.warnings;
                    snapshot.discovered = report.discovered;
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
                    let yaml = match hosts.iter().find(|h| h.id == project.host) {
                        Some(host) => traefik::render_via_edge(
                            &project,
                            &snapshot.settings,
                            &remote::routable_address(&host.address),
                            host.edge_port,
                        ),
                        None => traefik::render_for(
                            &project,
                            &snapshot.settings,
                            status.container.as_deref(),
                        ),
                    }
                    .unwrap_or_default();
                    ProjectView {
                        build_blocker: lifecycle::build_blocker(&project),
                        project,
                        status,
                        yaml,
                    }
                })
                .collect();
            drop(remotes);
            *self.links.lock().unwrap() = links;
            Ok(snapshot)
        })
    }

    fn remove_project(&self, slug: &str) -> Result<(), String> {
        register::remove(&self.store, slug).map_err(|e| e.to_string())
    }

    fn draft(&self, host: &str, folder: &str) -> Result<Project, String> {
        if host == LOCAL_HOST_ID {
            return register::draft(Path::new(folder.trim())).map_err(|e| e.to_string());
        }
        let host = self.store.host(host).map_err(|e| e.to_string())?;
        register::draft_remote(&host, folder).map_err(|e| e.to_string())
    }

    fn create_project(
        &self,
        parent: &str,
        name: &str,
        template: &str,
        extras: &[String],
    ) -> Result<String, String> {
        let parent = register::expand_home(Path::new(parent.trim()));
        let plugins = Plugins::load(self.store.dir());
        let options = templates::Options::from_ids(&plugins, extras).map_err(|e| e.to_string())?;
        templates::create(&plugins, &parent, name, template, &options)
            .map(|folder| folder.to_string_lossy().into_owned())
            .map_err(|e| e.to_string())
    }

    fn add_host(&self, details: &HostDetails) -> Result<(), String> {
        self.tokio
            .block_on(remote::register_host(&self.store, details))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn update_host(&self, id: &str, details: &HostDetails) -> Result<(), String> {
        self.tokio
            .block_on(remote::update_host(&self.store, id, details))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn remove_host(&self, id: &str) -> Result<Option<String>, String> {
        self.tokio
            .block_on(remote::forget_host(&self.store, id))
            .map_err(|e| e.to_string())
    }

    fn add_project(&self, project: &Project) -> Result<(), String> {
        register::add(&self.store, project).map_err(|e| e.to_string())
    }

    fn save_project(&self, project: &Project) -> Result<(), String> {
        register::update(&self.store, project).map_err(|e| e.to_string())
    }

    fn traefik_candidates(&self) -> Result<Vec<Candidate>, String> {
        self.tokio.block_on(async {
            let docker = DockerRuntime::connect().map_err(|e| e.to_string())?;
            setup::find_traefik(&docker)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn adopt(&self, container: &str) -> Result<(), String> {
        self.tokio.block_on(async {
            let docker = DockerRuntime::connect().map_err(|e| e.to_string())?;
            let found = setup::find_traefik(&docker)
                .await
                .map_err(|e| e.to_string())?;
            let candidate = found
                .iter()
                .find(|c| c.container == container)
                .ok_or_else(|| format!("Traefik container '{container}' is no longer there"))?;
            setup::adopt(&self.store, &docker, candidate)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    fn bootstrap(&self) -> Result<(), String> {
        self.tokio.block_on(async {
            let docker = DockerRuntime::connect().map_err(|e| e.to_string())?;
            setup::bootstrap(
                &self.store,
                &docker,
                None,
                certs::issuer(&Plugins::load(self.store.dir()), self.store.dir())
                    .ready()
                    .is_ok(),
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
    }

    fn open_in_editor(&self, slug: &str, editor: &Editor) -> Result<(), String> {
        let project = self.store.project(slug).map_err(|e| e.to_string())?;
        let host = match project.host.as_str() {
            LOCAL_HOST_ID => None,
            id => Some(self.store.host(id).map_err(|e| e.to_string())?),
        };
        ide::open(editor, &project, host.as_ref()).map_err(|e| e.to_string())
    }

    fn set_editor(&self, id: &str) -> Result<(), String> {
        let mut settings = self.store.settings().map_err(|e| e.to_string())?;
        settings.editor = Some(id.to_string());
        self.store
            .save_settings(&settings)
            .map_err(|e| e.to_string())
    }

    fn start(&self, slug: &str) -> Result<(), String> {
        let project = self.store.project(slug).map_err(|e| e.to_string())?;
        let (_tunnel, runtime) = self.project_runtime(&project)?;
        let started = self
            .tokio
            .block_on(lifecycle::start_existing(&runtime, &project))
            .map_err(|e| e.to_string())?;
        if started {
            return Ok(());
        }
        lifecycle::up(&project, false).map_err(|e| e.to_string())
    }

    fn stop(&self, slug: &str) -> Result<(), String> {
        let project = self.store.project(slug).map_err(|e| e.to_string())?;
        let (_tunnel, runtime) = self.project_runtime(&project)?;
        self.tokio
            .block_on(lifecycle::stop(&runtime, &project))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn rebuild(&self, slug: &str) -> Result<(), String> {
        let project = self.store.project(slug).map_err(|e| e.to_string())?;
        lifecycle::up(&project, true).map_err(|e| e.to_string())
    }

    fn logs(&self, slug: &str, lines: usize) -> Result<String, String> {
        let project = self.store.project(slug).map_err(|e| e.to_string())?;
        let (_tunnel, runtime) = self.project_runtime(&project)?;
        self.tokio
            .block_on(lifecycle::logs(&runtime, &project, lines))
            .map_err(|e| e.to_string())
    }
}

/// Example data for `--demo`, screenshots and tests. Touches nothing real.
pub struct DemoBackend {
    projects: Mutex<Vec<(Project, ContainerState, Option<&'static str>)>>,
    set_up: Mutex<bool>,
    editor: Mutex<Option<String>>,
    hosts: Mutex<Vec<Host>>,
}

impl DemoBackend {
    /// Starts as a machine where `zevo setup` has not run yet.
    pub fn fresh() -> Self {
        let demo = Self::default();
        demo.projects.lock().unwrap().clear();
        *demo.set_up.lock().unwrap() = false;
        demo
    }

    fn set_state(&self, slug: &str, state: ContainerState) -> Result<(), String> {
        let mut projects = self.projects.lock().unwrap();
        let entry = projects
            .iter_mut()
            .find(|(p, _, _)| p.slug == slug)
            .ok_or_else(|| format!("project '{slug}' is not registered"))?;
        entry.1 = state;
        entry.2.get_or_insert("example-container-1");
        Ok(())
    }

    fn check(&self, project: &Project) -> Result<(), String> {
        zevo_core::model::validate_slug(&project.slug).map_err(|e| e.to_string())?;
        if project.name.trim().is_empty() {
            return Err("the project needs a name".into());
        }
        Ok(())
    }
}

impl Default for DemoBackend {
    fn default() -> Self {
        let route = |subdomain: Option<&str>,
                     port: u16,
                     prefix: Option<&str>,
                     address: Option<&str>| Route {
            backend: zevo_core::model::Backend::default(),
            host: None,
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
            declared: false,
            address: None,
            env_file: None,
            skip: Vec::new(),
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
            set_up: Mutex::new(true),
            editor: Mutex::new(None),
            hosts: Mutex::new(vec![
                Host::from_ssh("dev@192.168.1.20", Some("buildbox"), None).unwrap(),
            ]),
        }
    }
}

impl Backend for DemoBackend {
    fn load(&self) -> Result<Snapshot, String> {
        let set_up = *self.set_up.lock().unwrap();
        let settings = Settings {
            editor: self.editor.lock().unwrap().clone(),
            traefik: set_up.then(|| TraefikSettings {
                tls: false,
                container_dir: None,
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
                        file: PathBuf::from("/home/dev/traefik/dynamic")
                            .join(traefik::file_name(&project.slug)),
                        router: format!("zevo-{}", project.slug),
                        hostnames: vec![r.hostname(&project.slug, &settings.domain_suffix)],
                        managed: true,
                        slug: Some(project.slug.clone()),
                    });
                }
                ProjectView {
                    build_blocker: (project.host != LOCAL_HOST_ID).then(|| {
                        "building containers on remote hosts is not supported yet".to_string()
                    }),
                    status: ProjectStatus {
                        slug: project.slug.clone(),
                        container: container.map(Into::into),
                        state: state.clone(),
                        // "docs" shows the running-but-unattached case.
                        attached: *state == ContainerState::Running && project.slug != "docs",
                        urls: reconcile::urls(project, &settings),
                        // Example states: one server bound to loopback only.
                        backends: project
                            .routes
                            .iter()
                            .map(|r| match (state, r.port) {
                                (ContainerState::Running, 8080) => Listening::LoopbackOnly,
                                (ContainerState::Running, 4321) => Listening::No,
                                (ContainerState::Running, _) => Listening::Yes,
                                _ => Listening::Unknown,
                            })
                            .collect(),
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
            editors: Plugins::builtin()
                .editors
                .into_iter()
                .filter(|e| ["vscode", "cursor", "zed"].contains(&e.id.as_str()))
                .collect(),
            known_editors: Plugins::builtin().editors,
            templates: Plugins::builtin().templates,
            extras: Plugins::builtin().extras,
            discovered: if set_up {
                vec![
                    Discovered {
                        host: LOCAL_HOST_ID.into(),
                        folder: "/home/dev/code/inventory".into(),
                        container: "vsc-inventory-a41".into(),
                        running: true,
                    },
                    Discovered {
                        host: LOCAL_HOST_ID.into(),
                        folder: "/home/dev/code/old-prototype".into(),
                        container: "vsc-old-prototype-9c2".into(),
                        running: false,
                    },
                    Discovered {
                        host: "buildbox".into(),
                        folder: "/srv/dev/reports".into(),
                        container: "vsc-reports-7d0".into(),
                        running: true,
                    },
                ]
                .into_iter()
                .filter(|d| !projects.iter().any(|(p, _, _)| p.path == d.folder))
                .collect()
            } else {
                Vec::new()
            },
            warnings: Vec::new(),
            hosts: self
                .hosts
                .lock()
                .unwrap()
                .iter()
                .map(|host| HostView {
                    host: host.clone(),
                    status: Ok("Docker Engine 28.3 (example)".into()),
                })
                .collect(),
        })
    }

    fn remove_project(&self, slug: &str) -> Result<(), String> {
        self.projects
            .lock()
            .unwrap()
            .retain(|(p, _, _)| p.slug != slug);
        Ok(())
    }

    fn create_project(
        &self,
        parent: &str,
        name: &str,
        _template: &str,
        _extras: &[String],
    ) -> Result<String, String> {
        let slug = zevo_core::model::slugify(name);
        if slug.is_empty() {
            return Err("give the project a name with letters or digits in it".into());
        }
        Ok(format!("{}/{slug}", parent.trim().trim_end_matches('/')))
    }

    fn add_host(&self, details: &HostDetails) -> Result<(), String> {
        let host = demo_host(details)?;
        let mut hosts = self.hosts.lock().unwrap();
        if hosts.iter().any(|h| h.id == host.id) {
            return Err(format!("host '{}' is already registered", host.id));
        }
        hosts.push(host);
        Ok(())
    }

    fn update_host(&self, id: &str, details: &HostDetails) -> Result<(), String> {
        let mut host = demo_host(details)?;
        let mut hosts = self.hosts.lock().unwrap();
        let existing = hosts
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or_else(|| format!("host '{id}' is not registered"))?;
        host.id = existing.id.clone();
        *existing = host;
        Ok(())
    }

    fn remove_host(&self, id: &str) -> Result<Option<String>, String> {
        if self
            .projects
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _, _)| p.host == id)
        {
            return Err(format!(
                "host '{id}' still has projects; remove those first"
            ));
        }
        self.hosts.lock().unwrap().retain(|h| h.id != id);
        Ok(None)
    }

    fn draft(&self, host: &str, folder: &str) -> Result<Project, String> {
        let folder = folder.trim();
        if folder.is_empty() {
            return Err("enter a folder".into());
        }
        let name = folder
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(folder);
        Ok(Project {
            declared: false,
            address: None,
            env_file: None,
            skip: Vec::new(),
            slug: zevo_core::model::slugify(name),
            name: name.to_string(),
            host: host.to_string(),
            path: PathBuf::from(folder),
            kind: DevcontainerKind::Compose,
            service: Some("app".into()),
            routes: vec![Route {
                backend: zevo_core::model::Backend::default(),
                host: None,
                subdomain: None,
                port: 5173,
                path_prefix: None,
                target: Target::Container,
            }],
            tags: Vec::new(),
        })
    }

    fn add_project(&self, project: &Project) -> Result<(), String> {
        self.check(project)?;
        let mut projects = self.projects.lock().unwrap();
        if projects.iter().any(|(p, _, _)| p.slug == project.slug) {
            return Err(format!("project '{}' is already registered", project.slug));
        }
        projects.push((
            project.clone(),
            ContainerState::Running,
            Some("example-container-1"),
        ));
        Ok(())
    }

    fn save_project(&self, project: &Project) -> Result<(), String> {
        self.check(project)?;
        let mut projects = self.projects.lock().unwrap();
        match projects.iter_mut().find(|(p, _, _)| p.slug == project.slug) {
            Some(entry) => {
                entry.0 = project.clone();
                Ok(())
            }
            None => Err(format!("project '{}' is not registered", project.slug)),
        }
    }

    fn traefik_candidates(&self) -> Result<Vec<Candidate>, String> {
        Ok(vec![Candidate {
            container: "traefik".into(),
            running: true,
            dynamic_dir: Some("/home/dev/traefik/dynamic".into()),
            entrypoint: Some("websecure".into()),
            http_port: Some(443),
            tls: true,
            container_dir: None,
            network: Some("proxy".into()),
            problems: Vec::new(),
        }])
    }

    fn adopt(&self, _container: &str) -> Result<(), String> {
        *self.set_up.lock().unwrap() = true;
        Ok(())
    }

    fn bootstrap(&self) -> Result<(), String> {
        *self.set_up.lock().unwrap() = true;
        Ok(())
    }

    fn open_in_editor(&self, _slug: &str, _editor: &Editor) -> Result<(), String> {
        Ok(())
    }

    fn set_editor(&self, id: &str) -> Result<(), String> {
        *self.editor.lock().unwrap() = Some(id.to_string());
        Ok(())
    }

    fn start(&self, slug: &str) -> Result<(), String> {
        self.set_state(slug, ContainerState::Running)
    }

    fn stop(&self, slug: &str) -> Result<(), String> {
        self.set_state(slug, ContainerState::Stopped)
    }

    fn rebuild(&self, slug: &str) -> Result<(), String> {
        // Long enough to see the busy state.
        std::thread::sleep(Duration::from_millis(1500));
        self.set_state(slug, ContainerState::Running)
    }

    fn logs(&self, slug: &str, _lines: usize) -> Result<String, String> {
        let projects = self.projects.lock().unwrap();
        let (project, state, _) = projects
            .iter()
            .find(|(p, _, _)| p.slug == slug)
            .ok_or_else(|| format!("project '{slug}' is not registered"))?;
        if *state == ContainerState::Missing {
            return Err("this project has no container yet".into());
        }
        let port = project.routes.first().map(|r| r.port).unwrap_or(3000);
        Ok(format!(
            "> dev\n> vite --host 0.0.0.0 --port {port}\n\n  VITE ready in 412 ms\n\n  \u{279c}  Local:   http://localhost:{port}/\n  \u{279c}  Network: http://172.18.0.4:{port}/\n12:04:19 GET / 200 14ms\n12:04:19 GET /assets/app.css 200 3ms\n12:05:02 GET /health 200 1ms\n"
        ))
    }
}

/// A host as the example data keeps it: built from the form, not checked.
fn demo_host(details: &HostDetails) -> Result<Host, String> {
    let mut host = Host::from_ssh(&details.ssh, Some(&details.name), Some(&details.address))
        .map_err(|e| e.to_string())?;
    host.projects_dir =
        zevo_core::model::projects_dir(&details.projects_dir).map_err(|e| e.to_string())?;
    Ok(host)
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
