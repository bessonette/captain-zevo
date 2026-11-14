//! `zevo` — command-line front end for Captain Zevo.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use zevo_core::certs;
use zevo_core::ide;
use zevo_core::model::{Host, HostDetails, LOCAL_HOST_ID, Route, Target};
use zevo_core::reconcile::{self, Action, ContainerState, Report};
use zevo_core::runtime::{DockerRuntime, Runtime};
use zevo_core::store::Store;
use zevo_core::{doctor, lifecycle, register, remote, setup, templates, traefik};

#[derive(Parser)]
#[command(
    name = "zevo",
    version,
    about = "Manage devcontainer projects and their Traefik routes"
)]
struct Cli {
    /// Print machine-readable JSON where supported.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Adopt a running Traefik or start one.
    Setup(SetupArgs),
    /// Register an existing project folder.
    Add(AddArgs),
    /// Create a new project folder from a starter template and register it.
    New {
        /// Project name; the folder is named after it.
        name: String,
        /// Starter template id (see `zevo plugins`; plugins can add more).
        #[arg(long, default_value = "node")]
        template: String,
        /// Folder to create the project in (default: the current folder).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Add an extra, by id (see `zevo plugins`). Repeatable.
        #[arg(long = "with", value_name = "EXTRA")]
        with: Vec<String>,
        /// Leave out an extra that is on by default. Repeatable.
        #[arg(long = "without", value_name = "EXTRA")]
        without: Vec<String>,
    },
    /// Unregister a project and remove its routes.
    Rm { slug: String },
    /// List projects with their status and URLs.
    Ls,
    /// Show one project, including the Traefik config generated for it.
    Show { slug: String },
    /// Add or remove a route on a project.
    Route {
        #[command(subcommand)]
        command: RouteCommand,
    },
    /// Write route files and attach project containers to the network.
    Reconcile {
        /// Keep running and reconcile repeatedly.
        #[arg(long)]
        watch: bool,
        /// Seconds between passes with --watch.
        #[arg(long, default_value_t = 5)]
        interval: u64,
    },
    /// Check that everything Zevo depends on is working.
    Doctor,
    /// List devcontainers, here and on remote hosts, that are not registered yet.
    Discover,
    /// Start a project's container, building it first if it does not exist.
    Up {
        slug: String,
        /// Rebuild the container from its devcontainer.json, replacing it.
        #[arg(long)]
        rebuild: bool,
    },
    /// Stop a project's container.
    Down { slug: String },
    /// Print the end of a project's container output.
    Logs {
        slug: String,
        /// Number of lines to show.
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
    },
    /// Manage other machines that run project containers.
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Open a project in an editor, attached to its devcontainer.
    Open {
        slug: String,
        /// Editor id: vscode, cursor, windsurf, vscodium, zed, or one a
        /// plugin adds (default: your setting, else the first one installed).
        #[arg(long)]
        editor: Option<String>,
    },
    /// Show the plugins folder and what the plugins in it add.
    Plugins,
    /// Any other command runs a program called `zevo-<command>`, from the
    /// PATH or the plugins `bin` folder, with the remaining arguments.
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Args)]
struct SetupArgs {
    /// Adopt this Traefik container instead of choosing automatically.
    #[arg(long, conflicts_with = "bootstrap")]
    adopt: Option<String>,
    /// Start a Zevo-managed Traefik even if another one is running.
    #[arg(long)]
    bootstrap: bool,
    /// Host HTTP port for the managed Traefik (default: 80, else first free from 8080).
    #[arg(long)]
    port: Option<u16>,
    /// Adopt a Traefik that Zevo cannot inspect: its file-provider directory.
    #[arg(long, requires = "entrypoint", conflicts_with_all = ["adopt", "bootstrap"])]
    dynamic_dir: Option<PathBuf>,
    /// Entrypoint name to use with --dynamic-dir.
    #[arg(long, requires = "dynamic_dir")]
    entrypoint: Option<String>,
    /// With --dynamic-dir: the entrypoint serves HTTPS.
    #[arg(long, requires = "dynamic_dir")]
    tls: bool,
}

