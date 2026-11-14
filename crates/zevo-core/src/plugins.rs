//! Plugins: what Zevo can be extended with, and where it is loaded from.
//!
//! A plugin is a TOML manifest in the `plugins` folder of Zevo's config
//! directory: either `plugins/<name>.toml`, or `plugins/<name>/plugin.toml`
//! when it ships files of its own. A manifest lists any number of starter
//! templates, extras and editors (plain data), and may name programs Zevo
//! runs: hooks, doctor checks and a certificate issuer.
//!
//! What Zevo ships with is one more manifest, `builtin.toml`, compiled in and
//! read through the same code. An entry in a user manifest with the id of an
//! earlier one replaces it.
//!
//! Manifests are only ever read from the user's own config directory, never
//! from a project: programs named in them run with the user's permissions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub mod run;

/// Folder, inside the config directory, that manifests are loaded from.
pub const PLUGINS_DIR: &str = "plugins";

/// Version of the JSON exchanged with plugin programs. Sent as `protocol` in
/// every request, so a program can refuse one it does not understand.
pub const PROTOCOL: u32 = 1;

const BUILTIN: &str = include_str!("builtin.toml");

/// A starter for `zevo new`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Devcontainer image.
    pub image: String,
    /// Port the usual dev server for this stack listens on, if there is one.
    #[serde(default)]
    pub port: Option<u16>,
    /// The image already has Node.js, so extras that need it add nothing.
    #[serde(default)]
    pub has_node: bool,
}

/// An optional extra for a new project.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extra {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Ticked unless the user says otherwise.
    #[serde(default)]
    pub default: bool,
    /// Needs Node.js: the Node feature is added unless the template has it.
    #[serde(default)]
    pub needs_node: bool,
    /// Devcontainer features to add, by reference, with their options.
    #[serde(default)]
    pub features: BTreeMap<String, serde_json::Value>,
    /// Shell command run once after the container is created.
    #[serde(default)]
    pub post_create: Option<String>,
    /// Folders in the home directory to keep across rebuilds, named without
    /// the leading dot: `claude` keeps `~/.claude`.
    #[serde(default)]
    pub persist_dirs: Vec<String>,
    /// Files in the home directory to keep across rebuilds, for tools that
    /// rewrite a file in place: `.claude.json`.
    #[serde(default)]
    pub persist_files: Vec<String>,
    /// Environment variables for the container. `{state_dir}` stands for the
    /// project's state folder, relative to the project.
    #[serde(default)]
    pub container_env: BTreeMap<String, String>,
    /// Needs the project's state folder even though it lists nothing to link.
    #[serde(default)]
    pub state: bool,
    /// Asks Zevo to write the project's addresses to its env file.
    #[serde(default)]
    pub address_file: bool,
}

/// How an editor is told what to open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditorKind {
    /// VS Code or a fork: attaches to the devcontainer with a
    /// `vscode-remote://` address.
    Vscode,
    /// Takes the project folder as its argument.
    #[default]
    Folder,
}

/// An editor "Open in editor" can start.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Editor {
    pub id: String,
    pub name: String,
    /// Command-line launcher name.
    pub launcher: String,
    /// On macOS, the app bundle that ships the launcher.
    #[serde(default)]
    pub macos_bundle: Option<String>,
    #[serde(default)]
    pub kind: EditorKind,
    /// For the VS Code family: folder under the user's configuration
    /// directory where the editor keeps its state.
    #[serde(default)]
    pub state_folder: Option<String>,
    /// For `kind = "folder"`: the argument that opens a folder on another
    /// machine, with `{ssh}` and `{path}` filled in. Without it the editor
    /// cannot open remote projects.
    #[serde(default)]
    pub remote: Option<String>,
}

/// A program run after a reconcile pass that changed something. It gets the
/// pass as JSON on its standard input.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    /// The program and its arguments. A program starting with `./` is
    /// relative to the manifest's folder.
    pub command: Vec<String>,
    /// Only run for passes with one of these actions (`wrote_routes`,
    /// `issued_certificate`, ...). Empty means any action.
    #[serde(default)]
    pub actions: Vec<String>,
}

/// A program that adds a line to `zevo doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckPlugin {
    pub name: String,
    pub command: Vec<String>,
}

/// A program that issues certificates in place of mkcert.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuerPlugin {
    pub name: String,
    pub command: Vec<String>,
    /// Hours after which a certificate is issued again. Zevo does not read
    /// certificates, so an authority that issues short-lived ones (step-ca's
    /// last a day) must say so. Default: a year, as for mkcert.
    #[serde(default)]
    pub renew_after_hours: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    #[serde(default, rename = "template")]
    templates: Vec<Template>,
    #[serde(default, rename = "extra")]
    extras: Vec<Extra>,
    #[serde(default, rename = "editor")]
    editors: Vec<Editor>,
    #[serde(default, rename = "hook")]
    hooks: Vec<Hook>,
    #[serde(default, rename = "check")]
    checks: Vec<CheckPlugin>,
    #[serde(default)]
    issuer: Option<IssuerPlugin>,
}

