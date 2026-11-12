//! Inspects a project folder and reports what kind of devcontainer it has.
//!
//! Follows the devcontainer spec (<https://containers.dev/implementors/json_reference/>):
//! the config is JSONC, the kind is derived from `dockerComposeFile`, `build.dockerfile`
//! (or legacy `dockerFile`) and `image`, and candidate app ports are gathered from
//! `forwardPorts`, `appPort`, `portsAttributes` and, for Compose, the service's
//! `ports:`/`expose:` lists.

use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use serde_norway::Value as Yaml;

use crate::model::{Backend, DevcontainerKind, Route, Target, validate_slug};
use crate::{Error, Result};

/// What [`detect`] found in a project folder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Detected {
    /// Path of the devcontainer.json that was used, if any.
    pub config_path: Option<PathBuf>,
    pub kind: DevcontainerKind,
    /// `name` from devcontainer.json, if set.
    pub name: Option<String>,
    /// Compose `service` from devcontainer.json (compose kind only).
    pub service: Option<String>,
    /// Candidate app ports, deduplicated, in first-seen order.
    pub ports: Vec<u16>,
    /// The `customizations.zevo` block, if the project has one.
    pub declared: Option<Declared>,
    /// `workspaceFolder`: where the project is mounted inside the container.
    pub workspace_folder: Option<String>,
}