#[derive(Args)]
struct AddArgs {
    /// Project folder (on the remote machine when --host is given).
    path: PathBuf,
    /// Id of the remote host the folder is on (see `zevo host ls`).
    #[arg(long)]
    host: Option<String>,
    /// Short id used in the hostname (default: derived from the folder name).
    #[arg(long)]
    slug: Option<String>,
    /// Display name.
    #[arg(long)]
    name: Option<String>,
    /// Route to add: PORT or SUBDOMAIN:PORT. Repeatable. Default: the first detected port.
    #[arg(long = "route", short = 'r', value_name = "[SUB:]PORT")]
    routes: Vec<String>,
    /// Route to this address instead of the container (remote host or published port).
    #[arg(long, value_name = "HOST")]
    address: Option<String>,
    #[arg(long = "tag")]
    tags: Vec<String>,
    /// Leave out a route the project declares, by subdomain or exact
    /// hostname. Repeatable. For a second checkout of the same repository.
    #[arg(long = "skip", value_name = "NAME")]
    skip: Vec<String>,
}

#[derive(Subcommand)]
enum HostCommand {
    /// Register a machine reached over SSH (key-based login required).
    Add {
        /// user@address, or a name from your SSH config.
        ssh: String,
        /// Display name (default: the address).
        #[arg(long)]
        name: Option<String>,
        /// Address this machine's Traefik uses to reach it, if different. A
        /// name is fine: Zevo looks up its current IP on every reconcile.
        #[arg(long)]
        address: Option<String>,
        /// Path of the Docker socket there, if Zevo cannot find it.
        #[arg(long, conflicts_with = "docker_command")]
        docker_socket: Option<String>,
        /// Command that runs Docker there, when it is reached through a
        /// wrapper and not a socket: a script, or "sudo -n docker".
        #[arg(long)]
        docker_command: Option<String>,
        /// Folder there that holds your projects; only devcontainers inside
        /// it are offered by `zevo discover`.
        #[arg(long, value_name = "FOLDER")]
        projects_dir: Option<String>,
    },
    /// Change a host. Options left out keep their value; pass an empty
    /// value ("") to clear one.
    Set {
        id: String,
        #[arg(long)]
        ssh: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        address: Option<String>,
        #[arg(long, conflicts_with = "docker_command")]
        docker_socket: Option<String>,
        #[arg(long)]
        docker_command: Option<String>,
        #[arg(long, value_name = "FOLDER")]
        projects_dir: Option<String>,
    },
    /// List hosts and whether Docker answers on each.
    Ls,
    /// Unregister a host that has no projects.
    Rm { id: String },
}

