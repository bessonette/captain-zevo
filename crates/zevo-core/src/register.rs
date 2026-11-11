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
    let routes = detected
        .ports
        .first()
        .map(|port| Route {
            subdomain: None,
            port: *port,
            path_prefix: None,
            target: Target::Container,
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
    })
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
        if let Some(prefix) = &route.path_prefix
            && !prefix.starts_with('/')
        {
            return Err(Error::Other(format!(
                "path prefix '{prefix}' must start with '/'"
            )));
        }
        let key = (route.subdomain.clone(), route.path_prefix.clone());
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
        setup::adopt_manual(&store, &routes, "web", 80).unwrap();
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
