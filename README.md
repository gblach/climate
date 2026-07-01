# climate

> Your CLI's new mate: run containerized command-line tools like they're installed natively. Think
> Flatpak, but for the terminal.

Each app is described by a TOML file that says how to fetch its image and how to run it. When
an app bind-mounts a host directory (your working directory or your home), `climate` runs it as your
own uid/gid at the same path, so the tool reads and writes those files with your ownership.

## Install

Build the binary and put it on your `PATH`:

```sh
cargo build --release
cp target/release/climate ~/.local/bin/        # any directory on your PATH
```

## Sync the apps

Download the app definitions before first use:

```sh
climate sync          # into ~/.local/share/climate/apps/
climate list          # show available apps
```

Definitions are pulled from `https://github.com/gblach/climate-apps.git` by default. Set
`$CLIMATE_APPS_URL` to sync from a different repository (https or ssh).

## Run your first app

Pull an image and run it; arguments are forwarded to the tool:

```sh
climate pull ffmpeg
climate run ffmpeg -i clip.mov clip.mp4
```

You can run any app the same way:

```sh
climate run nmap -sn 192.168.1.0/24
climate run nmap --help          # shows nmap's own help
```

## Symlink shortcuts

Symlink the binary under an app's name to call it directly. When `climate` is invoked under any name
other than `climate`, that name is used as the app and all arguments are forwarded to it:

```sh
ln -s climate ffmpeg          # in a directory on your PATH
ffmpeg -i clip.mov clip.mp4   # same as: climate run ffmpeg -i clip.mov clip.mp4
```

`climate link` creates these symlinks for you, next to the `climate` binary, pointing back
at it. Name the apps explicitly or use `-a`/`--all`:

```sh
climate link ffmpeg nmap      # link specific apps
climate link --all            # link every available app
```

Linking is idempotent (an existing correct symlink is left alone) and refuses to clobber
an unrelated file unless you pass `-f`/`--force`.

## Commands

```sh
climate sync                    # download or update the app definitions
climate sync -s | --system      # sync into the system directory (needs root)
climate list                    # show available apps
climate pull <app>              # fetch the image
climate pull -u | --update      # refresh already-downloaded images
climate run <app> [args...]     # run the app, forwarding args
climate link <app>...           # create symlink shortcuts
climate link -a | --all         # link every available app
climate link -f | --force       # replace existing files or symlinks
climate clean                   # reclaim orphaned image data and stale runtime files
```

## Automatic updates

A systemd user timer can refresh your downloaded images daily (it runs `climate pull --update`,
which only touches apps you have already pulled). Install the units from `systemd/` and enable
the timer:

```sh
mkdir -p ~/.config/systemd/user
cp systemd/climate-update.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now climate-update.timer
systemctl --user list-timers climate-update.timer    # check the next run
```

The service expects the binary at `~/.local/bin/climate`; edit `ExecStart`
in `climate-update.service` if you installed it elsewhere. To let the timer run while you are logged
out, enable lingering with `loginctl enable-linger $USER`.

## App definitions

App definitions are loaded at runtime from these directories, highest precedence first:

| Location                       | Notes                                                   |
| ------------------------------ | ------------------------------------------------------- |
| `$CLIMATE_APPS_DIR`            | override directory, searched first when the var is set  |
| `~/.config/climate/apps/`      | user-authored (`$XDG_CONFIG_HOME/climate/apps/` if set) |
| `~/.local/share/climate/apps/` | synced apps (`$XDG_DATA_HOME/climate/apps/` if set)     |
| `/usr/share/climate/apps/`     | system-wide                                             |

`climate sync` only writes the synced apps (the data directory, or `/usr/share/climate/apps/`
with `--system`); your own definitions in `~/.config/climate/apps/` are never touched by it.

To customize an app, copy its `*.toml` into a higher-precedence directory and edit it there:

```sh
cp ~/.local/share/climate/apps/ffmpeg.toml ~/.config/climate/apps/
```

You can also drop entirely new `*.toml` files into any of these directories. A definition
in a higher-precedence directory overrides one of the same name below it.

## How it works

`climate` is a self-contained container engine: it pulls an app's image, mounts the layers, and runs
the container in-process. There is no `podman`, `crun`, or `skopeo` to install - `climate`
is a single static binary. At runtime it needs two external programs, `fuse-overlayfs`
and `fusermount3`, to mount image layers read-only, plus a systemd user session (a `dbus` session
bus under `$XDG_RUNTIME_DIR`) to manage the container.

Containers run rootless, as your own user and with no extra privileges:

- The image filesystem is read-only. Writable space is provided at `/tmp`, `/run`, and `/var/tmp`,
  plus any host directory an app mounts.
- Networking is configured per app: full host access, none, or localhost only.
- There are no resource limits, so a tool runs as it would natively.
