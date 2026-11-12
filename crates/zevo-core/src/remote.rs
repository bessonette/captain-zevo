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
    /// The ssh process forwarding a socket (socket mode).
    child: Option<Child>,
    /// The local proxy running a Docker command over ssh (command mode).
    proxy: Option<CommandProxy>,
    /// Private directory holding the socket and ssh's log; removed on drop.
    dir: tempfile::TempDir,
    /// The Docker socket on the host that this tunnel reaches; empty in
    /// command mode.
    remote_socket: String,
}

/// Where Docker-compatible runtimes put their socket, in the order tried:
/// whatever `DOCKER_HOST` names, the system socket, then the per-user
/// sockets of Docker Desktop, OrbStack, Colima (any profile), Rancher
/// Desktop, rootless Docker and Podman. Prints the ones that exist.
const FIND_SOCKETS: &str = r#"
for s in "${DOCKER_HOST#unix://}" /var/run/docker.sock \
    "$HOME/.docker/run/docker.sock" "$HOME/.orbstack/run/docker.sock" \
    "$HOME"/.colima/*/docker.sock "$HOME/.rd/docker.sock" \
    "${XDG_RUNTIME_DIR:-/nonexistent}/docker.sock" \
    "${XDG_RUNTIME_DIR:-/nonexistent}/podman/podman.sock" \
    /run/podman/podman.sock; do
    if [ -S "$s" ]; then echo "$s"; fi
done
true
"#;

/// Docker sockets that exist on the host, most likely first.
pub fn find_sockets(host: &Host) -> Result<Vec<String>> {
    let output = run_script(host, FIND_SOCKETS)?;
    if !output.status.success() {
        return Err(ssh_error(host, &String::from_utf8_lossy(&output.stderr)));
    }
    let mut found: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() && !found.iter().any(|f| f == line) {
            found.push(line.to_string());
        }
    }
    Ok(found)
}

impl Tunnel {
    /// Opens the connection and waits until Docker answers through it. Uses
    /// the host's recorded socket, or finds one if none is recorded.
    #[cfg(unix)]
    pub fn open(host: &Host) -> Result<Tunnel> {
        if let Some(command) = &host.docker_command {
            return Self::open_command(host, command);
        }
        let candidates = match &host.docker_socket {
            Some(socket) => vec![socket.clone()],
            None => find_sockets(host)?,
        };
        if candidates.is_empty() {
            return Err(Error::Runtime(format!(
                "connected to {}, but found no Docker socket there. Is Docker (or Podman with its \
                 Docker-compatible socket) running for this user? If its socket is somewhere unusual, \
                 give the path",
                host.ssh
            )));
        }
        let mut last = None;
        for socket in &candidates {
            match Self::open_socket(host, socket) {
                Ok(tunnel) => return Ok(tunnel),
                Err(e) => last = Some(e),
            }
        }
        Err(match (candidates.len(), last) {
            (1, Some(e)) => e,
            (_, _) => Error::Runtime(format!(
                "connected to {}, but Docker did not answer on any socket found there ({}). \
                 Is it running, and may this user use it?",
                host.ssh,
                candidates.join(", ")
            )),
        })
    }

    #[cfg(unix)]
    fn open_socket(host: &Host, remote_socket: &str) -> Result<Tunnel> {
        // A directory only this user can enter: nobody else can plant a
        // socket at the path. Kept short, as socket paths are limited to
        // about 100 bytes.
        let dir = tempfile::Builder::new()
            .prefix("zevo-")
            .tempdir()
            .map_err(|e| Error::io("creating a temporary folder", e))?;
        let socket = dir.path().join("d.sock");
        let log = dir.path().join("ssh.log");

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
        let mut child = child;
        let tunnel = |child| Tunnel {
            child: Some(child),
            proxy: None,
            dir,
            remote_socket: remote_socket.to_string(),
        };

        let no_answer = || {
            Error::Runtime(format!(
                "connected to {}, but Docker did not answer on {remote_socket} there \
                 (is Docker running, and may this user use it?)",
                host.ssh
            ))
        };
        let started = Instant::now();
        // Once ssh is forwarding, a refused ping means the far socket is not
        // Docker; a few tries cover a daemon that is just starting.
        let mut refused = 0;
        loop {
            if child
                .try_wait()
                .map_err(|e| Error::io("waiting for ssh", e))?
                .is_some()
            {
                let stderr = std::fs::read_to_string(&log).unwrap_or_default();
                return Err(ssh_error(host, &stderr));
            }
            // ssh accepts local connections even when the remote socket is
            // missing or forbidden, so ask Docker a question through it.
            if socket.exists() {
                if docker_answers(&socket, Duration::from_secs(3)) {
                    return Ok(tunnel(child));
                }
                refused += 1;
            }
            if refused >= 6 || started.elapsed() > CONNECT_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                return Err(no_answer());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Command mode: Docker is reached by running a command on the host. The
    /// Docker client can carry a whole API conversation over a command's
    /// input and output (`docker system dial-stdio`), so a local socket is
    /// served by running `<command> system dial-stdio` over ssh for each
    /// connection.
    #[cfg(unix)]
    fn open_command(host: &Host, command: &str) -> Result<Tunnel> {
        let command = command.trim();
        if command.is_empty() || command.chars().any(char::is_control) {
            return Err(Error::Other(
                "the Docker command is empty or contains control characters".into(),
            ));
        }
        let dir = tempfile::Builder::new()
            .prefix("zevo-")
            .tempdir()
            .map_err(|e| Error::io("creating a temporary folder", e))?;
        let socket = dir.path().join("d.sock");
        let log = dir.path().join("ssh.log");
        let control = dir.path().join("cm");
        let destination = host.ssh.clone();
        let remote = format!(
            "sh -c {}",
            shell_quote(&format!("{command} system dial-stdio"))
        );

        let spawn = {
            let (log, control, destination, remote) =
                (log.clone(), control.clone(), destination.clone(), remote);
            move || {
                let mut ssh = ssh();
                // Share one connection between calls, so each API request
                // does not pay for a new SSH handshake.
                ssh.arg("-o")
                    .arg("ControlMaster=auto")
                    .arg("-o")
                    .arg(format!("ControlPath={}", control.display()))
                    .arg("-o")
                    .arg("ControlPersist=60")
                    .arg("-T")
                    .arg("--")
                    .arg(&destination)
                    .arg(&remote);
                if let Ok(file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log)
                {
                    ssh.stderr(file);
                }
                ssh
            }
        };
        let close = {
            let (control, destination) = (control.clone(), destination.clone());
            move || {
                let _ = ssh()
                    .arg("-o")
                    .arg(format!("ControlPath={}", control.display()))
                    .arg("-O")
                    .arg("exit")
                    .arg("--")
                    .arg(&destination)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        };
        let proxy = CommandProxy::start(&socket, spawn, close)?;

        // The first request also pays for the SSH handshake.
        if !docker_answers(&socket, CONNECT_TIMEOUT) {
            let stderr = std::fs::read_to_string(&log).unwrap_or_default();
            let detail = stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("no details")
                .trim();
            return Err(Error::Runtime(format!(
                "ran `{command} system dial-stdio` on {}, but Docker did not answer through it: {detail}",
                host.ssh
            )));
        }
        Ok(Tunnel {
            child: None,
            proxy: Some(proxy),
            dir,
            remote_socket: String::new(),
        })
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

    /// The Docker socket on the host that this connection reaches.
    pub fn remote_socket(&self) -> &str {
        &self.remote_socket
    }

    /// Whether the ssh process is still running.
    pub fn alive(&mut self) -> bool {
        match &mut self.child {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            // Command mode has no long-lived process to watch; each request
            // reports its own failure.
            None => true,
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Stops the proxy before `dir` (and the socket in it) is removed.
        self.proxy.take();
    }
}

/// Serves a local socket by running a command for each connection and
/// copying bytes both ways between the connection and the command.
struct CommandProxy {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    socket: PathBuf,
    close: Option<Box<dyn FnOnce() + Send>>,
}

#[cfg(unix)]
impl CommandProxy {
    fn start(
        socket: &std::path::Path,
        spawn: impl Fn() -> Command + Send + Sync + 'static,
        close: impl FnOnce() + Send + 'static,
    ) -> Result<CommandProxy> {
        use std::os::unix::net::UnixListener;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let listener =
            UnixListener::bind(socket).map_err(|e| Error::io("creating a local socket", e))?;
        let stop = Arc::new(AtomicBool::new(false));
        let spawn = Arc::new(spawn);
        std::thread::spawn({
            let stop = stop.clone();
            move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let spawn = spawn.clone();
                    std::thread::spawn(move || serve(stream, spawn()));
                }
            }
        });
        Ok(CommandProxy {
            stop,
            socket: socket.to_path_buf(),
            close: Some(Box::new(close)),
        })
    }
}

/// Copies bytes as they arrive until either side closes. Written out by
/// hand: each chunk must be forwarded at once, as the two ends are holding
/// a request/response conversation.
fn pump(mut from: impl Read, mut to: impl Write) {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buffer[..n]).is_err() || to.flush().is_err() {
                    break;
                }
            }
        }
    }
}

/// Connects one client to one run of the command.
#[cfg(unix)]
fn serve(stream: std::os::unix::net::UnixStream, mut command: Command) {
    let Ok(mut child) = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn() else {
        return;
    };
    let (Some(to_command), Some(from_command)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return;
    };
    let Ok(client_reader) = stream.try_clone() else {
        let _ = child.kill();
        return;
    };
    // Client to command, until the client hangs up; dropping the pipe then
    // closes the command's input.
    let upload = std::thread::spawn(move || pump(client_reader, to_command));
    // Command to client, until the command exits.
    pump(from_command, &stream);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = child.kill();
    let _ = child.wait();
    let _ = upload.join();
}

impl Drop for CommandProxy {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Wake the accept loop so it sees the flag.
        #[cfg(unix)]
        let _ = std::os::unix::net::UnixStream::connect(&self.socket);
        if let Some(close) = self.close.take() {
            close();
        }
    }
}

/// Sends Docker's ping request over the socket and looks for a 200.
#[cfg(unix)]
fn docker_answers(socket: &std::path::Path, timeout: Duration) -> bool {
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
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
    docker_socket: Option<&str>,
    docker_command: Option<&str>,
) -> Result<(Host, String)> {
    let mut host = Host::from_ssh(ssh_destination, name, address)?;
    // A name from the SSH config means nothing to Traefik; route to the
    // machine it stands for.
    if address.is_none_or(|a| a.trim().is_empty())
        && let Some(real) = resolved_hostname(&host.ssh)
    {
        host.address = real;
    }
    host.docker_socket = docker_socket
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    host.docker_command = docker_command
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let tunnel = Tunnel::open(&host)?;
    let version = tunnel.runtime()?.ping().await?;
    // Remember the socket that answered, so later connections go straight to it.
    if !tunnel.remote_socket().is_empty() {
        host.docker_socket = Some(tunnel.remote_socket().to_string());
    }
    store.add_host(&host)?;
    Ok((host, version))
}

/// The address to put in a route for a host: an IP address as is, or the
/// current IP of a name. Looked up here, where the machine's own resolver
/// (including mDNS and VPN names) works; Traefik, often in a container, may
/// not be able to resolve the name itself. Falls back to the name.
pub fn routable_address(address: &str) -> String {
    use std::net::ToSocketAddrs;
    if address.parse::<std::net::IpAddr>().is_ok() {
        return address.to_string();
    }
    let Ok(resolved) = (address, 0).to_socket_addrs() else {
        return address.to_string();
    };
    let resolved: Vec<_> = resolved.map(|a| a.ip()).collect();
    // Prefer IPv4: it needs no brackets and is what most LANs route.
    resolved
        .iter()
        .find(|ip| ip.is_ipv4())
        .or(resolved.first())
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| address.to_string())
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

    #[test]
    fn routable_addresses_resolve_names_and_keep_ips() {
        assert_eq!(routable_address("192.168.1.20"), "192.168.1.20");
        assert_eq!(routable_address("fd7a:115c:a1e0::1"), "fd7a:115c:a1e0::1");
        assert_eq!(routable_address("localhost"), "127.0.0.1");
        // A name that does not resolve is passed through for Traefik to try.
        assert_eq!(
            routable_address("no-such-machine.invalid"),
            "no-such-machine.invalid"
        );
    }

    #[test]
    fn the_socket_search_is_plain_posix_shell() {
        // Runs locally: the script must work under sh and never fail.
        let output = Command::new("sh")
            .arg("-c")
            .arg(FIND_SOCKETS)
            .output()
            .unwrap();
        assert!(output.status.success());
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            assert!(line.starts_with('/'), "{line}");
        }
    }

    /// The command-mode proxy against the real Docker CLI on this machine,
    /// without ssh. Needs Docker, so it only runs when asked for:
    /// `cargo test -p zevo-core -- --ignored command_proxy`.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn command_proxy_carries_the_docker_api() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("d.sock");
        let spawn = || {
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg("docker system dial-stdio")
                .stderr(Stdio::null());
            command
        };
        let _proxy = CommandProxy::start(&socket, spawn, || {}).unwrap();
        assert!(docker_answers(&socket, Duration::from_secs(5)));
        let runtime = DockerRuntime::connect_socket(&socket).unwrap();
        let version = runtime.ping().await.unwrap();
        assert!(!version.is_empty());
        // Several requests, including a larger response, over fresh connections.
        let containers = runtime.containers().await.unwrap();
        assert!(!containers.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_that_is_not_docker_does_not_count_as_connected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        assert!(!docker_answers(&path, Duration::from_secs(2)));
        // Something listens, accepts, and says nothing useful.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"HTTP/1.0 502 Bad Gateway\r\n\r\n");
            }
        });
        assert!(!docker_answers(&path, Duration::from_secs(2)));
        server.join().unwrap();
    }
}
