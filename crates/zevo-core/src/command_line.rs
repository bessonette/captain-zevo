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

/// Makes the bundled `zevo` runnable from a terminal: in `/usr/local/bin`
/// if this user may write there, else in `~/.local/bin`. Never asks for a
/// password.
pub fn install() -> Result<Installed> {
    let source = bundled().ok_or_else(|| {
        Error::Other("this copy of the app did not come with the zevo command".into())
    })?;
    let mut dirs = vec![PathBuf::from("/usr/local/bin")];
    let home = directories::BaseDirs::new().map(|d| d.home_dir().join(".local/bin"));
    dirs.extend(home.clone());
    // An AppImage is mounted somewhere new on every start, so a link into
    // it would break when the app closes.
    let copy = std::env::var_os("APPIMAGE").is_some();
    let path = install_into(&source, &dirs, copy)?;
    let on_path = std::env::var_os("PATH").is_some_and(|all| {
        std::env::split_paths(&all).any(|dir| Some(dir.as_path()) == path.parent())
    });
    let note = (!on_path && path.parent() == home.as_deref())
        .then(|| "If a terminal does not find `zevo`, add ~/.local/bin to your PATH.".to_string());
    Ok(Installed { path, note })
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
