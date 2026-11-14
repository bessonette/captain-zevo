//! Starting a new project: a folder with a devcontainer.json that already
//! declares its route, ready to be registered and built.

use std::path::{Path, PathBuf};

use crate::model::{slugify, validate_slug};
use crate::plugins::Plugins;
pub use crate::plugins::{Extra, Template};
use crate::{Error, Result};

/// Which extras a new project gets, resolved against the loaded plugins.
/// Kept in the plugins' order, whatever order they were asked for in, so the
/// generated files do not depend on how the user typed the list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    extras: Vec<Extra>,
}

impl Options {
    /// The extras that are on by default.
    pub fn defaults(plugins: &Plugins) -> Options {
        Options {
            extras: plugins
                .extras
                .iter()
                .filter(|e| e.default)
                .cloned()
                .collect(),
        }
    }

    /// The extras with these ids. An unknown id is an error that lists the
    /// ones there are.
    pub fn from_ids<S: AsRef<str>>(plugins: &Plugins, ids: &[S]) -> Result<Options> {
        if let Some(unknown) = ids
            .iter()
            .map(AsRef::as_ref)
            .find(|id| plugins.extra(id).is_none())
        {
            let known: Vec<&str> = plugins.extras.iter().map(|e| e.id.as_str()).collect();
            return Err(Error::Other(format!(
                "there is no extra called '{unknown}' (choose from {})",
                known.join(", ")
            )));
        }
        Ok(Options {
            extras: plugins
                .extras
                .iter()
                .filter(|e| ids.iter().any(|id| id.as_ref() == e.id))
                .cloned()
                .collect(),
        })
    }

    /// The chosen extras, in the plugins' order.
    pub fn extras(&self) -> &[Extra] {
        &self.extras
    }

    /// Whether anything is kept in the project's state folder.
    fn persists_state(&self) -> bool {
        self.extras
            .iter()
            .any(|e| e.state || !e.persist_dirs.is_empty() || !e.persist_files.is_empty())
    }

    fn address_file(&self) -> bool {
        self.extras.iter().any(|e| e.address_file)
    }
}

/// Folder, inside the project, holding tool state that must survive a
/// rebuild. It ends up with credentials in it, so it is git-ignored.
const STATE_DIR: &str = ".devcontainer/home";
const PERSIST_SCRIPT: &str = ".devcontainer/persist-home.sh";
const ADDRESS_FILE: &str = ".devcontainer/zevo.env";
/// Added for extras that need Node.js when the template's image lacks it.
const NODE_FEATURE: &str = "ghcr.io/devcontainers/features/node:1";

/// The devcontainer.json for a new project.
pub fn render(template: &Template, name: &str, slug: &str, options: &Options) -> String {
    use serde_json::json;
    // Built as JSON values so names with quotes or backslashes stay valid.
    let mut zevo = json!({ "name": name, "slug": slug });
    let mut config = json!({ "name": name, "image": template.image });
    if let Some(port) = template.port {
        config["forwardPorts"] = json!([port]);
        zevo["routes"] = json!([{ "port": port }]);
    }
    if options.address_file() {
        zevo["envFile"] = json!(ADDRESS_FILE);
    }

    let mut features = serde_json::Map::new();
    let mut post_create = Vec::new();
    let mut container_env = serde_json::Map::new();
    for extra in options.extras() {
        for (reference, settings) in &extra.features {
            features.insert(reference.clone(), settings.clone());
        }
        if extra.needs_node && !template.has_node {
            // Not over options an extra chose for it.
            features.entry(NODE_FEATURE).or_insert_with(|| json!({}));
        }
        post_create.extend(extra.post_create.as_deref());
        for (key, value) in &extra.container_env {
            container_env.insert(key.clone(), json!(value.replace("{state_dir}", STATE_DIR)));
        }
    }
    if !features.is_empty() {
        config["features"] = serde_json::Value::Object(features);
    }
    if !post_create.is_empty() {
        config["postCreateCommand"] = json!(post_create.join(" && "));
    }
    if options.persists_state() {
        config["postStartCommand"] = json!(format!("sh {PERSIST_SCRIPT}"));
    }
    if !container_env.is_empty() {
        config["containerEnv"] = serde_json::Value::Object(container_env);
    }

    config["customizations"] = json!({ "zevo": zevo });
    let mut text = serde_json::to_string_pretty(&config).expect("plain JSON always serializes");
    text.push('\n');
    text
}

