//! Reaching another machine over SSH.
//!
//! Zevo talks to a remote host's Docker by forwarding its Docker socket to a
//! local one through `ssh`, and then uses it exactly like the local daemon.
//! Nothing is installed on the remote. Authentication must not need typing
//! (keys or an agent): Zevo never prompts.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::model::Host;
use crate::runtime::DockerRuntime;
use crate::{Error, Result, programs};

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

/// Quotes a value for the remote shell.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
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
    socket: PathBuf,
}

impl Tunnel {
    /// Opens the connection and waits until Docker answers through it.
    #[cfg(unix)]
    pub fn open(host: &Host) -> Result<Tunnel> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        // Unix socket paths are limited to about 100 bytes, so keep it short.
        let socket = std::env::temp_dir().join(format!(
            "zevo-{}-{}.sock",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&socket);
        let remote_socket = host
            .docker_socket
            .as_deref()
            .unwrap_or(DEFAULT_REMOTE_SOCKET);

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
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::io("starting ssh", e))?;
        let mut tunnel = Tunnel { child, socket };

        let started = Instant::now();
        loop {
            if let Some(_status) = tunnel
                .child
                .try_wait()
                .map_err(|e| Error::io("waiting for ssh", e))?
            {
                let mut stderr = String::new();
                if let Some(mut pipe) = tunnel.child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                return Err(ssh_error(host, &stderr));
            }
            if std::os::unix::net::UnixStream::connect(&tunnel.socket).is_ok() {
                return Ok(tunnel);
            }
            if started.elapsed() > CONNECT_TIMEOUT {
                return Err(Error::Runtime(format!(
                    "ssh to {} connected, but Docker did not answer on {remote_socket} there",
                    host.ssh
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
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
        DockerRuntime::connect_socket(&self.socket)
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
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Text of a file on the host, or `None` if it does not exist.
pub fn read_file(host: &Host, path: &str) -> Result<Option<String>> {
    if path.contains(['\0', '\n']) {
        return Err(Error::Other(
            "the path contains characters that are not allowed".into(),
        ));
    }
    let quoted = shell_quote(path);
    // Exit status 3 marks "no such file", so it cannot be confused with a
    // connection failure (255) or a read error.
    let script = format!("if [ -f {quoted} ]; then cat -- {quoted}; else exit 3; fi");
    let output = ssh()
        .arg("--")
        .arg(&host.ssh)
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::io("starting ssh", e))?;
    match output.status.code() {
        Some(0) => Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned())),
        Some(3) => Ok(None),
        _ => Err(ssh_error(host, &String::from_utf8_lossy(&output.stderr))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_the_remote_shell() {
        assert_eq!(shell_quote("/srv/my app"), "'/srv/my app'");
        assert_eq!(shell_quote("it's; rm -rf /"), r"'it'\''s; rm -rf /'");
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
}
