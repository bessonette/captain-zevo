//! On-disk registry: `settings.toml` plus one `projects/<slug>.toml` per project.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Serialize, de::DeserializeOwned};

use crate::model::{Host, Project, Settings, validate_slug};
use crate::{Error, Result};

/// Overrides the config directory (used by tests and portable installs).
pub const CONFIG_DIR_ENV: &str = "ZEVO_CONFIG_DIR";

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Opens the store in `$ZEVO_CONFIG_DIR`, or the OS config directory.
    pub fn open_default() -> Result<Self> {
        if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
            return Ok(Self::open(dir));
        }
        let dirs = directories::ProjectDirs::from("dev", "captain-zevo", "captain-zevo")
            .ok_or_else(|| Error::Other("cannot determine the user config directory".into()))?;
        Ok(Self::open(dirs.config_dir()))
    }

    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Route directory used when Zevo runs its own Traefik.
    pub fn managed_dynamic_dir(&self) -> PathBuf {
        self.dir.join("traefik").join("dynamic")
    }

    fn settings_path(&self) -> PathBuf {
        self.dir.join("settings.toml")
    }

    fn projects_dir(&self) -> PathBuf {
        self.dir.join("projects")
    }

    fn project_path(&self, slug: &str) -> PathBuf {
        self.projects_dir().join(format!("{slug}.toml"))
    }

    pub fn settings(&self) -> Result<Settings> {
        Ok(read_toml(&self.settings_path())?.unwrap_or_default())
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        write_toml(&self.settings_path(), settings)
    }

    /// All projects, sorted by slug.
    pub fn projects(&self) -> Result<Vec<Project>> {
        let dir = self.projects_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io(format!("reading {}", dir.display()), e)),
        };
        let mut projects = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
                .path();
            if path.extension().is_some_and(|ext| ext == "toml")
                && let Some(project) = read_toml::<Project>(&path)?
            {
                projects.push(project);
            }
        }
        projects.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(projects)
    }

    pub fn project(&self, slug: &str) -> Result<Project> {
        validate_slug(slug)?;
        read_toml(&self.project_path(slug))?.ok_or_else(|| Error::UnknownProject(slug.to_string()))
    }

    /// Registers a new project; fails if the slug is taken.
    pub fn add_project(&self, project: &Project) -> Result<()> {
        validate_slug(&project.slug)?;
        if self.project_path(&project.slug).exists() {
            return Err(Error::DuplicateProject(project.slug.clone()));
        }
        write_toml(&self.project_path(&project.slug), project)
    }

    /// Overwrites an existing project.
    pub fn save_project(&self, project: &Project) -> Result<()> {
        validate_slug(&project.slug)?;
        if !self.project_path(&project.slug).exists() {
            return Err(Error::UnknownProject(project.slug.clone()));
        }
        write_toml(&self.project_path(&project.slug), project)
    }

    pub fn remove_project(&self, slug: &str) -> Result<()> {
        validate_slug(slug)?;
        let path = self.project_path(slug);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::UnknownProject(slug.to_string()))
            }
            Err(e) => Err(Error::io(format!("removing {}", path.display()), e)),
        }
    }
}

impl Store {
    fn hosts_dir(&self) -> PathBuf {
        self.dir.join("hosts")
    }

    fn host_path(&self, id: &str) -> PathBuf {
        self.hosts_dir().join(format!("{id}.toml"))
    }

    /// Remote hosts, sorted by id. This machine is not in the list.
    pub fn hosts(&self) -> Result<Vec<Host>> {
        let dir = self.hosts_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io(format!("reading {}", dir.display()), e)),
        };
        let mut hosts = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
                .path();
            if path.extension().is_some_and(|ext| ext == "toml")
                && let Some(host) = read_toml::<Host>(&path)?
            {
                hosts.push(host);
            }
        }
        hosts.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(hosts)
    }

    pub fn host(&self, id: &str) -> Result<Host> {
        validate_slug(id)?;
        read_toml(&self.host_path(id))?
            .ok_or_else(|| Error::Other(format!("host '{id}' is not registered")))
    }

    pub fn add_host(&self, host: &Host) -> Result<()> {
        validate_slug(&host.id)?;
        if self.host_path(&host.id).exists() {
            return Err(Error::Other(format!(
                "host '{}' is already registered",
                host.id
            )));
        }
        write_toml(&self.host_path(&host.id), host)
    }

    /// Saves changes to a registered host.
    pub fn update_host(&self, host: &Host) -> Result<()> {
        if !self.host_path(&host.id).exists() {
            return Err(Error::Other(format!(
                "host '{}' is not registered",
                host.id
            )));
        }
        write_toml(&self.host_path(&host.id), host)
    }

    /// Removes a host that no project uses.
    pub fn remove_host(&self, id: &str) -> Result<()> {
        validate_slug(id)?;
        if let Some(project) = self.projects()?.iter().find(|p| p.host == id) {
            return Err(Error::Other(format!(
                "host '{id}' is still used by project '{}'; remove that first",
                project.slug
            )));
        }
        let path = self.host_path(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::Other(format!("host '{id}' is not registered")))
            }
            Err(e) => Err(Error::io(format!("removing {}", path.display()), e)),
        }
    }
}

fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
    };
    toml::from_str(&text).map(Some).map_err(|e| Error::Config {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = toml::to_string_pretty(value).map_err(|e| Error::Config {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    atomic_write(path, text.as_bytes())
}

/// Writes via a temp file in the same directory, then renames over `path`.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    // Unique per call: two writers in one process must not share a temp file.
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    result.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::io(format!("writing {}", path.display()), e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DevcontainerKind, Route, Target, TraefikMode, TraefikSettings};

    fn project(slug: &str) -> Project {
        Project {
            declared: false,
            address: None,
            env_file: None,
            skip: Vec::new(),
            slug: slug.into(),
            name: slug.into(),
            host: "local".into(),
            path: "/work/shop".into(),
            kind: DevcontainerKind::Compose,
            service: Some("app".into()),
            routes: vec![
                Route {
                    backend: crate::model::Backend::default(),
                    host: None,
                    subdomain: None,
                    port: 3000,
                    path_prefix: None,
                    target: Target::Container,
                },
                Route {
                    backend: crate::model::Backend::default(),
                    host: None,
                    subdomain: Some("api".into()),
                    port: 8080,
                    path_prefix: Some("/v1".into()),
                    target: Target::HostPort {
                        address: "10.0.0.5".into(),
                    },
                },
            ],
            tags: vec!["web".into()],
        }
    }

    #[test]
    fn settings_default_then_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("cfg"));
        assert_eq!(store.settings().unwrap(), Settings::default());

        let settings = Settings {
            traefik: Some(TraefikSettings {
                tls: false,
                container_dir: None,
                mode: TraefikMode::Managed,
                container: Some("zevo-traefik".into()),
                dynamic_dir: store.managed_dynamic_dir(),
                entrypoint: "web".into(),
                http_port: 80,
            }),
            ..Settings::default()
        };
        store.save_settings(&settings).unwrap();
        assert_eq!(store.settings().unwrap(), settings);
    }

    #[test]
    fn project_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path());
        assert!(store.projects().unwrap().is_empty());

        store.add_project(&project("shop")).unwrap();
        store.add_project(&project("blog")).unwrap();
        assert!(matches!(
            store.add_project(&project("shop")),
            Err(Error::DuplicateProject(_))
        ));

        let slugs: Vec<_> = store
            .projects()
            .unwrap()
            .into_iter()
            .map(|p| p.slug)
            .collect();
        assert_eq!(slugs, ["blog", "shop"]);
        assert_eq!(store.project("shop").unwrap(), project("shop"));

        let mut edited = project("shop");
        edited.routes.truncate(1);
        store.save_project(&edited).unwrap();
        assert_eq!(store.project("shop").unwrap().routes.len(), 1);

        store.remove_project("shop").unwrap();
        assert!(matches!(
            store.project("shop"),
            Err(Error::UnknownProject(_))
        ));
        assert!(matches!(
            store.remove_project("shop"),
            Err(Error::UnknownProject(_))
        ));
        assert!(matches!(
            store.save_project(&project("nope")),
            Err(Error::UnknownProject(_))
        ));
    }

    #[test]
    fn host_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path());
        assert!(store.hosts().unwrap().is_empty());
        let host = Host::from_ssh("dev@10.0.0.5", Some("buildbox"), None).unwrap();
        store.add_host(&host).unwrap();
        assert!(store.add_host(&host).is_err());
        assert_eq!(store.hosts().unwrap(), std::slice::from_ref(&host));
        assert_eq!(store.host("buildbox").unwrap(), host);

        // A host with projects cannot be removed.
        let mut p = project("shop");
        p.host = "buildbox".into();
        store.add_project(&p).unwrap();
        assert!(store.remove_host("buildbox").is_err());
        store.remove_project("shop").unwrap();
        store.remove_host("buildbox").unwrap();
        assert!(store.host("buildbox").is_err());
    }

    #[test]
    fn rejects_path_traversal_slugs() {
        let store = Store::open(tempfile::tempdir().unwrap().path());
        assert!(matches!(
            store.project("../etc"),
            Err(Error::InvalidSlug(_))
        ));
    }

    #[test]
    fn corrupt_file_reports_its_path() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("settings.toml"), "domain_suffix = [").unwrap();
        let err = Store::open(tmp.path()).settings().unwrap_err();
        assert!(matches!(err, Error::Config { ref path, .. } if path.ends_with("settings.toml")));
    }
}
