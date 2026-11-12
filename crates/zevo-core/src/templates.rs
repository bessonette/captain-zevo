//! Starting a new project: a folder with a devcontainer.json that already
//! declares its route, ready to be registered and built.

use std::path::{Path, PathBuf};

use crate::model::{slugify, validate_slug};
use crate::{Error, Result};

pub struct Template {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Devcontainer image.
    image: &'static str,
    /// Port the usual dev server for this stack listens on, if there is one.
    port: Option<u16>,
}

/// Starters built on the maintained devcontainer base images.
pub const TEMPLATES: [Template; 6] = [
    Template {
        id: "node",
        name: "Node.js",
        description: "JavaScript and TypeScript, dev server on port 3000",
        image: "mcr.microsoft.com/devcontainers/javascript-node:22",
        port: Some(3000),
    },
    Template {
        id: "python",
        name: "Python",
        description: "Python 3, dev server on port 8000",
        image: "mcr.microsoft.com/devcontainers/python:3",
        port: Some(8000),
    },
    Template {
        id: "rust",
        name: "Rust",
        description: "Rust toolchain, server on port 8080",
        image: "mcr.microsoft.com/devcontainers/rust:1",
        port: Some(8080),
    },
    Template {
        id: "go",
        name: "Go",
        description: "Go toolchain, server on port 8080",
        image: "mcr.microsoft.com/devcontainers/go:1",
        port: Some(8080),
    },
    Template {
        id: "php",
        name: "PHP",
        description: "PHP 8, dev server on port 8000",
        image: "mcr.microsoft.com/devcontainers/php:8",
        port: Some(8000),
    },
    Template {
        id: "ubuntu",
        name: "Ubuntu",
        description: "A plain Ubuntu container; add a route when you have something to serve",
        image: "mcr.microsoft.com/devcontainers/base:ubuntu",
        port: None,
    },
];

pub fn find(id: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.id == id)
}

/// An optional extra for a new project.
pub struct Extra {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Ticked unless the user says otherwise.
    pub default: bool,
}

pub const EXTRAS: [Extra; 6] = [
    Extra {
        id: "claude-code",
        name: "Claude Code",
        description: "Installs Claude Code and keeps its login and history across rebuilds",
        default: false,
    },
    Extra {
        id: "codex",
        name: "Codex",
        description: "Installs OpenAI's Codex CLI and keeps its login and history across rebuilds",
        default: false,
    },
    Extra {
        id: "docker",
        name: "Docker access",
        description: "Makes this machine's Docker available inside the container",
        default: false,
    },
    Extra {
        id: "github-cli",
        name: "GitHub CLI",
        description: "Installs gh",
        default: false,
    },
    Extra {
        id: "shell-history",
        name: "Shell history",
        description: "Keeps command history across rebuilds",
        default: true,
    },
    Extra {
        id: "address-file",
        name: "Address file",
        description: "Zevo writes the project's own hostnames to .devcontainer/zevo.env",
        default: true,
    },
];

/// Which extras a new project gets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    pub claude_code: bool,
    pub codex: bool,
    pub docker: bool,
    pub github_cli: bool,
    pub shell_history: bool,
    pub address_file: bool,
}

impl Options {
    /// The extras that are on by default.
    pub fn defaults() -> Options {
        let ids: Vec<&str> = EXTRAS.iter().filter(|e| e.default).map(|e| e.id).collect();
        Options::from_ids(&ids).expect("default extras are valid ids")
    }

    pub fn from_ids<S: AsRef<str>>(ids: &[S]) -> Result<Options> {
        let mut options = Options::default();
        for id in ids {
            match id.as_ref() {
                "claude-code" => options.claude_code = true,
                "codex" => options.codex = true,
                "docker" => options.docker = true,
                "github-cli" => options.github_cli = true,
                "shell-history" => options.shell_history = true,
                "address-file" => options.address_file = true,
                other => {
                    let known: Vec<&str> = EXTRAS.iter().map(|e| e.id).collect();
                    return Err(Error::Other(format!(
                        "there is no extra called '{other}' (choose from {})",
                        known.join(", ")
                    )));
                }
            }
        }
        Ok(options)
    }