#[derive(Subcommand)]
enum RouteCommand {
    /// Add a route: PORT or SUBDOMAIN:PORT.
    Add {
        slug: String,
        #[arg(value_name = "[SUB:]PORT")]
        route: String,
        #[arg(long)]
        path_prefix: Option<String>,
        #[arg(long, value_name = "HOST")]
        address: Option<String>,
    },
    /// Remove the route for a subdomain (omit it for the main route).
    Rm {
        slug: String,
        subdomain: Option<String>,
        /// Path prefix of the route to remove, if it has one.
        #[arg(long)]
        path_prefix: Option<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .init();
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Connects to the runtime, returning `None` (with the reason) when unreachable.
async fn connect() -> (Option<DockerRuntime>, Option<String>) {
    match DockerRuntime::connect() {
        Ok(runtime) => match runtime.ping().await {
            Ok(_) => (Some(runtime), None),
            Err(e) => (None, Some(reason(e))),
        },
        Err(e) => (None, Some(reason(e))),
    }
}

fn reason(e: zevo_core::Error) -> String {
    match e {
        zevo_core::Error::Runtime(message) => message,
        other => other.to_string(),
    }
}

/// The runtime a project's container lives on. For a remote project the SSH
/// connection is returned too and must be kept until the runtime is done with.
async fn runtime_for(
    store: &Store,
    project: &zevo_core::model::Project,
) -> Result<(Option<remote::Tunnel>, DockerRuntime)> {
    if project.host == LOCAL_HOST_ID {
        return Ok((None, require_runtime().await?));
    }
    let tunnel = remote::Tunnel::open(&store.host(&project.host)?)?;
    let runtime = tunnel.runtime()?;
    Ok((Some(tunnel), runtime))
}

async fn require_runtime() -> Result<DockerRuntime> {
    let (runtime, reason) = connect().await;
    runtime.ok_or_else(|| {
        anyhow!(
            "cannot reach a container runtime: {}",
            reason.unwrap_or_default()
        )
    })
}

fn parse_route(spec: &str, address: Option<&str>, path_prefix: Option<String>) -> Result<Route> {
    let (subdomain, port) = match spec.split_once(':') {
        Some((sub, port)) => (Some(sub.to_string()), port),
        None => (None, spec),
    };
    if let Some(sub) = &subdomain {
        zevo_core::model::validate_slug(sub).context("invalid subdomain")?;
    }
    let port: u16 = port
        .parse()
        .with_context(|| format!("invalid port in route '{spec}'"))?;
    if let Some(prefix) = &path_prefix
        && !prefix.starts_with('/')
    {
        bail!("path prefix must start with '/'");
    }
    Ok(Route {
        backend: zevo_core::model::Backend::default(),
        host: None,
        subdomain,
        port,
        path_prefix,
        target: match address {
            Some(address) => Target::HostPort {
                address: address.to_string(),
            },
            None => Target::Container,
        },
    })
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let store = Store::open_default()?;
    match cli.command {
        Command::Setup(args) => cmd_setup(&store, args).await?,
        Command::Add(args) => cmd_add(&store, args, cli.json).await?,
        Command::New {
            name,
            template,
            dir,
            with,
            without,
        } => {
            if store.settings()?.traefik.is_none() {
                bail!(zevo_core::Error::NotSetUp);
            }
            let parent = match dir {
                Some(dir) => dir,
                None => std::env::current_dir().context("cannot tell the current folder")?,
            };
            let plugins = zevo_core::plugins::Plugins::load(store.dir());
            // Validate both lists, then: defaults, plus --with, minus --without.
            templates::Options::from_ids(&plugins, &without)?;
            let mut chosen: Vec<String> = templates::Options::defaults(&plugins)
                .extras()
                .iter()
                .map(|e| e.id.clone())
                .chain(with)
                .collect();
            chosen.retain(|id| !without.contains(id));
            let options = templates::Options::from_ids(&plugins, &chosen)?;
            let folder = templates::create(&plugins, &parent, &name, &template, &options)?;
            let project = register::draft(&folder)?;
            register::add(&store, &project)?;
            let report = reconcile_once(&store).await?;
            println!(
                "Created {} and registered it as '{}'.",
                folder.display(),
                project.slug
            );
            for url in reconcile::urls(&project, &store.settings()?) {
                println!("  {url}");
            }
            println!("Build and start it with: zevo up {}", project.slug);
            print_warnings(&report);
        }
        Command::Rm { slug } => {
            register::remove(&store, &slug)?;
            println!("Removed {slug}.");
        }
        Command::Ls => {
            let report = reconcile_once(&store).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report.projects)?);
            } else {
                print_projects(&report);
            }
        }
        Command::Show { slug } => {
            let project = store.project(&slug)?;
            let settings = store.settings()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&project)?);
            } else {
                println!("{} ({})", project.name, project.slug);
                println!("  folder: {}", project.path.display());
                println!("  host:   {}", project.host);
                for url in reconcile::urls(&project, &settings) {
                    println!("  url:    {url}");
                }
                println!("\n{}", traefik::render(&project, &settings)?);
            }
        }
        Command::Route { command } => cmd_route(&store, command).await?,
        Command::Reconcile { watch, interval } => {
            if !watch {
                let report = reconcile_once(&store).await?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    print_actions(&report);
                    if report.actions.is_empty() {
                        println!("Everything is up to date.");
                    }
                }
            } else {
                println!("Watching; press Ctrl-C to stop.");
                let mut last_warnings = Vec::new();
                loop {
                    match reconcile_once(&store).await {
                        Ok(report) => {
                            print_actions_only(&report);
                            if report.warnings != last_warnings {
                                print_warnings(&report);
                                last_warnings = report.warnings;
                            }
                        }
                        Err(e) => eprintln!("error: {e:#}"),
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(interval.max(1))) => {}
                        _ = tokio::signal::ctrl_c() => break,
                    }
                }
            }
        }
        Command::Host { command } => match command {
            HostCommand::Add {
                ssh,
                name,
                address,
                docker_socket,
                docker_command,
                projects_dir,
            } => {
                let details = HostDetails {
                    ssh,
                    name: name.unwrap_or_default(),
                    address: address.unwrap_or_default(),
                    docker_socket: docker_socket.unwrap_or_default(),
                    docker_command: docker_command.unwrap_or_default(),
                    projects_dir: projects_dir.unwrap_or_default(),
                };
                let (host, version) = remote::register_host(&store, &details).await?;
                println!(
                    "Added host '{}' ({version}), routed at {}.",
                    host.id, host.address
                );
                match (&host.docker_command, &host.docker_socket) {
                    (Some(command), _) => println!("Docker there is reached with: {command}"),
                    (None, Some(socket)) => println!("Docker socket there: {socket}"),
                    (None, None) => {}
                }
                println!(
                    "Add its projects with: zevo add --host {} <folder on that machine>",
                    host.id
                );
            }
            HostCommand::Set {
                id,
                ssh,
                name,
                address,
                docker_socket,
                docker_command,
                projects_dir,
            } => {
                let host = store.host(&id)?;
                // Another machine: the address and the socket found on the
                // old one no longer apply, unless given again.
                let moved = ssh.as_ref().is_some_and(|ssh| ssh.trim() != host.ssh);
                let kept = |value: Option<String>| {
                    if moved {
                        String::new()
                    } else {
                        value.unwrap_or_default()
                    }
                };
                // Choosing one way to reach Docker drops the other.
                let (socket, command) = match (docker_socket, docker_command) {
                    (Some(socket), None) => (socket, String::new()),
                    (None, Some(command)) => (String::new(), command),
                    _ => (
                        kept(host.docker_socket),
                        host.docker_command.unwrap_or_default(),
                    ),
                };
                let details = HostDetails {
                    ssh: ssh.unwrap_or(host.ssh),
                    name: name.unwrap_or(host.name),
                    address: address.unwrap_or_else(|| kept(Some(host.address))),
                    docker_socket: socket,
                    docker_command: command,
                    projects_dir: projects_dir.or(host.projects_dir).unwrap_or_default(),
                };
                let (host, version) = remote::update_host(&store, &id, &details).await?;
                println!(
                    "Saved host '{}' ({version}), routed at {}.",
                    host.id, host.address
                );
            }
            HostCommand::Ls => {
                let hosts = store.hosts()?;
                if hosts.is_empty() {
                    println!("No remote hosts. Add one with: zevo host add user@address");
                }
                for (host, (_, opened)) in hosts.iter().zip(remote::open_all(&hosts)) {
                    let status = match opened {
                        Ok((_tunnel, runtime)) => {
                            runtime.ping().await.unwrap_or_else(|e| e.to_string())
                        }
                        Err(e) => e,
                    };
                    println!("{:<16} {:<28} {}", host.id, host.ssh, status);
                }
            }
            HostCommand::Rm { id } => {
                let note = remote::forget_host(&store, &id).await?;
                println!("Removed host {id}.");
                if let Some(note) = note {
                    eprintln!("warning: {note}");
                }
            }
        },
        Command::Up { slug, rebuild } => {
            let project = store.project(&slug)?;
            let (_tunnel, runtime) = runtime_for(&store, &project).await?;
            if !rebuild && lifecycle::start_existing(&runtime, &project).await? {
                println!("Started {slug}.");
            } else {
                println!("Building the container for {slug}; this can take a few minutes...");
                tokio::task::spawn_blocking(move || lifecycle::up(&project, rebuild)).await??;
                println!("{slug} is up.");
            }
            // Attach the container and point the routes at it.
            print_actions(&reconcile_once(&store).await?);
        }
        Command::Down { slug } => {
            let project = store.project(&slug)?;
            let (_tunnel, runtime) = runtime_for(&store, &project).await?;
            if lifecycle::stop(&runtime, &project).await? {
                println!("Stopped {slug}.");
            } else {
                println!("{slug} was not running.");
            }
        }
        Command::Logs { slug, lines } => {
            let project = store.project(&slug)?;
            let (_tunnel, runtime) = runtime_for(&store, &project).await?;
            print!("{}", lifecycle::logs(&runtime, &project, lines).await?);
        }
        Command::Discover => {
            let report = reconcile_scope(&store, true).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report.discovered)?);
            } else if report.discovered.is_empty() {
                println!("Every devcontainer on this machine is already registered.");
            } else {
                for d in &report.discovered {
                    let state = if d.running { "running" } else { "stopped" };
                    let host = if d.host == LOCAL_HOST_ID {
                        "this machine"
                    } else {
                        d.host.as_str()
                    };
                    println!("{:<10} {:<16} {}", state, host, d.folder.display());
                }
                println!("\nRegister one with: zevo add [--host <id>] <folder>");
            }
        }
        Command::Open { slug, editor } => {
            let project = store.project(&slug)?;
            let known = zevo_core::plugins::Plugins::load(store.dir()).editors;
            let editor = match editor {
                Some(name) => {
                    let id = name.to_lowercase();
                    known.iter().find(|e| e.id == id).cloned().ok_or_else(|| {
                        let ids: Vec<&str> = known.iter().map(|e| e.id.as_str()).collect();
                        anyhow!("unknown editor '{name}' (use {})", ids.join(", "))
                    })?
                }
                None => {
                    let chosen = store.settings()?.editor;
                    let installed = ide::available(&known);
                    ide::preferred(chosen.as_deref(), &installed)
                        .cloned()
                        .ok_or_else(|| {
                            anyhow!(
                                "no supported editor found (looked for {})",
                                ide::list_names(&known)
                            )
                        })?
                }
            };
            let host = match project.host.as_str() {
                LOCAL_HOST_ID => None,
                id => Some(store.host(id)?),
            };
            ide::open(&editor, &project, host.as_ref())?;
            println!("Opening {} in {}.", project.slug, editor.name);
        }
        Command::Doctor => {
            let (runtime, _) = connect().await;
            let checks = doctor::run(&store, runtime.as_ref().map(|r| r as &dyn Runtime)).await;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&checks)?);
            } else {
                for c in &checks {
                    let mark = match c.level {
                        doctor::Level::Ok => "ok  ",
                        doctor::Level::Warn => "warn",
                        doctor::Level::Fail => "FAIL",
                    };
                    println!("[{mark}] {:<16} {}", c.name, c.detail);
                }
            }
            if checks.iter().any(|c| c.level == doctor::Level::Fail) {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Plugins => cmd_plugins(&store, cli.json)?,
        Command::External(args) => return run_external(&store, &args),
    }
    Ok(ExitCode::SUCCESS)
}

