//! HTTPS certificates for project hostnames, issued by a certificate
//! authority browsers on this machine trust: mkcert's local one, or whatever
//! the user's certificate plugin uses.
//!
//! Certificates live in a `zevo-certs` folder inside Traefik's route
//! directory: that directory is already mounted into Traefik, and Traefik
//! only loads `.yml`/`.toml` files from it, so the `.pem` files are ignored
//! as routes but readable as certificates.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::model::{Project, Settings, TraefikSettings};
use crate::plugins::{IssuerPlugin, Plugins, run};
use crate::{Error, Result};

pub const CERT_DIR: &str = "zevo-certs";

/// mkcert certificates last a little over two years; renew well before.
const RENEW_AFTER: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// How long a certificate plugin may take to say whether it is ready.
const PLUGIN_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a certificate plugin may take to issue one certificate; an
/// authority on another machine can be slow to answer.
const PLUGIN_ISSUE_TIMEOUT: Duration = Duration::from_secs(60);

/// Something that can issue locally trusted certificates.
pub trait Issuer: Send + Sync {
    /// `Err` explains why certificates cannot be issued and how to fix it.
    fn ready(&self) -> std::result::Result<(), String>;
    fn issue(&self, names: &[String], cert: &Path, key: &Path) -> Result<()>;
    /// Age at which a certificate from this issuer is replaced.
    fn renew_after(&self) -> Duration {
        RENEW_AFTER
    }
    /// Recorded next to each certificate, so that changing the issuer
    /// replaces certificates the previous one made. `None` for mkcert, which
    /// is what certificates recorded without one came from.
    fn label(&self) -> Option<String> {
        None
    }
}

/// The issuer to use: the certificate plugin when one is configured, else
/// mkcert. `config_dir` is Zevo's config directory, passed on to the plugin.
pub fn issuer(plugins: &Plugins, config_dir: &Path) -> Box<dyn Issuer> {
    match &plugins.issuer {
        Some(plugin) => Box::new(PluginIssuer::new(plugin.clone(), config_dir)),
        None => Box::new(Mkcert),
    }
}

/// Issues certificates by running a certificate plugin's program. It gets
/// `{"protocol":1,"request":"ready"}` or `{"protocol":1,"request":"issue",
/// "names":[...],"cert":"<path>","key":"<path>"}` on its standard input, and
/// answers `{"ok":true}` or `{"ok":false,"error":"..."}`. To issue, it writes
/// the certificate (with any intermediates) and the key as PEM at the paths
/// given.
pub struct PluginIssuer {
    plugin: IssuerPlugin,
    config_dir: PathBuf,
}

/// What a certificate plugin answers.
#[derive(serde::Deserialize)]
struct PluginReply {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

impl PluginIssuer {
    pub fn new(plugin: IssuerPlugin, config_dir: &Path) -> Self {
        Self {
            plugin,
            config_dir: config_dir.to_path_buf(),
        }
    }

    /// Sends one request; `Err` is a sentence naming the plugin.
    fn ask(
        &self,
        request: serde_json::Value,
        timeout: Duration,
    ) -> std::result::Result<(), String> {
        let name = &self.plugin.name;
        let out = run::run_blocking(&self.plugin.command, request, &self.config_dir, timeout)
            .map_err(|e| format!("The certificate plugin '{name}' {e}"))?;
        let reply: PluginReply = serde_json::from_str(out.trim()).map_err(|_| {
            format!(
                "The certificate plugin '{name}' gave an answer Zevo does not understand: '{}'",
                run::quote(&out)
            )
        })?;
        match reply {
            PluginReply { ok: true, .. } => Ok(()),
            PluginReply { error, .. } => Err(format!(
                "The certificate plugin '{name}' says: {}",
                error
                    .as_deref()
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .unwrap_or("it cannot")
            )),
        }
    }
}

impl Issuer for PluginIssuer {
    fn ready(&self) -> std::result::Result<(), String> {
        self.ask(
            serde_json::json!({"request": "ready"}),
            PLUGIN_READY_TIMEOUT,
        )
    }

