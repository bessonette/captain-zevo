//! Starting, stopping and rebuilding a project's container, and reading its
//! output.
//!
//! Stopping and restarting an existing container go through the runtime.
//! Creating or rebuilding one is the devcontainer tooling's job, so those run
//! the `devcontainer` command-line tool.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::model::{DevcontainerKind, LOCAL_HOST_ID, Project};
use crate::runtime::{ContainerInfo, Runtime};
use crate::{Error, Result, programs, reconcile};

/// Lines of output shown when a build fails.
const FAILURE_TAIL: usize = 25;

/// The project's container on the given runtime, if it has one.
pub async fn container(runtime: &dyn Runtime, project: &Project) -> Result<Option<ContainerInfo>> {
    let containers = runtime.containers().await?;
    Ok(reconcile::find_container(project, &containers).cloned())
}

/// Stops the project's container. Returns false if it was not running.
pub async fn stop(runtime: &dyn Runtime, project: &Project) -> Result<bool> {
    match container(runtime, project).await? {
        Some(c) if c.running => {
            runtime.stop(&c.id).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Starts the project's existing container. Returns false if there is no
/// container to start (it has to be built first).
pub async fn start_existing(runtime: &dyn Runtime, project: &Project) -> Result<bool> {
    match container(runtime, project).await? {
        Some(c) => {
            if !c.running {
                runtime.start(&c.id).await?;
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

/// The last `lines` lines the project's container printed.
pub async fn logs(runtime: &dyn Runtime, project: &Project, lines: usize) -> Result<String> {
    match container(runtime, project).await? {
        Some(c) => runtime.logs(&c.id, lines).await,
        None => Err(Error::Other("this project has no container yet".into())),
    }
}

/// Where the `devcontainer` command-line tool is installed, if it is.
pub fn devcontainer_cli() -> Option<PathBuf> {
    programs::find("devcontainer", &[])
}

/// Why a project cannot be built from here, if it cannot.
pub fn build_blocker(project: &Project) -> Option<String> {
    if project.host != LOCAL_HOST_ID {
        return Some("building containers on remote hosts is not supported yet".into());
    }
    if project.kind == DevcontainerKind::None {
        return Some("this folder has no devcontainer.json to build from".into());
    }
    if devcontainer_cli().is_none() {
        return Some(
            "the devcontainer command-line tool is not installed \
             (install it with `npm install -g @devcontainers/cli`)"
                .into(),
        );
    }
    None
}

/// Builds and starts the project's container with `devcontainer up`,
/// replacing the existing one when `rebuild` is set. Blocks until done,
/// which can take minutes.
pub fn up(project: &Project, rebuild: bool) -> Result<()> {
    if let Some(reason) = build_blocker(project) {
        return Err(Error::Other(reason));
    }
    let cli = devcontainer_cli().expect("build_blocker checked that the tool is installed");
    let mut command = Command::new(cli);
    command
        .arg("up")
        .arg("--workspace-folder")
        .arg(&project.path);
    if rebuild {
        command.arg("--remove-existing-container");
    }
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::io("starting the devcontainer tool", e))?;
    if output.status.success() {
        return Ok(());
    }
    // The tool logs progress to stderr; the end of it says what went wrong.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(FAILURE_TAIL)..].join("\n");
    Err(Error::Other(format!(
        "the container could not be built:\n{tail}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeRuntime;

    fn project(kind: DevcontainerKind, host: &str) -> Project {
        Project {
            slug: "shop".into(),
            name: "Shop".into(),
            host: host.into(),
            path: "/work/shop".into(),
            kind,
            service: None,
            routes: vec![],
            tags: vec![],
            declared: false,
            address: None,
            env_file: None,
        }
    }

    fn devcontainer(running: bool) -> ContainerInfo {
        ContainerInfo {
            id: "id-vsc-shop".into(),
            name: "vsc-shop".into(),
            running,
            labels: [(
                "devcontainer.local_folder".to_string(),
                "/work/shop".to_string(),
            )]
            .into(),
            ..ContainerInfo::default()
        }
    }

    #[tokio::test]
    async fn stops_and_starts_the_existing_container() {
        let rt = FakeRuntime::new();
        let p = project(DevcontainerKind::Image, LOCAL_HOST_ID);
        // Nothing to act on yet.
        assert!(!stop(&rt, &p).await.unwrap());
        assert!(!start_existing(&rt, &p).await.unwrap());
        assert!(logs(&rt, &p, 10).await.is_err());

        rt.add_container(devcontainer(true));
        rt.set_logs("vsc-shop", "one\ntwo\nthree\n");
        assert_eq!(logs(&rt, &p, 2).await.unwrap(), "two\nthree\n");

        assert!(stop(&rt, &p).await.unwrap());
        assert!(!rt.container("vsc-shop").unwrap().running);
        assert!(!stop(&rt, &p).await.unwrap());

        assert!(start_existing(&rt, &p).await.unwrap());
        assert!(rt.container("vsc-shop").unwrap().running);
    }

    #[test]
    fn explains_why_a_project_cannot_be_built() {
        let remote = build_blocker(&project(DevcontainerKind::Image, "buildbox")).unwrap();
        assert!(remote.contains("remote hosts"), "{remote}");
        let plain = build_blocker(&project(DevcontainerKind::None, LOCAL_HOST_ID)).unwrap();
        assert!(plain.contains("devcontainer.json"), "{plain}");
    }
}