/// `zevo plugins`: where plugins go and what they add.
fn cmd_plugins(store: &Store, json: bool) -> Result<()> {
    use zevo_core::plugins::{PLUGINS_DIR, Plugins};
    let dir = store.dir().join(PLUGINS_DIR);
    let plugins = Plugins::load(store.dir());
    let builtin = Plugins::builtin();
    // Templates, extras and editors as id, name, description, and whether
    // it is the one Zevo ships with.
    let templates: Vec<_> = plugins
        .templates
        .iter()
        .map(|t| {
            (
                &t.id,
                &t.name,
                &t.description,
                builtin.template(&t.id) == Some(t),
            )
        })
        .collect();
    let extras: Vec<_> = plugins
        .extras
        .iter()
        .map(|e| {
            (
                &e.id,
                &e.name,
                &e.description,
                builtin.extra(&e.id) == Some(e),
            )
        })
        .collect();
    let editors: Vec<_> = plugins
        .editors
        .iter()
        .map(|e| {
            (
                &e.id,
                &e.name,
                &e.launcher,
                builtin.editor(&e.id) == Some(e),
            )
        })
        .collect();

    if json {
        let entries = |items: &[(&String, &String, &String, bool)], third: &str| {
            items
                .iter()
                .map(|(id, name, other, builtin)| {
                    serde_json::json!({"id": id, "name": name, third: other, "builtin": builtin})
                })
                .collect::<Vec<_>>()
        };
        let out = serde_json::json!({
            "dir": dir,
            "templates": entries(&templates, "description"),
            "extras": entries(&extras, "description"),
            "editors": entries(&editors, "launcher"),
            "hooks": plugins.hooks.iter().map(|h| serde_json::json!({
                "command": h.command,
                "actions": h.actions,
            })).collect::<Vec<_>>(),
            "checks": plugins.checks.iter().map(|c| serde_json::json!({
                "name": c.name,
                "command": c.command,
            })).collect::<Vec<_>>(),
            "issuer": plugins.issuer.as_ref().map(|i| serde_json::json!({
                "name": i.name,
                "command": i.command,
                "renew_after_hours": i.renew_after_hours,
            })),
            "problems": plugins.problems,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let mark = |builtin: bool| if builtin { "  (built in)" } else { "" };
    println!("Plugins folder: {}", dir.display());
    for (title, items) in [
        ("Templates", &templates),
        ("Extras", &extras),
        ("Editors", &editors),
    ] {
        println!("\n{title}:");
        for (id, name, _, builtin) in items {
            println!("  {id:<16} {name}{}", mark(*builtin));
        }
    }
    println!("\nHooks:");
    if plugins.hooks.is_empty() {
        println!("  none");
    }
    for hook in &plugins.hooks {
        let when = if hook.actions.is_empty() {
            "after any change".to_string()
        } else {
            format!("after {}", hook.actions.join(", "))
        };
        println!("  {}  ({when})", hook.command.join(" "));
    }
    println!("\nChecks:");
    if plugins.checks.is_empty() {
        println!("  none");
    }
    for check in &plugins.checks {
        println!("  {:<16} {}", check.name, check.command.join(" "));
    }
    println!("\nCertificate issuer:");
    match &plugins.issuer {
        Some(issuer) => println!("  {:<16} {}", issuer.name, issuer.command.join(" ")),
        None => println!("  mkcert{}", mark(true)),
    }
    if !plugins.problems.is_empty() {
        println!("\nProblems:");
        for problem in &plugins.problems {
            println!("  {problem}");
        }
    }
    Ok(())
}

/// Runs `zevo-<name>` for a command Zevo does not have, as git and cargo
/// do, and exits with its status. The program shares the terminal and gets
/// `ZEVO_CONFIG_DIR` (this registry) and `ZEVO_BIN` (this `zevo`).
fn run_external(store: &Store, args: &[String]) -> Result<ExitCode> {
    let Some((name, rest)) = args.split_first() else {
        bail!("no command given; run `zevo --help` to see the commands");
    };
    let program = format!("zevo-{name}");
    let bin = zevo_core::plugins::run::bin_dir(store.dir());
    let found = if name.is_empty() || name.contains(['/', '\\']) {
        None
    } else {
        zevo_core::programs::find(&program, std::slice::from_ref(&bin))
    };
    let Some(path) = found else {
        // Most often this is a mistyped command, not a missing plugin.
        let hint = match nearest_command(name) {
            Some(near) => format!("Did you mean `zevo {near}`?"),
            None => "Run `zevo --help` to see the commands.".to_string(),
        };
        bail!(
            "'{name}' is not a zevo command, and no program called {program} was found on the \
             PATH or in {}. {hint}",
            bin.display()
        );
    };
    let mut command = std::process::Command::new(&path);
    command
        .args(rest)
        .env(zevo_core::store::CONFIG_DIR_ENV, store.dir());
    if let Ok(exe) = std::env::current_exe() {
        command.env("ZEVO_BIN", exe);
    }
    let status = command
        .status()
        .with_context(|| format!("could not run {}", path.display()))?;
    Ok(match status.code() {
        Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        // Stopped by a signal.
        None => ExitCode::FAILURE,
    })
}

/// The built-in command closest to a mistyped one, if any is close.
fn nearest_command(typed: &str) -> Option<String> {
    use clap::CommandFactory as _;
    Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .map(|name| (edit_distance(typed, &name), name))
        .filter(|(distance, _)| *distance <= 2)
        .min()
        .map(|(_, name)| name)
}

/// How many single-character insertions, deletions and replacements turn
/// `a` into `b`.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let replace = diagonal + usize::from(x != *y);
            diagonal = row[j + 1];
            row[j + 1] = replace.min(diagonal + 1).min(row[j] + 1);
        }
    }
    row[b.len()]
}

