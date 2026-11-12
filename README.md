<img src="assets/logo.svg" alt="" width="88" align="right">

# Captain Zevo

Captain Zevo gives every devcontainer project a stable local address such as
`http://shop.localhost`, by managing [Traefik](https://traefik.io) routes for
you. Register a project folder once; Zevo writes its Traefik config, attaches
its container to a shared network, and keeps both in place across rebuilds. No
published ports, no port conflicts.

> **Status: early development.** The core library and the `zevo` command-line
> tool work for projects on the local machine. The desktop app (built with
> [GPUI](https://gpui.rs)), remote hosts, and editor launching are planned; see
> the roadmap below.

## How it works

- Traefik's file provider watches a directory. Zevo writes one file per
  project there, named `zevo-<slug>.yml`, and never touches files it did not
  create.
- Project containers share a Docker network with Traefik, which reaches them
  by container name. Zevo reuses the network your Traefik already has, or
  creates one called `zevo`. Containers already on it are left untouched.
- Addresses are HTTPS wherever possible. Zevo creates a certificate per project
  with [mkcert](https://github.com/FiloSottile/mkcert) (install it and run
  `mkcert -install` once), keeps it in a `zevo-certs` folder inside the route
  directory, and lists it in the project's route file. Without mkcert, routes
  are still written and Zevo tells you what to install.
- If your Traefik redirects HTTP to HTTPS, Zevo detects that and writes HTTPS
  routes. A Traefik that Zevo starts itself listens on 443 and redirects 80. A route can also talk HTTPS to the app behind it
  (`backend = "https"`), for dev servers with self-signed certificates.
- Zevo finds a project's container by the labels the devcontainer tooling sets,
  so nothing in your repository changes. `zevo reconcile --watch` re-attaches
  containers after a rebuild.
- Projects that cannot join the network (another machine, a Traefik that runs
  outside Docker) use a fixed address instead: `--address <host>`.

## Requirements

- A Docker-compatible runtime: Docker Engine, Docker Desktop, Podman, OrbStack,
  Colima or Rancher Desktop.
- [mkcert](https://github.com/FiloSottile/mkcert) for HTTPS addresses
  (optional, recommended).
- Rust 1.90 or newer to build from source.

A running Traefik is not required. `zevo setup` adopts one if it finds it, and
otherwise starts its own.

## Install

```sh
cargo install --path crates/zevo-cli
```

## Quick start

```sh
zevo setup                 # adopt your Traefik, or start a managed one
zevo add ~/code/shop       # detects the devcontainer and its ports
zevo ls                    # status and URLs
zevo reconcile --watch     # keep routes and network attachments in place
```

More commands:

| Command | Purpose |
|---|---|
| `zevo add <folder> -r 3000 -r api:8080` | Register with explicit routes (`api.shop.localhost` → 8080) |
| `zevo add <folder> --address 10.0.0.5 -r 3000` | Route to a fixed host and port |
| `zevo route add <slug> [sub:]port` / `zevo route rm <slug> [sub]` | Edit routes |
| `zevo show <slug>` | Show a project and its generated Traefik config |
| `zevo rm <slug>` | Unregister and remove its routes |
| `zevo doctor` | Check the runtime, Traefik, the route directory and DNS |

Add `--json` to `ls`, `show`, `reconcile` and `doctor` for scripting.

### Declaring routes in the repository

A project can describe its own routes in `devcontainer.json`, so anyone who
adds it gets the same addresses with nothing to type:

```jsonc
"customizations": {
  "zevo": {
    "name": "Shop",
    "slug": "shop",
    "routes": [
      { "port": 3000 },                                  // shop.localhost
      { "subdomain": "api", "port": 8080, "pathPrefix": "/v1" },
      { "subdomains": ["admin", "docs"], "port": 4000 }, // several names, one port
      { "subdomain": "vite", "port": 5173, "backend": "https" },
      { "host": "local.example.com", "port": 9000 }      // an exact hostname
    ]
  }
}
```

- `backend: "https"` is for dev servers with self-signed certificates: Traefik
  connects over TLS without verifying, and sends the route's hostname as SNI.
- `slug` is a suggestion. Give a second checkout of the same repository its
  own short name (`zevo add <folder> --slug shop-lane2`) and it gets its own
  addresses under `shop-lane2.localhost`.
- Zevo re-reads the block on every reconcile, so editing the file updates the
  routes. Declared routes cannot be edited in Zevo; remove the block to manage
  them in Zevo instead.
- Where the app runs is a fact about your machine, not the repository:
  `zevo add <folder> --address host.docker.internal` points every declared
  route at that address instead of the container.

### Adopting an existing Traefik

`zevo setup` inspects running Traefik containers. To be adopted, a Traefik
needs the file provider pointed at a directory mounted from the host, for
example:

```
--providers.file.directory=/etc/traefik/dynamic
--providers.file.watch=true
```

If Zevo cannot adopt yours it says why. For a Traefik that runs outside a
container, point Zevo at its directory:

```sh
zevo setup --dynamic-dir /etc/traefik/dynamic --entrypoint web
```

## Configuration

Everything Zevo knows lives in plain TOML files that you can read, edit, copy
to another machine, or keep under version control:

| OS | Location |
|---|---|
| Linux | `~/.config/captain-zevo/` |
| macOS | `~/Library/Application Support/dev.captain-zevo.captain-zevo/` |
| Windows | `%APPDATA%\captain-zevo\captain-zevo\config\` |

Set `ZEVO_CONFIG_DIR` to use a different directory.

- `settings.toml` — domain suffix, network name, Traefik details.
- `projects/<slug>.toml` — one file per project:

```toml
slug = "shop"
name = "Shop"
host = "local"
path = "/home/me/code/shop"
kind = "compose"
service = "app"

[[routes]]
port = 3000

[[routes]]
subdomain = "api"
port = 8080
path_prefix = "/v1"
```

Hand edits take effect on the next `zevo reconcile`.

## Roadmap

1. **Core and CLI, local projects** — done.
2. **Desktop app** — done for local projects: first-run setup, adding projects,
   editing routes, and the project, Traefik and settings screens.
3. **Lifecycle and editors** — start/stop/rebuild, logs, discovery of running
   devcontainers, new projects from templates, open in VS Code (and forks),
   Zed and JetBrains.
4. **Remote hosts** — projects on other machines over SSH, with an edge Traefik
   per host.
5. **Polish** — installers, system tray, local HTTPS.

## Desktop app (preview)

```sh
cargo run -p zevo-app            # your real projects
cargo run -p zevo-app -- --demo  # example data; touches nothing
```

The app and the CLI share one registry, so either can be used at any time. The
app sets up Traefik on first run, adds projects from a folder, edits their
routes, and refreshes every few seconds. `--demo --fresh` shows the first-run
screen. On Linux it needs the libraries listed in
[.devcontainer/devcontainer.json](.devcontainer/devcontainer.json).

### Installing the app on macOS

```sh
scripts/bundle-macos.sh --install
```

This builds `Captain Zevo.app` with its icon and copies it to `/Applications`.
It is signed for the Mac that built it only.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

The repository includes a devcontainer. Core logic is tested against an
in-memory runtime, so the test suite needs no Docker daemon. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## Logo and icon

The logo is the single file [assets/logo.svg](assets/logo.svg). Replace it,
then run `scripts/make-icons.sh` to rebuild the app icon files in
`assets/icons` from it. The README and the desktop app read the logo directly.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in this work
by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