    /// Whether anything is kept in the project's state folder.
    fn persists_state(&self) -> bool {
        self.claude_code || self.codex || self.shell_history
    }
}

/// Folder, inside the project, holding tool state that must survive a
/// rebuild. It ends up with credentials in it, so it is git-ignored.
const STATE_DIR: &str = ".devcontainer/home";
const PERSIST_SCRIPT: &str = ".devcontainer/persist-home.sh";
const ADDRESS_FILE: &str = ".devcontainer/zevo.env";

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
    if options.address_file {
        zevo["envFile"] = json!(ADDRESS_FILE);
    }

    let mut features = serde_json::Map::new();
    if options.claude_code {
        features.insert(
            "ghcr.io/anthropics/devcontainer-features/claude-code:1.0".into(),
            json!({}),
        );
    }
    // Claude Code's feature and the Codex CLI both need Node.js.
    if (options.claude_code || options.codex) && template.id != "node" {
        features.insert("ghcr.io/devcontainers/features/node:1".into(), json!({}));
    }
    if options.docker {
        // The CLI from Docker's own packages: the Moby ones are not
        // available on every base image.
        features.insert(
            "ghcr.io/devcontainers/features/docker-outside-of-docker:1".into(),
            json!({ "moby": false }),
        );
    }
    if options.github_cli {
        features.insert(
            "ghcr.io/devcontainers/features/github-cli:1".into(),
            json!({}),
        );
    }
    if !features.is_empty() {
        config["features"] = serde_json::Value::Object(features);
    }

    if options.codex {
        config["postCreateCommand"] = json!("npm install -g @openai/codex");
    }
    if options.persists_state() {
        config["postStartCommand"] = json!(format!("sh {PERSIST_SCRIPT}"));
    }
    if options.shell_history {
        config["containerEnv"] = json!({ "HISTFILE": format!("${{containerWorkspaceFolder}}/{STATE_DIR}/shell_history") });
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
    if options.claude_code {
        script.push_str("link_dir claude\nkeep_file .claude.json\n");
    }
    if options.codex {
        script.push_str("link_dir codex\n");
    }
    script
}

/// Creates `parent/<slug>` with a devcontainer.json from the template and
/// returns the new folder. Refuses to touch a folder that already exists.
pub fn create(parent: &Path, name: &str, template_id: &str, options: &Options) -> Result<PathBuf> {
    let template = find(template_id)
        .ok_or_else(|| Error::Other(format!("there is no template called '{template_id}'")))?;
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
    if options.address_file {
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

    #[test]
    fn every_template_produces_a_config_zevo_can_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        for template in &TEMPLATES {
            let name = format!("My \"{}\" App", template.name);
            let folder = create(tmp.path(), &name, template.id, &Options::defaults()).unwrap();
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
        let all: Vec<&str> = EXTRAS.iter().map(|e| e.id).collect();
        let folder = create(
            tmp.path(),
            "Shop",
            "python",
            &Options::from_ids(&all).unwrap(),
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
        let bare = create(tmp.path(), "Bare", "node", &Options::default()).unwrap();
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
            find("node").unwrap(),
            "n",
            "n",
            &Options::from_ids(&["codex"]).unwrap(),
        );
        assert!(!node.contains("features/node"), "{node}");

        assert!(Options::from_ids(&["telepathy"]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_persist_script_links_state_and_survives_a_rebuild() {
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let options = Options::from_ids(&["claude-code", "codex"]).unwrap();
        let folder = create(tmp.path(), "Shop", "node", &options).unwrap();
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
        let none = Options::default();
        assert!(create(tmp.path(), "!!!", "node", &none).is_err());
        assert!(create(tmp.path(), "shop", "cobol", &none).is_err());
        assert!(create(&tmp.path().join("missing"), "shop", "node", &none).is_err());

        let folder = create(tmp.path(), "Shop", "node", &none).unwrap();
        assert_eq!(folder, tmp.path().join("shop"));
        std::fs::write(folder.join("keep.txt"), "mine").unwrap();
        // A second attempt leaves the existing folder exactly as it is.
        assert!(create(tmp.path(), "shop", "python", &none).is_err());
        assert_eq!(
            std::fs::read_to_string(folder.join("keep.txt")).unwrap(),
            "mine"
        );
    }
}
