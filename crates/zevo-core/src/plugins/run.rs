//! Running plugin programs: a JSON request on standard input, a reply on
//! standard output, the way Zevo already drives mkcert and ssh.
//!
//! Nothing a program does can break Zevo: a missing program, a non-zero
//! exit, output that is not text or a program that never finishes all come
//! back as an error message for the caller to turn into a warning.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::store::CONFIG_DIR_ENV;

/// Folder, inside the plugins folder, where Zevo also looks for programs
/// named without a path (and for `zevo-<name>` subcommands).
pub const BIN_DIR: &str = "bin";

/// How long to keep reading a program's output after it has exited. Output
/// already written arrives at once; this only matters when something the
/// program left running still holds the pipes open, which must not hold up
/// the caller.
const OUTPUT_GRACE: Duration = Duration::from_millis(250);

/// Longest stretch of a program's error output quoted in a message.
const STDERR_LIMIT: usize = 300;

/// `<config_dir>/plugins/bin`.
pub fn bin_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(super::PLUGINS_DIR).join(BIN_DIR)
}

/// The executable a command's program names. A bare name is looked up the
/// way Zevo finds its other tools (the desktop app does not get the shell's
/// PATH), then in [`bin_dir`]; a path is used as it is.
pub fn resolve(program: &str, config_dir: &Path) -> Option<PathBuf> {
    if program.contains('/') || program.contains('\\') {
        return Some(PathBuf::from(program));
    }
    crate::programs::find(program, &[bin_dir(config_dir)])
}

/// Runs `command` with `request` on its standard input and returns what it
/// printed on standard output, or why it did not work. The request gets a
/// `protocol` field. `ZEVO_CONFIG_DIR` is set to `config_dir`, so a program
/// that calls `zevo` sees the same registry. The program is killed if it
/// has not finished after `timeout`.
///
/// Blocks the calling thread; async code uses [`run`].
pub fn run_blocking(
    command: &[String],
    request: serde_json::Value,
    config_dir: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let Some((program, args)) = command.split_first() else {
        return Err("has an empty command".to_string());
    };
    let Some(path) = resolve(program, config_dir) else {
        return Err(format!(
            "could not find the program '{program}' (looked on the PATH and in {})",
            bin_dir(config_dir).display()
        ));
    };
    let mut request = request;
    if let Some(fields) = request.as_object_mut() {
        fields.insert("protocol".to_string(), super::PROTOCOL.into());
    }
    let mut input = request.to_string().into_bytes();
    input.push(b'\n');

    let mut command = Command::new(&path);
    command
        .args(args)
        .env(CONFIG_DIR_ENV, config_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so that on a timeout everything it started can
    // be stopped with it.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", path.display()))?;

    // Writing and reading happen on their own threads so a program that
    // fills one pipe before reading the other cannot deadlock with Zevo.
    // Dropping the writer's handle closes standard input.
    if let Some(mut stdin) = child.stdin.take() {
        std::thread::spawn(move || {
            // A program that does not read its input is not an error.
            let _ = stdin.write_all(&input);
        });
    }
    let stdout = collect(child.stdout.take());
    let stderr = collect(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "did not finish within {} and was stopped",
                    seconds(timeout)
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("could not wait for {}: {e}", path.display())),
        }
    };
    // A program can exit while something it started in the background still
    // holds its output open; that is not waited for.
    let stdout = stdout.recv_timeout(OUTPUT_GRACE).unwrap_or_default();
    let stderr = stderr.recv_timeout(OUTPUT_GRACE).unwrap_or_default();

    if !status.success() {
        let how = match status.code() {
            Some(code) => format!("exited with status {code}"),
            None => "was stopped by a signal".to_string(),
        };
        let said = quote(&String::from_utf8_lossy(&stderr));
        return Err(if said.is_empty() {
            how
        } else {
            format!("{how}: {said}")
        });
    }
    String::from_utf8(stdout).map_err(|_| "printed output that is not text".to_string())
}

/// [`run_blocking`] on a thread set aside for blocking work, so the async
/// runtime keeps going while the program runs.
pub async fn run(
    command: &[String],
    request: serde_json::Value,
    config_dir: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let command = command.to_vec();
    let config_dir = config_dir.to_path_buf();
    tokio::task::spawn_blocking(move || run_blocking(&command, request, &config_dir, timeout))
        .await
        .unwrap_or_else(|e| Err(format!("stopped unexpectedly: {e}")))
}

/// A short name for a command in messages: its program's file name, and
/// its first argument's when that is not an option, so that
/// `["python3", "/long/path/notify.py"]` reads as `python3 notify.py`.
pub fn program_name(command: &[String]) -> String {
    let short = |word: &String| {
        Path::new(word)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| word.clone())
    };
    let mut words: Vec<String> = command.iter().take(1).map(short).collect();
    if let Some(first) = command.get(1).filter(|arg| !arg.starts_with('-')) {
        words.push(short(first));
    }
    words.join(" ")
}

