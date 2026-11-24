> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Yonder

Yonder is a native desktop editor for working on code that lives on other machines. Your laptop runs only the editor UI. Projects, language servers, terminals and builds stay on the remote machines and run there directly over SSH. They are not in containers.

Yonder is a fork of [Zed](https://github.com/zed-industries/zed). It keeps Zed's editor and remote development, and adds a sidebar of your machines and their projects so you can switch between them quickly.

## What it adds to Zed

- **Fleet panel.** The left sidebar groups open and recent projects under This Mac and each SSH machine. New projects appear when you open them, and the current project is highlighted. Clicking a project opens it in the same window.
- **Fast switching.** Each window keeps up to four projects open, so switching back to one of them is instant. When you open a fifth, the least recently shown project is released and reopens from its server next time. Projects with unsaved changes are never released.
- **Pre-warmed connections.** Resting the pointer on a project in the fleet panel for 250 ms opens its machine's SSH connection ahead of the click. Pre-warming never shows UI, never downloads a server, and skips machines that ask for a password.
- **Fleet switcher.** `ctrl-cmd-k` on macOS searches the same open and recent projects shown in Fleet. Highlighting a project pre-warms its connection. The fleet panel can also be toggled with the `fleet_panel::ToggleFocus` action.
- **Switch timing.** Each switch is logged with its duration and whether the connection was already warm. Search the Yonder log for `fleet:`.

## How it differs from Zed

- **Its own identity.** The app, binary (`yonder`), CLI, menus, data and config folders, and macOS bundle IDs (`io.github.aabdlwahab.Yonder*`) are all named Yonder. It can be installed beside Zed without sharing settings. It registers a `yonder://` URL scheme instead of `zed://`.
- **No Zed services.** Telemetry and crash-report uploads are forced off whatever your settings say. Update checks are disabled, so Zed's update server can never replace Yonder. Zed sign-in is disabled and the title-bar Sign In button is hidden.
- **Still enabled.** The extension store and the agent registry work as in Zed.

To keep merges from upstream small, internal crate names, project-level `.zed/` folders and the remote server's directory on the machine still use Zed's names.

## Adding machines and projects

Fleet reads machine names from `ssh_connections`. You can also save folders for the remote project picker in your settings file (**Yonder → Settings → Open Settings File**):

```json
{
  "ssh_connections": [
    {
      "host": "build-box.example.com",
      "username": "me",
      "nickname": "Build box",
      "projects": [
        { "paths": ["~/src/api"] },
        { "paths": ["~/src/web"] }
      ]
    }
  ]
}
```

Open a folder over SSH to add it to Fleet. See [Remote Development](./docs/src/remote-development.md) for the full connection options, such as ports, SSH arguments and port forwards.

## Building and installing

Yonder runs on macOS and on Ubuntu (22.04 and newer, Wayland or X11). It builds the same way as Zed, so set up Rust and the system packages with the upstream guides first:

- [Building on macOS](./docs/src/development/macos.md)
- [Building on Linux](./docs/src/development/linux.md) (`script/linux` installs the packages)

Then:

- **Mac:** `script/install-mac` builds a release, signs it for this Mac and installs `/Applications/Yonder.app`. Add `-l <ssh host>` to also build the program for Linux machines on one of them and carry it inside the app.
- **Ubuntu:** `script/install-linux` builds a release and installs it in `~/.local/yonder.app`, with `~/.local/bin/yonder` and a menu entry. It needs no sudo.
- **Ubuntu without build tools:** `script/build-linux-docker <host>` builds `yonder-linux-x86_64.tar.gz` in an Ubuntu 22.04 container on any machine with Docker, and `script/install-linux yonder-linux-x86_64.tar.gz` installs it.

`cargo run` starts a development build of the `yonder` binary. There are no Yonder release downloads yet.

## Status and plans

Done so far:

1. The rename, and turning off Zed's telemetry, updates and sign-in.
2. The fleet panel.
3. Fast switching: the four-project cap, pre-warming, the fleet switcher and switch timing.

Next are fleet-wide agent jobs: running agent threads on any machine under different accounts, handing a thread to another agent, and forking threads to run in parallel in their own git worktrees. A phone app that controls Yonder is planned for later.

## Upstream

The `upstream` remote points at [zed-industries/zed](https://github.com/zed-industries/zed) and is fetch-only. Yonder-specific code sits in its own crates where possible (for example `crates/fleet_panel`), and changes to Zed's crates are kept small so upstream can be merged regularly.

Yonder is not affiliated with or endorsed by Zed Industries. Please report Yonder problems here, not to the Zed project.

## Licensing

Like Zed, Yonder's source code is licensed primarily under GPL-3.0-or-later ([LICENSE-GPL](./LICENSE-GPL)), with Apache-2.0 components where marked ([LICENSE-APACHE](./LICENSE-APACHE)). Zed's source code is copyright Zed Industries, Inc. and its contributors.

Third-party license notices are generated with [`cargo-about`](https://github.com/EmbarkStudios/cargo-about). If a license check fails, see `script/licenses/zed-licenses.toml`. New crates that are not published should set `publish = false` under `[package]` in their `Cargo.toml`.
