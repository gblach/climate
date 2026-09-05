# CLImate

> Your CLI's new mate: run containerized command-line tools like they're installed natively. Think
> Flatpak, but for the terminal.

Each app is described by a TOML file that says how to fetch its image and how to run it. When
an app shares a directory from your computer (your working directory or your home), CLImate runs
the tool as you, at the same path, so the files it reads and writes stay yours.

## Requirements

CLImate mounts image layers with `fuse-overlayfs` and unmounts them with `fusermount3`, so both
have to be installed. `fusermount3` comes with `fuse3`, which every `fuse-overlayfs` package depends
on:

```sh
sudo dnf install fuse-overlayfs      # Fedora
sudo apt install fuse-overlayfs      # Debian, Ubuntu
```

Containers are managed through your systemd user session, so one has to be running (it provides
the `dbus` session bus under `$XDG_RUNTIME_DIR`). A normal desktop or `ssh` login has one.

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

Definitions are pulled from `https://github.com/gblach/climate-apps.git` by default.
Set `$CLIMATE_APPS_URL` to sync from a different repository (https or ssh).

## Run your first app

Pull an image and run it; arguments are forwarded to the tool:

```sh
climate pull ffmpeg
climate run ffmpeg -i clip.mov clip.mp4
```

`climate run` downloads a missing image by itself, so `climate pull` is only needed when you want
the image fetched ahead of time.

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

Linking again is harmless: a symlink that already points at the binary is left alone. Anything else
in the way is never replaced unless you pass `-f`/`--force`.

## Commands

```sh
climate sync                    # download or update the app definitions
climate sync -s | --system      # sync into the system directory (needs root)
climate list                    # show available apps
climate show <app>              # print an app definition, defaults included
climate show -n | --no-defaults # print only the keys the definition states
climate pull <app>              # fetch the image
climate pull -u | --update      # refresh already-downloaded images
climate run <app> [args...]     # run the app, forwarding args
climate link <app>...           # create symlink shortcuts
climate link -a | --all         # link every available app
climate link -f | --force       # replace existing files or symlinks
climate clean                   # free the space of unused images, clean up after killed runs
```

## Automatic updates

A systemd user timer can refresh your downloaded images daily (it runs `climate sync` to update app
definitions, followed by `climate pull --update` to refresh images for apps you have already
pulled). Install the units from `systemd/` and enable the timer:

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

`climate sync` only writes the synced apps (the data directory, or `/usr/share/climate/apps/` with
`--system`); your own definitions in `~/.config/climate/apps/` are never touched by it.

Set `$CLIMATE_APPS_DIR` to a directory of your own - a checkout you are working
on, for example - and it is searched before all the others.

To customize an app, copy its `*.toml` into a higher-precedence directory and edit it there:

```sh
cp ~/.local/share/climate/apps/ffmpeg.toml ~/.config/climate/apps/
```

`climate show <app>` prints the definition that is actually in effect, with every key the file
leaves out filled in from its default and grayed out. Redirecting the output drops the colors,
so it doubles as a starting point for your own copy:

```sh
climate show ffmpeg > ~/.config/climate/apps/ffmpeg.toml
```

Pass `-n`/`--no-defaults` to leave the defaults out and print only what the definition itself
states.

You can also drop entirely new `*.toml` files into any of these directories. A definition
in a higher-precedence directory overrides one of the same name below it.

## Networking

An app picks one of three modes in `[run]`:

```toml
[run]
network = "localhost"
```

| Mode        | What the app can reach                          |
| ----------- | ----------------------------------------------- |
| `none`      | nothing; the default                            |
| `full`      | the host's own network, so the internet as well |
| `localhost` | services on the host's loopback, nothing else   |

`localhost` still gives the container a private network of its own - no LAN, no internet. What it
adds is a bridge between the two loopbacks, both ways: a port the host listens on is reachable at
the same `127.0.0.1:<port>` inside the container, and a port the container listens on turns up at
that address on the host. Nothing is listed per app; both sides are rescanned once a second, so a
service that starts or stops is picked up a moment later.

