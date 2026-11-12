//! Opening a project in an editor, attached to its devcontainer.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::model::{Host, Project};
use crate::{Error, Result, devcontainer, programs, remote};

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

    /// Folder under the user's configuration directory where this editor
    /// keeps its state, if it is one of the VS Code family.
    fn state_folder(self) -> Option<&'static str> {
        match self {
            Editor::VsCode => Some("Code"),
            Editor::Cursor => Some("Cursor"),
            Editor::Windsurf => Some("Windsurf"),
            Editor::VsCodium => Some("VSCodium"),
            Editor::Zed => None,
        }
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

/// The same for a project on another machine: VS Code connects to the host
/// over SSH first, then attaches to the devcontainer there.
pub fn remote_devcontainer_uri(ssh: &str, host_folder: &Path, workspace_folder: &str) -> String {
    let hex: String = host_folder
        .to_string_lossy()
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("vscode-remote://dev-container+{hex}@ssh-remote+{ssh}{workspace_folder}")
}

/// Addresses of devcontainer windows the editor has recorded in its own
/// state files.
///
/// The editor only brings an open window forward when it is asked for exactly
/// the address that window has, and the form of that address varies: recent
/// versions encode a block of settings (the Docker context, the configuration
/// file) where older ones encoded only the folder. Reusing what the editor
/// wrote down is the one way to be sure of a match.
fn recorded_addresses(editor: Editor) -> Vec<String> {
    let (Some(folder), Some(dirs)) = (editor.state_folder(), directories::BaseDirs::new()) else {
        return Vec::new();
    };
    let storage = dirs.config_dir().join(folder).join("User/globalStorage");
    let mut found = Vec::new();
    for file in ["state.vscdb", "storage.json"] {
        if let Ok(bytes) = std::fs::read(storage.join(file)) {
            scan_addresses(&bytes, &mut found);
        }
    }
    found
}

