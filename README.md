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
- Zevo checks, inside each container, whether a route's port is listening where
  Traefik can reach it, and says so when a dev server is bound to 127.0.0.1
  only.
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
| `zevo new "My app" --template node --with claude-code` | Create a project folder from a starter and register it |
| `zevo rm <slug>` | Unregister and remove its routes |
| `zevo up <slug> [--rebuild]` / `zevo down <slug>` | Start (building if needed), rebuild, or stop a project's container |
| `zevo logs <slug> [-n 200]` | Print the end of the container's output |
| `zevo discover` | List devcontainers on this machine that are not registered yet |
| `zevo open <slug> [--editor zed]` | Open a project in VS Code, Cursor, Windsurf, VSCodium or Zed, attached to its devcontainer |
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
  Routes that only belong to one checkout, such as an exact hostname, can be
  left out of the others: `zevo add <folder> --slug shop-lane2 --skip wp
  --skip local.example.com`.
- Zevo re-reads the block on every reconcile, so editing the file updates the
  routes. Declared routes cannot be edited in Zevo; remove the block to manage
  them in Zevo instead.
- `"envFile": ".devcontainer/zevo.env"` asks Zevo to write the project's
  addresses there as environment variables (`ZEVO_DOMAIN`, `ZEVO_URL`,
  `ZEVO_HOST_API`, `ZEVO_URL_API`, ...), so the app can read its own hostnames
  for allowed-host lists, CORS and absolute URLs. They differ per checkout,
  so add the file to `.gitignore`. Zevo writes nothing into a project that
  does not ask, and never overwrites a file it did not write.
- Where the app runs is a fact about your machine, not the repository:
  `zevo add <folder> --address host.docker.internal` points every declared
  route at that address instead of the container.

### Projects on other machines

Zevo can manage devcontainers that run on another machine you reach over SSH:

```sh
zevo host add dev@192.168.1.20 --name buildbox   # checks SSH and Docker first
zevo discover                                    # lists devcontainers there too
zevo add --host buildbox /srv/dev/billing
```

- SSH must work without typing (keys or an agent); Zevo never prompts.
- Zevo finds the Docker socket on the remote wherever its runtime puts it
  (Docker Engine, Docker Desktop, OrbStack, Colima, Rancher Desktop, rootless
  Docker, Podman). Pass `--docker-socket <path>` if yours is somewhere else.
- A host's address can be a name, such as a Tailscale name or `box.local`.
  Zevo looks up its current IP on every reconcile, so routes follow the
  machine when its IP changes.
- The remote needs only Docker. Zevo talks to it by forwarding its Docker
  socket over SSH, and starts one container there, `zevo-edge`: a small
  Traefik listening on port 8480 that routes to that machine's project
  containers over a shared network. No project publishes ports.
- Your local Traefik stays the front door. It serves HTTPS with the local
  certificates and forwards each remote project's hostnames to the host's
  edge, so addresses keep working with no editor window open.
- The edge port speaks plain HTTP to anyone who can reach that machine on
  your network. Use this on networks you trust.
- Remote hosts are not supported when Zevo itself runs on Windows yet.

### Starting a new project

`zevo new` (or "Start a new project" in the app) creates a folder with a
`devcontainer.json` for a stack: `node`, `python`, `rust`, `go`, `php` or
`ubuntu`. The file already declares the project's route, so it gets an address
as soon as it is built (`zevo up <slug>`).

Extras, added with `--with` or ticked in the app:

| Extra | What it adds |
|---|---|
| `claude-code` | Claude Code, with its login and history kept across rebuilds |
| `codex` | OpenAI's Codex CLI, with its login and history kept across rebuilds |
| `docker` | This machine's Docker available inside the container |
| `github-cli` | `gh` |
| `shell-history` (on by default) | Command history kept across rebuilds |
| `address-file` (on by default) | The env file with the project's own hostnames |

State that must survive a rebuild lives in `.devcontainer/home/` inside the
project, linked into the home folder by a small script on every start. It
holds credentials, so the new project's `.gitignore` excludes it.

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
3. **Lifecycle and editors** — done: discovery, open in VS Code (and forks) or
   Zed, start/stop/rebuild and logs. Building needs the `devcontainer`
   command-line tool (`npm install -g @devcontainers/cli`). Still to come: new
   projects from templates, JetBrains.
4. **Remote hosts** — built: hosts over SSH, discovery, and an edge Traefik per
   host. Not yet: re-reading a remote project's declaration after it is added,
   and its env file.
5. **Polish** — done: local HTTPS, the app's own theme, installer settings and
   a release workflow (unsigned). Still to come: code signing, system tray.

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