/// Everything Zevo was extended with, built-ins included.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plugins {
    pub templates: Vec<Template>,
    pub extras: Vec<Extra>,
    pub editors: Vec<Editor>,
    pub hooks: Vec<Hook>,
    pub checks: Vec<CheckPlugin>,
    /// The last manifest to name one wins.
    pub issuer: Option<IssuerPlugin>,
    /// Manifests that could not be used, and why. Loading never fails as a
    /// whole: a broken plugin must not stop Zevo from working.
    pub problems: Vec<String>,
}

impl Plugins {
    /// Only what Zevo ships with.
    pub fn builtin() -> Plugins {
        // Parsed once: this is asked for on every reconcile pass.
        static BUILTINS: std::sync::OnceLock<Plugins> = std::sync::OnceLock::new();
        BUILTINS
            .get_or_init(|| {
                let mut plugins = Plugins::default();
                let manifest: Manifest = toml::from_str(BUILTIN).expect("builtin.toml is valid");
                plugins.merge(manifest, Path::new(""));
                plugins
            })
            .clone()
    }

    /// The built-ins plus every manifest in `<config_dir>/plugins`, in
    /// file-name order.
    pub fn load(config_dir: &Path) -> Plugins {
        let mut plugins = Plugins::builtin();
        for path in manifest_paths(&config_dir.join(PLUGINS_DIR)) {
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| {
                    // The message alone: the full error draws the offending
                    // line, which does not fit in a one-line warning.
                    toml::from_str::<Manifest>(&text).map_err(|e| e.message().to_string())
                })
                .and_then(|manifest| validate(&manifest).map(|()| manifest));
            match parsed {
                Ok(manifest) => {
                    let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
                    plugins.merge(manifest, &dir);
                }
                Err(reason) => plugins.problems.push(format!(
                    "plugin {} was skipped: {}",
                    path.display(),
                    reason.trim()
                )),
            }
        }
        plugins
    }

    pub fn template(&self, id: &str) -> Option<&Template> {
        self.templates.iter().find(|t| t.id == id)
    }

    pub fn extra(&self, id: &str) -> Option<&Extra> {
        self.extras.iter().find(|e| e.id == id)
    }

    pub fn editor(&self, id: &str) -> Option<&Editor> {
        self.editors.iter().find(|e| e.id == id)
    }

    fn merge(&mut self, manifest: Manifest, dir: &Path) {
        for template in manifest.templates {
            put(&mut self.templates, template, |t| &t.id);
        }
        for extra in manifest.extras {
            put(&mut self.extras, extra, |e| &e.id);
        }
        for editor in manifest.editors {
            put(&mut self.editors, editor, |e| &e.id);
        }
        self.hooks.extend(manifest.hooks.into_iter().map(|mut h| {
            anchor(&mut h.command, dir);
            h
        }));
        for mut check in manifest.checks {
            anchor(&mut check.command, dir);
            put(&mut self.checks, check, |c| &c.name);
        }
        if let Some(mut issuer) = manifest.issuer {
            anchor(&mut issuer.command, dir);
            self.issuer = Some(issuer);
        }
    }
}

/// Adds `item`, replacing in place an earlier one with the same id.
fn put<T>(items: &mut Vec<T>, item: T, id: impl Fn(&T) -> &String) {
    match items.iter().position(|have| id(have) == id(&item)) {
        Some(at) => items[at] = item,
        None => items.push(item),
    }
}

/// Makes every word of a command given as `./something` relative to the
/// manifest's folder: the program, and a script passed to an interpreter
/// (`["python3", "./notify.py"]`). Zevo's own working directory is whatever
/// it was started from, so nothing may depend on it.
fn anchor(command: &mut [String], dir: &Path) {
    for word in command {
        if let Some(relative) = word.strip_prefix("./") {
            *word = dir.join(relative).to_string_lossy().into_owned();
        }
    }
}

fn validate(manifest: &Manifest) -> Result<(), String> {
    // Zero would make every certificate due again on every pass.
    if manifest
        .issuer
        .as_ref()
        .is_some_and(|issuer| issuer.renew_after_hours == Some(0))
    {
        return Err("the issuer's renew_after_hours must be at least 1".to_string());
    }
    let ids = manifest
        .templates
        .iter()
        .map(|t| ("template", &t.id))
        .chain(manifest.extras.iter().map(|e| ("extra", &e.id)))
        .chain(manifest.editors.iter().map(|e| ("editor", &e.id)));
    for (what, id) in ids {
        let plain = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !plain {
            return Err(format!(
                "{what} id '{id}' must be lowercase letters, digits and '-'"
            ));
        }
    }
    // These become arguments in the project's persist script, so they must
    // be plain names: nothing the shell would read as more than a word, and
    // nothing that leaves the home or state folder.
    for extra in &manifest.extras {
        for name in extra.persist_dirs.iter().chain(&extra.persist_files) {
            let plain = name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
                && name.chars().any(|c| c != '.')
                && !name.starts_with('-');
            if !plain {
                return Err(format!(
                    "extra '{}' keeps '{name}', which must be a plain file or folder name \
                     (letters, digits, '.', '-' and '_')",
                    extra.id
                ));
            }
        }
    }
    let commands = manifest
        .hooks
        .iter()
        .map(|h| ("hook", &h.command))
        .chain(manifest.checks.iter().map(|c| ("check", &c.command)))
        .chain(manifest.issuer.iter().map(|i| ("issuer", &i.command)));
    for (what, command) in commands {
        if command.first().is_none_or(|program| program.is_empty()) {
            return Err(format!("a {what} has an empty command"));
        }
    }
    Ok(())
}

