//! Traefik file-provider config generation.
//!
//! Zevo writes exactly one file per project, `zevo-<slug>.yml`, into Traefik's
//! dynamic-config directory. Every such file starts with [`MANAGED_HEADER`].
//! A file is only ever modified or deleted when it is both named `zevo-*.yml`
//! and starts with that header; anything else in the directory is treated as
//! read-only input (for conflict detection).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::{Project, Settings, Target, TraefikSettings, validate_slug};
use crate::{Error, Result};

/// First line of every file Zevo manages.
pub const MANAGED_HEADER: &str = "# managed by captain-zevo — do not edit; changes are overwritten";

const FILE_PREFIX: &str = "zevo-";
const FILE_EXT: &str = ".yml";

/// File name for a project's config: `zevo-<slug>.yml`.
pub fn file_name(slug: &str) -> String {
    format!("{FILE_PREFIX}{slug}{FILE_EXT}")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Doc {
    http: Http,
}

#[derive(Serialize)]
struct Http {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    routers: BTreeMap<String, Router>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    services: BTreeMap<String, Service>,
}

#[derive(Serialize)]
struct Router {
    rule: String,
    #[serde(rename = "entryPoints")]
    entry_points: Vec<String>,
    service: String,
}

#[derive(Serialize)]
struct Service {
    #[serde(rename = "loadBalancer")]
    load_balancer: LoadBalancer,
}

#[derive(Serialize)]
struct LoadBalancer {
    servers: Vec<Server>,
}

#[derive(Serialize)]
struct Server {
    url: String,
}

fn traefik_settings(settings: &Settings) -> Result<&TraefikSettings> {
    settings.traefik.as_ref().ok_or(Error::NotSetUp)
}

/// Router/service names for each route, in route order. Collisions get a
/// deterministic numeric suffix (`-2`, `-3`, ...) based on route order.
fn route_names(project: &Project) -> Vec<String> {
    let mut used = BTreeSet::new();
    let mut names = Vec::with_capacity(project.routes.len());
    for route in &project.routes {
        let base = match &route.subdomain {
            Some(sub) => format!("{FILE_PREFIX}{}-{sub}", project.slug),
            None => format!("{FILE_PREFIX}{}", project.slug),
        };
        let mut name = base.clone();
        let mut n = 2;
        while used.contains(&name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        used.insert(name.clone());
        names.push(name);
    }
    names
}

fn url_host(address: &str) -> String {
    if address.contains(':') && !address.starts_with('[') {
        format!("[{address}]")
    } else {
        address.to_string()
    }
}

/// YAML for a project, header included. Output is deterministic.
pub fn render(project: &Project, settings: &Settings) -> Result<String> {
    let traefik = traefik_settings(settings)?;
    let mut routers = BTreeMap::new();
    let mut services = BTreeMap::new();

    for (route, name) in project.routes.iter().zip(route_names(project)) {
        let hostname = route.hostname(&project.slug, &settings.domain_suffix);
        let mut rule = format!("Host(`{hostname}`)");
        if let Some(prefix) = &route.path_prefix {
            rule.push_str(&format!(" && PathPrefix(`{prefix}`)"));
        }
        let host = match &route.target {
            Target::Container => project.slug.clone(),
            Target::HostPort { address } => url_host(address),
        };
        routers.insert(
            name.clone(),
            Router {
                rule,
                entry_points: vec![traefik.entrypoint.clone()],
                service: name.clone(),
            },
        );
        services.insert(
            name,
            Service {
                load_balancer: LoadBalancer {
                    servers: vec![Server {
                        url: format!("http://{host}:{}", route.port),
                    }],
                },
            },
        );
    }

    let doc = Doc {
        http: Http { routers, services },
    };
    let body = serde_norway::to_string(&doc).map_err(|e| {
        Error::Other(format!(
            "rendering Traefik config for '{}': {e}",
            project.slug
        ))
    })?;
    Ok(format!("{MANAGED_HEADER}\n{body}"))
}

// ---------------------------------------------------------------------------
// Writing / removing
// ---------------------------------------------------------------------------

fn has_header(content: &str) -> bool {
    content.lines().next().map(|l| l.trim_end_matches('\r')) == Some(MANAGED_HEADER)
}

/// Slug encoded in a managed-looking file name (`zevo-<slug>.yml`).
fn slug_from_name(name: &str) -> Option<&str> {
    let slug = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_EXT)?;
    validate_slug(slug).ok().map(|_| slug)
}

/// Reads a file; `Ok(None)` if it does not exist.
fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(format!("reading {}", path.display()), e)),
    }
}

