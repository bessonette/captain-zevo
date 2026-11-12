//! Environment checks behind `zevo doctor`.

use std::net::ToSocketAddrs;

use serde::Serialize;

use crate::runtime::Runtime;
use crate::setup::port_in_use;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
}

fn check(name: &'static str, level: Level, detail: impl Into<String>) -> Check {
    Check {
        name,
        level,
        detail: detail.into(),
    }
}

/// Runs every check; never fails as a whole.
pub async fn run(store: &Store, runtime: Option<&dyn Runtime>) -> Vec<Check> {
    let mut checks = Vec::new();

    let settings = match store.settings() {
        Ok(settings) => {
            checks.push(check(
                "config",
                Level::Ok,
                store.dir().display().to_string(),
            ));
            Some(settings)
        }
        Err(e) => {
            checks.push(check("config", Level::Fail, e.to_string()));
            None
        }
    };

    let containers = match runtime {
        None => {
            checks.push(check(
                "runtime",
                Level::Fail,
                "no container runtime connection",
            ));
            None
        }
        Some(runtime) => match runtime.ping().await {
            Ok(version) => {
                checks.push(check("runtime", Level::Ok, version));
                runtime.containers().await.ok()
            }
            Err(e) => {
                checks.push(check("runtime", Level::Fail, e.to_string()));
                None
            }
        },
    };

    if let Some(settings) = &settings {
        match &settings.traefik {
            None => checks.push(check(
                "traefik",
                Level::Fail,
                "not set up; run `zevo setup`",
            )),
            Some(traefik) => {
                let dir = &traefik.dynamic_dir;
                let writable = dir.is_dir() && tempfile_probe(dir);
                checks.push(if writable {
                    check("route directory", Level::Ok, dir.display().to_string())
                } else {
                    check(
                        "route directory",
                        Level::Fail,
                        format!("{} is missing or not writable", dir.display()),
                    )
                });

                if let (Some(name), Some(containers)) = (&traefik.container, &containers) {
                    checks.push(match containers.iter().find(|c| &c.name == name) {
                        None => check("traefik", Level::Fail, format!("container '{name}' not found")),
                        Some(c) if !c.running => {
                            check("traefik", Level::Fail, format!("container '{name}' is stopped"))
                        }
                        Some(c) if !c.host_network && !c.networks.contains_key(&settings.network) => {
                            check(
                                "traefik",
                                Level::Warn,
                                format!(
                                    "container '{name}' is not on network '{}'; run `zevo reconcile`",
                                    settings.network
                                ),
                            )
                        }
                        Some(_) => check("traefik", Level::Ok, format!("container '{name}' is running")),
                    });
                }

                checks.push(if port_in_use(traefik.http_port) {
                    check(
                        "entrypoint",
                        Level::Ok,
                        format!("port {} accepts connections", traefik.http_port),
                    )
                } else {
                    check(
                        "entrypoint",
                        Level::Fail,
                        format!("nothing is listening on port {}", traefik.http_port),
                    )
                });
            }
        }

        let probe = format!("zevo-doctor.{}", settings.domain_suffix);
        let resolved = (probe.as_str(), 80)
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next());
        checks.push(match resolved {
            Some(addr) if addr.ip().is_loopback() => check(
                "dns",
                Level::Ok,
                format!("*.{} resolves to this machine", settings.domain_suffix),
            ),
            Some(addr) => check(
                "dns",
                Level::Warn,
                format!("{probe} resolves to {}", addr.ip()),
            ),
            None => check(
                "dns",
                Level::Warn,
                format!(
                    "{probe} does not resolve from the system resolver; browsers resolve \
                     *.localhost themselves, but curl and other tools may not"
                ),
            ),
        });
    }

    checks
}

fn tempfile_probe(dir: &std::path::Path) -> bool {
    let probe = dir.join(format!(".zevo-doctor-{}.tmp", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeRuntime;
    use crate::setup;

    fn level(checks: &[Check], name: &str) -> Level {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check {name}"))
            .level
    }

    #[tokio::test]
    async fn reports_missing_setup_and_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let checks = run(&Store::open(tmp.path()), None).await;
        assert_eq!(level(&checks, "runtime"), Level::Fail);
        assert_eq!(level(&checks, "traefik"), Level::Fail);
    }

    #[tokio::test]
    async fn healthy_setup_and_stopped_traefik() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path());
        let rt = FakeRuntime::new();
        setup::bootstrap(&store, &rt, Some(8099), false)
            .await
            .unwrap();

        let checks = run(&store, Some(&rt)).await;
        assert_eq!(level(&checks, "runtime"), Level::Ok);
        assert_eq!(level(&checks, "route directory"), Level::Ok);
        assert_eq!(level(&checks, "traefik"), Level::Ok);

        rt.remove_container(setup::MANAGED_CONTAINER);
        let checks = run(&store, Some(&rt)).await;
        assert_eq!(level(&checks, "traefik"), Level::Fail);
    }
}
