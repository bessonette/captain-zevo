//! Environment checks behind `zevo doctor`.

use std::net::ToSocketAddrs;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::plugins::{Plugins, run as program};
use crate::runtime::Runtime;
use crate::setup::port_in_use;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
}

/// How long a check plugin may take. Checks run side by side, so this is
/// also about the most they add to `zevo doctor`.
const PLUGIN_TIMEOUT: Duration = Duration::from_secs(10);

fn check(name: impl Into<String>, level: Level, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
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

    let plugins = Plugins::load(store.dir());
    checks.push(plugins_check(&plugins, store.dir()));
    checks.extend(plugin_checks(&plugins, store.dir(), PLUGIN_TIMEOUT).await);
    checks
}

/// What the plugins folder adds to the built-ins, or what is wrong with it.
fn plugins_check(plugins: &Plugins, config_dir: &Path) -> Check {
    let dir = config_dir.join(crate::plugins::PLUGINS_DIR);
    if !plugins.problems.is_empty() {
        return check("plugins", Level::Warn, plugins.problems.join("; "));
    }
    let builtin = Plugins::builtin();
    let changed = |count: usize, one: &str, many: &str| match count {
        0 => None,
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {many}")),
    };
    let parts: Vec<String> = [
        changed(
            plugins
                .templates
                .iter()
                .filter(|t| builtin.template(&t.id) != Some(*t))
                .count(),
            "template",
            "templates",
        ),
        changed(
            plugins
                .extras
                .iter()
                .filter(|e| builtin.extra(&e.id) != Some(*e))
                .count(),
            "extra",
            "extras",
        ),
        changed(
            plugins
                .editors
                .iter()
                .filter(|e| builtin.editor(&e.id) != Some(*e))
                .count(),
            "editor",
            "editors",
        ),
        changed(plugins.hooks.len(), "hook", "hooks"),
        changed(plugins.checks.len(), "check", "checks"),
        plugins
            .issuer
            .as_ref()
            .map(|i| format!("certificate issuer '{}'", i.name)),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        check(
            "plugins",
            Level::Ok,
            format!("none installed in {}", dir.display()),
        )
    } else {
        check(
            "plugins",
            Level::Ok,
            format!("{} from {}", parts.join(", "), dir.display()),
        )
    }
}

/// What a check plugin prints.
#[derive(Deserialize)]
struct PluginResult {
    level: Level,
    #[serde(default)]
    detail: String,
}

/// Runs every check plugin, side by side; each gives one check, in order.
async fn plugin_checks(plugins: &Plugins, config_dir: &Path, timeout: Duration) -> Vec<Check> {
    let runs = plugins.checks.iter().map(|plugin| async move {
        let request = serde_json::json!({"request": "check"});
        let result = program::run(&plugin.command, request, config_dir, timeout).await;
        match result {
            Err(e) => check(&plugin.name, Level::Fail, format!("the check plugin {e}")),
            Ok(out) => match serde_json::from_str::<PluginResult>(out.trim()) {
                Ok(result) => check(&plugin.name, result.level, result.detail),
                Err(_) => check(
                    &plugin.name,
                    Level::Fail,
                    format!(
                        "the check plugin gave an answer Zevo does not understand: '{}'",
                        program::quote(&out)
                    ),
                ),
            },
        }
    });
    futures_util::future::join_all(runs).await
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

    #[test]
    fn the_plugins_check_summarises_what_was_added_or_warns_about_problems() {
        let tmp = tempfile::tempdir().unwrap();
        let none = plugins_check(&Plugins::load(tmp.path()), tmp.path());
        assert_eq!(none.level, Level::Ok);
        assert!(
            none.detail.starts_with("none installed in "),
            "{}",
            none.detail
        );

        let dir = tmp.path().join(crate::plugins::PLUGINS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.toml"),
            "[[check]]\nname = \"vpn\"\ncommand = [\"vpn-status\"]\n\
             [[editor]]\nid = \"idea\"\nname = \"IDEA\"\nlauncher = \"idea\"\n\
             [issuer]\nname = \"step-ca\"\ncommand = [\"step\"]\n",
        )
        .unwrap();
        let some = plugins_check(&Plugins::load(tmp.path()), tmp.path());
        assert_eq!(some.level, Level::Ok);
        assert!(
            some.detail
                .starts_with("1 editor, 1 check, certificate issuer 'step-ca' from "),
            "{}",
            some.detail
        );

        std::fs::write(dir.join("b.toml"), "[[hook]]\n").unwrap();
        let broken = plugins_check(&Plugins::load(tmp.path()), tmp.path());
        assert_eq!(broken.level, Level::Warn);
        assert!(broken.detail.contains("b.toml"), "{}", broken.detail);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_plugins_report_their_level_and_misbehaviour_is_a_failure() {
        use crate::plugins::run::testing::script;
        let tmp = tempfile::tempdir().unwrap();
        let mut plugins = Plugins::default();
        let mut add = |name: &str, body: &str| {
            plugins.checks.push(crate::plugins::CheckPlugin {
                name: name.into(),
                command: script(tmp.path(), &format!("{name}.sh"), body),
            });
        };
        add(
            "good",
            "read -r line\ncase \"$line\" in *'\"request\":\"check\"'*) \
             echo '{\"level\":\"ok\",\"detail\":\"connected\"}';; esac\n",
        );
        add("meh", "echo '{\"level\":\"warn\",\"detail\":\"slow\"}'\n");
        add("bad", "echo '{\"level\":\"fail\",\"detail\":\"down\"}'\n");
        add("garbage", "echo 'all good!'\n");
        add("unknown-level", "echo '{\"level\":\"great\"}'\n");
        add("crash", "echo oops >&2\nexit 1\n");
        add("hang", "exec sleep 30\n");

        let checks = plugin_checks(&plugins, tmp.path(), Duration::from_millis(500)).await;
        let got: Vec<(&str, Level, &str)> = checks
            .iter()
            .map(|c| (c.name.as_str(), c.level, c.detail.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("good", Level::Ok, "connected"),
                ("meh", Level::Warn, "slow"),
                ("bad", Level::Fail, "down"),
                (
                    "garbage",
                    Level::Fail,
                    "the check plugin gave an answer Zevo does not understand: 'all good!'"
                ),
                (
                    "unknown-level",
                    Level::Fail,
                    "the check plugin gave an answer Zevo does not understand: \
                     '{\"level\":\"great\"}'"
                ),
                (
                    "crash",
                    Level::Fail,
                    "the check plugin exited with status 1: oops"
                ),
                (
                    "hang",
                    Level::Fail,
                    "the check plugin did not finish within 500 ms and was stopped"
                ),
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn doctor_runs_the_check_plugins_after_the_built_in_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(crate::plugins::PLUGINS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("vpn.sh"),
            "echo '{\"level\":\"warn\",\"detail\":\"off\"}'\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("vpn.toml"),
            format!(
                "[[check]]\nname = \"vpn\"\ncommand = [\"/bin/sh\", {:?}]\n",
                dir.join("vpn.sh")
            ),
        )
        .unwrap();
        let checks = run(&Store::open(tmp.path()), None).await;
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names[names.len() - 2..], ["plugins", "vpn"]);
        assert_eq!(level(&checks, "vpn"), Level::Warn);
    }
}