fn not_managed(path: &Path) -> Error {
    Error::Other(format!(
        "{} exists but is not managed by captain-zevo (missing header); refusing to touch it",
        path.display()
    ))
}

/// Writes the project's file atomically (temp file in the same dir + rename),
/// only if the content differs. Returns `true` if it wrote.
///
/// Refuses (with an error) to overwrite an existing `zevo-<slug>.yml` that
/// lacks the managed header. The dynamic directory must already exist.
pub fn write_project(project: &Project, settings: &Settings) -> Result<bool> {
    validate_slug(&project.slug)?;
    let dir = &traefik_settings(settings)?.dynamic_dir;
    let content = render(project, settings)?;
    let path = dir.join(file_name(&project.slug));

    if let Some(existing) = read_optional(&path)? {
        if !has_header(&existing) {
            return Err(not_managed(&path));
        }
        if existing == content {
            return Ok(false);
        }
    }

    // The temp name does not end in .yml/.yaml/.toml, so Traefik ignores it.
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        file_name(&project.slug),
        std::process::id()
    ));
    let result = (|| {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| Error::io(format!("creating {}", tmp.display()), e))?;
        f.write_all(content.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| Error::io(format!("writing {}", tmp.display()), e))?;
        drop(f);
        fs::rename(&tmp, &path).map_err(|e| {
            Error::io(
                format!("renaming {} to {}", tmp.display(), path.display()),
                e,
            )
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    tracing::debug!(path = %path.display(), "wrote Traefik config");
    Ok(true)
}

/// Removes the project's managed file if present and managed. Returns `true`
/// if removed, `false` if there was no file.
///
/// Errors (without touching anything) if `slug` is not a valid slug or if the
/// file exists but lacks the managed header.
pub fn remove_project(slug: &str, dynamic_dir: &Path) -> Result<bool> {
    validate_slug(slug)?;
    let path = dynamic_dir.join(file_name(slug));
    let Some(existing) = read_optional(&path)? else {
        return Ok(false);
    };
    if !has_header(&existing) {
        return Err(not_managed(&path));
    }
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io(format!("removing {}", path.display()), e)),
    }
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterInfo {
    pub file: PathBuf,
    pub router: String,
    /// Hostnames from `Host(...)` matchers in the rule, lowercased.
    pub hostnames: Vec<String>,
    /// File is named `zevo-*.yml` and carries the managed header.
    pub managed: bool,
    /// Project slug, for managed files.
    pub slug: Option<String>,
}

/// Extracts hostnames from `Host(...)` matchers in a Traefik rule. Handles
/// ``Host(`a`)``, ``Host(`a`, `b`)`` and ``Host(`a`) || Host(`b`)``; also
/// accepts double-quoted arguments. Ignores `HostRegexp`, `HostSNI`, etc.
fn rule_hostnames(rule: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = rule;
    while let Some(i) = rest.find("Host(") {
        let preceded_by_ident = rest[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let after = &rest[i + "Host(".len()..];
        if preceded_by_ident {
            rest = after;
            continue;
        }
        // Walk the argument list up to the closing paren (outside quotes).
        let mut quote: Option<char> = None;
        let mut current = String::new();
        let mut end = after.len();
        for (j, c) in after.char_indices() {
            match quote {
                Some(q) if c == q => {
                    let h = current.trim().to_ascii_lowercase();
                    if !h.is_empty() {
                        out.push(h);
                    }
                    current.clear();
                    quote = None;
                }
                Some(_) => current.push(c),
                None if c == '`' || c == '"' => quote = Some(c),
                None if c == ')' => {
                    end = j + 1;
                    break;
                }
                None => {}
            }
        }
        rest = &after[end..];
    }
    out
}

fn is_yaml(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yml" | "yaml")
    )
}

/// Sorted list of YAML files in `dir`; empty if the dir does not exist.
fn yaml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(format!("reading directory {}", dir.display()), e)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| Error::io(format!("reading directory {}", dir.display()), e))?;
        let path = entry.path();
        if is_yaml(&path) && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Slug if the file at `path` with `content` is managed by Zevo.
fn managed_slug(path: &Path, content: &str) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let slug = slug_from_name(name)?;
    has_header(content).then(|| slug.to_string())
}

