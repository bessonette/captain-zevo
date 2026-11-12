//! Access to a Docker-compatible container runtime.
//!
//! Everything else in the crate talks to the [`Runtime`] trait, so logic can be
//! tested against [`fake::FakeRuntime`] without a daemon.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::Result;

mod docker;
pub mod fake;

pub use docker::DockerRuntime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// Path on the host (empty for anonymous volumes).
    pub source: String,
    /// Path inside the container.
    pub destination: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    pub running: bool,
    pub labels: BTreeMap<String, String>,
    /// Entrypoint arguments followed by the command.
    pub args: Vec<String>,
    /// `KEY=value` pairs.
    pub env: Vec<String>,
    pub mounts: Vec<Mount>,
    /// Network name to the aliases this container has on it.
    pub networks: BTreeMap<String, Vec<String>>,
    /// Container port to published host port (TCP only).
    pub published_ports: BTreeMap<u16, u16>,
    pub host_network: bool,
}

/// What `zevo setup` needs to start its own Traefik.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraefikSpec {
    pub container: String,
    pub image: String,
    pub network: String,
    /// Host directory mounted as the file-provider directory.
    pub dynamic_dir: PathBuf,
    /// Host port published for the `web` entrypoint.
    pub http_port: u16,
    /// Host port for the HTTPS `websecure` entrypoint. When set, `web`
    /// redirects everything to it.
    pub https_port: Option<u16>,
}

impl TraefikSpec {
    /// Command-line arguments for the Traefik container.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--providers.file.directory=/etc/traefik/dynamic".to_string(),
            "--providers.file.watch=true".to_string(),
            "--entrypoints.web.address=:80".to_string(),
        ];
        if let Some(port) = self.https_port {
            args.extend([
                "--entrypoints.websecure.address=:443".to_string(),
                // A port rather than the entrypoint name, so the redirect is
                // right even when 443 is published on another host port.
                format!("--entrypoints.web.http.redirections.entrypoint.to=:{port}"),
                "--entrypoints.web.http.redirections.entrypoint.scheme=https".to_string(),
            ]);
        }
        args
    }
}

/// The edge Traefik Zevo runs on a remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeSpec {
    pub container: String,
    pub image: String,
    pub network: String,
    /// Volume holding the route files.
    pub volume: String,
    /// Host port published for its `web` entrypoint.
    pub port: u16,
}

/// Directory inside the edge container where route files live.
pub const EDGE_ROUTES_DIR: &str = "/etc/traefik/dynamic";

#[async_trait]
pub trait Runtime: Send + Sync {
    /// Returns a short description of the runtime (name and version).
    async fn ping(&self) -> Result<String>;
    /// All containers, running or not, with full detail.
    async fn containers(&self) -> Result<Vec<ContainerInfo>>;
    /// Creates the network if missing. Returns true if it was created.
    async fn ensure_network(&self, name: &str) -> Result<bool>;
    async fn connect(&self, container: &str, network: &str, aliases: &[String]) -> Result<()>;
    async fn disconnect(&self, container: &str, network: &str) -> Result<()>;
    /// Runs a command in a running container and returns what it printed.
    async fn exec(&self, container: &str, command: &[&str]) -> Result<String>;
    /// Makes sure the edge Traefik exists and is running. Returns true if it
    /// had to be created or started.
    async fn ensure_edge(&self, spec: &EdgeSpec) -> Result<bool>;
    /// Removes the edge Traefik and its routes volume, if Zevo created them.
    async fn remove_edge(&self, container: &str, volume: &str) -> Result<()>;
    /// Text of a file inside a running container, or `None` if it is missing.
    async fn read_file(&self, container: &str, path: &str) -> Result<Option<String>>;
    async fn write_file(&self, container: &str, path: &str, content: &str) -> Result<()>;
    async fn remove_file(&self, container: &str, path: &str) -> Result<()>;
    /// File names in a directory inside a running container.
    async fn list_dir(&self, container: &str, dir: &str) -> Result<Vec<String>>;
    /// Pulls the image if needed, then creates and starts the Traefik container.
    async fn run_traefik(&self, spec: &TraefikSpec) -> Result<()>;
}