async fn reconcile_once(store: &Store) -> Result<Report> {
    reconcile_scope(store, false).await
}

/// Reconciles, connecting to the remote hosts that have projects, or to every
/// host when `all_hosts` is set (discovery needs to look at all of them).
async fn reconcile_scope(store: &Store, all_hosts: bool) -> Result<Report> {
    let (runtime, reason) = connect().await;

    // One SSH connection per host, opened together and kept for this pass.
    let projects = store.projects()?;
    let hosts: Vec<Host> = store
        .hosts()?
        .into_iter()
        .filter(|h| all_hosts || projects.iter().any(|p| p.host == h.id))
        .collect();
    let tunnels = remote::open_all(&hosts);
    let mut remotes = reconcile::RemoteRuntimes::new();
    for (id, opened) in &tunnels {
        remotes.insert(
            id.clone(),
            match opened {
                Ok((_, runtime)) => Ok(runtime as &dyn Runtime),
                Err(e) => Err(e.clone()),
            },
        );
    }

    let mut report = reconcile::reconcile_hosts(
        store,
        runtime.as_ref().map(|r| r as &dyn Runtime),
        &remotes,
        certs::issuer(&zevo_core::plugins::Plugins::load(store.dir()), store.dir()).as_ref(),
    )
    .await?;
    if let Some(reason) = reason {
        report
            .warnings
            .insert(0, format!("container runtime unreachable: {reason}"));
    }
    Ok(report)
}

