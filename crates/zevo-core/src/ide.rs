//! Opening a project in an editor, attached to its devcontainer.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::model::{Host, Project};
use crate::{Error, Result, devcontainer, programs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Editor {
    VsCode,
    Cursor,
    Windsurf,
    VsCodium,
    Zed,
}

impl Editor {
    pub const ALL: [Editor; 5] = [
        Editor::VsCode,
        Editor::Cursor,
        Editor::Windsurf,
        Editor::VsCodium,
        Editor::Zed,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Editor::VsCode => "VS Code",
            Editor::Cursor => "Cursor",
            Editor::Windsurf => "Windsurf",
            Editor::VsCodium => "VSCodium",
            Editor::Zed => "Zed",
        }
    }

    /// Command-line launcher name, and on macOS the app bundle that ships it.
    fn launcher(self) -> (&'static str, &'static str) {
        match self {
            Editor::VsCode => ("code", "Visual Studio Code.app"),
            Editor::Cursor => ("cursor", "Cursor.app"),
            Editor::Windsurf => ("windsurf", "Windsurf.app"),
            Editor::VsCodium => ("codium", "VSCodium.app"),
            Editor::Zed => ("zed", "Zed.app"),
        }
    }

    /// Where this editor's launcher is installed, if it is.
    pub fn program(self) -> Option<PathBuf> {
        let (name, bundle) = self.launcher();
        let mut extra = Vec::new();
        if cfg!(target_os = "macos") {
            let home = directories::BaseDirs::new().map(|d| d.home_dir().join("Applications"));
            for apps in std::iter::once(PathBuf::from("/Applications")).chain(home) {
                // VS Code and its forks keep the launcher inside the bundle.
                extra.push(apps.join(bundle).join("Contents/Resources/app/bin"));
                extra.push(apps.join(bundle).join("Contents/MacOS"));
            }
        }
        programs::find(name, &extra)
    }

    fn attaches_to_devcontainers(self) -> bool {
        !matches!(self, Editor::Zed)
    }
}

/// Editors installed on this machine, in a stable order.
pub fn available() -> Vec<Editor> {
    Editor::ALL
        .into_iter()
        .filter(|e| e.program().is_some())
        .collect()
}

/// The address VS Code and its forks use for "this folder, opened inside its
/// devcontainer": the host folder hex-encoded, then the folder inside the
/// container.
pub fn devcontainer_uri(host_folder: &Path, workspace_folder: &str) -> String {
    let hex: String = host_folder
        .to_string_lossy()
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("vscode-remote://dev-container+{hex}{workspace_folder}")
}

/// Folder the project is mounted at inside its container: the configured
/// `workspaceFolder`, or the devcontainer default.
fn workspace_folder(project: &Project) -> String {
    let configured = devcontainer::detect(&project.path)
        .ok()
        .and_then(|d| d.workspace_folder);
    configured.unwrap_or_else(|| {
        let name = project
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        format!("/workspaces/{}", name.unwrap_or_default())
    })
}

/// Arguments that open the project in the editor. `host` is the remote
/// machine the project lives on, if it is not this one.
pub fn arguments(editor: Editor, project: &Project, host: Option<&Host>) -> Vec<String> {
    let path = project.path.to_string_lossy();
    if let Some(host) = host {
        // Open the folder over SSH; the editor then offers to reopen it in
        // its container.
        return if editor.attaches_to_devcontainers() {
            vec![
                "--folder-uri".to_string(),
                format!("vscode-remote://ssh-remote+{}{path}", host.ssh),
            ]
        } else {
            vec![format!("ssh://{}{path}", host.ssh)]
        };
    }
    let has_devcontainer = project.kind != crate::model::DevcontainerKind::None;
    if editor.attaches_to_devcontainers() && has_devcontainer {
        vec![
            "--folder-uri".to_string(),
            devcontainer_uri(&project.path, &workspace_folder(project)),
        ]
    } else {
        // Zed opens the folder and offers to reopen it in the container.
        vec![path.into_owned()]
    }
}

/// Starts the editor on the project and returns without waiting for it.
pub fn open(editor: Editor, project: &Project, host: Option<&Host>) -> Result<()> {
    let program = editor.program().ok_or_else(|| {
        Error::Other(format!("{} does not appear to be installed", editor.name()))
    })?;
    Command::new(&program)
        .args(arguments(editor, project, host))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| Error::io(format!("starting {}", program.display()), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DevcontainerKind;

    fn project(path: &Path, kind: DevcontainerKind) -> Project {
        Project {
            slug: "shop".into(),
            name: "Shop".into(),
            host: "local".into(),
            path: path.to_path_buf(),
            kind,
            service: None,
            routes: vec![],
            tags: vec![],
            declared: false,
            address: None,
            env_file: None,
        }
    }

    #[test]
    fn devcontainer_uri_hex_encodes_the_host_folder() {
        assert_eq!(
            devcontainer_uri(Path::new("/home/me/shop"), "/workspaces/shop"),
            "vscode-remote://dev-container+2f686f6d652f6d652f73686f70/workspaces/shop"
        );
    }

    #[test]
    fn remote_projects_open_over_ssh() {
        let host = Host::from_ssh("dev@10.0.0.5", Some("buildbox"), None).unwrap();
        let p = project(Path::new("/srv/billing"), DevcontainerKind::Compose);
        assert_eq!(
            arguments(Editor::VsCode, &p, Some(&host)),
            [
                "--folder-uri",
                "vscode-remote://ssh-remote+dev@10.0.0.5/srv/billing"
            ]
        );
        assert_eq!(
            arguments(Editor::Zed, &p, Some(&host)),
            ["ssh://dev@10.0.0.5/srv/billing"]
        );
    }

    #[test]
    fn arguments_follow_the_editor_and_the_workspace_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("shop");
        std::fs::create_dir_all(dir.join(".devcontainer")).unwrap();
        let p = project(&dir, DevcontainerKind::Image);

        // No workspaceFolder set: the devcontainer default.
        std::fs::write(
            dir.join(".devcontainer/devcontainer.json"),
            r#"{"image":"node"}"#,
        )
        .unwrap();
        let args = arguments(Editor::VsCode, &p, None);
        assert_eq!(args[0], "--folder-uri");
        assert!(args[1].ends_with("/workspaces/shop"), "{}", args[1]);

        std::fs::write(
            dir.join(".devcontainer/devcontainer.json"),
            r#"{"image":"node","workspaceFolder":"/srv/app"}"#,
        )
        .unwrap();
        assert!(arguments(Editor::Cursor, &p, None)[1].ends_with("/srv/app"));

        // Zed, and folders without a devcontainer, just open the folder.
        assert_eq!(arguments(Editor::Zed, &p, None), [dir.to_string_lossy()]);
        assert_eq!(
            arguments(Editor::VsCode, &project(&dir, DevcontainerKind::None), None),
            [dir.to_string_lossy()]
        );
    }
}
