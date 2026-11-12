//! [`Runtime`] backed by the Docker Engine API (Docker, Podman, OrbStack,
//! Colima and Rancher Desktop all serve it).

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use bollard::Docker;
use bollard::models::{
    ContainerCreateBody, ContainerInspectResponse, EndpointSettings, HostConfig,
    NetworkConnectRequest, NetworkCreateRequest, NetworkDisconnectRequest, NetworkingConfig,
    PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
};
use futures_util::TryStreamExt;

use super::{ContainerInfo, Mount, Runtime, TraefikSpec};
use crate::{Error, Result};

pub struct DockerRuntime {
    docker: Docker,
}

impl DockerRuntime {
    /// Connects using `DOCKER_HOST` if set, otherwise the platform's default
    /// socket or named pipe. Connection errors surface on first use.
    pub fn connect() -> Result<Self> {
        Docker::connect_with_defaults()
            .map(|docker| Self { docker })
            .map_err(err)
    }
}

fn err(e: bollard::errors::Error) -> Error {
    Error::Runtime(e.to_string())
}

fn is_status(e: &bollard::errors::Error, code: u16) -> bool {
    matches!(e, bollard::errors::Error::DockerResponseServerError { status_code, .. } if *status_code == code)
}

fn to_info(c: ContainerInspectResponse) -> ContainerInfo {
    let config = c.config.unwrap_or_default();
    let host_config = c.host_config.unwrap_or_default();
    let network_settings = c.network_settings.unwrap_or_default();

    // `path` + `args` is the resolved entrypoint and command.
    let mut args = Vec::new();
    args.extend(c.path);
    args.extend(c.args.unwrap_or_default());

    let mut published_ports = BTreeMap::new();
    for (key, bindings) in network_settings.ports.unwrap_or_default() {
        let Some((port, proto)) = key.split_once('/') else {
            continue;
        };
        if proto != "tcp" {
            continue;
        }
        let host_port = bindings
            .into_iter()
            .flatten()
            .find_map(|b| b.host_port.and_then(|p| p.parse::<u16>().ok()));
        if let (Ok(port), Some(host_port)) = (port.parse::<u16>(), host_port) {
            published_ports.insert(port, host_port);
        }
    }

    ContainerInfo {
        id: c.id.unwrap_or_default(),
        name: c
            .name
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string(),
        image: config.image.unwrap_or_default(),
        running: c.state.and_then(|s| s.running).unwrap_or(false),
        labels: config.labels.unwrap_or_default().into_iter().collect(),
        args,
        env: config.env.unwrap_or_default(),
        mounts: c
            .mounts
            .unwrap_or_default()
            .into_iter()
            .map(|m| Mount {
                source: m.source.unwrap_or_default(),
                destination: m.destination.unwrap_or_default(),
            })
            .collect(),
        networks: network_settings
            .networks
            .unwrap_or_default()
            .into_iter()
            .map(|(name, endpoint)| (name, endpoint.aliases.unwrap_or_default()))
            .collect(),
        published_ports,
        host_network: host_config.network_mode.as_deref() == Some("host"),
    }
}

#[async_trait]
impl Runtime for DockerRuntime {
    async fn ping(&self) -> Result<String> {
        let v = self.docker.version().await.map_err(err)?;
        let name = v
            .components
            .and_then(|c| c.into_iter().next())
            .map(|c| c.name)
            .unwrap_or_else(|| "Docker".to_string());
        Ok(format!("{name} {}", v.version.unwrap_or_default()))
    }

    async fn containers(&self) -> Result<Vec<ContainerInfo>> {
        let options = ListContainersOptionsBuilder::default().all(true).build();
        let summaries = self
            .docker
            .list_containers(Some(options))
            .await
            .map_err(err)?;
        let mut out = Vec::with_capacity(summaries.len());
        for summary in summaries {
            let Some(id) = summary.id else { continue };
            match self.docker.inspect_container(&id, None).await {
                Ok(detail) => out.push(to_info(detail)),
                // Removed between list and inspect.
                Err(e) if is_status(&e, 404) => {}
                Err(e) => return Err(err(e)),
            }
        }
        Ok(out)
    }

    async fn ensure_network(&self, name: &str) -> Result<bool> {
        match self.docker.inspect_network(name, None).await {
            Ok(_) => return Ok(false),
            Err(e) if is_status(&e, 404) => {}
            Err(e) => return Err(err(e)),
        }
        let request = NetworkCreateRequest {
            name: name.to_string(),
            ..Default::default()
        };
        self.docker.create_network(request).await.map_err(err)?;
        Ok(true)
    }

    async fn connect(&self, container: &str, network: &str, aliases: &[String]) -> Result<()> {
        let request = NetworkConnectRequest {
            container: container.to_string(),
            endpoint_config: Some(EndpointSettings {
                aliases: Some(aliases.to_vec()),
                ..Default::default()
            }),
        };
        self.docker
            .connect_network(network, request)
            .await
            .map_err(err)
    }

    async fn disconnect(&self, container: &str, network: &str) -> Result<()> {
        let request = NetworkDisconnectRequest {
            container: container.to_string(),
            force: None,
        };
        self.docker
            .disconnect_network(network, request)
            .await
            .map_err(err)
    }

    async fn run_traefik(&self, spec: &TraefikSpec) -> Result<()> {
        let pull = CreateImageOptionsBuilder::default()
            .from_image(&spec.image)
            .build();
        self.docker
            .create_image(Some(pull), None, None)
            .try_collect::<Vec<_>>()
            .await
            .map_err(err)?;

        let dynamic_dir = spec.dynamic_dir.to_string_lossy();
        // (container port, host port)
        let mut ports = vec![(80u16, spec.http_port)];
        ports.extend(spec.https_port.map(|port| (443, port)));
        let body = ContainerCreateBody {
            image: Some(spec.image.clone()),
            cmd: Some(spec.args()),
            labels: Some(HashMap::from([(
                "dev.captain-zevo.managed".to_string(),
                "true".to_string(),
            )])),
            exposed_ports: Some(
                ports
                    .iter()
                    .map(|(inner, _)| format!("{inner}/tcp"))
                    .collect(),
            ),
            host_config: Some(HostConfig {
                binds: Some(vec![format!("{dynamic_dir}:/etc/traefik/dynamic:ro")]),
                port_bindings: Some(
                    ports
                        .iter()
                        .map(|(inner, host)| {
                            (
                                format!("{inner}/tcp"),
                                Some(vec![PortBinding {
                                    host_ip: None,
                                    host_port: Some(host.to_string()),
                                }]),
                            )
                        })
                        .collect(),
                ),
                restart_policy: Some(RestartPolicy {
                    name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: None,
                }),
                ..Default::default()
            }),
            networking_config: Some(NetworkingConfig {
                endpoints_config: Some(HashMap::from([(
                    spec.network.clone(),
                    EndpointSettings::default(),
                )])),
            }),
            ..Default::default()
        };
        let options = CreateContainerOptionsBuilder::default()
            .name(&spec.container)
            .build();
        self.docker
            .create_container(Some(options), body)
            .await
            .map_err(err)?;
        self.docker
            .start_container(&spec.container, None)
            .await
            .map_err(err)
    }
}