/// What a project says about itself in `customizations.zevo`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Declared {
    pub name: Option<String>,
    /// Suggested short name; the user may pick another when adding.
    pub slug: Option<String>,
    pub routes: Vec<Route>,
    /// Where the project wants its addresses written as an env file.
    pub env_file: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawDeclared {
    name: Option<String>,
    slug: Option<String>,
    #[serde(default)]
    routes: Vec<RawRoute>,
    env_file: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawRoute {
    subdomain: Option<String>,
    #[serde(default)]
    subdomains: Vec<String>,
    host: Option<String>,
    port: u16,
    path_prefix: Option<String>,
    #[serde(default)]
    backend: Backend,
}

/// Reads `customizations.zevo`, expanding `subdomains` lists into one route each.
fn parse_declared(value: &Json) -> std::result::Result<Declared, String> {
    let raw: RawDeclared = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if let Some(slug) = &raw.slug {
        validate_slug(slug).map_err(|e| e.to_string())?;
    }
    let mut routes = Vec::new();
    for r in raw.routes {
        if r.port == 0 {
            return Err("a route's port cannot be 0".into());
        }
        if let Some(prefix) = &r.path_prefix
            && !prefix.starts_with('/')
        {
            return Err(format!("pathPrefix '{prefix}' must start with '/'"));
        }
        let route = |subdomain: Option<String>, host: Option<String>| Route {
            subdomain,
            port: r.port,
            path_prefix: r.path_prefix.clone(),
            target: Target::Container,
            backend: r.backend,
            host,
        };
        if let Some(host) = &r.host {
            if r.subdomain.is_some() || !r.subdomains.is_empty() {
                return Err(format!(
                    "route '{host}': use either host or subdomain, not both"
                ));
            }
            let ok = !host.is_empty()
                && host
                    .split('.')
                    .all(|label| validate_slug(&label.to_lowercase()).is_ok());
            if !ok {
                return Err(format!("'{host}' is not a valid hostname"));
            }
            routes.push(route(None, Some(host.to_lowercase())));
            continue;
        }
        let mut subs: Vec<String> = r.subdomain.iter().cloned().collect();
        subs.extend(r.subdomains.iter().cloned());
        if subs.is_empty() {
            routes.push(route(None, None));
        }
        for sub in subs {
            validate_slug(&sub).map_err(|e| e.to_string())?;
            routes.push(route(Some(sub), None));
        }
    }
    if let Some(path) = &raw.env_file {
        crate::envfile::validate_path(path)?;
    }
    Ok(Declared {
        name: raw.name,
        slug: raw.slug,
        routes,
        env_file: raw.env_file,
    })
}

/// Inspects `folder`.
///
/// A folder with no devcontainer config returns `Ok` with kind `None` and empty
/// fields; a config that exists but can't be parsed (or has no `image`, `build`
/// or `dockerComposeFile`) is `Err(Error::Config { path, message })`.
pub fn detect(folder: &Path) -> Result<Detected> {
    let Some(config_path) = find_config(folder) else {
        return Ok(Detected::default());
    };
    let text = std::fs::read_to_string(&config_path)
        .map_err(|e| Error::io(format!("reading {}", config_path.display()), e))?;
    parse_config(folder, config_path, &text, true)
}

/// Like [`detect`], for a devcontainer.json whose text was fetched from
/// somewhere else (another machine). Compose files are not read, so ports
/// come from the devcontainer.json alone.
pub fn detect_text(config_path: &Path, text: &str) -> Result<Detected> {
    let folder = config_path.parent().unwrap_or(Path::new("/"));
    parse_config(folder, config_path.to_path_buf(), text, false)
}

fn parse_config(
    folder: &Path,
    config_path: PathBuf,
    text: &str,
    read_compose: bool,
) -> Result<Detected> {
    let config_err = |message: String| Error::Config {
        path: config_path.clone(),
        message,
    };

    let json: Json =
        jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
            .map_err(|e| config_err(e.to_string()))?;
    let Json::Object(obj) = &json else {
        return Err(config_err("top-level value must be an object".into()));
    };

    let has = |key: &str| obj.get(key).is_some_and(|v| !v.is_null());
    let kind = if has("dockerComposeFile") {
        DevcontainerKind::Compose
    } else if obj
        .get("build")
        .and_then(|b| b.get("dockerfile"))
        .is_some_and(|v| !v.is_null())
        || has("dockerFile")
    {
        DevcontainerKind::Dockerfile
    } else if has("image") {
        DevcontainerKind::Image
    } else {
        return Err(config_err(
            "it has no image, build or dockerComposeFile".into(),
        ));
    };

    let name = obj.get("name").and_then(Json::as_str).map(str::to_owned);
    let service = match kind {
        DevcontainerKind::Compose => obj.get("service").and_then(Json::as_str).map(str::to_owned),
        _ => None,
    };

    let declared = match obj.get("customizations").and_then(|c| c.get("zevo")) {
        Some(value) if !value.is_null() => Some(
            parse_declared(value).map_err(|e| config_err(format!("customizations.zevo: {e}")))?,
        ),
        _ => None,
    };

    let mut ports = Ports::default();

    if let Some(fwd) = obj.get("forwardPorts").and_then(Json::as_array) {
        for entry in fwd {
            match entry {
                Json::Number(n) => ports.push_u64(n.as_u64()),
                Json::String(s) => match s.split_once(':') {
                    None => ports.push_str(s),
                    Some((svc, port)) => {
                        if service.as_deref() == Some(svc) {
                            ports.push_str(port);
                        }
                    }
                },
                _ => {}
            }
        }
    }

    match obj.get("appPort") {
        Some(Json::Array(items)) => items.iter().for_each(|v| push_app_port(&mut ports, v)),
        Some(v) => push_app_port(&mut ports, v),
        None => {}
    }

    if let Some(Json::Object(attrs)) = obj.get("portsAttributes") {
        for key in attrs.keys() {
            if !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()) {
                ports.push_str(key);
            }
        }
    }

    if read_compose && kind == DevcontainerKind::Compose {
        let base = config_path.parent().unwrap_or(folder);
        let files: Vec<&str> = match obj.get("dockerComposeFile") {
            Some(Json::String(s)) => vec![s.as_str()],
            Some(Json::Array(items)) => items.iter().filter_map(Json::as_str).collect(),
            _ => Vec::new(),
        };
        if let Some(service) = &service {
            for file in files {
                compose_ports(&base.join(file), service, &mut ports);
            }
        }
    }

    Ok(Detected {
        config_path: Some(config_path),
        kind,
        name,
        service,
        ports: ports.0,
        declared,
        workspace_folder: obj
            .get("workspaceFolder")
            .and_then(Json::as_str)
            .map(str::to_owned),
    })
}

/// Finds the devcontainer.json to use, following the spec's lookup order.
fn find_config(folder: &Path) -> Option<PathBuf> {
    let dc_dir = folder.join(".devcontainer");
    let primary = dc_dir.join("devcontainer.json");
    if primary.is_file() {
        return Some(primary);
    }
    let root = folder.join(".devcontainer.json");
    if root.is_file() {
        return Some(root);
    }
    let mut subs: Vec<PathBuf> = std::fs::read_dir(&dc_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("devcontainer.json"))
        .filter(|p| p.is_file())
        .collect();
    subs.sort();
    subs.into_iter().next()
}

/// Ordered, deduplicated port list that ignores out-of-range values.
#[derive(Default)]
struct Ports(Vec<u16>);

