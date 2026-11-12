//! Reaching another machine over SSH.
//!
//! Zevo talks to a remote host's Docker by forwarding its Docker socket to a
//! local one through `ssh`, and then uses it exactly like the local daemon.
//! Nothing is installed on the remote. Authentication must not need typing
//! (keys or an agent): Zevo never prompts.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::model::Host;
use crate::runtime::{DockerRuntime, Runtime};
use crate::store::Store;
use crate::{Error, Result, programs, reconcile};

const DEFAULT_REMOTE_SOCKET: &str = "/var/run/docker.sock";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

/// `ssh` with the options every call needs: never prompt, give up quickly.
fn ssh() -> Command {
    let program = programs::find("ssh", &[]).unwrap_or_else(|| PathBuf::from("ssh"));
    let mut command = Command::new(program);
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=8")
        // A new machine's key is recorded on first contact; a changed key is
        // still refused.
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .stdin(Stdio::null());
    command
}

/// Quotes a value for a shell.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Runs a POSIX shell script on the host. The script goes through `sh -c`,
/// so it behaves the same whatever the account's login shell is.
fn run_script(host: &Host, script: &str) -> Result<std::process::Output> {
    ssh()
        .arg("--")
        .arg(&host.ssh)
        .arg(format!("sh -c {}", shell_quote(script)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::io("starting ssh", e))
}

/// What ssh printed, reduced to something worth showing.
fn ssh_error(host: &Host, stderr: &str) -> Error {
    let detail = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no details")
        .trim();
    let hint = if detail.contains("Permission denied") {
        " (Zevo cannot type a password; set up an SSH key for this host)"
    } else {
        ""
    };
    Error::Runtime(format!("ssh to {} failed: {detail}{hint}", host.ssh))
}

/// A forwarded Docker socket. Dropping it closes the connection.
pub struct Tunnel {
    child: Child,
    /// Private directory holding the socket and ssh's log; removed on drop.
    dir: tempfile::TempDir,
}

impl Tunnel {
    /// Opens the connection and waits until Docker answers through it.
    #[cfg(unix)]
    pub fn open(host: &Host) -> Result<Tunnel> {
        // A directory only this user can enter: nobody else can plant a
        // socket at the path. Kept short, as socket paths are limited to
        // about 100 bytes.
        let dir = tempfile::Builder::new()
            .prefix("zevo-")
            .tempdir()
            .map_err(|e| Error::io("creating a temporary folder", e))?;
        let socket = dir.path().join("d.sock");
        let log = dir.path().join("ssh.log");
        let remote_socket = host
            .docker_socket
            .as_deref()
            .unwrap_or(DEFAULT_REMOTE_SOCKET);

        // ssh's messages go to a file, not a pipe: nothing reads a pipe while
        // the connection lives, and a full pipe would freeze ssh.
        let log_file =
            std::fs::File::create(&log).map_err(|e| Error::io("creating the ssh log", e))?;
        let child = ssh()
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-nNT")
            .arg("-L")
            .arg(format!("{}:{remote_socket}", socket.display()))
            .arg("--")
            .arg(&host.ssh)
            .stdout(Stdio::null())
            .stderr(log_file)
            .spawn()
            .map_err(|e| Error::io("starting ssh", e))?;
        let mut tunnel = Tunnel { child, dir };

        let started = Instant::now();
        loop {
            if tunnel
                .child
                .try_wait()
                .map_err(|e| Error::io("waiting for ssh", e))?
                .is_some()
            {
                let stderr = std::fs::read_to_string(&log).unwrap_or_default();
                return Err(ssh_error(host, &stderr));
            }
            // ssh accepts local connections even when the remote socket is
            // missing or forbidden, so ask Docker a question through it.
            if docker_answers(&socket) {
                return Ok(tunnel);
            }
            if started.elapsed() > CONNECT_TIMEOUT {
                return Err(Error::Runtime(format!(
                    "connected to {}, but Docker did not answer on {remote_socket} there \
                     (is Docker running, and may this user use it?)",
                    host.ssh
                )));
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    #[cfg(not(unix))]
    pub fn open(_host: &Host) -> Result<Tunnel> {
        Err(Error::Other(
            "remote hosts are not supported on Windows yet".into(),
        ))
    }

    /// The remote Docker, through this connection.
    pub fn runtime(&self) -> Result<DockerRuntime> {
        DockerRuntime::connect_socket(&self.dir.path().join("d.sock"))
    }

    /// Whether the ssh process is still running.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Sends Docker's ping request over the socket and looks for a 200.
#[cfg(unix)]
fn docker_answers(socket: &std::path::Path) -> bool {
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    if stream
        .write_all(b"GET /_ping HTTP/1.0\r\nHost: docker\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut head = [0u8; 32];
    let read = stream.read(&mut head).unwrap_or(0);
    String::from_utf8_lossy(&head[..read]).contains(" 200")
}

/// A connection attempt: the tunnel with its runtime, or why it failed.
pub type Opened = std::result::Result<(Tunnel, DockerRuntime), String>;

/// Opens connections to several hosts at once, so one slow or unreachable
/// machine does not hold up the others. Results are in the order given.
pub fn open_all(hosts: &[Host]) -> Vec<(String, Opened)> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|host| {
                scope.spawn(move || {
                    let opened = Tunnel::open(host).and_then(|tunnel| {
                        let runtime = tunnel.runtime()?;
                        Ok((tunnel, runtime))
                    });
                    (host.id.clone(), opened.map_err(|e| e.to_string()))
                })
            })
            .collect();
        handles
            .into_iter()
            .zip(hosts)
            .map(|(handle, host)| {
                handle
                    .join()
                    .unwrap_or_else(|_| (host.id.clone(), Err("connection attempt crashed".into())))
            })
            .collect()
    })
}

/// The machine name or IP that `ssh` would connect to for a destination,
/// resolving aliases from the SSH config. `None` if ssh cannot say.
pub fn resolved_hostname(ssh_destination: &str) -> Option<String> {
    let output = ssh()
        .arg("-G")
        .arg("--")
        .arg(ssh_destination)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("hostname "))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// Registers a host after proving that SSH and Docker work on it. Returns
/// the host and the Docker version found there.
pub async fn register_host(
    store: &Store,
    ssh_destination: &str,
    name: Option<&str>,
    address: Option<&str>,
) -> Result<(Host, String)> {
    let mut host = Host::from_ssh(ssh_destination, name, address)?;
    // A name from the SSH config means nothing to Traefik; route to the
    // machine it stands for.
    if address.is_none_or(|a| a.trim().is_empty())
        && let Some(real) = resolved_hostname(&host.ssh)
    {
        host.address = real;
    }
    let tunnel = Tunnel::open(&host)?;
    let version = tunnel.runtime()?.ping().await?;
    store.add_host(&host)?;
    Ok((host, version))
}

/// Unregisters a host and removes the edge Traefik Zevo started on it.
/// Returns a note when the edge could not be removed (host unreachable).
pub async fn forget_host(store: &Store, id: &str) -> Result<Option<String>> {
    let host = store.host(id)?;
    // Refuses while projects still use the host.
    store.remove_host(id)?;
    let cleaned = async {
        let tunnel = Tunnel::open(&host)?;
        tunnel
            .runtime()?
            .remove_edge(reconcile::EDGE_CONTAINER, reconcile::EDGE_VOLUME)
            .await
    }
    .await;
    Ok(cleaned.err().map(|e| {
        format!(
            "the host was removed from Zevo, but its '{}' container could not be removed ({e}); \
             remove it there with `docker rm -f {}`",
            reconcile::EDGE_CONTAINER,
            reconcile::EDGE_CONTAINER
        )
    }))
}

/// Whether a folder exists on the host.
pub fn folder_exists(host: &Host, path: &str) -> Result<bool> {
    check_path(path)?;
    let output = run_script(host, &format!("[ -d {} ] || exit 3", shell_quote(path)))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(3) => Ok(false),
        _ => Err(ssh_error(host, &String::from_utf8_lossy(&output.stderr))),
    }
}

/// Text of a file on the host, or `None` if it does not exist.
pub fn read_file(host: &Host, path: &str) -> Result<Option<String>> {
    check_path(path)?;
    let quoted = shell_quote(path);
    // Exit status 3 marks "no such file", so it cannot be confused with a
    // connection failure (255) or a read error.
    let output = run_script(
        host,
        &format!("if [ -f {quoted} ]; then cat -- {quoted}; else exit 3; fi"),
    )?;
    match output.status.code() {
        Some(0) => Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned())),
        Some(3) => Ok(None),
        _ => Err(ssh_error(host, &String::from_utf8_lossy(&output.stderr))),
    }
}

