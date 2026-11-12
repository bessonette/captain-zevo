//! Registering, editing and removing projects. Shared by the CLI and the app
//! so both apply the same checks.

use std::path::Path;

use crate::model::{LOCAL_HOST_ID, Project, Route, Target, slugify, validate_slug};
use crate::store::Store;
use crate::{Error, Result, devcontainer, traefik};

/// Inspects a folder and proposes a project for it: name and slug from the
/// devcontainer config or folder name, and a route for the first port found.
/// Nothing is saved.
pub fn draft(folder: &Path) -> Result<Project> {
    let folder = &expand_home(folder);
    let path = std::fs::canonicalize(folder)
        .map_err(|e| Error::io(format!("cannot open {}", folder.display()), e))?;
    if !path.is_dir() {
        return Err(Error::Other(format!("{} is not a folder", path.display())));
    }
    let detected = devcontainer::detect(&path)?;
    let folder_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // A project that declares itself wins over guesses from its ports.
    if let Some(declared) = detected.declared {
        return Ok(Project {
            slug: declared.slug.unwrap_or_else(|| slugify(&folder_name)),
            name: declared.name.or(detected.name).unwrap_or(folder_name),
            host: LOCAL_HOST_ID.to_string(),
            path,
            kind: detected.kind,
            service: detected.service,
            routes: declared.routes,
            tags: Vec::new(),
            declared: true,
            address: None,
            env_file: declared.env_file,
        });
    }

    let routes = detected
        .ports
        .first()
        .map(|port| Route {
            subdomain: None,
            port: *port,
            path_prefix: None,
            target: Target::Container,
            backend: crate::model::Backend::default(),
            host: None,
        })
        .into_iter()
        .collect();
    Ok(Project {
        slug: slugify(&folder_name),
        name: detected.name.unwrap_or(folder_name),
        host: LOCAL_HOST_ID.to_string(),
        path,
        kind: detected.kind,
        service: detected.service,
        routes,
        tags: Vec::new(),
        declared: false,
        address: None,
        env_file: None,
    })
}

/// Points every route of a declared project at `address`, or back at the
/// container when there is none.
pub fn apply_address(project: &mut Project) {
    for route in &mut project.routes {
        route.target = match &project.address {
            Some(address) => Target::HostPort {
                address: address.clone(),
            },
            None => Target::Container,
        };
    }
}

/// Re-reads a declared project's routes from its devcontainer.json. Returns
/// the updated project if anything changed (already saved), `None` otherwise.
/// A folder that is not visible from here is left alone.
pub fn sync_declared(store: &Store, project: &Project) -> Result<Option<Project>> {
    if !project.declared || !project.path.is_dir() {
        return Ok(None);
    }
    let mut updated = project.clone();
    match devcontainer::detect(&project.path)?.declared {
        Some(declared) => {
            updated.env_file = declared.env_file;
            updated.routes = declared.routes;
            apply_address(&mut updated);
        }
        // The block was removed: keep the routes, now editable in Zevo.
        None => {
            updated.declared = false;
            updated.env_file = None;
        }
    }
    if updated == *project {
        return Ok(None);
    }
    validate(store, &updated)?;
    store.save_project(&updated)?;
    Ok(Some(updated))
}

/// Fails for projects whose routes are defined in their repository.
pub fn ensure_routes_editable(project: &Project) -> Result<()> {
    if project.declared {
        return Err(Error::Other(format!(
            "the routes of '{}' are defined in its devcontainer.json (customizations.zevo); edit them there",
            project.slug
        )));
    }
    Ok(())
}

