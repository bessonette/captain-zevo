//! Finding command-line programs. An app started from the Finder or a
//! desktop launcher does not get the shell's PATH, so the PATH alone is not
//! enough to find tools the user installed.

use std::path::{Path, PathBuf};

/// The first executable called `name` on the PATH, then in common install
/// locations, then in `extra_dirs`.
pub fn find(name: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf> {
    let file = if cfg!(windows) && Path::new(name).extension().is_none() {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    dirs.extend(
        [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/opt/local/bin",
            "/home/linuxbrew/.linuxbrew/bin",
        ]
        .map(PathBuf::from),
    );
    if let Some(base) = directories::BaseDirs::new() {
        dirs.extend([".local/bin", "go/bin", ".nix-profile/bin"].map(|d| base.home_dir().join(d)));
    }
    dirs.extend(extra_dirs.iter().cloned());
    dirs.into_iter()
        .map(|dir| dir.join(&file))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn finds_only_executables_including_in_extra_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("zevo-test-tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        assert_eq!(find("zevo-test-tool", &[tmp.path().to_path_buf()]), None);
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find("zevo-test-tool", &[tmp.path().to_path_buf()]),
            Some(tool)
        );
        assert_eq!(find("zevo-test-tool", &[]), None);
    }
}
