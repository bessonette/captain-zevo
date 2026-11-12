//! HTTPS certificates for project hostnames, issued by the machine's local
//! certificate authority (mkcert) so browsers trust them.
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
use crate::{Error, Result};

pub const CERT_DIR: &str = "zevo-certs";

/// mkcert certificates last a little over two years; renew well before.
const RENEW_AFTER: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Something that can issue locally trusted certificates.
pub trait Issuer: Send + Sync {
    /// `Err` explains why certificates cannot be issued and how to fix it.
    fn ready(&self) -> std::result::Result<(), String>;
    fn issue(&self, names: &[String], cert: &Path, key: &Path) -> Result<()>;
}

/// Issues certificates by running the `mkcert` command.
pub struct Mkcert;

/// `mkcert` on the PATH, or in the usual install locations: an app started
/// from the Finder or a desktop launcher does not get the shell's PATH.
fn mkcert() -> Command {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("mkcert").is_file()));
    if !on_path {
        let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
        let candidates = [
            Some(PathBuf::from("/opt/homebrew/bin/mkcert")),
            Some(PathBuf::from("/usr/local/bin/mkcert")),
            Some(PathBuf::from("/home/linuxbrew/.linuxbrew/bin/mkcert")),
            home.as_ref().map(|h| h.join(".local/bin/mkcert")),
            home.as_ref().map(|h| h.join("go/bin/mkcert")),
        ];
        if let Some(found) = candidates.into_iter().flatten().find(|p| p.is_file()) {
            return Command::new(found);
        }
    }
    Command::new("mkcert")
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
        cert: dir.join(format!("{slug}.pem")),
        key: dir.join(format!("{slug}-key.pem")),
        names: dir.join(format!("{slug}.names")),
    }
}

/// Certificate and key paths as Traefik sees them, if the project has a
/// certificate on disk.
pub fn existing(slug: &str, traefik: &TraefikSettings) -> Option<(String, String)> {
    let f = files(slug, &traefik.dynamic_dir);
    (f.cert.is_file() && f.key.is_file()).then(|| {
        (
            traefik.path_in_traefik(&format!("{CERT_DIR}/{slug}.pem")),
            traefik.path_in_traefik(&format!("{CERT_DIR}/{slug}-key.pem")),
        )
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
    let f = files(&project.slug, &traefik.dynamic_dir);
    let listed = wanted.join("\n");
    let fresh = f.key.is_file()
        && std::fs::read_to_string(&f.names).is_ok_and(|have| have == listed)
        && std::fs::metadata(&f.cert)
            .and_then(|m| m.modified())
            .is_ok_and(|at| SystemTime::now().duration_since(at).unwrap_or_default() < RENEW_AFTER);
    if fresh {
        return Ok(false);
    }

    let dir = traefik.dynamic_dir.join(CERT_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
    issuer.issue(&wanted, &f.cert, &f.key)?;
    std::fs::write(&f.names, listed)
        .map_err(|e| Error::io(format!("writing {}", f.names.display()), e))?;
    Ok(true)
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
                "/etc/traefik/dynamic/zevo-certs/shop.pem".to_string(),
                "/etc/traefik/dynamic/zevo-certs/shop-key.pem".to_string()
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
}
