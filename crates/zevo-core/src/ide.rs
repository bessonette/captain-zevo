//! Opening a project in an editor, attached to its devcontainer.
//!
//! Editors are plain data from [`Plugins`](crate::plugins::Plugins): the
//! built-in ones and any a manifest adds. This module turns one into a
//! program and arguments.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::{Host, Project};
pub use crate::plugins::{Editor, EditorKind};
use crate::{Error, Result, devcontainer, programs, remote};

impl Editor {
    /// Where this editor's launcher is installed, if it is.
    pub fn program(&self) -> Option<PathBuf> {
        let mut extra = Vec::new();
        if cfg!(target_os = "macos")
            && let Some(bundle) = &self.macos_bundle
        {
            let home = directories::BaseDirs::new().map(|d| d.home_dir().join("Applications"));
            for apps in std::iter::once(PathBuf::from("/Applications")).chain(home) {
                // VS Code and its forks keep the launcher inside the bundle.
                extra.push(apps.join(bundle).join("Contents/Resources/app/bin"));
                extra.push(apps.join(bundle).join("Contents/MacOS"));
            }
        }
        programs::find(&self.launcher, &extra)
    }

    fn attaches_to_devcontainers(&self) -> bool {
        self.kind == EditorKind::Vscode
    }
}

/// Of `editors`, the ones installed on this machine, in manifest order.
pub fn available(editors: &[Editor]) -> Vec<Editor> {
    editors
        .iter()
        .filter(|e| e.program().is_some())
        .cloned()
        .collect()
}

/// The editor "Open in editor" uses: the one with the chosen id if it is
/// installed, else the first installed one. A stored id can outlive its
/// editor (uninstalled, or its plugin removed), and that must not stop
/// projects from opening.
pub fn preferred<'a>(chosen: Option<&str>, installed: &'a [Editor]) -> Option<&'a Editor> {
    chosen
        .and_then(|id| installed.iter().find(|e| e.id == id))
        .or_else(|| installed.first())
}