/// Collects every `vscode-remote://dev-container...` address in `bytes`. The
/// state database is scanned as raw bytes, which needs no database library
/// and does not depend on which table or key the editor uses this year.
fn scan_addresses(bytes: &[u8], found: &mut Vec<String>) {
    const START: &[u8] = b"vscode-remote://dev-container";
    let mut at = 0;
    while let Some(offset) = bytes[at..].windows(START.len()).position(|w| w == START) {
        let begin = at + offset;
        let len = bytes[begin..]
            .iter()
            .position(|b| !(b.is_ascii_alphanumeric() || b"%+@._~/:-".contains(b)))
            .unwrap_or(bytes.len() - begin);
        if let Ok(text) = std::str::from_utf8(&bytes[begin..begin + len]) {
            let text = text.to_string();
            if !found.contains(&text) {
                found.push(text);
            }
        }
        at = begin + START.len();
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let pair = (bytes[i] == b'%')
            .then(|| text.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match pair {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_decode(hex: &str) -> Option<String> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..hex.len())
        .step_by(2)
        .map(|i| {
            hex.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect();
    String::from_utf8(bytes?).ok()
}

/// The part of an SSH destination that names the machine: `dev@box` and
/// `box` are the same host to this comparison.
fn machine(ssh: &str) -> &str {
    ssh.rsplit_once('@').map_or(ssh, |(_, machine)| machine)
}

/// Picks, from addresses the editor recorded, the one for this project's
/// devcontainer: same folder on the host, same machine, same folder inside
/// the container. Prefers the settings-block form, which is what the editor
/// writes when it opens a container itself.
fn pick_recorded(
    recorded: &[String],
    host_folder: &Path,
    host: Option<&Host>,
    workspace: &str,
) -> Option<String> {
    let host_folder = host_folder.to_string_lossy();
    let mut best: Option<(bool, String)> = None;
    for address in recorded {
        let Some(rest) = address.strip_prefix("vscode-remote://") else {
            continue;
        };
        let (authority, path) = match rest.find('/') {
            Some(slash) => (&rest[..slash], &rest[slash..]),
            None => continue,
        };
        if percent_decode(path).trim_end_matches('/') != workspace.trim_end_matches('/') {
            continue;
        }
        let authority = percent_decode(authority);
        let Some(container) = authority.strip_prefix("dev-container+") else {
            continue;
        };
        let (hex, over_ssh) = match container.split_once("@ssh-remote+") {
            Some((hex, ssh)) => (hex, Some(ssh)),
            None => (container, None),
        };
        let same_machine = match (host, over_ssh) {
            (None, None) => true,
            (Some(host), Some(ssh)) => {
                let recorded = machine(ssh);
                recorded == machine(&host.ssh) || recorded == host.address
            }
            _ => false,
        };
        if !same_machine {
            continue;
        }
        let Some(decoded) = hex_decode(hex) else {
            continue;
        };
        let settings_block = decoded.starts_with('{');
        let folder = if settings_block {
            serde_json::from_str::<serde_json::Value>(&decoded)
                .ok()
                .and_then(|v| v.get("hostPath")?.as_str().map(str::to_string))
        } else {
            Some(decoded)
        };
        if folder.as_deref() != Some(host_folder.as_ref()) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(block, _)| settings_block && !block)
        {
            best = Some((settings_block, format!("vscode-remote://{authority}{path}")));
        }
    }
    best.map(|(_, address)| address)
}

/// Folder the project is mounted at inside its container: the configured
/// `workspaceFolder`, or the devcontainer default. For a project on another
/// machine the configuration is read over SSH.
fn workspace_folder(project: &Project, host: Option<&Host>) -> String {
    let configured = match host {
        None => devcontainer::detect(&project.path)
            .ok()
            .and_then(|d| d.workspace_folder),
        Some(host) => {
            let folder = project.path.to_string_lossy();
            [".devcontainer/devcontainer.json", ".devcontainer.json"]
                .into_iter()
                .find_map(|candidate| {
                    let config = format!("{folder}/{candidate}");
                    let text = remote::read_file(host, &config).ok().flatten()?;
                    devcontainer::detect_text(Path::new(&config), &text).ok()
                })
                .and_then(|d| d.workspace_folder)
        }
    };
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
    let has_devcontainer = project.kind != crate::model::DevcontainerKind::None;
    let workspace = (editor.attaches_to_devcontainers() && has_devcontainer)
        .then(|| workspace_folder(project, host));
    if let Some(workspace) = &workspace
        && let Some(address) =
            pick_recorded(&recorded_addresses(editor), &project.path, host, workspace)
    {
        return vec!["--folder-uri".to_string(), address];
    }
    arguments_for(editor, project, host, workspace.as_deref())
}

/// [`arguments`], given the folder inside the container (`None` opens the
/// plain folder).
fn arguments_for(
    editor: Editor,
    project: &Project,
    host: Option<&Host>,
    workspace: Option<&str>,
) -> Vec<String> {
    let path = project.path.to_string_lossy();
    let uri = match (host, workspace) {
        (Some(host), Some(workspace)) => {
            remote_devcontainer_uri(&host.ssh, &project.path, workspace)
        }
        (None, Some(workspace)) => devcontainer_uri(&project.path, workspace),
        (Some(host), None) if editor.attaches_to_devcontainers() => {
            format!("vscode-remote://ssh-remote+{}{path}", host.ssh)
        }
        // Zed opens the folder and offers to reopen it in the container.
        (Some(host), None) => return vec![format!("ssh://{}{path}", host.ssh)],
        (None, None) => return vec![path.into_owned()],
    };
    vec!["--folder-uri".to_string(), uri]
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
            skip: Vec::new(),
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
        // Attached to the devcontainer on that machine.
        assert_eq!(
            arguments_for(Editor::VsCode, &p, Some(&host), Some("/workspaces/billing")),
            [
                "--folder-uri",
                "vscode-remote://dev-container+2f7372762f62696c6c696e67@ssh-remote+dev@10.0.0.5/workspaces/billing"
            ]
        );
        // No devcontainer: the folder over SSH.
        assert_eq!(
            arguments_for(Editor::VsCode, &p, Some(&host), None),
            [
                "--folder-uri",
                "vscode-remote://ssh-remote+dev@10.0.0.5/srv/billing"
            ]
        );
        assert_eq!(
            arguments_for(Editor::Zed, &p, Some(&host), None),
            ["ssh://dev@10.0.0.5/srv/billing"]
        );
    }

    #[test]
    fn a_recorded_address_is_reused_so_the_open_window_comes_forward() {
        let folder = Path::new("/Users/me/shop");
        let plain = devcontainer_uri(folder, "/workspaces/shop").replace('+', "%2B");
        let block = r#"{"hostPath":"/Users/me/shop","localDocker":false,"settings":{"context":"desktop-linux"}}"#;
        let hex: String = block.bytes().map(|b| format!("{b:02x}")).collect();
        // As it sits in the editor's state file, among unrelated bytes.
        let mut state = b"\x00junk\"".to_vec();
        state.extend(format!("{plain}\",\"x\":\"").bytes());
        state.extend(format!("vscode-remote://dev-container%2B{hex}/workspaces/shop\"").bytes());
        state.extend(
            format!("vscode-remote://dev-container%2B{hex}/workspaces/shop/src/a.rs\"").bytes(),
        );
        state.extend(
            format!(
                "vscode-remote://dev-container%2B{hex}@ssh-remote%2B10.0.0.5/workspaces/shop\""
            )
            .bytes(),
        );
        let mut recorded = Vec::new();
        scan_addresses(&state, &mut recorded);
        assert_eq!(recorded.len(), 4, "{recorded:?}");

        // Local: the settings-block form wins over the plain one.
        assert_eq!(
            pick_recorded(&recorded, folder, None, "/workspaces/shop").unwrap(),
            format!("vscode-remote://dev-container+{hex}/workspaces/shop")
        );
        // Another folder, or another folder inside the container: no match.
        assert_eq!(
            pick_recorded(
                &recorded,
                Path::new("/Users/me/other"),
                None,
                "/workspaces/shop"
            ),
            None
        );
        assert_eq!(pick_recorded(&recorded, folder, None, "/srv/app"), None);

        // Remote: matched on the machine, whatever user the editor used.
        let host = Host::from_ssh("dev@10.0.0.5", Some("buildbox"), None).unwrap();
        assert_eq!(
            pick_recorded(&recorded, folder, Some(&host), "/workspaces/shop").unwrap(),
            format!("vscode-remote://dev-container+{hex}@ssh-remote+10.0.0.5/workspaces/shop")
        );
        let elsewhere = Host::from_ssh("dev@10.0.0.6", None, None).unwrap();
        assert_eq!(
            pick_recorded(&recorded, folder, Some(&elsewhere), "/workspaces/shop"),
            None
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