async fn cmd_setup(store: &Store, args: SetupArgs) -> Result<()> {
    if let (Some(dir), Some(entrypoint)) = (&args.dynamic_dir, &args.entrypoint) {
        let port = args.port.unwrap_or(if args.tls { 443 } else { 80 });
        setup::adopt_manual(store, dir, entrypoint, port, args.tls)?;
        println!("Using Traefik routes directory {}.", dir.display());
        println!("Zevo cannot attach containers to this Traefik; add projects with --address.");
        return Ok(());
    }

    let runtime = require_runtime().await?;
    let found = setup::find_traefik(&runtime).await?;
    let chosen = if args.bootstrap {
        None
    } else if let Some(name) = &args.adopt {
        Some(
            found
                .iter()
                .find(|c| &c.container == name)
                .ok_or_else(|| anyhow!("no Traefik container named '{name}'"))?,
        )
    } else {
        match found.iter().find(|c| c.adoptable()) {
            Some(candidate) => Some(candidate),
            None if found
                .iter()
                .any(|c| c.running && c.container != setup::MANAGED_CONTAINER) =>
            {
                // A Traefik is running but unusable: starting a second one would
                // fight it for ports, so stop and explain instead.
                for c in found.iter().filter(|c| c.running) {
                    eprintln!("Traefik container '{}' cannot be adopted:", c.container);
                    for problem in &c.problems {
                        eprintln!("  - {problem}");
                    }
                }
                bail!("fix the above, or run `zevo setup --bootstrap` to start a separate Traefik");
            }
            None => None,
        }
    };

    let settings = match chosen {
        Some(candidate) => {
            let settings = setup::adopt(store, &runtime, candidate).await?;
            println!("Adopted Traefik container '{}'.", candidate.container);
            settings
        }
        None => {
            println!(
                "Starting Traefik container '{}'...",
                setup::MANAGED_CONTAINER
            );
            {
                // HTTPS needs locally trusted certificates; without mkcert
                // (or the certificate plugin), use HTTP.
                let https = match certs::issuer(
                    &zevo_core::plugins::Plugins::load(store.dir()),
                    store.dir(),
                )
                .ready()
                {
                    Ok(()) => true,
                    Err(reason) => {
                        eprintln!("warning: starting Traefik without HTTPS. {reason}");
                        false
                    }
                };
                setup::bootstrap(store, &runtime, args.port, https).await?
            }
        }
    };
    let traefik = settings
        .traefik
        .expect("setup always records Traefik settings");
    println!("  routes directory: {}", traefik.dynamic_dir.display());
    println!(
        "  entrypoint:       {} (port {})",
        traefik.entrypoint, traefik.http_port
    );
    println!("  network:          {}", settings.network);
    Ok(())
}

