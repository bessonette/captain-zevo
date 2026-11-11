//! Domain types shared by every module. These are also the on-disk TOML shapes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const DEFAULT_NETWORK: &str = "zevo";
pub const DEFAULT_DOMAIN_SUFFIX: &str = "localhost";
pub const LOCAL_HOST_ID: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Projects are served at `<slug>.<domain_suffix>`.
    #[serde(default = "default_suffix")]
    pub domain_suffix: String,
    /// Docker network shared by Traefik and project containers.
    #[serde(default = "default_network")]
    pub network: String,
    /// `None` until `zevo setup` has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traefik: Option<TraefikSettings>,
}

fn default_suffix() -> String {
    DEFAULT_DOMAIN_SUFFIX.into()
}
fn default_network() -> String {
    DEFAULT_NETWORK.into()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            domain_suffix: default_suffix(),
            network: default_network(),
            traefik: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraefikMode {
    /// A Traefik the user already ran; Zevo only writes route files.
    Adopted,
    /// The `zevo-traefik` container Zevo created.
    Managed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraefikSettings {
    pub mode: TraefikMode,
    /// Container name, when Traefik runs in a container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// File-provider directory as seen from where Zevo runs.
    pub dynamic_dir: PathBuf,
    /// Traefik entrypoint name routes bind to (e.g. `web`).
    pub entrypoint: String,
    /// Host port of that entrypoint, used to build URLs.
    pub http_port: u16,
    /// Whether that entrypoint serves HTTPS (routes then get `tls: {}`).
    #[serde(default)]
    pub tls: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DevcontainerKind {
    Image,
    Dockerfile,
    Compose,
    /// No devcontainer.json found.
    #[default]
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Target {
    /// Reached over the shared network by alias `<slug>`.
    #[default]
    Container,
    /// Reached at a fixed address (remote host or published port).
    HostPort { address: String },
}

/// How Traefik talks to the app behind a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Http,
    /// TLS without certificate checks (dev servers use self-signed
    /// certificates), sending the route's hostname as SNI.
    Https,
}

impl Backend {
    fn is_http(&self) -> bool {
        *self == Backend::Http
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// `None` serves `<slug>.<suffix>`; `Some("api")` serves `api.<slug>.<suffix>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdomain: Option<String>,
    /// Port the app listens on at the target.
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub target: Target,
    #[serde(default, skip_serializing_if = "Backend::is_http")]
    pub backend: Backend,
}

impl Route {
    pub fn hostname(&self, slug: &str, suffix: &str) -> String {
        match &self.subdomain {
            Some(sub) => format!("{sub}.{slug}.{suffix}"),
            None => format!("{slug}.{suffix}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Unique id; also the network alias and hostname label.
    pub slug: String,
    pub name: String,
    /// Host id; `local` for this machine.
    #[serde(default = "default_host")]
    pub host: String,
    /// Project folder on that host.
    pub path: PathBuf,
    #[serde(default)]
    pub kind: DevcontainerKind,
    /// Compose service that receives the network alias (compose projects only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

fn default_host() -> String {
    LOCAL_HOST_ID.into()
}

/// Checks that `s` is usable as a DNS label and Docker network alias.
pub fn validate_slug(s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidSlug(s.to_string()))
    }
}

/// Derives a slug from a free-form name such as a folder name.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    out.chars()
        .take(63)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_folder_names() {
        assert_eq!(slugify("My Project_2"), "my-project-2");
        assert_eq!(slugify("--weird--"), "weird");
        assert!(validate_slug(&slugify("captain-zevo")).is_ok());
        assert!(validate_slug("Bad_Name").is_err());
        assert!(validate_slug("").is_err());
    }

    #[test]
    fn hostnames() {
        let r = Route {
            backend: Backend::Http,
            subdomain: None,
            port: 3000,
            path_prefix: None,
            target: Target::Container,
        };
        assert_eq!(r.hostname("shop", "localhost"), "shop.localhost");
        let r = Route {
            backend: Backend::Http,
            subdomain: Some("api".into()),
            ..r
        };
        assert_eq!(r.hostname("shop", "localhost"), "api.shop.localhost");
    }
}