    fn issue(&self, names: &[String], cert: &Path, key: &Path) -> Result<()> {
        // The plugin writes beside the real files, which are then replaced
        // only if it wrote both. Otherwise a plugin that answers "ok" without
        // writing would have the certificate already there counted as new,
        // and one that fails halfway would leave a certificate and key that
        // do not belong together.
        let beside = |path: &Path| {
            let mut name = path.as_os_str().to_owned();
            name.push(".new");
            PathBuf::from(name)
        };
        let (new_cert, new_key) = (beside(cert), beside(key));
        let discard = || {
            let _ = std::fs::remove_file(&new_cert);
            let _ = std::fs::remove_file(&new_key);
        };
        discard();
        let request = serde_json::json!({
            "request": "issue",
            "names": names,
            "cert": new_cert,
            "key": new_key,
        });
        if let Err(e) = self.ask(request, PLUGIN_ISSUE_TIMEOUT) {
            discard();
            return Err(Error::Other(e));
        }
        if let Some(missing) = [&new_cert, &new_key].into_iter().find(|p| !p.is_file()) {
            let message = format!(
                "The certificate plugin '{}' said it issued a certificate but did not write {}",
                self.plugin.name,
                missing.display()
            );
            discard();
            return Err(Error::Other(message));
        }
        for (new, real) in [(&new_key, key), (&new_cert, cert)] {
            std::fs::rename(new, real)
                .map_err(|e| Error::io(format!("replacing {}", real.display()), e))?;
        }
        Ok(())
    }

    fn label(&self) -> Option<String> {
        Some(format!("plugin {}", self.plugin.name))
    }

    fn renew_after(&self) -> Duration {
        match self.plugin.renew_after_hours {
            Some(hours) => Duration::from_secs(hours.saturating_mul(60 * 60)),
            None => RENEW_AFTER,
        }
    }
}

/// Issues certificates by running the `mkcert` command.
pub struct Mkcert;

/// The `mkcert` to run, wherever it is installed.
fn mkcert() -> Command {
    match crate::programs::find("mkcert", &[]) {
        Some(path) => Command::new(path),
        None => Command::new("mkcert"),
    }
}

impl Issuer for Mkcert {
    fn ready(&self) -> std::result::Result<(), String> {
        let not_installed = "mkcert is not installed. Install it (macOS: `brew install mkcert`; \
                             others: https://github.com/FiloSottile/mkcert), then run `mkcert -install`";
        let output = mkcert()
            .arg("-CAROOT")
            .output()
            .map_err(|_| not_installed.to_string())?;
        if !output.status.success() {
            return Err(not_installed.to_string());
        }
        let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if root.join("rootCA.pem").is_file() {
            Ok(())
        } else {
            Err(
                "mkcert has no certificate authority yet. Run `mkcert -install` once in a terminal"
                    .to_string(),
            )
        }
    }