/// The script that links tool state from the project into the home folder
/// on every container start.
pub fn render_persist_script(options: &Options) -> String {
    let mut script = String::from(
        r#"#!/bin/sh
# Keeps tool state (logins, history) in the project's git-ignored
# .devcontainer/home folder, so it survives a container rebuild.
# Runs on every container start; safe to run by hand.
set -eu

state="$(cd "$(dirname "$0")" && pwd)/home"
mkdir -p "$state"
chmod 700 "$state"

# link_dir NAME: make ~/.NAME a link to the folder kept in the project.
link_dir() {
    target="$state/$1"
    link="$HOME/.$1"
    mkdir -p "$target"
    if [ "$(readlink "$link" 2>/dev/null || true)" != "$target" ]; then
        if [ -e "$link" ] || [ -L "$link" ]; then
            # A fresh container may already have one. Keep anything the
            # project copy lacks, then set the original aside.
            if [ -d "$link" ]; then cp -an "$link/." "$target/"; fi
            mv "$link" "$link.before-link.$(date +%s)"
        fi
        ln -s "$target" "$link"
    fi
}

# keep_file NAME: for a file its tool rewrites in place, which therefore
# cannot be a link. Restore it after a rebuild, otherwise refresh the copy.
keep_file() {
    file="$HOME/$1"
    copy="$state/$(printf '%s' "$1" | sed 's/^\.//')"
    if [ ! -e "$file" ] && [ -e "$copy" ]; then
        cp -p "$copy" "$file"
    elif [ -e "$file" ]; then
        cp -p "$file" "$copy"
    fi
}

"#,
    );
    // Plugin manifests are checked to only name plain files and folders
    // here (see `plugins::validate`), so these lines cannot run anything.
    for extra in options.extras() {
        for dir in &extra.persist_dirs {
            script.push_str(&format!("link_dir {dir}\n"));
        }
        for file in &extra.persist_files {
            script.push_str(&format!("keep_file {file}\n"));
        }
    }
    script
}

