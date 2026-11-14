//! Putting the `zevo` command within reach of a terminal.
//!
//! The desktop app ships with the command-line tool beside its own program.
//! A package manager puts both on the PATH; an app dragged into a folder
//! (macOS) or run from an AppImage does not, so the app offers to do it.

use std::path::{Path, PathBuf};

use crate::{Error, Result, programs};

/// File name of the command-line tool.
pub const PROGRAM: &str = if cfg!(windows) { "zevo.exe" } else { "zevo" };

/// Where the `zevo` command stands on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// A terminal can already run it; this is the one it finds.
    Installed(PathBuf),
    /// The app has one, at this path, that is not on the PATH yet.
    Bundled(PathBuf),
    /// Neither: the app was built without it.
    Missing,
}

/// The command-line tool that came with the running program: the `zevo`
/// in the same folder, if there is one.
pub fn bundled() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join(PROGRAM);
    candidate.is_file().then_some(candidate)
}

pub fn status() -> Status {
    match (programs::find("zevo", &[]), bundled()) {
        (Some(found), _) => Status::Installed(found),
        (None, Some(bundled)) => Status::Bundled(bundled),
        (None, None) => Status::Missing,
    }
}

/// What [`install`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub path: PathBuf,
    /// Set when the folder is probably not on the terminal's PATH: what the
    /// user has to do about it.
    pub note: Option<String>,
}

/// Folder every shell searches, on macOS and Linux alike. Usually only an
/// administrator may write there.
pub const SYSTEM_DIR: &str = "/usr/local/bin";

/// Where to put the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// [`SYSTEM_DIR`]. On macOS, if this user may not write there, the
    /// system asks for an administrator's password; elsewhere it fails.
    System,
    /// `~/.local/bin`: never needs a password, but the shell has to be told
    /// to search it (see [`path_hint`]).
    Home,
    /// [`SYSTEM_DIR`] if this user may write there, else `~/.local/bin`.
    /// Never asks for a password.
    Auto,
}

fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".local/bin"))
}

/// Makes the bundled `zevo` runnable from a terminal.
pub fn install(place: Place) -> Result<Installed> {
    let source = bundled().ok_or_else(|| {
        Error::Other("this copy of the app did not come with the zevo command".into())
    })?;
    let system = PathBuf::from(SYSTEM_DIR);
    let home = home_dir();
    // An AppImage is mounted somewhere new on every start, so a link into
    // it would break when the app closes.
    let copy = std::env::var_os("APPIMAGE").is_some();
    let path = match place {
        Place::Auto => {
            let dirs: Vec<PathBuf> = std::iter::once(system).chain(home).collect();
            install_into(&source, &dirs, copy)?
        }
        Place::Home => install_into(&source, &home.into_iter().collect::<Vec<_>>(), copy)?,
        Place::System => {
            let target = system.join(PROGRAM);
            match install_into(&source, std::slice::from_ref(&system), copy) {
                Ok(path) => path,
                // Something of the user's own is in the way: asking for a
                // password would only be a way to overwrite it.
                Err(e) if in_the_way(&target) => return Err(e),
                Err(e) => {
                    install_as_administrator(&source, &target).map_err(|reason| {
                        Error::Other(match reason {
                            Some(reason) => reason,
                            None => e.to_string(),
                        })
                    })?;
                    target
                }
            }
        }
    };
    // A link left in the other folder by an earlier install would shadow
    // or outlive this one.
    if path.parent() == Some(Path::new(SYSTEM_DIR))
        && let Some(old) = home_dir().map(|dir| dir.join(PROGRAM))
        && std::fs::read_link(&old).is_ok_and(|to| to == source)
    {
        let _ = std::fs::remove_file(&old);
    }
    let note = path_hint(&path);
    Ok(Installed { path, note })
}

/// Whether a `zevo` that is not a link sits at `target`: one the user put
/// there, which no install replaces.
fn in_the_way(target: &Path) -> bool {
    std::fs::symlink_metadata(target).is_ok_and(|m| !m.file_type().is_symlink())
}

/// The shell command, run as an administrator, that links `source` as
/// `target`. Both are quoted so that no path can add a command of its own.
fn link_command(source: &Path, target: &Path) -> String {
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"));
    let dir = target.parent().unwrap_or(Path::new("/"));
    format!(
        "/bin/mkdir -p {} && /bin/ln -sf {} {}",
        quote(dir),
        quote(source),
        quote(target)
    )
}