/// `dir/*.toml` and `dir/*/plugin.toml`, sorted by the name inside `dir`.
fn manifest_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    entries
        .into_iter()
        .filter_map(|path| {
            if path.is_dir() {
                let inner = path.join("plugin.toml");
                inner.is_file().then_some(inner)
            } else {
                (path.extension().is_some_and(|ext| ext == "toml")).then_some(path)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, relative: &str, text: &str) {
        let path = dir.join(PLUGINS_DIR).join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_builtins_load_and_a_missing_folder_adds_nothing() {
        let builtin = Plugins::builtin();
        assert_eq!(builtin.templates.len(), 6);
        assert_eq!(builtin.extras.len(), 6);
        assert_eq!(builtin.editors.len(), 5);
        assert!(builtin.template("node").unwrap().has_node);
        assert_eq!(builtin.editor("zed").unwrap().kind, EditorKind::Folder);
        assert_eq!(
            builtin.extra("docker").unwrap().features["ghcr.io/devcontainers/features/docker-outside-of-docker:1"]
                ["moby"],
            false
        );

        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(Plugins::load(tmp.path()), builtin);
    }

    #[test]
    fn manifests_add_entries_and_replace_builtins_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "ruby.toml",
            r#"
            [[template]]
            id = "ruby"
            name = "Ruby"
            image = "mcr.microsoft.com/devcontainers/ruby:3"
            port = 3000

            [[template]]
            id = "node"
            name = "Node.js 20"
            image = "mcr.microsoft.com/devcontainers/javascript-node:20"
            has_node = true
            "#,
        );
        write(
            tmp.path(),
            "tools/plugin.toml",
            r#"
            [[hook]]
            command = ["./notify.sh", "--quiet", "./rules.txt"]
            actions = ["issued_certificate"]

            [[check]]
            name = "vpn"
            command = ["vpn-status"]

            [issuer]
            name = "step-ca"
            command = ["./issue"]
            "#,
        );
        let plugins = Plugins::load(tmp.path());
        assert_eq!(plugins.problems, Vec::<String>::new());
        // Replaced where it was; the new one goes last.
        assert_eq!(plugins.templates[0].name, "Node.js 20");
        assert_eq!(plugins.templates.len(), 7);
        assert_eq!(plugins.templates[6].id, "ruby");
        // `./` is the manifest's folder; anything else is left to the PATH.
        let tools = tmp.path().join(PLUGINS_DIR).join("tools");
        assert_eq!(
            plugins.hooks[0].command,
            [
                tools.join("notify.sh").to_string_lossy().into_owned(),
                "--quiet".to_string(),
                tools.join("rules.txt").to_string_lossy().into_owned()
            ]
        );
        assert_eq!(plugins.checks[0].command, ["vpn-status"]);
        assert_eq!(
            plugins.issuer.unwrap().command,
            [tools.join("issue").to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn a_broken_manifest_is_reported_and_the_rest_still_load() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a-typo.toml", "[[template]]\nid = \"x\"\n");
        write(
            tmp.path(),
            "b-bad-id.toml",
            "[[editor]]\nid = \"My Editor\"\nname = \"M\"\nlauncher = \"m\"\n",
        );
        write(tmp.path(), "c-empty.toml", "[[hook]]\ncommand = []\n");
        write(
            tmp.path(),
            "c-renew.toml",
            "[issuer]\nname = \"x\"\ncommand = [\"x\"]\nrenew_after_hours = 0\n",
        );
        write(
            tmp.path(),
            "d-fine.toml",
            "[[editor]]\nid = \"idea\"\nname = \"IntelliJ IDEA\"\nlauncher = \"idea\"\n",
        );
        write(tmp.path(), "notes.txt", "not a manifest");
        let plugins = Plugins::load(tmp.path());
        assert_eq!(plugins.problems.len(), 4, "{:?}", plugins.problems);
        assert!(plugins.problems[3].contains("renew_after_hours"));
        assert_eq!(plugins.issuer, None);
        assert!(plugins.problems[0].contains("a-typo.toml"));
        assert!(plugins.problems[1].contains("My Editor"));
        assert!(plugins.problems[2].contains("empty command"));
        assert_eq!(plugins.editors.len(), 6);
        assert_eq!(plugins.editor("idea").unwrap().remote, None);
    }
}