/// Editor names for messages: "VS Code, Cursor or Zed".
pub fn list_names(editors: &[Editor]) -> String {
    match editors {
        [] => "none".to_string(),
        [only] => only.name.clone(),
        [rest @ .., last] => {
            let rest: Vec<&str> = rest.iter().map(|e| e.name.as_str()).collect();
            format!("{} or {}", rest.join(", "), last.name)
        }
    }
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
fn recorded_addresses(editor: &Editor) -> Vec<String> {
    let (Some(folder), Some(dirs)) = (&editor.state_folder, directories::BaseDirs::new()) else {
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
///
/// Fails for a folder editor that has no way to open a folder on another
/// machine: handing it the remote path would open an empty or wrong folder
/// on this one.
pub fn arguments(editor: &Editor, project: &Project, host: Option<&Host>) -> Result<Vec<String>> {
    check_reach(editor, host)?;
    let has_devcontainer = project.kind != crate::model::DevcontainerKind::None;
    let workspace = (editor.attaches_to_devcontainers() && has_devcontainer)
        .then(|| workspace_folder(project, host));
    if let Some(workspace) = &workspace
        && let Some(address) =
            pick_recorded(&recorded_addresses(editor), &project.path, host, workspace)
    {
        return Ok(vec!["--folder-uri".to_string(), address]);
    }
    arguments_for(editor, project, host, workspace.as_deref())
}

/// Fails when the project is on another machine and the editor cannot open
/// folders there.
fn check_reach(editor: &Editor, host: Option<&Host>) -> Result<()> {
    match host {
        Some(host) if !editor.attaches_to_devcontainers() && editor.remote.is_none() => {
            Err(Error::Other(format!(
                "{} cannot open projects on another machine ({}); choose another editor, \
                 or give it a `remote` setting in its plugin manifest",
                editor.name, host.ssh
            )))
        }
        _ => Ok(()),
    }
}

/// [`arguments`], given the folder inside the container (`None` opens the
/// plain folder).
fn arguments_for(
    editor: &Editor,
    project: &Project,
    host: Option<&Host>,
    workspace: Option<&str>,
) -> Result<Vec<String>> {
    check_reach(editor, host)?;
    let path = project.path.to_string_lossy();
    let uri = match (host, workspace) {
        (Some(host), Some(workspace)) => {
            remote_devcontainer_uri(&host.ssh, &project.path, workspace)
        }
        (None, Some(workspace)) => devcontainer_uri(&project.path, workspace),
        (Some(host), None) if editor.attaches_to_devcontainers() => {
            format!("vscode-remote://ssh-remote+{}{path}", host.ssh)
        }
        // A folder editor opens the folder over SSH its own way; Zed then
        // offers to reopen it in the container.
        (Some(host), None) => {
            let template = editor.remote.as_deref().unwrap_or_default();
            return Ok(vec![
                template
                    .replace("{ssh}", &host.ssh)
                    .replace("{path}", &path),
            ]);
        }
        (None, None) => return Ok(vec![path.into_owned()]),
    };
    Ok(vec!["--folder-uri".to_string(), uri])
}

/// Starts the editor on the project and returns without waiting for it.
pub fn open(editor: &Editor, project: &Project, host: Option<&Host>) -> Result<()> {
    // Before looking for the program: "cannot open remote projects" is the
    // answer whether or not it is installed.
    check_reach(editor, host)?;
    let program = editor
        .program()
        .ok_or_else(|| Error::Other(format!("{} does not appear to be installed", editor.name)))?;
    Command::new(&program)
        .args(arguments(editor, project, host)?)
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

    fn builtin(id: &str) -> Editor {
        crate::plugins::Plugins::builtin()
            .editor(id)
            .unwrap()
            .clone()
    }

    /// A folder editor as a user's manifest would add it.
    fn manifest_editor(remote: Option<&str>) -> Editor {
        Editor {
            id: "idea".into(),
            name: "IntelliJ IDEA".into(),
            launcher: "zevo-test-no-such-idea".into(),
            macos_bundle: Some("IntelliJ IDEA.app".into()),
            kind: EditorKind::Folder,
            state_folder: None,
            remote: remote.map(str::to_string),
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
        let vscode = builtin("vscode");
        // Attached to the devcontainer on that machine.
        assert_eq!(
            arguments_for(&vscode, &p, Some(&host), Some("/workspaces/billing")).unwrap(),
            [
                "--folder-uri",
                "vscode-remote://dev-container+2f7372762f62696c6c696e67@ssh-remote+dev@10.0.0.5/workspaces/billing"
            ]
        );
        // No devcontainer: the folder over SSH.
        assert_eq!(
            arguments_for(&vscode, &p, Some(&host), None).unwrap(),
            [
                "--folder-uri",
                "vscode-remote://ssh-remote+dev@10.0.0.5/srv/billing"
            ]
        );
        assert_eq!(
            arguments_for(&builtin("zed"), &p, Some(&host), None).unwrap(),
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
        let args = arguments(&builtin("vscode"), &p, None).unwrap();
        assert_eq!(args[0], "--folder-uri");
        assert!(args[1].ends_with("/workspaces/shop"), "{}", args[1]);

        std::fs::write(
            dir.join(".devcontainer/devcontainer.json"),
            r#"{"image":"node","workspaceFolder":"/srv/app"}"#,
        )
        .unwrap();
        assert!(arguments(&builtin("cursor"), &p, None).unwrap()[1].ends_with("/srv/app"));

        // Zed, and folders without a devcontainer, just open the folder.
        assert_eq!(
            arguments(&builtin("zed"), &p, None).unwrap(),
            [dir.to_string_lossy()]
        );
        assert_eq!(
            arguments(
                &builtin("vscode"),
                &project(&dir, DevcontainerKind::None),
                None
            )
            .unwrap(),
            [dir.to_string_lossy()]
        );
    }

    #[test]
    fn a_manifest_folder_editor_opens_local_folders_even_with_a_devcontainer() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("shop");
        std::fs::create_dir_all(dir.join(".devcontainer")).unwrap();
        std::fs::write(
            dir.join(".devcontainer/devcontainer.json"),
            r#"{"image":"node","workspaceFolder":"/srv/app"}"#,
        )
        .unwrap();
        let p = project(&dir, DevcontainerKind::Image);
        assert_eq!(
            arguments(&manifest_editor(None), &p, None).unwrap(),
            [dir.to_string_lossy()]
        );
    }

    #[test]
    fn a_manifest_folder_editor_opens_remote_folders_through_its_template() {
        let host = Host::from_ssh("dev@10.0.0.5", None, None).unwrap();
        let p = project(Path::new("/srv/billing"), DevcontainerKind::Compose);
        let editor = manifest_editor(Some("--remote={ssh}:{path}"));
        assert_eq!(
            arguments(&editor, &p, Some(&host)).unwrap(),
            ["--remote=dev@10.0.0.5:/srv/billing"]
        );
    }

    #[test]
    fn a_folder_editor_without_a_remote_template_refuses_remote_projects() {
        let host = Host::from_ssh("dev@10.0.0.5", None, None).unwrap();
        let p = project(Path::new("/srv/billing"), DevcontainerKind::Compose);
        let editor = manifest_editor(None);
        let err = arguments(&editor, &p, Some(&host)).unwrap_err().to_string();
        assert!(
            err.contains("cannot open projects on another machine"),
            "{err}"
        );
        // Said even though it is not installed, and nothing is started.
        let err = open(&editor, &p, Some(&host)).unwrap_err().to_string();
        assert!(
            err.contains("IntelliJ IDEA cannot open projects on another machine"),
            "{err}"
        );
        // Locally it is only missing.
        let err = open(&editor, &p, None).unwrap_err().to_string();
        assert!(err.contains("does not appear to be installed"), "{err}");
    }

    #[test]
    fn the_stored_editor_is_used_only_while_it_is_installed() {
        let installed = [builtin("cursor"), builtin("zed")];
        assert_eq!(preferred(Some("zed"), &installed).unwrap().id, "zed");
        // Uninstalled, or its plugin removed: the first installed one.
        assert_eq!(preferred(Some("vscode"), &installed).unwrap().id, "cursor");
        assert_eq!(preferred(Some("gone"), &installed).unwrap().id, "cursor");
        assert_eq!(preferred(None, &installed).unwrap().id, "cursor");
        assert_eq!(preferred(Some("zed"), &[]), None);
        // Settings written when editors were a fixed list still load.
        let settings: crate::model::Settings = toml::from_str("editor = \"vscodium\"").unwrap();
        assert_eq!(settings.editor.as_deref(), Some("vscodium"));
        assert_eq!(list_names(&installed), "Cursor or Zed");
        assert_eq!(
            list_names(&crate::plugins::Plugins::builtin().editors),
            "VS Code, Cursor, Windsurf, VSCodium or Zed"
        );
    }
}
