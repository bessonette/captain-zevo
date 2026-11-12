//! An env file telling a project its own addresses.
//!
//! Apps often need to know their public hostname (allowed hosts, CORS
//! origins, absolute URLs), and that name depends on the short name the
//! project was registered under, so two checkouts of one repository differ.
//! A project opts in by naming a file in `customizations.zevo.envFile`; Zevo
//! keeps it current. Nothing is written into a project that did not ask.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use crate::model::{Project, Settings};
use crate::{Error, Result, reconcile, store};

pub const HEADER: &str = "# Written by Captain Zevo. Do not edit; changes are overwritten.";

/// Checks that an env file path stays inside the project folder.
pub fn validate_path(path: &str) -> std::result::Result<(), String> {
    let p = Path::new(path);
    let inside = !path.is_empty()
        && p.is_relative()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if inside {
        Ok(())
    } else {
        Err(format!(
            "envFile '{path}' must be a relative path inside the project folder"
        ))
    }
}

/// Variable-name form of a subdomain: `widget-dev` becomes `WIDGET_DEV`.
fn var(subdomain: &str) -> String {
    subdomain.to_ascii_uppercase().replace('-', "_")
}

/// File content for a project.
pub fn render(project: &Project, settings: &Settings) -> String {
    let scheme = if settings.traefik.as_ref().is_some_and(|t| t.tls) {
        "https"
    } else {
        "http"
    };
    let mut lines = vec![
        HEADER.to_string(),
        format!("ZEVO_PROJECT={}", project.slug),
        format!("ZEVO_DOMAIN={}.{}", project.slug, settings.domain_suffix),
        format!("ZEVO_SCHEME={scheme}"),
    ];
    let urls = reconcile::urls(project, settings);
    let mut seen = BTreeSet::new();
    for (route, url) in project.routes.iter().zip(urls) {
        // Exact hostnames are the same in every checkout; the app knows them.
        if route.host.is_some() {
            continue;
        }
        let suffix = match &route.subdomain {
            Some(sub) => format!("_{}", var(sub)),
            None => String::new(),
        };
        // Several routes can share a name (different path prefixes); the first wins.
        if !seen.insert(suffix.clone()) {
            continue;
        }
        let host = route.hostname(&project.slug, &settings.domain_suffix);
        lines.push(format!("ZEVO_HOST{suffix}={host}"));
        lines.push(format!("ZEVO_URL{suffix}={url}"));
    }
    lines.join("\n") + "\n"
}

/// Writes the project's env file if it asked for one and the content
/// changed. Returns true if it wrote. A folder not visible from here, or a
/// file there that Zevo did not write, is left alone.
pub fn write(project: &Project, settings: &Settings) -> Result<bool> {
    let Some(relative) = &project.env_file else {
        return Ok(false);
    };
    if !project.path.is_dir() {
        return Ok(false);
    }
    validate_path(relative).map_err(Error::Other)?;
    let path = project.path.join(relative);
    let content = render(project, settings);
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == content => return Ok(false),
        Ok(existing) if !existing.starts_with(HEADER) => {
            return Err(Error::Other(format!(
                "{} exists and was not written by Zevo; remove it or choose another envFile",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
    }
    store::atomic_write(&path, content.as_bytes())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Backend, DevcontainerKind, Route, Target, TraefikMode, TraefikSettings};

    fn project(path: &Path) -> Project {
        let route = |sub: Option<&str>, prefix: Option<&str>, host: Option<&str>| Route {
            subdomain: sub.map(Into::into),
            port: 3000,
            path_prefix: prefix.map(Into::into),
            target: Target::Container,
            backend: Backend::Http,
            host: host.map(Into::into),
        };
        Project {
            slug: "dtrlabs-lane2".into(),
            name: "DTR Labs".into(),
            host: "local".into(),
            path: path.to_path_buf(),
            kind: DevcontainerKind::Image,
            service: None,
            routes: vec![
                route(None, None, None),
                route(Some("widget-dev"), None, None),
                route(Some("api"), Some("/v1"), None),
                route(Some("api"), Some("/v2"), None),
                route(None, None, Some("local.example.com")),
            ],
            tags: vec![],
            declared: true,
            address: None,
            env_file: Some(".devcontainer/zevo.env".into()),
        }
    }

    fn settings() -> Settings {
        Settings {
            traefik: Some(TraefikSettings {
                mode: TraefikMode::Adopted,
                container: None,
                dynamic_dir: "/x".into(),
                entrypoint: "websecure".into(),
                http_port: 443,
                tls: true,
                container_dir: None,
            }),
            ..Settings::default()
        }
    }

    #[test]
    fn renders_hosts_and_urls_per_subdomain() {
        assert_eq!(
            render(&project(Path::new("/p")), &settings()),
            "# Written by Captain Zevo. Do not edit; changes are overwritten.
ZEVO_PROJECT=dtrlabs-lane2
ZEVO_DOMAIN=dtrlabs-lane2.localhost
ZEVO_SCHEME=https
ZEVO_HOST=dtrlabs-lane2.localhost
ZEVO_URL=https://dtrlabs-lane2.localhost
ZEVO_HOST_WIDGET_DEV=widget-dev.dtrlabs-lane2.localhost
ZEVO_URL_WIDGET_DEV=https://widget-dev.dtrlabs-lane2.localhost
ZEVO_HOST_API=api.dtrlabs-lane2.localhost
ZEVO_URL_API=https://api.dtrlabs-lane2.localhost/v1
"
        );
    }

    #[test]
    fn writes_only_when_asked_and_never_over_someone_elses_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut p = project(tmp.path());
        let file = tmp.path().join(".devcontainer/zevo.env");

        assert!(write(&p, &settings()).unwrap());
        assert!(!write(&p, &settings()).unwrap());
        assert!(std::fs::read_to_string(&file).unwrap().starts_with(HEADER));

        std::fs::write(&file, "SECRET=1\n").unwrap();
        assert!(write(&p, &settings()).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "SECRET=1\n");

        p.env_file = None;
        assert!(!write(&p, &settings()).unwrap());

        p.env_file = Some("../outside.env".into());
        assert!(write(&p, &settings()).is_err());
        assert!(validate_path("/etc/passwd").is_err());
        assert!(validate_path("").is_err());
        assert!(validate_path("./.env.zevo").is_ok());
    }
}
