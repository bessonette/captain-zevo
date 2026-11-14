# Example plugins

Each folder here is a complete plugin, laid out exactly as it would sit in
your plugins folder: a `plugin.toml`, plus any scripts it runs. Copy one,
change it to suit you, and Zevo picks it up the next time it loads plugins.
The format itself is described in the "Plugins" section of the main
[README](../../README.md).

To install one, find your plugins folder:

```sh
zevo plugins
```

It prints the folder's path (and creates it if it is missing). Copy the
example's whole folder into it, so you end up with `plugins/notify/plugin.toml`
and `plugins/notify/notify.sh`, for example. Run `zevo plugins` again: the
plugin should be listed, and if Zevo could not read it, it says why. Scripts
must stay executable (`chmod +x`).

Programs named by a plugin run with your permissions. Read a script before
you install it.

- `team-starter/`: a Rails starter template and two extras (AWS CLI, Terraform), as a team might share them. Data only.
- `jetbrains/`: IntelliJ IDEA, PyCharm and GoLand for "Open in editor". They open the project folder; they do not attach to the devcontainer.
- `notify/`: a hook that shows a desktop notification when a project's container is attached, with `osascript` on macOS or `notify-send` on Linux.
- `vpn-check/`: a `zevo doctor` check that warns when a host name only reachable on the VPN does not resolve. Change the name at the top of the script.
