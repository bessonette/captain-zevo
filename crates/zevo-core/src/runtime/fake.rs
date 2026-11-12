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
    /// (container name, path) to content, for the file operations.
    files: std::collections::BTreeMap<(String, String), String>,
    /// Container name to its log output.
    logs: std::collections::BTreeMap<String, String>,
    /// Container name to what `exec` prints in it.
    exec_output: std::collections::BTreeMap<String, String>,
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

    /// Sets the container's log output.
    pub fn set_logs(&self, container: &str, output: &str) {
        self.state
            .lock()
            .unwrap()
            .logs
            .insert(container.to_string(), output.to_string());
    }

    /// Sets what any command run in the container prints.
    pub fn set_exec_output(&self, container: &str, output: &str) {
        self.state
            .lock()
            .unwrap()
            .exec_output
            .insert(container.to_string(), output.to_string());
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

    async fn start(&self, container: &str) -> Result<()> {
        self.with(|s| {
            find(s, container)?.running = true;
            Ok(())
        })
    }

    async fn stop(&self, container: &str) -> Result<()> {
        self.with(|s| {
            find(s, container)?.running = false;
            Ok(())
        })
    }

    async fn logs(&self, container: &str, lines: usize) -> Result<String> {
        self.with(|s| {
            let name = find(s, container)?.name.clone();
            let all = s.logs.get(&name).cloned().unwrap_or_default();
            let kept: Vec<&str> = all.lines().collect();
            let tail = &kept[kept.len().saturating_sub(lines)..];
            Ok(tail.iter().map(|l| format!("{l}\n")).collect())
        })
    }

    async fn exec(&self, container: &str, _command: &[&str]) -> Result<String> {
        self.with(|s| {
            let name = find(s, container)?.name.clone();
            s.exec_output
                .get(&name)
                .cloned()
                .ok_or_else(|| Error::Runtime(format!("cannot run commands in {name}")))
        })
    }

    async fn ensure_edge(&self, spec: &super::EdgeSpec) -> Result<bool> {
        self.with(|s| {
            if let Some(existing) = s.containers.iter_mut().find(|c| c.name == spec.container) {
                existing.networks.entry(spec.network.clone()).or_default();
                let was_running = std::mem::replace(&mut existing.running, true);
                return Ok(!was_running);
            }
            if !s.networks.iter().any(|n| n == &spec.network) {
                return Err(Error::Runtime(format!(
                    "network {} not found",
                    spec.network
                )));
            }
            s.containers.push(ContainerInfo {
                id: format!("id-{}", spec.container),
                name: spec.container.clone(),
                image: spec.image.clone(),
                running: true,
                networks: [(spec.network.clone(), Vec::new())].into(),
                published_ports: [(80, spec.port)].into(),
                ..ContainerInfo::default()
            });
            Ok(true)
        })
    }

    async fn remove_edge(&self, container: &str, _volume: &str) -> Result<()> {
        self.with(|s| {
            s.containers.retain(|c| c.name != container);
            s.files.retain(|(c, _), _| c != container);
            Ok(())
        })
    }

    async fn read_file(&self, container: &str, path: &str) -> Result<Option<String>> {
        self.with(|s| {
            let name = find(s, container)?.name.clone();
            Ok(s.files.get(&(name, path.to_string())).cloned())
        })
    }

    async fn write_file(&self, container: &str, path: &str, content: &str) -> Result<()> {
        self.with(|s| {
            let name = find(s, container)?.name.clone();
            s.files
                .insert((name, path.to_string()), content.to_string());
            Ok(())
        })
    }

    async fn remove_file(&self, container: &str, path: &str) -> Result<()> {
        self.with(|s| {
            let name = find(s, container)?.name.clone();
            s.files.remove(&(name, path.to_string()));
            Ok(())
        })
    }

    async fn list_dir(&self, container: &str, dir: &str) -> Result<Vec<String>> {
        self.with(|s| {
            let name = find(s, container)?.name.clone();
            let prefix = format!("{}/", dir.trim_end_matches('/'));
            Ok(s.files
                .keys()
                .filter(|(c, _)| *c == name)
                .filter_map(|(_, path)| path.strip_prefix(&prefix))
                .filter(|rest| !rest.contains('/'))
                .map(str::to_string)
                .collect())
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
