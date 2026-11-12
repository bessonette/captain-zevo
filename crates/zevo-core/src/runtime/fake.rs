//! In-memory [`Runtime`] for tests and UI previews.

use std::sync::Mutex;

use async_trait::async_trait;

use super::{ContainerInfo, Mount, Runtime, TraefikSpec};
use crate::{Error, Result};

#[derive(Debug, Default)]
pub struct FakeRuntime {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    containers: Vec<ContainerInfo>,
    networks: Vec<String>,
    /// When set, every call fails with this message.
    down: Option<String>,
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_container(&self, container: ContainerInfo) {
        self.state.lock().unwrap().containers.push(container);
    }

    pub fn remove_container(&self, name: &str) {
        self.state
            .lock()
            .unwrap()
            .containers
            .retain(|c| c.name != name);
    }

    pub fn container(&self, name: &str) -> Option<ContainerInfo> {
        self.state
            .lock()
            .unwrap()
            .containers
            .iter()
            .find(|c| c.name == name)
            .cloned()
    }

    pub fn networks(&self) -> Vec<String> {
        self.state.lock().unwrap().networks.clone()
    }

    /// Makes every later call fail, as if the daemon were unreachable.
    pub fn set_down(&self, message: &str) {
        self.state.lock().unwrap().down = Some(message.to_string());
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut state = self.state.lock().unwrap();
        if let Some(message) = &state.down {
            return Err(Error::Runtime(message.clone()));
        }
        f(&mut state)
    }
}

fn find<'a>(state: &'a mut State, container: &str) -> Result<&'a mut ContainerInfo> {
    state
        .containers
        .iter_mut()
        .find(|c| c.id == container || c.name == container)
        .ok_or_else(|| Error::Runtime(format!("no such container: {container}")))
}

#[async_trait]
impl Runtime for FakeRuntime {
    async fn ping(&self) -> Result<String> {
        self.with(|_| Ok("fake runtime".to_string()))
    }

    async fn containers(&self) -> Result<Vec<ContainerInfo>> {
        self.with(|s| Ok(s.containers.clone()))
    }

    async fn ensure_network(&self, name: &str) -> Result<bool> {
        self.with(|s| {
            if s.networks.iter().any(|n| n == name) {
                return Ok(false);
            }
            s.networks.push(name.to_string());
            Ok(true)
        })
    }

    async fn connect(&self, container: &str, network: &str, aliases: &[String]) -> Result<()> {
        self.with(|s| {
            if !s.networks.iter().any(|n| n == network) {
                return Err(Error::Runtime(format!("network {network} not found")));
            }
            let c = find(s, container)?;
            if c.networks.contains_key(network) {
                return Err(Error::Runtime(format!("already connected to {network}")));
            }
            c.networks.insert(network.to_string(), aliases.to_vec());
            Ok(())
        })
    }

    async fn disconnect(&self, container: &str, network: &str) -> Result<()> {
        self.with(|s| {
            find(s, container)?.networks.remove(network);
            Ok(())
        })
    }

    async fn run_traefik(&self, spec: &TraefikSpec) -> Result<()> {
        self.with(|s| {
            if s.containers.iter().any(|c| c.name == spec.container) {
                return Err(Error::Runtime(format!(
                    "container {} already exists",
                    spec.container
                )));
            }
            s.containers.push(ContainerInfo {
                id: format!("id-{}", spec.container),
                name: spec.container.clone(),
                image: spec.image.clone(),
                running: true,
                args: spec.args(),
                mounts: vec![Mount {
                    source: spec.dynamic_dir.to_string_lossy().into_owned(),
                    destination: "/etc/traefik/dynamic".into(),
                }],
                networks: [(spec.network.clone(), Vec::new())].into(),
                published_ports: std::iter::once((80, spec.http_port))
                    .chain(spec.https_port.map(|port| (443, port)))
                    .collect(),
                ..ContainerInfo::default()
            });
            Ok(())
        })
    }
}
