//! `zevo` — command-line front end for Captain Zevo.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use zevo_core::model::{Route, Target};
use zevo_core::reconcile::{self, Action, ContainerState, Report};
use zevo_core::runtime::{DockerRuntime, Runtime};
use zevo_core::store::Store;
use zevo_core::{doctor, register, setup, traefik};

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
}

#[derive(Args)]
struct AddArgs {
    /// Project folder.
    path: PathBuf,
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
    }
    Ok(ExitCode::SUCCESS)
}

async fn reconcile_once(store: &Store) -> Result<Report> {
    let (runtime, reason) = connect().await;
    let mut report =
        reconcile::reconcile(store, runtime.as_ref().map(|r| r as &dyn Runtime)).await?;
    if let Some(reason) = reason {
        report
            .warnings
            .insert(0, format!("container runtime unreachable: {reason}"));
    }
    Ok(report)
}

async fn cmd_setup(store: &Store, args: SetupArgs) -> Result<()> {
    if let (Some(dir), Some(entrypoint)) = (&args.dynamic_dir, &args.entrypoint) {
        setup::adopt_manual(store, dir, entrypoint, args.port.unwrap_or(80))?;
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
            setup::bootstrap(store, &runtime, args.port).await?
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
    let mut project = register::draft(&args.path)?;
    if let Some(slug) = args.slug {
        project.slug = slug;
    }
    if let Some(name) = args.name {
        project.name = name;
    }
    project.tags = args.tags;
    if !args.routes.is_empty() {
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
            project
                .routes
                .push(parse_route(&route, address.as_deref(), path_prefix)?);
            project
        }
        RouteCommand::Rm { slug, subdomain } => {
            let mut project = store.project(&slug)?;
            let before = project.routes.len();
            project.routes.retain(|r| r.subdomain != subdomain);
            if project.routes.len() == before {
                bail!("project '{slug}' has no such route");
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