/// Text a program printed, cut down to fit in a one-line message.
pub fn quote(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(STDERR_LIMIT) {
        Some((at, _)) => format!("{}...", &text[..at]),
        None => text,
    }
}

/// Stops every process in the group the program was started in: a shell
/// script's own children would otherwise outlive it and keep its pipes open.
/// Done with the `kill` command because signalling a group from Rust needs
/// unsafe code.
fn kill_group(id: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{id}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    let _ = id;
}

fn seconds(duration: Duration) -> String {
    match duration.as_secs() {
        1 => "1 second".to_string(),
        0 => format!("{} ms", duration.as_millis()),
        n => format!("{n} seconds"),
    }
}

/// Reads a pipe to the end on its own thread; the bytes arrive on the channel.
fn collect(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (send, receive) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = send.send(bytes);
        });
    }
    receive
}

/// Test helpers: scripts standing in for plugin programs.
#[cfg(all(test, unix))]
pub(crate) mod testing {
    use std::path::Path;

    /// A command that runs `body` with `/bin/sh`. Running the shell, not the
    /// script itself, avoids "text file busy" when tests run in parallel.
    pub fn script(dir: &Path, name: &str, body: &str) -> Vec<String> {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        vec!["/bin/sh".to_string(), path.to_string_lossy().into_owned()]
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::testing::script;
    use super::*;

    const SHORT: Duration = Duration::from_secs(10);

    #[test]
    fn sends_the_request_with_the_protocol_and_returns_the_output() {
        let tmp = tempfile::tempdir().unwrap();
        let mut command = script(tmp.path(), "echo.sh", "cat\necho \"arg=$1\"\n");
        command.push("one".into());
        let out = run_blocking(
            &command,
            serde_json::json!({"request": "ready"}),
            tmp.path(),
            SHORT,
        )
        .unwrap();
        let mut lines = out.lines();
        let sent: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({"protocol": crate::plugins::PROTOCOL, "request": "ready"})
        );
        assert_eq!(lines.next(), Some("arg=one"));
    }

    #[test]
    fn sets_the_config_dir_for_the_program() {
        let tmp = tempfile::tempdir().unwrap();
        let command = script(tmp.path(), "env.sh", "printf '%s' \"$ZEVO_CONFIG_DIR\"\n");
        let out = run_blocking(&command, serde_json::json!({}), tmp.path(), SHORT).unwrap();
        assert_eq!(out, tmp.path().to_string_lossy());
    }

    #[test]
    fn a_failing_program_reports_its_status_and_error_output() {
        let tmp = tempfile::tempdir().unwrap();
        let command = script(tmp.path(), "fail.sh", "echo 'no  token\nset' >&2\nexit 3\n");
        let err = run_blocking(&command, serde_json::json!({}), tmp.path(), SHORT).unwrap_err();
        assert_eq!(err, "exited with status 3: no token set");
    }

    #[test]
    fn a_program_that_hangs_is_stopped_at_the_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let command = script(tmp.path(), "hang.sh", "exec sleep 30\n");
        let started = Instant::now();
        let err = run_blocking(
            &command,
            serde_json::json!({}),
            tmp.path(),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert_eq!(err, "did not finish within 300 ms and was stopped");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run_blocking(
            &["zevo-no-such-plugin-program".to_string()],
            serde_json::json!({}),
            tmp.path(),
            SHORT,
        )
        .unwrap_err();
        assert!(err.contains("could not find the program"), "{err}");
        let err = run_blocking(
            &[tmp.path().join("gone").to_string_lossy().into_owned()],
            serde_json::json!({}),
            tmp.path(),
            SHORT,
        )
        .unwrap_err();
        assert!(err.starts_with("could not start"), "{err}");
    }

    #[test]
    fn bare_names_are_also_found_in_the_plugins_bin_folder() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let bin = bin_dir(tmp.path());
        std::fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("zevo-test-plugin-tool");
        std::fs::write(&tool, "#!/bin/sh\necho found\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resolve("zevo-test-plugin-tool", tmp.path()), Some(tool));
    }

    #[tokio::test]
    async fn the_async_runner_gives_the_same_result() {
        let tmp = tempfile::tempdir().unwrap();
        let command = script(tmp.path(), "ok.sh", "echo '{\"ok\":true}'\n");
        let out = run(&command, serde_json::json!({}), tmp.path(), SHORT)
            .await
            .unwrap();
        assert_eq!(out.trim(), r#"{"ok":true}"#);
    }

    #[test]
    fn long_error_output_is_shortened() {
        let quoted = quote(&"x".repeat(1000));
        assert_eq!(quoted.len(), STDERR_LIMIT + 3);
        assert_eq!(program_name(&["/a/b/notify.sh".into()]), "notify.sh");
        assert_eq!(
            program_name(&["python3".into(), "/a/b/notify.py".into(), "-q".into()]),
            "python3 notify.py"
        );
        assert_eq!(program_name(&["notify".into(), "--quiet".into()]), "notify");
    }
}