async fn cmd_add(store: &Store, args: AddArgs, json: bool) -> Result<()> {
    let settings = store.settings()?;
    let mut project = match &args.host {
        Some(id) => register::draft_remote(&store.host(id)?, &args.path.to_string_lossy())?,
        None => register::draft(&args.path)?,
    };
    if let Some(slug) = args.slug {
        project.slug = slug;
    }
    if let Some(name) = args.name {
        project.name = name;
    }
    project.tags = args.tags;
    if project.declared {
        if !args.routes.is_empty() {
            bail!(
                "this project defines its routes in devcontainer.json (customizations.zevo); remove --route"
            );
        }
        project.skip = args.skip.clone();
        project.apply_skip();
        project.address = args.address.clone();
        register::apply_address(&mut project);
    } else if !args.routes.is_empty() {
        project.routes.clear();
        for spec in &args.routes {
            project
                .routes
                .push(parse_route(spec, args.address.as_deref(), None)?);
        }
    } else if let Some(address) = &args.address {
        for route in &mut project.routes {
            route.target = Target::HostPort {
                address: address.clone(),
            };
        }
    }
    register::add(store, &project)?;
    let report = reconcile_once(store).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&project)?);
        return Ok(());
    }
    println!(
        "Registered {} ({:?} devcontainer).",
        project.slug, project.kind
    );
    if project.routes.is_empty() {
        println!(
            "No ports detected, so no routes yet. Add one with: zevo route add {} PORT",
            project.slug
        );
    }
    for url in reconcile::urls(&project, &settings) {
        println!("  {url}");
    }
    print_warnings(&report);
    Ok(())
}

async fn cmd_route(store: &Store, command: RouteCommand) -> Result<()> {
    let settings = store.settings()?;
    let project = match command {
        RouteCommand::Add {
            slug,
            route,
            path_prefix,
            address,
        } => {
            let mut project = store.project(&slug)?;
            register::ensure_routes_editable(&project)?;
            project
                .routes
                .push(parse_route(&route, address.as_deref(), path_prefix)?);
            project
        }
        RouteCommand::Rm {
            slug,
            subdomain,
            path_prefix,
        } => {
            let mut project = store.project(&slug)?;
            register::ensure_routes_editable(&project)?;
            let before = project.routes.len();
            // Exactly the route with this subdomain and path prefix; routes
            // with an exact hostname are not addressed by subdomain.
            project.routes.retain(|r| {
                !(r.host.is_none() && r.subdomain == subdomain && r.path_prefix == path_prefix)
            });
            if project.routes.len() == before {
                bail!(
                    "project '{slug}' has no such route (if it has a path prefix, pass --path-prefix)"
                );
            }
            project
        }
    };
    register::update(store, &project)?;
    let report = reconcile_once(store).await?;
    for url in reconcile::urls(&project, &settings) {
        println!("  {url}");
    }
    print_warnings(&report);
    Ok(())
}

