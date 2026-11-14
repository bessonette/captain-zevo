//! Keeps `examples/plugins` working: every example loads without problems,
//! and on unix its scripts run and answer the way Zevo expects.

use std::path::{Path, PathBuf};

use zevo_core::plugins::{EditorKind, PLUGINS_DIR, Plugins};

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins")
}

/// Copies `from` into `to`, keeping file permissions (and so the scripts'
/// executable bit).
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A config directory whose plugins folder holds every example, installed
/// the way the README says: each example's folder copied in whole.
fn install_examples() -> (tempfile::TempDir, Plugins) {
    let tmp = tempfile::tempdir().unwrap();
    let plugins_dir = tmp.path().join(PLUGINS_DIR);
    std::fs::create_dir_all(&plugins_dir).unwrap();
    let mut installed = 0;
    for entry in std::fs::read_dir(examples_dir()).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &plugins_dir.join(entry.file_name()));
            installed += 1;
        }
    }
    assert_eq!(installed, 4, "expected four example plugins");
    let plugins = Plugins::load(tmp.path());
    (tmp, plugins)
}

#[test]
fn the_examples_load_without_problems() {
    let (tmp, plugins) = install_examples();
    assert_eq!(plugins.problems, Vec::<String>::new());

    let rails = plugins.template("team-rails").expect("team-rails template");
    assert_eq!(rails.image, "mcr.microsoft.com/devcontainers/ruby:3");
    assert_eq!(rails.port, Some(3000));

    let aws = plugins.extra("team-aws").expect("team-aws extra");
    assert!(aws.default);
    assert!(
        aws.features
            .contains_key("ghcr.io/devcontainers/features/aws-cli:1")
    );
    assert_eq!(aws.persist_dirs, ["aws"]);
    let terraform = plugins
        .extra("team-terraform")
        .expect("team-terraform extra");
    assert!(
        terraform
            .features
            .contains_key("ghcr.io/devcontainers/features/terraform:1")
    );

    for (id, launcher) in [
        ("idea", "idea"),
        ("pycharm", "pycharm"),
        ("goland", "goland"),
    ] {
        let editor = plugins.editor(id).unwrap_or_else(|| panic!("editor {id}"));
        assert_eq!(editor.launcher, launcher);
        assert_eq!(editor.kind, EditorKind::Folder);
        assert_eq!(editor.remote, None);
    }

    let plugins_dir = tmp.path().join(PLUGINS_DIR);
    assert_eq!(plugins.hooks.len(), 1);
    assert_eq!(
        plugins.hooks[0].command,
        [plugins_dir
            .join("notify")
            .join("notify.sh")
            .to_string_lossy()
            .into_owned()]
    );
    assert_eq!(plugins.hooks[0].actions, ["attached_project"]);

    assert_eq!(plugins.checks.len(), 1);
    assert_eq!(plugins.checks[0].name, "vpn");
    assert_eq!(
        plugins.checks[0].command,
        [plugins_dir
            .join("vpn-check")
            .join("vpn-check.sh")
            .to_string_lossy()
            .into_owned()]
    );

    assert_eq!(plugins.issuer, None);
}

#[cfg(unix)]
#[test]
fn the_example_scripts_run_and_answer() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use zevo_core::doctor::Level;
    use zevo_core::plugins::run::run_blocking;

    let (tmp, plugins) = install_examples();

    // The check: one line of JSON with a level Zevo knows. Which level
    // depends on this machine's network, so any is accepted. 10 seconds is
    // what `zevo doctor` allows.
    let out = run_blocking(
        &plugins.checks[0].command,
        serde_json::json!({"request": "check"}),
        tmp.path(),
        Duration::from_secs(10),
    )
    .expect("the check runs");
    let reply: serde_json::Value = serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("the check printed {out:?}, not JSON: {e}"));
    let level: Level = serde_json::from_value(reply["level"].clone())
        .unwrap_or_else(|e| panic!("the check replied {reply}, with no valid level: {e}"));
    assert!(matches!(level, Level::Ok | Level::Warn | Level::Fail));
    assert!(
        reply["detail"].as_str().is_some_and(|d| !d.is_empty()),
        "{reply}"
    );

    // The hook, given a pass that attached a project, as reconcile sends
    // it. It runs with a PATH holding only the tools it needs, so that no
    // real notifier is found: first none at all, which must still succeed,
    // then a stand-in `notify-send` that records its message.
    let request = serde_json::json!({
        "event": "reconcile",
        "actions": [
            {"action": "wrote_routes", "slug": "shop"},
            {"action": "attached_project", "slug": "shop", "container": "shop-app-1"},
        ],
        "projects": [
            {"slug": "shop", "state": "running", "attached": true, "urls": ["https://shop.localhost"]},
        ],
    });
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for tool in ["tr", "grep", "sed"] {
        let found = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join(tool))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("{tool} is not on the PATH"));
        std::os::unix::fs::symlink(found, bin.join(tool)).unwrap();
    }
    let hook = |request: serde_json::Value| {
        let mut command = vec![
            "/usr/bin/env".to_string(),
            format!("PATH={}", bin.display()),
        ];
        command.extend(plugins.hooks[0].command.iter().cloned());
        // Hooks get 5 seconds.
        run_blocking(&command, request, tmp.path(), Duration::from_secs(5))
    };
    hook(request.clone()).expect("the hook succeeds without a notifier");

    let said = tmp.path().join("notified");
    let fake = bin.join("notify-send");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\nprintf '%s' \"$2\" > '{}'\n", said.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    hook(request).expect("the hook succeeds with a notifier");
    // The notifier is started in the background.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !said.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(&said).expect("the hook ran notify-send"),
        "Ready: shop"
    );
}
