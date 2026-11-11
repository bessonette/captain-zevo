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
}

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
    /// Pulls the image if needed, then creates and starts the Traefik container.
    async fn run_traefik(&self, spec: &TraefikSpec) -> Result<()>;
}