impl Ports {
    fn push_u64(&mut self, n: Option<u64>) {
        if let Some(p) = n.and_then(|n| u16::try_from(n).ok()).filter(|&p| p != 0)
            && !self.0.contains(&p)
        {
            self.0.push(p);
        }
    }

    /// Pushes a plain decimal port string; anything else (ranges etc.) is ignored.
    fn push_str(&mut self, s: &str) {
        let s = s.trim();
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            self.push_u64(s.parse().ok());
        }
    }
}

/// `appPort` entry: a number, `"8080"` or `"8080:80"` (container side is last).
fn push_app_port(ports: &mut Ports, v: &Json) {
    match v {
        Json::Number(n) => ports.push_u64(n.as_u64()),
        Json::String(s) => ports.push_str(s.rsplit(':').next().unwrap_or(s)),
        _ => {}
    }
}

/// Adds container-side ports of `service` from the compose file at `path`.
/// Problems are logged and otherwise ignored.
fn compose_ports(path: &Path, service: &str, ports: &mut Ports) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read compose file");
            return;
        }
    };
    let doc: Yaml = match serde_norway::from_str(&text) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot parse compose file");
            return;
        }
    };
    let Some(svc) = doc.get("services").and_then(|s| s.get(service)) else {
        return;
    };
    if let Some(list) = svc.get("ports").and_then(Yaml::as_sequence) {
        for entry in list {
            match entry {
                Yaml::Mapping(_) => match entry.get("target") {
                    Some(Yaml::Number(n)) => ports.push_u64(n.as_u64()),
                    Some(Yaml::String(s)) => ports.push_str(s),
                    _ => {}
                },
                other => push_compose_short(ports, other),
            }
        }
    }
    if let Some(list) = svc.get("expose").and_then(Yaml::as_sequence) {
        for entry in list {
            push_compose_short(ports, entry);
        }
    }
}

