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
    /// Editor that "Open in editor" uses; the first installed one if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<crate::ide::Editor>,
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
            editor: None,
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
    /// The file-provider directory as Traefik itself sees it, when that
    /// differs from `dynamic_dir` (Traefik in a container). Needed to tell
    /// Traefik where certificates are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_dir: Option<String>,
}

impl TraefikSettings {
    /// Path of a file in the route directory, as Traefik sees it.
    pub fn path_in_traefik(&self, relative: &str) -> String {
        match &self.container_dir {
            Some(dir) => format!("{}/{relative}", dir.trim_end_matches('/')),
            None => self
                .dynamic_dir
                .join(relative)
                .to_string_lossy()
                .into_owned(),
        }
    }
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
    /// Exact hostname, for addresses outside `<slug>.<suffix>`. Overrides
    /// `subdomain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

impl Route {
    pub fn hostname(&self, slug: &str, suffix: &str) -> String {
        if let Some(host) = &self.host {
            return host.to_lowercase();
        }
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
    /// Routes come from `customizations.zevo` in the project's
    /// devcontainer.json and are re-read from there on every reconcile.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub declared: bool,
    /// Reach the project at this address instead of its container (another
    /// machine, or a port forwarded to this one). Applies to declared routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// File, relative to the project folder, where Zevo writes the project's
    /// addresses as environment variables. Set by the project's declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_file: Option<String>,
    /// Declared routes this checkout leaves out, named by subdomain or exact
    /// hostname. For routes that only make sense in one checkout of a
    /// repository that is registered more than once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<String>,
}

impl Project {
    /// Drops the routes named in `skip`.
    pub fn apply_skip(&mut self) {
        let skip = &self.skip;
        self.routes.retain(|r| {
            let named = |name: &Option<String>| {
                name.as_ref()
                    .is_some_and(|n| skip.iter().any(|s| s.eq_ignore_ascii_case(n)))
            };
            !(named(&r.subdomain) || named(&r.host))
        });
    }
}

fn default_host() -> String {
    LOCAL_HOST_ID.into()
}

/// Port the edge Traefik on a remote host listens on, unless changed.
pub const DEFAULT_EDGE_PORT: u16 = 8480;

/// Another machine that runs project containers, reached over SSH.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// Unique id, used in project files.
    pub id: String,
    pub name: String,
    /// What to pass to `ssh`: `user@address` or a name from `~/.ssh/config`.
    pub ssh: String,
    /// Address this machine's Traefik uses to reach the host.
    pub address: String,
    /// Docker socket on the host, if not `/var/run/docker.sock`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_socket: Option<String>,
    /// Command that runs Docker on the host, for setups where it is reached
    /// through a wrapper (a script, `sudo -n docker`) and not a socket this
    /// user can open. Takes precedence over `docker_socket`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_command: Option<String>,
    /// Port of the host's edge Traefik.
    #[serde(default = "default_edge_port")]
    pub edge_port: u16,
}

fn default_edge_port() -> u16 {
    DEFAULT_EDGE_PORT
}

impl Host {
    /// Builds a host from an SSH destination, deriving the rest.
    pub fn from_ssh(ssh: &str, name: Option<&str>, address: Option<&str>) -> Result<Host> {
        let ssh = ssh.trim();
        let plain = !ssh.is_empty()
            && !ssh.starts_with('-')
            && ssh
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'.' | b'-' | b'_'));
        if !plain {
            return Err(Error::Other(format!(
                "'{ssh}' is not a valid SSH destination (use user@address or a name from your SSH \
                 config; a non-standard port belongs in the SSH config)"
            )));
        }
        let machine = ssh.rsplit('@').next().unwrap_or(ssh);
        let name = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or(machine)
            .to_string();
        let id = slugify(&name);
        validate_slug(&id)?;
        if id == LOCAL_HOST_ID {
            return Err(Error::Other(
                "'local' is reserved for this machine; choose another name".into(),
            ));
        }
        Ok(Host {
            id,
            name,
            ssh: ssh.to_string(),
            address: address
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .unwrap_or(machine)
                .to_string(),
            docker_socket: None,
            docker_command: None,
            edge_port: DEFAULT_EDGE_PORT,
        })
    }
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
    fn hosts_from_ssh_destinations() {
        let h = Host::from_ssh("dev@192.168.1.20", None, None).unwrap();
        assert_eq!(
            (h.id.as_str(), h.address.as_str()),
            ("192-168-1-20", "192.168.1.20")
        );
        let h = Host::from_ssh("buildbox", Some("Build Box"), Some("10.0.0.5")).unwrap();
        assert_eq!(
            (h.id.as_str(), h.name.as_str(), h.address.as_str()),
            ("build-box", "Build Box", "10.0.0.5")
        );
        assert_eq!(h.edge_port, DEFAULT_EDGE_PORT);
        // Nothing that ssh could read as an option or a command.
        assert!(Host::from_ssh("-oProxyCommand=evil", None, None).is_err());
        assert!(Host::from_ssh("host; rm -rf /", None, None).is_err());
        assert!(Host::from_ssh("me@box", Some("local"), None).is_err());
        assert!(Host::from_ssh("dev@box:2222", None, None).is_err());
    }

    #[test]
    fn hostnames() {
        let r = Route {
            backend: Backend::Http,
            host: None,
            subdomain: None,
            port: 3000,
            path_prefix: None,
            target: Target::Container,
        };
        assert_eq!(r.hostname("shop", "localhost"), "shop.localhost");
        let r = Route {
            backend: Backend::Http,
            host: None,
            subdomain: Some("api".into()),
            ..r
        };
        assert_eq!(r.hostname("shop", "localhost"), "api.shop.localhost");
    }
}