fn check_path(path: &str) -> Result<()> {
    if path.contains(['\0', '\n']) {
        return Err(Error::Other(
            "the path contains characters that are not allowed".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_the_remote_shell() {
        assert_eq!(shell_quote("/srv/my app"), "'/srv/my app'");
        assert_eq!(shell_quote("it's; rm -rf /"), r"'it'\''s; rm -rf /'");
        // A whole script survives being wrapped for `sh -c`.
        assert_eq!(
            shell_quote("[ -d '/a b' ] || exit 3"),
            r"'[ -d '\''/a b'\'' ] || exit 3'"
        );
    }

    #[test]
    fn reports_the_useful_line_of_an_ssh_failure() {
        let host = Host::from_ssh("dev@10.0.0.5", None, None).unwrap();
        let e = ssh_error(
            &host,
            "Warning: added key\ndev@10.0.0.5: Permission denied (publickey).\n\n",
        );
        let text = e.to_string();
        assert!(text.contains("Permission denied (publickey)."), "{text}");
        assert!(text.contains("SSH key"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_that_is_not_docker_does_not_count_as_connected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        assert!(!docker_answers(&path));
        // Something listens, accepts, and says nothing useful.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"HTTP/1.0 502 Bad Gateway\r\n\r\n");
            }
        });
        assert!(!docker_answers(&path));
        server.join().unwrap();
    }
}