/// Replaces a leading `~` with the user's home folder, as a shell would.
fn expand_home(path: &Path) -> std::path::PathBuf {
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

fn validate(store: &Store, project: &Project) -> Result<()> {
    let settings = store.settings()?;
    if settings.traefik.is_none() {
        return Err(Error::NotSetUp);
    }
    validate_slug(&project.slug)?;
    if project.name.trim().is_empty() {
        return Err(Error::Other("the project needs a name".into()));
    }
    let mut seen = Vec::new();
    for route in &project.routes {
        if let Some(sub) = &route.subdomain {
            validate_slug(sub)?;
        }
        if route.port == 0 {
            return Err(Error::Other("a route's port cannot be 0".into()));
        }
        // These values are written into Traefik rules and URLs, so only
        // characters that cannot change their meaning are allowed.
        if let Some(prefix) = &route.path_prefix {
            let plain = prefix.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~' | b'%')
            });
            if !prefix.starts_with('/') || !plain {
                return Err(Error::Other(format!(
                    "path prefix '{prefix}' must start with '/' and use only letters, digits and / - _ . ~ %"
                )));
            }
        }
        if let Some(host) = &route.host {
            let ok = !host.is_empty()
                && host.len() <= 253
                && host
                    .split('.')
                    .all(|label| validate_slug(&label.to_lowercase()).is_ok());
            if !ok {
                return Err(Error::Other(format!("'{host}' is not a valid hostname")));
            }
        }
        if let Target::HostPort { address } = &route.target {
            let ok = !address.is_empty()
                && address.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']')
                });
            if !ok {
                return Err(Error::Other(format!(
                    "'{address}' is not a valid address (use a hostname or IP address)"
                )));
            }
        }
        let key = (
            route.hostname(&project.slug, &settings.domain_suffix),
            route.path_prefix.clone(),
        );
        if seen.contains(&key) {
            return Err(Error::Other(format!(
                "two routes use the address {}{}",
                route.hostname(&project.slug, &settings.domain_suffix),
                route.path_prefix.as_deref().unwrap_or("")
            )));
        }
        seen.push(key);
    }
    traefik::check_conflicts(project, &settings)
}

/// Registers a new project after checking it. Call `reconcile` afterwards to
/// write its routes.
pub fn add(store: &Store, project: &Project) -> Result<()> {
    validate(store, project)?;
    store.add_project(project)
}

/// Saves changes to an existing project after checking them.
pub fn update(store: &Store, project: &Project) -> Result<()> {
    validate(store, project)?;
    store.save_project(project)
}