fn print_projects(report: &Report) {
    if report.projects.is_empty() {
        println!("No projects yet. Register one with: zevo add <folder>");
    }
    for p in &report.projects {
        let state = match p.state {
            ContainerState::Running if p.attached => "running",
            ContainerState::Running => "running (not attached)",
            ContainerState::Stopped => "stopped",
            ContainerState::Missing => "no container",
            ContainerState::Unknown => "unknown",
        };
        println!("{:<20} {:<24} {}", p.slug, state, p.urls.join("  "));
        for (url, backend) in p.urls.iter().zip(&p.backends) {
            if *backend == zevo_core::health::Listening::LoopbackOnly {
                println!(
                    "  ! {url}: the app listens only on 127.0.0.1 inside the container, so \
                     Traefik cannot reach it. Start it on 0.0.0.0."
                );
            }
        }
    }
    print_warnings(report);
}

fn print_actions_only(report: &Report) {
    for action in &report.actions {
        match action {
            Action::WroteRoutes { slug } => println!("wrote routes for {slug}"),
            Action::RemovedOrphanRoutes { slug } => println!("removed leftover routes for {slug}"),
            Action::CreatedNetwork { network } => println!("created network {network}"),
            Action::AttachedTraefik { container } => {
                println!("attached Traefik ({container}) to the network")
            }
            Action::AttachedProject { slug, container } => {
                println!("attached {slug} ({container})")
            }
            Action::IssuedCertificate { slug } => println!("created HTTPS certificate for {slug}"),
            Action::WroteEnvFile { slug } => println!("wrote env file for {slug}"),
            Action::StartedEdge { host } => println!("started the edge Traefik on {host}"),
            Action::WroteEdgeRoutes { slug } => println!("wrote edge routes for {slug}"),
        }
    }
}

fn print_actions(report: &Report) {
    print_actions_only(report);
    print_warnings(report);
}

fn print_warnings(report: &Report) {
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_command_is_kept_with_its_arguments_for_a_zevo_program() {
        let cli = Cli::try_parse_from(["zevo", "frobnicate", "a", "--json", "-x"]).unwrap();
        match cli.command {
            Command::External(args) => assert_eq!(args, ["frobnicate", "a", "--json", "-x"]),
            _ => panic!("expected an external command"),
        }
        assert!(matches!(
            Cli::try_parse_from(["zevo", "plugins"]).unwrap().command,
            Command::Plugins
        ));
    }

    #[test]
    fn an_unknown_command_without_a_program_explains_itself() {
        let tmp = tempfile_dir();
        let err = run_external(&Store::open(&tmp), &["zevo-test-nothing-here".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not a zevo command"), "{err}");
        assert!(err.contains("zevo --help"), "{err}");
        let err = run_external(&Store::open(&tmp), &["../x".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not a zevo command"), "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn an_external_command_gets_its_arguments_and_environment_and_its_exit_code_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile_dir();
        let bin = zevo_core::plugins::run::bin_dir(&tmp);
        std::fs::create_dir_all(&bin).unwrap();
        let program = bin.join("zevo-zevo-test-hello");
        let out = tmp.join("out");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$*\" \"$ZEVO_CONFIG_DIR\" \"$ZEVO_BIN\" > {out:?}\nexit 3\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let args = ["zevo-test-hello".to_string(), "a".into(), "--b".into()];
        // Another test thread forking while the script was open for writing
        // can make it briefly "busy"; try again.
        let mut result = run_external(&Store::open(&tmp), &args);
        for _ in 0..5 {
            match &result {
                Err(e) if format!("{e:#}").contains("busy") => {
                    std::thread::sleep(Duration::from_millis(50));
                    result = run_external(&Store::open(&tmp), &args);
                }
                _ => break,
            }
        }
        assert_eq!(
            format!("{:?}", result.unwrap()),
            format!("{:?}", ExitCode::from(3))
        );
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            format!("a --b|{}|{}\n", tmp.display(), exe.display())
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A fresh folder; the CLI crate has no tempfile dependency.
    fn tempfile_dir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zevo-cli-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