/// AppleScript that runs `command` after the system's own password prompt,
/// which says `reason`.
fn administrator_script(command: &str, reason: &str) -> String {
    let quote = |text: &str| format!("\"{}\"", text.replace('\\', r"\\").replace('"', "\\\""));
    format!(
        "do shell script {} with prompt {} with administrator privileges",
        quote(command),
        quote(reason)
    )
}

/// Links `source` as `target` with an administrator's rights, through the
/// password prompt macOS shows for it. `Err(None)` where there is no such
/// prompt (the caller reports why the plain attempt failed).
fn install_as_administrator(
    source: &Path,
    target: &Path,
) -> std::result::Result<(), Option<String>> {
    if !cfg!(target_os = "macos") {
        return Err(None);
    }
    let script = administrator_script(
        &link_command(source, target),
        &format!(
            "Captain Zevo wants to add the zevo command to {}.",
            target.parent().unwrap_or(Path::new("/")).display()
        ),
    );
    let output = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            Some(format!(
                "could not ask for an administrator's password: {e}"
            ))
        })?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    // -128 is AppleScript's "the user cancelled".
    Err(Some(if said.contains("-128") {
        "Nothing was installed: the password prompt was cancelled.".to_string()
    } else {
        format!("could not install the zevo command: {}", said.trim())
    }))
}

/// What to tell the user when the `zevo` at `installed` may be out of a
/// terminal's reach: it is in `~/.local/bin`, which a shell searches only
/// if its startup file says so (macOS's zsh does not by default). `None`
/// when it is somewhere every shell looks, or somewhere the user set up.
///
/// The app cannot simply look at its own PATH: started from the Finder or a
/// launcher it does not have the shell's.
pub fn path_hint(installed: &Path) -> Option<String> {
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    hint_for(
        installed,
        &home,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("SHELL").as_deref(),
    )
}

fn hint_for(
    installed: &Path,
    home: &Path,
    path: Option<&std::ffi::OsStr>,
    shell: Option<&std::ffi::OsStr>,
) -> Option<String> {
    let dir = installed.parent()?;
    if dir != home.join(".local/bin") {
        return None;
    }
    if path.is_some_and(|all| std::env::split_paths(all).any(|d| d == dir)) {
        return None;
    }
    let shell = shell.map(Path::new).and_then(Path::file_name);
    let line = r#"export PATH="$HOME/.local/bin:$PATH""#;
    let file = match shell.and_then(|name| name.to_str()) {
        Some("zsh") => "~/.zshrc",
        Some("bash") => "~/.bashrc",
        Some("fish") => {
            return Some(
                "If a terminal does not find zevo, run: fish_add_path ~/.local/bin".to_string(),
            );
        }
        _ => "your shell's startup file",
    };
    Some(format!(
        "If a terminal does not find zevo, add this line to {file} and open a new terminal: {line}"
    ))
}

/// Links (or copies) `source` as `zevo` into the first of `dirs` that takes
/// it, creating the folder if it can. Something else of that name is only
/// replaced if it is a link, or when copying: a link is what an earlier
/// install left, a file may be a `zevo` the user built.
fn install_into(source: &Path, dirs: &[PathBuf], copy: bool) -> Result<PathBuf> {
    let mut failures = Vec::new();
    for dir in dirs {
        let target = dir.join(PROGRAM);
        match place(source, &target, copy) {
            Ok(()) => return Ok(target),
            Err(reason) => failures.push(format!("{}: {reason}", dir.display())),
        }
    }
    Err(Error::Other(format!(
        "could not install the zevo command ({})",
        failures.join("; ")
    )))
}

#[cfg(unix)]
fn place(source: &Path, target: &Path, copy: bool) -> std::result::Result<(), String> {
    let text = |e: std::io::Error| e.to_string();
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(text)?;
    }
    match std::fs::symlink_metadata(target) {
        Ok(existing) if existing.file_type().is_symlink() || copy => {
            std::fs::remove_file(target).map_err(text)?;
        }
        Ok(_) => return Err("another zevo is already there".to_string()),
        Err(_) => {}
    }
    if copy {
        // `copy` keeps the permissions, so the result is executable.
        std::fs::copy(source, target).map(|_| ()).map_err(text)
    } else {
        std::os::unix::fs::symlink(source, target).map_err(text)
    }
}