/// Short compose syntax: `3000`, `"8080:80"`, `"127.0.0.1:8080:80"`, `"80/tcp"`.
/// Ranges are skipped by [`Ports::push_str`].
fn push_compose_short(ports: &mut Ports, v: &Yaml) {
    match v {
        Yaml::Number(n) => ports.push_u64(n.as_u64()),
        Yaml::String(s) => {
            let s = s.split('/').next().unwrap_or(s);
            ports.push_str(s.rsplit(':').next().unwrap_or(s));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, contents: &str) -> PathBuf {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, contents).unwrap();
        p
    }

    fn detect_with(rel: &str, json: &str) -> Detected {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), rel, json);
        detect(tmp.path()).unwrap()
    }

    #[test]
    fn no_config() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(detect(tmp.path()).unwrap(), Detected::default());
        // An empty .devcontainer dir is also "no config".
        fs::create_dir(tmp.path().join(".devcontainer")).unwrap();
        assert_eq!(detect(tmp.path()).unwrap().kind, DevcontainerKind::None);
    }

    #[test]
    fn image_kind() {
        let d = detect_with(
            ".devcontainer/devcontainer.json",
            r#"{"name":"App","image":"x"}"#,
        );
        assert_eq!(d.kind, DevcontainerKind::Image);
        assert_eq!(d.name.as_deref(), Some("App"));
        assert_eq!(d.service, None);
        assert!(d.ports.is_empty());
    }

    #[test]
    fn dockerfile_kind() {
        let d = detect_with(
            ".devcontainer/devcontainer.json",
            r#"{"build":{"dockerfile":"Dockerfile"},"image":"ignored"}"#,
        );
        assert_eq!(d.kind, DevcontainerKind::Dockerfile);
        let d = detect_with(
            ".devcontainer/devcontainer.json",
            r#"{"dockerFile":"Dockerfile"}"#,
        );
        assert_eq!(d.kind, DevcontainerKind::Dockerfile);
    }

    #[test]
    fn compose_kind_wins() {
        let d = detect_with(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"missing.yml","service":"app","image":"x","build":{"dockerfile":"D"}}"#,
        );
        assert_eq!(d.kind, DevcontainerKind::Compose);
        assert_eq!(d.service.as_deref(), Some("app"));
        // Missing compose file is not an error.
        assert!(d.ports.is_empty());
    }

    #[test]
    fn service_only_for_compose() {
        let d = detect_with(
            ".devcontainer/devcontainer.json",
            r#"{"image":"x","service":"app"}"#,
        );
        assert_eq!(d.service, None);
    }

    #[test]
    fn config_without_kind_keys_is_error() {
        let tmp = TempDir::new().unwrap();
        let p = write(tmp.path(), ".devcontainer.json", r#"{"name":"nothing"}"#);
        match detect(tmp.path()) {
            Err(Error::Config { path, message }) => {
                assert_eq!(path, p);
                assert!(
                    message.contains("no image, build or dockerComposeFile"),
                    "{message}"
                );
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_config_error() {
        let tmp = TempDir::new().unwrap();
        let p = write(
            tmp.path(),
            ".devcontainer/devcontainer.json",
            r#"{"image": "x""#,
        );
        match detect(tmp.path()) {
            Err(Error::Config { path, .. }) => assert_eq!(path, p),
            other => panic!("expected Config error, got {other:?}"),
        }
        let tmp = TempDir::new().unwrap();
        let p = write(tmp.path(), ".devcontainer.json", "[1, 2]");
        match detect(tmp.path()) {
            Err(Error::Config { path, .. }) => assert_eq!(path, p),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn lookup_order() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let b = write(
            dir,
            ".devcontainer/b/devcontainer.json",
            r#"{"name":"b","image":"x"}"#,
        );
        write(
            dir,
            ".devcontainer/c/devcontainer.json",
            r#"{"name":"c","image":"x"}"#,
        );
        write(
            dir,
            ".devcontainer/a/other.json",
            r#"{"name":"a","image":"x"}"#,
        );
        let d = detect(dir).unwrap();
        assert_eq!(d.name.as_deref(), Some("b"));
        assert_eq!(d.config_path, Some(b));

        let root = write(dir, ".devcontainer.json", r#"{"name":"root","image":"x"}"#);
        let d = detect(dir).unwrap();
        assert_eq!(d.name.as_deref(), Some("root"));
        assert_eq!(d.config_path, Some(root));

        let main = write(
            dir,
            ".devcontainer/devcontainer.json",
            r#"{"name":"main","image":"x"}"#,
        );
        let d = detect(dir).unwrap();
        assert_eq!(d.name.as_deref(), Some("main"));
        assert_eq!(d.config_path, Some(main));
    }

    #[test]
    fn jsonc_comments_and_trailing_commas() {
        let d = detect_with(
            ".devcontainer/devcontainer.json",
            r#"
            // leading comment
            {
                /* block */ "name": "J", // trailing
                "image": "x",
                "forwardPorts": [3000, 4000,],
            }
            "#,
        );
        assert_eq!(d.kind, DevcontainerKind::Image);
        assert_eq!(d.name.as_deref(), Some("J"));
        assert_eq!(d.ports, vec![3000, 4000]);
    }

    #[test]
    fn devcontainer_ports_all_forms() {
        let d = detect_with(
            ".devcontainer/devcontainer.json",
            r#"{
                "image": "x",
                "forwardPorts": [3000, "4000", "db:5432", 3000, 0, 70000, "nope", true],
                "appPort": [8000, "9000", "8080:80", "127.0.0.1:8443:443", 4000],
                "portsAttributes": {
                    "5000": {}, "6000-6010": {}, ".+\\/server.js": {}, "3000": {}
                }
            }"#,
        );
        assert_eq!(d.ports, vec![3000, 4000, 8000, 9000, 80, 443, 5000]);
    }

    #[test]
    fn app_port_scalar_forms() {
        let d = detect_with(".devcontainer.json", r#"{"image":"x","appPort":7000}"#);
        assert_eq!(d.ports, vec![7000]);
        let d = detect_with(".devcontainer.json", r#"{"image":"x","appPort":"7001"}"#);
        assert_eq!(d.ports, vec![7001]);
        let d = detect_with(".devcontainer.json", r#"{"image":"x","appPort":"7002:81"}"#);
        assert_eq!(d.ports, vec![81]);
    }

    #[test]
    fn forward_ports_service_prefix_matches_compose_service() {
        let d = detect_with(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"none.yml","service":"app",
                "forwardPorts":["db:5432","app:3000",8000]}"#,
        );
        assert_eq!(d.ports, vec![3000, 8000]);
    }

    #[test]
    fn compose_ports_from_multiple_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write(
            dir,
            ".devcontainer/devcontainer.json",
            r#"{
                "name": "C",
                "dockerComposeFile": ["../docker-compose.yml", "docker-compose.extend.yml", "missing.yml", "bad.yml"],
                "service": "app",
                "forwardPorts": [9999]
            }"#,
        );
        write(
            dir,
            "docker-compose.yml",
            r#"
services:
  app:
    image: x
    ports:
      - "3000"
      - "8080:80"
      - "127.0.0.1:8443:443"
      - "53/udp"
      - "9000-9005:9000-9005"
      - 4000
      - target: 5000
        published: 15000
      - target: "5001"
      - "[::1]:6060:6061"
    expose:
      - "7000"
      - 7001
      - "7002/tcp"
      - "7100-7200"
  db:
    image: postgres
    ports:
      - "5432:5432"
"#,
        );
        write(
            dir,
            ".devcontainer/docker-compose.extend.yml",
            r#"
services:
  app:
    ports:
      - "3000"
      - "8000:8001"
    expose:
      - "99999"
"#,
        );
        write(dir, ".devcontainer/bad.yml", "services: [unclosed");
        let d = detect(dir).unwrap();
        assert_eq!(d.kind, DevcontainerKind::Compose);
        assert_eq!(d.service.as_deref(), Some("app"));
        assert_eq!(
            d.ports,
            vec![
                9999, 3000, 80, 443, 53, 4000, 5000, 5001, 6061, 7000, 7001, 7002, 8001
            ]
        );
    }

    #[test]
    fn this_repository() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let d = detect(&root).unwrap();
        assert_eq!(d.kind, DevcontainerKind::Image);
        assert_eq!(d.name.as_deref(), Some("Rust"));
        assert!(
            d.config_path
                .unwrap()
                .ends_with(".devcontainer/devcontainer.json")
        );
    }

    #[test]
    fn reads_the_zevo_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".devcontainer")).unwrap();
        std::fs::write(
            tmp.path().join(".devcontainer/devcontainer.json"),
            r#"{
              "image": "node",
              "customizations": {
                "vscode": { "extensions": [] },
                "zevo": {
                  "name": "Kindly", "slug": "kindly",
                  "routes": [
                    { "port": 3001, "backend": "https" },
                    { "subdomains": ["donate", "funds"], "port": 1355, "backend": "https" },
                    { "subdomain": "api", "port": 8000, "pathPrefix": "/v1" },
                    { "host": "Local.Dash.Example.ai", "port": 4000 },
                  ]
                }
              }
            }"#,
        )
        .unwrap();
        let d = detect(tmp.path()).unwrap().declared.unwrap();
        assert_eq!(
            (d.name.as_deref(), d.slug.as_deref()),
            (Some("Kindly"), Some("kindly"))
        );
        let names: Vec<String> = d
            .routes
            .iter()
            .map(|r| r.hostname("kindly", "localhost"))
            .collect();
        assert_eq!(
            names,
            [
                "kindly.localhost",
                "donate.kindly.localhost",
                "funds.kindly.localhost",
                "api.kindly.localhost",
                "local.dash.example.ai"
            ]
        );
        assert_eq!(d.routes[1].backend, Backend::Https);
        assert_eq!(d.routes[1].port, 1355);
        assert_eq!(d.routes[3].path_prefix.as_deref(), Some("/v1"));
        assert_eq!(d.routes[3].backend, Backend::Http);
    }

    #[test]
    fn rejects_a_bad_declaration_with_a_useful_message() {
        let tmp = tempfile::tempdir().unwrap();
        let write = |zevo: &str| {
            std::fs::write(
                tmp.path().join(".devcontainer.json"),
                format!(r#"{{"image":"node","customizations":{{"zevo":{zevo}}}}}"#),
            )
            .unwrap();
            detect(tmp.path()).unwrap_err().to_string()
        };
        assert!(write(r#"{"routes":[{"port":3000,"subdomian":"api"}]}"#).contains("subdomian"));
        assert!(write(r#"{"routes":[{"port":3000,"subdomain":"Bad_Name"}]}"#).contains("Bad_Name"));
        assert!(
            write(r#"{"routes":[{"port":3000,"host":"a.b","subdomain":"x"}]}"#)
                .contains("either host or subdomain")
        );
        assert!(
            write(r#"{"routes":[{"port":3000,"pathPrefix":"v1"}]}"#).contains("must start with")
        );
        assert!(write(r#"{"routes":[{"subdomain":"api"}]}"#).contains("port"));
    }
}