/// Lists HTTP routers in every YAML file in `dynamic_dir`, with the hostnames
/// from their rules. Unparseable or unreadable files are skipped with a
/// warning. Non-YAML files (including TOML) are ignored. A missing directory
/// yields an empty list.
pub fn scan(dynamic_dir: &Path) -> Result<Vec<RouterInfo>> {
    let mut out = Vec::new();
    for path in yaml_files(dynamic_dir)? {
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "skipping unreadable Traefik config");
                continue;
            }
        };
        let doc: serde_norway::Value = match serde_norway::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "skipping unparseable Traefik config");
                continue;
            }
        };
        let slug = managed_slug(&path, &content);
        let Some(routers) = doc
            .get("http")
            .and_then(|h| h.get("routers"))
            .and_then(|r| r.as_mapping())
        else {
            continue;
        };
        let mut infos: Vec<RouterInfo> = routers
            .iter()
            .filter_map(|(name, router)| {
                let name = name.as_str()?.to_string();
                let rule = router
                    .get("rule")
                    .and_then(|r| r.as_str())
                    .unwrap_or_default();
                Some(RouterInfo {
                    file: path.clone(),
                    router: name,
                    hostnames: rule_hostnames(rule),
                    managed: slug.is_some(),
                    slug: slug.clone(),
                })
            })
            .collect();
        infos.sort_by(|a, b| a.router.cmp(&b.router));
        out.extend(infos);
    }
    Ok(out)
}

/// Fails with [`Error::HostnameConflict`] for the first hostname this project
/// would claim that a router owned by someone else (an unmanaged file or
/// another project's managed file) already claims. The project's own file is
/// ignored. Hostnames compare case-insensitively.
pub fn check_conflicts(project: &Project, settings: &Settings) -> Result<()> {
    let dir = &traefik_settings(settings)?.dynamic_dir;
    let routers = scan(dir)?;
    for route in &project.routes {
        let hostname = route
            .hostname(&project.slug, &settings.domain_suffix)
            .to_ascii_lowercase();
        let clash = routers.iter().find(|r| {
            r.slug.as_deref() != Some(project.slug.as_str()) && r.hostnames.contains(&hostname)
        });
        if let Some(r) = clash {
            let owner = match &r.slug {
                Some(slug) => format!("project '{slug}' ({})", r.file.display()),
                None => format!("router '{}' in {}", r.router, r.file.display()),
            };
            return Err(Error::HostnameConflict { hostname, owner });
        }
    }
    Ok(())
}

/// Slugs of managed files in the dir (for finding orphans), sorted.
pub fn managed_slugs(dynamic_dir: &Path) -> Result<Vec<String>> {
    let mut slugs = Vec::new();
    for path in yaml_files(dynamic_dir)? {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if slug_from_name(name).is_none() {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "skipping unreadable file");
                continue;
            }
        };
        if let Some(slug) = managed_slug(&path, &content) {
            slugs.push(slug);
        }
    }
    Ok(slugs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Route, TraefikMode};

    fn settings(dir: &Path) -> Settings {
        Settings {
            traefik: Some(TraefikSettings {
                mode: TraefikMode::Managed,
                container: Some("zevo-traefik".into()),
                dynamic_dir: dir.to_path_buf(),
                entrypoint: "web".into(),
                http_port: 80,
            }),
            ..Settings::default()
        }
    }

    fn route(sub: Option<&str>, port: u16) -> Route {
        Route {
            subdomain: sub.map(Into::into),
            port,
            path_prefix: None,
            target: Target::Container,
        }
    }

    fn project(slug: &str, routes: Vec<Route>) -> Project {
        Project {
            slug: slug.into(),
            name: slug.into(),
            host: "local".into(),
            path: PathBuf::from("/src").join(slug),
            kind: Default::default(),
            service: None,
            routes,
            tags: vec![],
        }
    }

    fn multi_route_project() -> Project {
        project(
            "shop",
            vec![
                route(None, 3000),
                route(Some("api"), 8080),
                Route {
                    path_prefix: Some("/v2".into()),
                    ..route(Some("api"), 8081)
                },
                Route {
                    target: Target::HostPort {
                        address: "192.168.1.20".into(),
                    },
                    ..route(Some("db"), 5050)
                },
            ],
        )
    }

    #[test]
    fn file_name_format() {
        assert_eq!(file_name("shop"), "zevo-shop.yml");
    }

    #[test]
    fn render_multi_route() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = render(&multi_route_project(), &settings(dir.path())).unwrap();
        let expected = "\
# managed by captain-zevo — do not edit; changes are overwritten
http:
  routers:
    zevo-shop:
      rule: Host(`shop.localhost`)
      entryPoints:
      - web
      service: zevo-shop
    zevo-shop-api:
      rule: Host(`api.shop.localhost`)
      entryPoints:
      - web
      service: zevo-shop-api
    zevo-shop-api-2:
      rule: Host(`api.shop.localhost`) && PathPrefix(`/v2`)
      entryPoints:
      - web
      service: zevo-shop-api-2
    zevo-shop-db:
      rule: Host(`db.shop.localhost`)
      entryPoints:
      - web
      service: zevo-shop-db
  services:
    zevo-shop:
      loadBalancer:
        servers:
        - url: http://shop:3000
    zevo-shop-api:
      loadBalancer:
        servers:
        - url: http://shop:8080
    zevo-shop-api-2:
      loadBalancer:
        servers:
        - url: http://shop:8081
    zevo-shop-db:
      loadBalancer:
        servers:
        - url: http://192.168.1.20:5050
