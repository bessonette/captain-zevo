# Contributing to Captain Zevo

Thanks for your interest. Issues and pull requests are welcome.

## Layout

| Crate | Contents |
|---|---|
| `crates/zevo-core` | All logic: registry (`store`), Traefik files (`traefik`), devcontainer detection (`devcontainer`), runtime access (`runtime`), setup, reconciliation, doctor. No UI code. |
| `crates/zevo-cli` | The `zevo` binary; a thin layer over `zevo-core`. |

New behaviour belongs in `zevo-core` so the CLI and the desktop app share it.

## Ground rules

- Code that talks to the container runtime goes through the `Runtime` trait.
  Test it with `runtime::fake::FakeRuntime`; tests must not need a daemon.
- Zevo only ever writes or deletes Traefik files that are named `zevo-*.yml`
  and start with the managed header. Do not weaken that check.
- Before opening a pull request, run:

  ```sh
  cargo fmt --all
  cargo clippy --workspace --all-targets
  cargo test --workspace
  ```

- Keep pull requests focused, and describe how you tested the change.

## Trying the CLI without touching your setup

```sh
export ZEVO_CONFIG_DIR=$(mktemp -d)
mkdir "$ZEVO_CONFIG_DIR/routes"
cargo run -p zevo-cli -- setup --dynamic-dir "$ZEVO_CONFIG_DIR/routes" --entrypoint web
cargo run -p zevo-cli -- add /path/to/project --address 127.0.0.1 -r 3000
```

## License

By contributing you agree that your contributions are dual licensed under the
MIT and Apache-2.0 licenses, as described in the README.