Three limits are worth knowing:

- A port the host already listens on is taken inside the container too, so an app cannot bind it
  for itself. Give the app another port, or run it with `network = "none"`.
- The reverse mirror binds on the host as your own user, so a container service below port 1024
  is not reachable from the host unless `net.ipv4.ip_unprivileged_port_start` allows it.
- Only TCP is bridged, not UDP. Both `127.0.0.1` and `::1` are, and either address reaches a
  service listening on the other, so an app need not know which one it is on.

## Capabilities

Containers hold no capabilities. An app that genuinely needs one lists it in `[run]`:

```toml
[run]
capabilities = ["NET_BIND_SERVICE"]   # bind ports below 1024
```

Names drop the kernel's `CAP_` prefix and ignore case. A capability reaches only as far as the
container's own user namespace, so it grants nothing over the host, and most do nothing at all: the
[app definition format](https://github.com/gblach/climate-apps/blob/main/README.md) lists all 41
names and which of them have any effect.

## Resource limits

An app can cap how much of the machine it takes in a `[limits]` section:

```toml
[limits]
memory = "2G"        # memory ceiling
swap = "0"           # swap on top of it; "0" makes the ceiling hard
memory-high = "1G"   # slow the app down here instead of killing it at the ceiling
cpu = 1.5            # CPU cores' worth of time per second
cpu-shares = 512     # share of a contended CPU, against 1024 for an ordinary process
pids = 512           # processes and threads at once
```

| Key           | Type             | Unit                | Example  |
| ------------- | ---------------- | ------------------- | -------- |
| `memory`      | string or number | bytes               | `"512M"` |
| `swap`        | string or number | bytes               | `"0"`    |
| `memory-high` | string or number | bytes               | `"256M"` |
| `cpu`         | number           | CPU cores           | `1.5`    |
| `cpu-shares`  | number           | relative share      | `512`    |
| `pids`        | number           | processes + threads | `512`    |

Every key is optional, and a key left out is not limited at all, so definitions without a
`[limits]` section keep running exactly as before.

Sizes are a byte count with an optional binary unit - `K`, `M`, `G` or `T`, where `M` means
1024 * 1024. `MB` and `MiB` spell the same unit; no unit at all means plain bytes. A size with no
unit can be written as a plain number instead of a string, so `memory = 536870912` and
`memory = "512M"` mean the same thing.

Two things about memory are worth knowing. `memory` on its own is not the ceiling it looks like:
an app that grows past it is pushed into swap rather than killed, and `swap = "0"` is what closes
that. And `memory-high` needs somewhere to reclaim pages to, so pairing the two leaves it little
to work with when the app's memory is mostly files it wrote under `/tmp` - such an app is killed
at `memory` anyway.

`cpu` caps total CPU time, not how many cores the app spreads over. `cpu-shares` only matters
while something else wants the CPU, and youki rescales the number, so `512` does not mean half.

The limits are applied to the container's systemd scope in your user session, which is what makes
them work without root. Only `cpu`, `memory` and `pids` are delegated to a user session, so there
are no keys for CPU pinning or disk I/O.

## How it works

CLImate is a self-contained container engine: it pulls an app's image, mounts the layers, and runs
the container in-process. There is no `podman`, `crun`, or `skopeo` to install - CLImate
is a single binary, next to the `fuse-overlayfs` and `fusermount3` helpers listed under
Requirements.

Containers run rootless, as your own user and with no extra privileges:

- The image filesystem is read-only. Writable space is provided at `/tmp`, `/run`, and `/var/tmp`,
  plus any host directory an app mounts.
- Networking is configured per app: full host access, none, or a loopback bridged to the host's.
- Resource limits are configured per app and enforced by the kernel through cgroup v2. An app that
  sets none runs with the whole machine available, as it would natively.
- Containers hold no capabilities, not even the three an OCI runtime grants by default. An app
  that needs one asks for it by name in its definition.