/// Unregisters a project and deletes its route file.
pub fn remove(store: &Store, slug: &str) -> Result<()> {
    let settings = store.settings()?;
    store.remove_project(slug)?;
    if let Some(t) = &settings.traefik {
        traefik::remove_project(slug, &t.dynamic_dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup;

    fn set_up() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let routes = tmp.path().join("routes");
        std::fs::create_dir(&routes).unwrap();
        let store = Store::open(tmp.path().join("cfg"));
        setup::adopt_manual(&store, &routes, "web", 80, false).unwrap();
        (tmp, store)
    }

    fn folder(tmp: &Path, name: &str, config: &str) -> std::path::PathBuf {
        let dir = tmp.join(name);
        std::fs::create_dir_all(dir.join(".devcontainer")).unwrap();
        std::fs::write(dir.join(".devcontainer/devcontainer.json"), config).unwrap();
        dir
    }

    #[test]
    fn drafts_from_a_devcontainer_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = folder(
            tmp.path(),
            "My Shop",
            r#"{"image":"node","name":"Shop","forwardPorts":[3000, 9229]}"#,
        );
        let p = draft(&dir).unwrap();
        assert_eq!((p.slug.as_str(), p.name.as_str()), ("my-shop", "Shop"));
        assert_eq!(p.routes.len(), 1);
        assert_eq!(p.routes[0].port, 3000);

        let plain = tmp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        let p = draft(&plain).unwrap();
        assert_eq!(p.name, "plain");
        assert!(p.routes.is_empty());
        assert!(draft(&tmp.path().join("missing")).is_err());
    }

    #[test]
    fn declared_projects_follow_their_devcontainer_json() {
        let (tmp, store) = set_up();
        let config = |routes: &str| {
            format!(
                r#"{{"image":"node","forwardPorts":[9999],"customizations":{{"zevo":{{"name":"Shop","routes":{routes}}}}}}}"#
            )
        };
        let dir = folder(
            tmp.path(),
            "shop",
            &config(r#"[{"port":3000},{"subdomain":"api","port":8080}]"#),
        );
        let p = draft(&dir).unwrap();
        assert!(p.declared);
        assert_eq!(p.name, "Shop");
        // The declaration replaces the guess from forwardPorts.
        assert_eq!(
            p.routes.iter().map(|r| r.port).collect::<Vec<_>>(),
            [3000, 8080]
        );
        add(&store, &p).unwrap();
        assert!(ensure_routes_editable(&p).is_err());
        assert_eq!(sync_declared(&store, &p).unwrap(), None);

        // Editing the file changes the routes on the next sync.
        std::fs::write(
            dir.join(".devcontainer/devcontainer.json"),
            config(r#"[{"port":3000}]"#),
        )
        .unwrap();
        let updated = sync_declared(&store, &p).unwrap().unwrap();
        assert_eq!(updated.routes.len(), 1);
        assert_eq!(store.project("shop").unwrap(), updated);

        // A machine-level address applies to every declared route.
        let mut remote = updated.clone();
        remote.address = Some("host.docker.internal".into());
        store.save_project(&remote).unwrap();
        std::fs::write(
            dir.join(".devcontainer/devcontainer.json"),
            config(r#"[{"port":3000},{"subdomain":"x","port":1}]"#),
        )
        .unwrap();
        let synced = sync_declared(&store, &remote).unwrap().unwrap();
        assert!(synced.routes.iter().all(|r| r.target
            == Target::HostPort {
                address: "host.docker.internal".into()
            }));

        // A broken declaration is an error and changes nothing.
        std::fs::write(
            dir.join(".devcontainer/devcontainer.json"),
            config(r#"[{"prot":3000}]"#),
        )
        .unwrap();
        assert!(sync_declared(&store, &synced).is_err());
        assert_eq!(store.project("shop").unwrap(), synced);

        // Removing the block hands the routes back to Zevo.
        std::fs::write(
            dir.join(".devcontainer/devcontainer.json"),
            r#"{"image":"node"}"#,
        )
        .unwrap();
        let freed = sync_declared(&store, &synced).unwrap().unwrap();
        assert!(!freed.declared);
        assert_eq!(freed.routes.len(), 2);
        assert!(ensure_routes_editable(&freed).is_ok());
    }

    #[test]
    fn rejects_values_that_could_alter_a_traefik_rule() {
        let (tmp, store) = set_up();
        let dir = folder(
            tmp.path(),
            "shop",
            r#"{"image":"node","forwardPorts":[3000]}"#,
        );
        let base = draft(&dir).unwrap();

        let mut p = base.clone();
        p.routes[0].path_prefix = Some("/x`) || Host(`bank.localhost".into());
        assert!(add(&store, &p).is_err());

        let mut p = base.clone();
        p.routes[0].host = Some("a.localhost`) || Host(`b".into());
        assert!(add(&store, &p).is_err());

        let mut p = base.clone();
        p.routes[0].target = Target::HostPort {
            address: "evil/../x y".into(),
        };
        assert!(add(&store, &p).is_err());

        let mut p = base;
        p.routes[0].path_prefix = Some("/api/v1.2".into());
        p.routes[0].target = Target::HostPort {
            address: "host.docker.internal".into(),
        };
        add(&store, &p).unwrap();
    }

    #[test]
    fn expands_a_leading_tilde_only() {
        let home = directories::BaseDirs::new()
            .unwrap()
            .home_dir()
            .to_path_buf();
        assert_eq!(expand_home(Path::new("~")), home);
        assert_eq!(
            expand_home(Path::new("~/code/shop")),
            home.join("code/shop")
        );
        assert_eq!(expand_home(Path::new("/srv/~/x")), Path::new("/srv/~/x"));
        assert_eq!(expand_home(Path::new("~other/x")), Path::new("~other/x"));
    }

    #[test]
    fn add_update_remove_with_checks() {
        let (tmp, store) = set_up();
        let dir = folder(
            tmp.path(),
            "shop",
            r#"{"image":"node","forwardPorts":[3000]}"#,
        );
        let mut p = draft(&dir).unwrap();
        add(&store, &p).unwrap();
        assert!(matches!(add(&store, &p), Err(Error::DuplicateProject(_))));

        // Same address twice is rejected; a different subdomain is fine.
        p.routes.push(p.routes[0].clone());
        assert!(update(&store, &p).is_err());
        p.routes[1].subdomain = Some("api".into());
        update(&store, &p).unwrap();
        assert_eq!(store.project("shop").unwrap().routes.len(), 2);

        p.routes[1].path_prefix = Some("v1".into());
        assert!(update(&store, &p).is_err());

        traefik::write_project(&store.project("shop").unwrap(), &store.settings().unwrap())
            .unwrap();
        remove(&store, "shop").unwrap();
        assert!(
            std::fs::read_dir(tmp.path().join("routes"))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn add_requires_setup_and_a_free_hostname() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = folder(
            tmp.path(),
            "shop",
            r#"{"image":"node","forwardPorts":[3000]}"#,
        );
        let p = draft(&dir).unwrap();
        assert!(matches!(
            add(&Store::open(tmp.path().join("cfg")), &p),
            Err(Error::NotSetUp)
        ));

        let (tmp2, store) = set_up();
        std::fs::write(
            tmp2.path().join("routes/other.yml"),
            "http:\n  routers:\n    x:\n      rule: Host(`shop.localhost`)\n      service: x\n",
        )
        .unwrap();
        assert!(matches!(
            add(&store, &p),
            Err(Error::HostnameConflict { .. })
        ));
    }
}