";
        assert_eq!(yaml, expected);
        // Deterministic.
        assert_eq!(
            yaml,
            render(&multi_route_project(), &settings(dir.path())).unwrap()
        );
        // Round-trips through our own scanner's view of hostnames.
        let v: serde_norway::Value = serde_norway::from_str(&yaml).unwrap();
        assert!(
            v.get("http")
                .unwrap()
                .get("routers")
                .unwrap()
                .as_mapping()
                .unwrap()
                .len()
                == 4
        );
    }

    #[test]
    fn render_zero_routes_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = render(&project("empty", vec![]), &settings(dir.path())).unwrap();
        assert_eq!(yaml, format!("{MANAGED_HEADER}\nhttp: {{}}\n"));
        let v: serde_norway::Value = serde_norway::from_str(&yaml).unwrap();
        assert!(v.get("http").unwrap().as_mapping().unwrap().is_empty());
    }

    #[test]
    fn render_requires_setup() {
        let err = render(&multi_route_project(), &Settings::default()).unwrap_err();
        assert!(matches!(err, Error::NotSetUp));
    }

    #[test]
    fn render_ipv6_host_port() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(
            "v6",
            vec![Route {
                target: Target::HostPort {
                    address: "::1".into(),
                },
                ..route(None, 80)
            }],
        );
        assert!(
            render(&p, &settings(dir.path()))
                .unwrap()
                .contains("url: http://[::1]:80")
        );
    }

    #[test]
    fn write_is_idempotent_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let p = multi_route_project();
        assert!(write_project(&p, &s).unwrap());
        assert!(!write_project(&p, &s).unwrap());

        let path = dir.path().join("zevo-shop.yml");
        assert_eq!(fs::read_to_string(&path).unwrap(), render(&p, &s).unwrap());

        // Content change triggers a rewrite.
        let p2 = project("shop", vec![route(None, 4000)]);
        assert!(write_project(&p2, &s).unwrap());
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("http://shop:4000")
        );

        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["zevo-shop.yml".to_string()]);
    }

    #[test]
    fn write_refuses_to_clobber_unmanaged_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zevo-shop.yml");
        fs::write(&path, "http: {}\n").unwrap();
        assert!(write_project(&multi_route_project(), &settings(dir.path())).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "http: {}\n");
    }

    #[test]
    fn remove_project_only_removes_managed_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());

        // Missing file: nothing to do.
        assert!(!remove_project("shop", dir.path()).unwrap());

        // Managed file is removed.
        write_project(&multi_route_project(), &s).unwrap();
        assert!(remove_project("shop", dir.path()).unwrap());
        assert!(!dir.path().join("zevo-shop.yml").exists());

        // Right name, no header: refused and left intact.
        let x = dir.path().join("zevo-x.yml");
        fs::write(&x, "# hand written\nhttp: {}\n").unwrap();
        assert!(remove_project("x", dir.path()).is_err());
        assert!(x.exists());

        // Names that don't map to zevo-<slug>.yml are refused outright.
        let other = dir.path().join("other.yml");
        fs::write(&other, format!("{MANAGED_HEADER}\nhttp: {{}}\n")).unwrap();
        for bad in ["../other", "other.yml", "", "Shop", "a/b"] {
            assert!(
                matches!(remove_project(bad, dir.path()), Err(Error::InvalidSlug(_))),
                "{bad}"
            );
        }
        assert!(other.exists());
    }

    #[test]
    fn rule_hostname_forms() {
        assert_eq!(rule_hostnames("Host(`a.test`)"), vec!["a.test"]);
        assert_eq!(
            rule_hostnames("Host(`a.test`, `B.test`)"),
            vec!["a.test", "b.test"]
        );
        assert_eq!(
            rule_hostnames("Host(`a.test`) || Host(`b.test`) && PathPrefix(`/x`)"),
            vec!["a.test", "b.test"]
        );
        assert_eq!(rule_hostnames("Host(\"a.test\")"), vec!["a.test"]);
        assert!(rule_hostnames("HostRegexp(`{x:.+}`) || HostSNI(`s.test`)").is_empty());
        assert!(rule_hostnames("PathPrefix(`/`)").is_empty());
    }

    #[test]
    fn scan_lists_routers_and_marks_managed() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        write_project(&project("shop", vec![route(None, 3000)]), &s).unwrap();
        fs::write(
            dir.path().join("manual.yaml"),
            "http:\n  routers:\n    one:\n      rule: Host(`one.test`)\n      service: s\n    \
             two:\n      rule: Host(`two.test`, `deux.test`)\n      service: s\n    \
             three:\n      rule: Host(`a.test`) || Host(`b.test`)\n      service: s\n",
        )
        .unwrap();
        // Named like ours but not managed.
        fs::write(
            dir.path().join("zevo-fake.yml"),
            "http:\n  routers:\n    f:\n      rule: Host(`fake.test`)\n",
        )
        .unwrap();
        fs::write(dir.path().join("broken.yml"), "http: [unclosed\n").unwrap();
        fs::write(dir.path().join("notes.toml"), "[http]\n").unwrap();
        fs::write(dir.path().join("empty.yml"), "").unwrap();

        let routers = scan(dir.path()).unwrap();
        let find = |name: &str| routers.iter().find(|r| r.router == name).unwrap();
        assert_eq!(routers.len(), 5);

        assert_eq!(find("one").hostnames, vec!["one.test"]);
        assert_eq!(find("two").hostnames, vec!["two.test", "deux.test"]);
        assert_eq!(find("three").hostnames, vec!["a.test", "b.test"]);
        assert!(!find("one").managed && find("one").slug.is_none());

        let f = find("f");
        assert!(!f.managed && f.slug.is_none());

        let shop = find("zevo-shop");
        assert!(shop.managed);
        assert_eq!(shop.slug.as_deref(), Some("shop"));
        assert_eq!(shop.hostnames, vec!["shop.localhost"]);
        assert_eq!(shop.file, dir.path().join("zevo-shop.yml"));

        assert_eq!(managed_slugs(dir.path()).unwrap(), vec!["shop"]);
    }

    #[test]
    fn scan_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan(&dir.path().join("nope")).unwrap().is_empty());
        assert!(managed_slugs(&dir.path().join("nope")).unwrap().is_empty());
    }

    #[test]
    fn check_conflicts_detects_other_owners() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let shop = multi_route_project();

        // No other files; own existing file doesn't count.
        check_conflicts(&shop, &s).unwrap();
        write_project(&shop, &s).unwrap();
        check_conflicts(&shop, &s).unwrap();

        // Another project claims one of our hostnames.
        let thief = project("thief", vec![route(None, 1)]);
        let mut thief_file = render(&thief, &s).unwrap();
        thief_file = thief_file.replace("thief.localhost", "api.shop.localhost");
        fs::write(dir.path().join("zevo-thief.yml"), thief_file).unwrap();
        match check_conflicts(&shop, &s) {
            Err(Error::HostnameConflict { hostname, owner }) => {
                assert_eq!(hostname, "api.shop.localhost");
                assert!(owner.contains("thief"), "{owner}");
            }
            other => panic!("expected conflict, got {other:?}"),
        }
        fs::remove_file(dir.path().join("zevo-thief.yml")).unwrap();

        // An unmanaged file claims one (case-insensitively).
        fs::write(
            dir.path().join("legacy.yml"),
            "http:\n  routers:\n    old:\n      rule: Host(`x.test`) || Host(`SHOP.localhost`)\n",
        )
        .unwrap();
        match check_conflicts(&shop, &s) {
            Err(Error::HostnameConflict { hostname, owner }) => {
                assert_eq!(hostname, "shop.localhost");
                assert!(owner.contains("legacy.yml"), "{owner}");
            }
            other => panic!("expected conflict, got {other:?}"),
        }

        // A project without that hostname is fine.
        check_conflicts(&project("other", vec![route(None, 1)]), &s).unwrap();
    }
}