#[cfg(not(unix))]
fn place(_source: &Path, _target: &Path, _copy: bool) -> std::result::Result<(), String> {
    Err("not supported on Windows yet; add the app's folder to your PATH".to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn program(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn the_hint_is_only_for_a_folder_the_shell_may_not_search() {
        use std::ffi::OsStr;
        let home = Path::new("/Users/me");
        let local = Path::new("/Users/me/.local/bin/zevo");
        let zsh = Some(OsStr::new("/bin/zsh"));
        // Started from the Finder: a PATH without the user's own folders.
        let finder = Some(OsStr::new("/usr/bin:/bin"));

        let hint = hint_for(local, home, finder, zsh).unwrap();
        assert!(hint.contains("~/.zshrc"), "{hint}");
        assert!(
            hint.contains(r#"export PATH="$HOME/.local/bin:$PATH""#),
            "{hint}"
        );
        let fish = Some(OsStr::new("/opt/homebrew/bin/fish"));
        assert!(
            hint_for(local, home, finder, fish)
                .unwrap()
                .contains("fish_add_path")
        );
        assert!(
            hint_for(local, home, finder, None)
                .unwrap()
                .contains("your shell's startup file")
        );

        // Already searched, or installed where every shell looks: nothing to say.
        let with_local = Some(OsStr::new("/usr/bin:/Users/me/.local/bin"));
        assert_eq!(hint_for(local, home, with_local, zsh), None);
        assert_eq!(
            hint_for(Path::new("/usr/local/bin/zevo"), home, finder, zsh),
            None
        );
    }

    #[test]
    fn the_administrator_command_cannot_be_extended_by_a_path() {
        let command = link_command(
            Path::new("/Applications/Captain Zevo.app/Contents/MacOS/zevo"),
            Path::new("/usr/local/bin/zevo"),
        );
        assert_eq!(
            command,
            "/bin/mkdir -p '/usr/local/bin' && /bin/ln -sf \
             '/Applications/Captain Zevo.app/Contents/MacOS/zevo' '/usr/local/bin/zevo'"
        );
        // A quote in the app's path ends neither the shell word...
        let odd = link_command(
            Path::new("/Apps/it's \"x\"; rm -rf ~/zevo"),
            Path::new("/usr/local/bin/zevo"),
        );
        assert!(
            odd.contains(r#"'/Apps/it'\''s "x"; rm -rf ~/zevo'"#),
            "{odd}"
        );
        // ...nor the AppleScript string it is passed in.
        let script = administrator_script(&odd, "Add \"zevo\"");
        assert!(
            script.starts_with("do shell script \"/bin/mkdir"),
            "{script}"
        );
        assert!(script.contains(r#"\"x\"; rm"#), "{script}");
        assert!(script.contains(r#"it'\\''s"#), "{script}");
        assert!(
            script.ends_with(r#"with prompt "Add \"zevo\"" with administrator privileges"#),
            "{script}"
        );
    }

    #[test]
    fn links_into_the_first_folder_that_takes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let source = program(tmp.path(), "bundled-zevo", "one");
        // A file where the first folder should be: it cannot be created.
        std::fs::write(tmp.path().join("blocked"), "").unwrap();
        let dirs = [tmp.path().join("blocked/bin"), tmp.path().join("home/bin")];

        let target = install_into(&source, &dirs, false).unwrap();
        assert_eq!(target, tmp.path().join("home/bin/zevo"));
        assert_eq!(std::fs::read_link(&target).unwrap(), source);

        // Installing again, from an app that moved, replaces the old link.
        let moved = program(tmp.path(), "moved-zevo", "two");
        install_into(&moved, &dirs, false).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "two");
    }

    #[test]
    fn a_zevo_the_user_put_there_is_left_alone_unless_copying() {
        let tmp = tempfile::tempdir().unwrap();
        let source = program(tmp.path(), "bundled-zevo", "bundled");
        let dirs = [tmp.path().join("bin")];
        std::fs::create_dir_all(&dirs[0]).unwrap();
        let theirs = program(&dirs[0], "zevo", "theirs");

        let err = install_into(&source, &dirs, false).unwrap_err().to_string();
        assert!(err.contains("another zevo is already there"), "{err}");
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");

        // A copy (the AppImage case) is how an earlier install looks too, so
        // it is replaced, and stays executable.
        let target = install_into(&source, &dirs, true).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "bundled");
        assert!(!std::fs::symlink_metadata(&target).unwrap().is_symlink());
        assert!(programs::find("zevo", std::slice::from_ref(&dirs[0])).is_some());
    }
}