    fn issue(&self, names: &[String], cert: &Path, key: &Path) -> Result<()> {
        let output = mkcert()
            .arg("-cert-file")
            .arg(cert)
            .arg("-key-file")
            .arg(key)
            .args(names)
            .output()
            .map_err(|e| Error::io("running mkcert", e))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(Error::Other(format!(
                "mkcert failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

/// Names a project's certificate must cover: the project domain and one level
/// of subdomains under it, plus any exact hostnames.
pub fn names(project: &Project, settings: &Settings) -> Vec<String> {
    let mut names = BTreeSet::new();
    for route in &project.routes {
        match &route.host {
            Some(host) => {
                names.insert(host.to_lowercase());
            }
            None => {
                let domain = format!("{}.{}", project.slug, settings.domain_suffix);
                names.insert(format!("*.{domain}"));
                names.insert(domain);
            }
        }
    }
    names.into_iter().collect()
}

struct Files {
    cert: PathBuf,
    key: PathBuf,
    names: PathBuf,
}

fn files(slug: &str, dynamic_dir: &Path) -> Files {
    let dir = dynamic_dir.join(CERT_DIR);
    Files {
        // Slugs contain no dots, so `<slug>.key.pem` cannot be another
        // project's certificate.
        cert: dir.join(format!("{slug}.cert.pem")),
        key: dir.join(format!("{slug}.key.pem")),
        names: dir.join(format!("{slug}.names")),
    }
}

/// Certificate and key paths as Traefik sees them, if the project has a
/// certificate on disk.
pub fn existing(slug: &str, traefik: &TraefikSettings) -> Option<(String, String)> {
    let f = files(slug, &traefik.dynamic_dir);
    (f.cert.is_file() && f.key.is_file()).then(|| {
        (
            traefik.path_in_traefik(&format!("{CERT_DIR}/{slug}.cert.pem")),
            traefik.path_in_traefik(&format!("{CERT_DIR}/{slug}.key.pem")),
        )
    })
}

/// Whether the project already has a certificate that covers its current
/// hostnames and is not due for renewal by `issuer`. Cheap: reads two small
/// files.
pub fn is_current(project: &Project, settings: &Settings, issuer: &dyn Issuer) -> bool {
    let Some(traefik) = &settings.traefik else {
        return false;
    };
    let wanted = names(project, settings);
    let f = files(&project.slug, &traefik.dynamic_dir);
    !wanted.is_empty()
        && f.key.is_file()
        && std::fs::read_to_string(&f.names).is_ok_and(|have| have == record(&wanted, issuer))
        && std::fs::metadata(&f.cert)
            .and_then(|m| m.modified())
            .is_ok_and(|at| {
                SystemTime::now().duration_since(at).unwrap_or_default() < issuer.renew_after()
            })
}

/// Makes sure the project has a current certificate for its hostnames.
/// Returns true if one was issued. A project with no routes has none.
pub fn ensure(project: &Project, settings: &Settings, issuer: &dyn Issuer) -> Result<bool> {
    let Some(traefik) = &settings.traefik else {
        return Err(Error::NotSetUp);
    };
    let wanted = names(project, settings);
    if wanted.is_empty() {
        remove(&project.slug, &traefik.dynamic_dir)?;
        return Ok(false);
    }
    if is_current(project, settings, issuer) {
        return Ok(false);
    }
    let f = files(&project.slug, &traefik.dynamic_dir);
    let dir = traefik.dynamic_dir.join(CERT_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
    issuer.issue(&wanted, &f.cert, &f.key)?;
    std::fs::write(&f.names, record(&wanted, issuer))
        .map_err(|e| Error::io(format!("writing {}", f.names.display()), e))?;
    Ok(true)
}

/// What the `.names` file next to a certificate holds: the names it covers,
/// one per line, then the issuer if it is not mkcert.
fn record(names: &[String], issuer: &dyn Issuer) -> String {
    let mut text = names.join("\n");
    if let Some(label) = issuer.label() {
        text.push_str(&format!("\n# issued by {label}"));
    }
    text
}

/// Deletes a project's certificate files, if any.
pub fn remove(slug: &str, dynamic_dir: &Path) -> Result<()> {
    let f = files(slug, dynamic_dir);
    for path in [f.cert, f.key, f.names] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(format!("removing {}", path.display()), e)),
        }
    }
    Ok(())
}

/// Test double: writes placeholder files and records what was asked for.
#[derive(Default)]
pub struct FakeIssuer {
    /// When set, `ready` fails with this message.
    pub unavailable: Option<String>,
    pub issued: std::sync::Mutex<Vec<Vec<String>>>,
}

impl Issuer for FakeIssuer {
    fn ready(&self) -> std::result::Result<(), String> {
        match &self.unavailable {
            Some(reason) => Err(reason.clone()),
            None => Ok(()),
        }
    }

    fn issue(&self, names: &[String], cert: &Path, key: &Path) -> Result<()> {
        self.issued.lock().unwrap().push(names.to_vec());
        for path in [cert, key] {
            std::fs::write(path, "placeholder").map_err(|e| Error::io("writing certificate", e))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Backend, DevcontainerKind, Route, Target, TraefikMode};

    fn settings(dir: &Path) -> Settings {
        Settings {
            traefik: Some(TraefikSettings {
                mode: TraefikMode::Adopted,
                container: Some("traefik".into()),
                dynamic_dir: dir.to_path_buf(),
                entrypoint: "websecure".into(),
                http_port: 443,
                tls: true,
                container_dir: Some("/etc/traefik/dynamic".into()),
            }),
            ..Settings::default()
        }
    }

    fn project(routes: &[(Option<&str>, Option<&str>)]) -> Project {
        Project {
            slug: "shop".into(),
            name: "Shop".into(),
            host: "local".into(),
            path: "/work/shop".into(),
            kind: DevcontainerKind::Image,
            service: None,
            routes: routes
                .iter()
                .map(|(sub, host)| Route {
                    subdomain: sub.map(Into::into),
                    port: 3000,
                    path_prefix: None,
                    target: Target::Container,
                    backend: Backend::Http,
                    host: host.map(Into::into),
                })
                .collect(),
            tags: vec![],
            declared: false,
            address: None,
            env_file: None,
            skip: Vec::new(),
        }
    }

    #[test]
    fn names_cover_the_domain_one_wildcard_level_and_exact_hosts() {
        let tmp = tempfile::tempdir().unwrap();
        let p = project(&[
            (None, None),
            (Some("api"), None),
            (None, Some("Local.Example.com")),
        ]);
        assert_eq!(
            names(&p, &settings(tmp.path())),
            ["*.shop.localhost", "local.example.com", "shop.localhost"]
        );
        assert!(names(&project(&[]), &settings(tmp.path())).is_empty());
    }

    #[test]
    fn issues_once_reissues_when_names_change_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let s = settings(tmp.path());
        let issuer = FakeIssuer::default();
        let p = project(&[(None, None)]);

        assert!(ensure(&p, &s, &issuer).unwrap());
        assert!(!ensure(&p, &s, &issuer).unwrap());
        assert_eq!(
            existing("shop", s.traefik.as_ref().unwrap()),
            Some((
                "/etc/traefik/dynamic/zevo-certs/shop.cert.pem".to_string(),
                "/etc/traefik/dynamic/zevo-certs/shop.key.pem".to_string()
            ))
        );

        // A new exact hostname needs a new certificate; another subdomain does not.
        assert!(!ensure(&project(&[(None, None), (Some("api"), None)]), &s, &issuer).unwrap());
        assert!(
            ensure(
                &project(&[(None, None), (None, Some("x.example.com"))]),
                &s,
                &issuer
            )
            .unwrap()
        );
        assert_eq!(issuer.issued.lock().unwrap().len(), 2);

        // No routes, no certificate.
        assert!(!ensure(&project(&[]), &s, &issuer).unwrap());
        assert_eq!(existing("shop", s.traefik.as_ref().unwrap()), None);
        assert_eq!(
            std::fs::read_dir(tmp.path().join(CERT_DIR))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn mkcert_is_used_unless_a_certificate_plugin_is_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let mut plugins = Plugins::default();
        assert_eq!(issuer(&plugins, tmp.path()).label(), None);
        plugins.issuer = Some(IssuerPlugin {
            name: "step-ca".into(),
            command: vec!["step-issue".into()],
            renew_after_hours: Some(16),
        });
        let chosen = issuer(&plugins, tmp.path());
        assert_eq!(chosen.label().as_deref(), Some("plugin step-ca"));
        assert_eq!(chosen.renew_after(), Duration::from_secs(16 * 60 * 60));
    }

    #[cfg(unix)]
    mod plugin {
        use super::*;
        use crate::plugins::run::testing::script;

        /// A certificate plugin that keeps each request in `requests` and
        /// writes placeholder PEM files where it is told to.
        const ISSUE: &str = r#"
read -r line
printf '%s\n' "$line" >> "$(dirname "$0")/requests"
case "$line" in
  *'"request":"ready"'*) echo '{"ok":true}' ;;
  *'"request":"issue"'*)
    cert=$(printf '%s' "$line" | sed 's/.*"cert":"\([^"]*\)".*/\1/')
    key=$(printf '%s' "$line" | sed 's/.*"key":"\([^"]*\)".*/\1/')
    echo CERT > "$cert"; echo KEY > "$key"
    echo '{"ok":true}' ;;
  *) echo '{"ok":false,"error":"unknown request"}' ;;
esac
"#;

        fn plugin_issuer(dir: &Path, body: &str) -> PluginIssuer {
            PluginIssuer::new(
                IssuerPlugin {
                    name: "test-ca".into(),
                    command: script(dir, "issue.sh", body),
                    renew_after_hours: None,
                },
                dir,
            )
        }

        #[test]
        fn a_plugin_answers_ready_and_issues_where_it_is_told() {
            let tmp = tempfile::tempdir().unwrap();
            let issuer = plugin_issuer(tmp.path(), ISSUE);
            assert_eq!(issuer.ready(), Ok(()));

            let s = settings(tmp.path());
            let p = project(&[(None, None)]);
            assert!(ensure(&p, &s, &issuer).unwrap());
            let f = files("shop", tmp.path());
            assert_eq!(std::fs::read_to_string(&f.cert).unwrap(), "CERT\n");
            assert_eq!(std::fs::read_to_string(&f.key).unwrap(), "KEY\n");
            assert!(!ensure(&p, &s, &issuer).unwrap());

            let requests: Vec<serde_json::Value> =
                std::fs::read_to_string(tmp.path().join("requests"))
                    .unwrap()
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect();
            assert_eq!(
                requests,
                [
                    serde_json::json!({"protocol": 1, "request": "ready"}),
                    serde_json::json!({
                        "protocol": 1,
                        "request": "issue",
                        "names": ["*.shop.localhost", "shop.localhost"],
                        // Beside the real files, which Zevo then replaces.
                        "cert": format!("{}.new", f.cert.display()),
                        "key": format!("{}.new", f.key.display()),
                    }),
                ]
            );
        }

        #[test]
        fn switching_issuer_replaces_the_certificates_the_old_one_made() {
            let tmp = tempfile::tempdir().unwrap();
            let s = settings(tmp.path());
            let p = project(&[(None, None)]);
            assert!(ensure(&p, &s, &FakeIssuer::default()).unwrap());
            let plugin = plugin_issuer(tmp.path(), ISSUE);
            assert!(!is_current(&p, &s, &plugin));
            assert!(ensure(&p, &s, &plugin).unwrap());
            assert!(is_current(&p, &s, &plugin));
            assert!(!is_current(&p, &s, &FakeIssuer::default()));
        }

        #[test]
        fn a_plugin_that_refuses_or_misbehaves_explains_what_happened() {
            let tmp = tempfile::tempdir().unwrap();
            let refuses = plugin_issuer(
                tmp.path(),
                "echo '{\"ok\":false,\"error\":\"run step ca bootstrap first\"}'\n",
            );
            assert_eq!(
                refuses.ready(),
                Err("The certificate plugin 'test-ca' says: run step ca bootstrap first".into())
            );

            let garbage = plugin_issuer(tmp.path(), "echo hello\n");
            assert_eq!(
                garbage.ready(),
                Err(
                    "The certificate plugin 'test-ca' gave an answer Zevo does not understand: \
                     'hello'"
                        .into()
                )
            );

            let crashes = plugin_issuer(tmp.path(), "echo 'token expired' >&2\nexit 2\n");
            assert_eq!(
                crashes.ready(),
                Err("The certificate plugin 'test-ca' exited with status 2: token expired".into())
            );

            // Says yes but writes nothing.
            let lies = plugin_issuer(tmp.path(), "echo '{\"ok\":true}'\n");
            let err = ensure(&project(&[(None, None)]), &settings(tmp.path()), &lies)
                .unwrap_err()
                .to_string();
            assert!(err.contains("did not write"), "{err}");
        }
    }
}