/// Creates `parent/<slug>` with a devcontainer.json from the template and
/// returns the new folder. Refuses to touch a folder that already exists.
pub fn create(
    plugins: &Plugins,
    parent: &Path,
    name: &str,
    template_id: &str,
    options: &Options,
) -> Result<PathBuf> {
    let template = plugins.template(template_id).ok_or_else(|| {
        let known: Vec<&str> = plugins.templates.iter().map(|t| t.id.as_str()).collect();
        Error::Other(format!(
            "there is no template called '{template_id}' (choose from {})",
            known.join(", ")
        ))
    })?;
    let name = name.trim();
    let slug = slugify(name);
    validate_slug(&slug)
        .map_err(|_| Error::Other("give the project a name with letters or digits in it".into()))?;
    if !parent.is_dir() {
        return Err(Error::Other(format!(
            "{} is not a folder",
            parent.display()
        )));
    }
    let folder = parent.join(&slug);
    if folder.exists() {
        return Err(Error::Other(format!("{} already exists", folder.display())));
    }
    let write = |relative: &str, content: String| {
        let path = folder.join(relative);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        std::fs::write(&path, content)
            .map_err(|e| Error::io(format!("writing {}", path.display()), e))
    };

    write(
        ".devcontainer/devcontainer.json",
        render(template, name, &slug, options),
    )?;
    let mut ignored = Vec::new();
    if options.persists_state() {
        write(PERSIST_SCRIPT, render_persist_script(options))?;
        ignored.push(format!(
            "# Tool logins and history, kept across container rebuilds\n/{STATE_DIR}/"
        ));
    }
    if options.address_file() {
        ignored.push(format!(
            "# This checkout's addresses, written by Captain Zevo\n/{ADDRESS_FILE}"
        ));
    }
    if !ignored.is_empty() {
        write(".gitignore", ignored.join("\n") + "\n")?;
    }
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devcontainer;
    use crate::model::DevcontainerKind;

    fn builtin() -> Plugins {
        Plugins::builtin()
    }

    #[test]
    fn every_template_produces_a_config_zevo_can_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = builtin();
        for template in &plugins.templates {
            let name = format!("My \"{}\" App", template.name);
            let folder = create(
                &plugins,
                tmp.path(),
                &name,
                &template.id,
                &Options::defaults(&plugins),
            )
            .unwrap();
            let detected = devcontainer::detect(&folder).unwrap();
            assert_eq!(detected.kind, DevcontainerKind::Image, "{}", template.id);
            let declared = detected.declared.unwrap();
            assert_eq!(declared.name.as_deref(), Some(name.as_str()));
            assert_eq!(
                declared.routes.len(),
                usize::from(template.port.is_some()),
                "{}",
                template.id
            );
            assert_eq!(declared.env_file.as_deref(), Some(".devcontainer/zevo.env"));
            assert_eq!(
                detected.ports,
                template.port.into_iter().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn extras_add_features_persistence_and_ignore_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = builtin();
        let all: Vec<&str> = plugins.extras.iter().map(|e| e.id.as_str()).collect();
        let folder = create(
            &plugins,
            tmp.path(),
            "Shop",
            "python",
            &Options::from_ids(&plugins, &all).unwrap(),
        )
        .unwrap();
        let config: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(folder.join(".devcontainer/devcontainer.json")).unwrap(),
        )
        .unwrap();
        let features = config["features"].as_object().unwrap();
        assert!(features.contains_key("ghcr.io/anthropics/devcontainer-features/claude-code:1.0"));
        // Python has no Node.js of its own, and both assistants need it.
        assert!(features.contains_key("ghcr.io/devcontainers/features/node:1"));
        assert_eq!(
            features["ghcr.io/devcontainers/features/docker-outside-of-docker:1"]["moby"],
            false
        );
        assert!(features.contains_key("ghcr.io/devcontainers/features/github-cli:1"));
        assert_eq!(config["postCreateCommand"], "npm install -g @openai/codex");
        assert_eq!(
            config["postStartCommand"],
            "sh .devcontainer/persist-home.sh"
        );
        assert_eq!(
            config["containerEnv"]["HISTFILE"],
            "${containerWorkspaceFolder}/.devcontainer/home/shell_history"
        );

        let script = std::fs::read_to_string(folder.join(".devcontainer/persist-home.sh")).unwrap();
        assert!(
            script.contains("link_dir claude\nkeep_file .claude.json\nlink_dir codex\n"),
            "{script}"
        );
        let ignore = std::fs::read_to_string(folder.join(".gitignore")).unwrap();
        assert!(
            ignore.contains("/.devcontainer/home/") && ignore.contains("/.devcontainer/zevo.env")
        );

        // With nothing chosen, none of it is written.
        let bare = create(&plugins, tmp.path(), "Bare", "node", &Options::default()).unwrap();
        let config = std::fs::read_to_string(bare.join(".devcontainer/devcontainer.json")).unwrap();
        assert!(
            !config.contains("features")
                && !config.contains("postStartCommand")
                && !config.contains("envFile")
        );
        assert!(!bare.join(".devcontainer/persist-home.sh").exists());
        assert!(!bare.join(".gitignore").exists());
        // Node templates do not add the Node.js feature twice.
        let node = render(
            plugins.template("node").unwrap(),
            "n",
            "n",
            &Options::from_ids(&plugins, &["codex"]).unwrap(),
        );
        assert!(!node.contains("features/node"), "{node}");

        let unknown = Options::from_ids(&plugins, &["telepathy"])
            .unwrap_err()
            .to_string();
        assert!(
            unknown.contains("'telepathy'") && unknown.contains("claude-code, codex, docker"),
            "{unknown}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_persist_script_links_state_and_survives_a_rebuild() {
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let plugins = builtin();
        let options = Options::from_ids(&plugins, &["claude-code", "codex"]).unwrap();
        let folder = create(&plugins, tmp.path(), "Shop", "node", &options).unwrap();
        let script = folder.join(".devcontainer/persist-home.sh");
        let run = |home: &Path| {
            let status = Command::new("sh")
                .arg(&script)
                .env("HOME", home)
                .status()
                .unwrap();
            assert!(status.success());
        };

        // First start: the tool has already created its folder and login file.
        let home = tmp.path().join("home1");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/history"), "hello").unwrap();
        std::fs::write(home.join(".claude.json"), "{\"login\":1}").unwrap();
        run(&home);
        run(&home); // twice is harmless
        let state = folder.join(".devcontainer/home");
        assert_eq!(
            std::fs::read_to_string(state.join("claude/history")).unwrap(),
            "hello"
        );
        assert!(home.join(".claude").is_symlink() && home.join(".codex").is_symlink());

        // "Rebuild": a new, empty home gets the history and the login back.
        let fresh = tmp.path().join("home2");
        std::fs::create_dir_all(&fresh).unwrap();
        run(&fresh);
        assert_eq!(
            std::fs::read_to_string(fresh.join(".claude/history")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(fresh.join(".claude.json")).unwrap(),
            "{\"login\":1}"
        );
    }

    #[test]
    fn refuses_bad_names_unknown_templates_and_existing_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = builtin();
        let none = Options::default();
        assert!(create(&plugins, tmp.path(), "!!!", "node", &none).is_err());
        let unknown = create(&plugins, tmp.path(), "shop", "cobol", &none)
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("node, python"), "{unknown}");
        assert!(create(&plugins, &tmp.path().join("missing"), "shop", "node", &none).is_err());

        let folder = create(&plugins, tmp.path(), "Shop", "node", &none).unwrap();
        assert_eq!(folder, tmp.path().join("shop"));
        std::fs::write(folder.join("keep.txt"), "mine").unwrap();
        // A second attempt leaves the existing folder exactly as it is.
        assert!(create(&plugins, tmp.path(), "shop", "python", &none).is_err());
        assert_eq!(
            std::fs::read_to_string(folder.join("keep.txt")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn templates_and_extras_from_a_manifest_shape_the_config() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp
            .path()
            .join(crate::plugins::PLUGINS_DIR)
            .join("elixir.toml");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest,
            r#"
            [[template]]
            id = "elixir"
            name = "Elixir"
            image = "mcr.microsoft.com/devcontainers/base:debian"
            port = 4000

            [[extra]]
            id = "hex"
            name = "Hex"
            needs_node = true
            post_create = "mix local.hex --force"
            persist_dirs = ["hex"]
            [extra.features]
            "ghcr.io/example/features/elixir:1" = { version = "1.17" }
            "#,
        )
        .unwrap();
        let plugins = Plugins::load(tmp.path());
        assert_eq!(plugins.problems, Vec::<String>::new());

        let options = Options::from_ids(&plugins, &["hex", "codex"]).unwrap();
        let folder = create(&plugins, tmp.path(), "Phoenix", "elixir", &options).unwrap();
        let config: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(folder.join(".devcontainer/devcontainer.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            config["image"],
            "mcr.microsoft.com/devcontainers/base:debian"
        );
        assert_eq!(config["customizations"]["zevo"]["routes"][0]["port"], 4000);
        assert_eq!(
            config["features"]["ghcr.io/example/features/elixir:1"]["version"],
            "1.17"
        );
        assert!(config["features"]["ghcr.io/devcontainers/features/node:1"].is_object());
        // Built-ins come first, then the manifest's, whatever order was asked.
        assert_eq!(
            config["postCreateCommand"],
            "npm install -g @openai/codex && mix local.hex --force"
        );
        assert_eq!(
            config["postStartCommand"],
            "sh .devcontainer/persist-home.sh"
        );
        let script = std::fs::read_to_string(folder.join(".devcontainer/persist-home.sh")).unwrap();
        assert!(
            script.ends_with("link_dir codex\nlink_dir hex\n"),
            "{script}"
        );
        let ignore = std::fs::read_to_string(folder.join(".gitignore")).unwrap();
        assert!(ignore.contains("/.devcontainer/home/"), "{ignore}");
    }

    #[test]
    fn a_manifest_cannot_put_shell_into_the_persist_script() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join(crate::plugins::PLUGINS_DIR);
        std::fs::create_dir_all(&folder).unwrap();
        for (file, keeps) in [
            ("a.toml", r#"persist_dirs = ["x; rm -rf ~"]"#),
            ("b.toml", r#"persist_files = ["$(touch pwned)"]"#),
            ("c.toml", r#"persist_dirs = ["../outside"]"#),
            ("d.toml", r#"persist_files = [".."]"#),
        ] {
            std::fs::write(
                folder.join(file),
                format!("[[extra]]\nid = \"bad\"\nname = \"Bad\"\n{keeps}\n"),
            )
            .unwrap();
        }
        let plugins = Plugins::load(tmp.path());
        assert_eq!(plugins.problems.len(), 4, "{:?}", plugins.problems);
        assert!(
            plugins
                .problems
                .iter()
                .all(|p| p.contains("plain file or folder name"))
        );
        assert!(plugins.extra("bad").is_none());
    }
}
