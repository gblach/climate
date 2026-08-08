use anyhow::{Context, Result, bail};
use oci_spec::image::{Config as ImageExecConfig, ImageConfiguration};
use oci_spec::runtime::{
    HookBuilder, HooksBuilder, LinuxNamespaceBuilder, LinuxNamespaceType, Mount, MountBuilder,
    ProcessBuilder, RootBuilder, Spec,
};
use rustix::net::{AddressFamily, SocketType, socket};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::config::{AppConfig, Entrypoint, Network, RunConfig};
use crate::runtime::MountPoint;

// PATH used when the image itself does not define one.
const DEFAULT_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

// Directories the app must be able to write to, with their permission mode. Each gets an in-memory
// filesystem, as the root filesystem is read-only.
const TMPFS_DIRS: [(&str, &str); 3] = [("/tmp", "1777"), ("/run", "0755"), ("/var/tmp", "1777")];

// Kernel filesystems every container gets. Minimal images often lack these directories, and nothing
// can be mounted onto a directory that does not exist, so `mountpoints` creates them. Deeper paths
// like /dev/pts land inside these.
const DEFAULT_MOUNT_DIRS: [&str; 3] = ["/proc", "/dev", "/sys"];

// Host files shared read-only with containers that use the host network, so that DNS lookups behave
// the same inside and outside the container.
const HOST_NET_FILES: [&str; 2] = ["/etc/resolv.conf", "/etc/hosts"];

// Marker argument this binary passes to itself when the container runtime calls it to bring
// the container's loopback interface up.
pub const LOOPBACK_HOOK_ARG: &str = "__lo-up";

// Assemble the command line to run, following the same rules as docker and podman: an entrypoint
// set by the app definition replaces both the image's entrypoint and its default command; otherwise
// the image's entrypoint stays, and its default command is used only when the user passed
// no arguments.
fn command(run: &RunConfig, image: Option<&ImageExecConfig>, user_args: &[String]) -> Vec<String> {
    let extra: Vec<String> = run.args.iter().chain(user_args).cloned().collect();
    let mut argv = Vec::new();

    match &run.entrypoint {
        Some(Entrypoint::String(s)) => argv.push(s.clone()),
        Some(Entrypoint::List(list)) => argv.extend(list.iter().cloned()),
        None => {
            if let Some(entrypoint) = image.and_then(|c| c.entrypoint().as_ref()) {
                argv.extend(entrypoint.iter().cloned());
            }
        }
    }

    if run.entrypoint.is_none() && extra.is_empty() {
        if let Some(cmd) = image.and_then(|c| c.cmd().as_ref()) {
            argv.extend(cmd.iter().cloned());
        }
    } else {
        argv.extend(extra);
    }

    argv
}

// The environment for the container: the image's own variables first, then the app's on top.
// "NAME=VALUE" is used as written, a bare "NAME" copies the value from the host if it is set there.
// A PATH is added when neither supplies one.
fn environment(run: &RunConfig, image: Option<&ImageExecConfig>) -> Vec<String> {
    let mut env: Vec<String> = image
        .and_then(|c| c.env().as_ref())
        .map(|e| e.to_vec())
        .unwrap_or_default();

    for entry in &run.env {
        if entry.contains('=') {
            env.push(entry.clone());
        } else if let Ok(value) = std::env::var(entry) {
            env.push(format!("{entry}={value}"));
        }
    }

    if !env.iter().any(|e| e.starts_with("PATH=")) {
        env.push(DEFAULT_PATH.to_string());
    }
    env
}

fn tmpfs(destination: &str, mode: &str) -> Result<Mount> {
    MountBuilder::default()
        .destination(destination)
        .typ("tmpfs")
        .source("tmpfs")
        .options(vec![
            "nosuid".to_string(),
            "nodev".to_string(),
            format!("mode={mode}"),
        ])
        .build()
        .with_context(|| format!("building tmpfs mount for {destination}"))
}

fn bind(source: &Path, destination: &Path, readonly: bool) -> Result<Mount> {
    let access = if readonly { "ro" } else { "rw" };
    // The type must be "bind", not "none": with "bind" youki creates a file as the mount target
    // for a file source, while "none" always creates a directory.
    MountBuilder::default()
        .destination(destination.to_path_buf())
        .typ("bind")
        .source(source.to_path_buf())
        .options(vec!["rbind".to_string(), access.to_string()])
        .build()
        .with_context(|| format!("building bind mount for {}", destination.display()))
}

// The host directory shared with the container under the same path, or None when the app shares
// nothing.
fn host_dir(run: &RunConfig) -> Result<Option<PathBuf>> {
    Ok(if run.cwd {
        Some(std::env::current_dir().context("resolving current directory")?)
    } else {
        None
    })
}

// A path from an app definition, made absolute. A leading '~' becomes the user's home directory,
// so a definition can name a config directory without knowing where home is.
fn expand_path(path: &str) -> Result<PathBuf> {
    let expanded = if path == "~" || path.starts_with("~/") {
        let home = dirs::home_dir().context("resolving the home directory")?;
        home.join(path.trim_start_matches('~').trim_start_matches('/'))
    } else {
        PathBuf::from(path)
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        bail!("mount path '{path}' is not absolute");
    }
}

// One entry of `run.mount`, written the way docker and podman spell a share: up to three
// ':' separated fields, "<host path>:<path inside the container>:<ro|rw>". Leaving the middle field
// out keeps the path's own name inside the container:
//
//   "dir"             shared read-write at the same path
//   "dir:ro"          shared read-only at the same path
//   "dir1:dir2"       dir1 shared read-write, as dir2 inside
//   "dir1:dir2:ro"    dir1 shared read-only, as dir2 inside
//
// A path inside the container is always absolute, so a middle field of just "ro" or "rw" can only
// be the flag. Returned as (host path, path inside the container, read-only).
fn parse_mount(entry: &str) -> Result<(&str, &str, bool)> {
    let fields: Vec<&str> = entry.split(':').collect();
    let (source, destination, access) = match fields.as_slice() {
        [source] => (*source, *source, None),
        [source, access @ ("ro" | "rw")] => (*source, *source, Some(*access)),
        [source, destination] => (*source, *destination, None),
        [source, destination, access] => (*source, *destination, Some(*access)),
        _ => bail!("mount '{entry}' has more than three ':' separated fields"),
    };
    let readonly = match access {
        Some("ro") => true,
        None | Some("rw") => false,
        Some(access) => bail!("mount '{entry}': expected 'ro' or 'rw', not '{access}'"),
    };
    Ok((source, destination, readonly))
}

// Create a host path that is shared but does not exist yet, so that a tool can be given its config
// before it has written any. A bind mount needs a file to share for a file and a directory for a
// directory, and a path that is not there says nothing about which it should be, so the definition
// tells us: a trailing '/' asks for a directory, anything else for an empty file.
fn create_source(path: &Path, as_dir: bool) -> Result<()> {
    if as_dir {
        return std::fs::create_dir_all(path)
            .with_context(|| format!("creating {}", path.display()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    File::create(path).with_context(|| format!("creating {}", path.display()))?;
    Ok(())
}

// The extra host paths an app shares, as (host path, path inside the container, read-only) triples.
// Shares are read-write unless the definition says otherwise. A path that exists is shared as
// whatever it is, file or directory; a missing one is created, unless it is shared read-only, where
// an empty path is never what the definition meant.
fn extra_mounts(run: &RunConfig) -> Result<Vec<(PathBuf, PathBuf, bool)>> {
    let mut mounts = Vec::new();
    for entry in &run.mount {
        let (source, destination, readonly) = parse_mount(entry)?;
        let as_dir = source.ends_with('/');
        let source = expand_path(source)?;
        if source == Path::new("/") {
            bail!("refusing to mount / (the whole host filesystem) into the container");
        }
        if !source.exists() {
            if readonly {
                bail!(
                    "{} does not exist and is shared read-only",
                    source.display()
                );
            }
            create_source(&source, as_dir)?;
        }
        mounts.push((source, expand_path(destination)?, readonly));
    }
    Ok(mounts)
}

// Stop the working directory share from handing over far more than intended. Started from `/`,
// it would share the whole host filesystem read-write and hide the image's own files, so refuse.
// Started from the home directory, it would expose everything in it (~/.ssh, keyrings, ...)
// read-write, so warn. Subdirectories of home are the normal case and pass without a word.
pub fn check_host_dir(run: &RunConfig) -> Result<()> {
    let Some(dir) = host_dir(run)? else {
        return Ok(());
    };
    if dir == Path::new("/") {
        bail!(
            "refusing to bind-mount / (the whole host filesystem) into the container; \
             run from a working directory, or set run.cwd = false to share none"
        );
    }
    if dirs::home_dir().is_some_and(|home| dir == home) {
        eprintln!(
            "warning: running from your home directory bind-mounts all of it \
             read-write into the container"
        );
    }
    Ok(())
}

// Which of the host's name resolution files to share. Nothing is shared unless the app uses
// the host network, and a file that does not exist is skipped.
fn host_net_files(run: &RunConfig) -> Vec<PathBuf> {
    if run.network != Network::Full {
        return Vec::new();
    }
    HOST_NET_FILES
        .iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect()
}

// Every path something will be mounted onto. The image may not contain these directories,
// and the container's root filesystem is read-only, so they are created in a separate stub layer.
// Keep in step with the mounts in `build`.
pub fn mountpoints(cfg: &AppConfig) -> Result<Vec<MountPoint>> {
    let run = &cfg.run;
    let mut points: Vec<MountPoint> = DEFAULT_MOUNT_DIRS
        .iter()
        .chain(TMPFS_DIRS.iter().map(|(dir, _)| dir))
        .map(|dir| MountPoint::dir(*dir))
        .collect();
    if let Some(host) = host_dir(run)? {
        points.push(MountPoint::dir(host));
    }
    for (source, destination, _) in extra_mounts(run)? {
        points.push(if source.is_file() {
            MountPoint::file(destination)
        } else {
            MountPoint::dir(destination)
        });
    }
    for file in host_net_files(run) {
        points.push(MountPoint::file(file));
    }
    Ok(points)
}

// Build the description of one container run that the runtime consumes: the read-only root
// filesystem, the command, environment and start directory, the user and isolation settings,
// and the mounts.
pub fn build(
    cfg: &AppConfig,
    image: &ImageConfiguration,
    root: &Path,
    user_args: &[String],
    uid: u32,
    gid: u32,
    tty: bool,
) -> Result<Spec> {
    let run = &cfg.run;
    let image_config = image.config().as_ref();

    let mut spec = Spec::rootless(uid, gid);

    // The root filesystem is a stack of image layers with no writable layer on top, so it already
    // rejects every write. The spec's own read-only flag is deliberately left off: it would
    // add nothing, and would make youki remount the root, which an unprivileged user cannot
    // do on this kind of mount.
    spec.set_root(Some(
        RootBuilder::default()
            .path(root.to_path_buf())
            .readonly(false)
            .build()
            .context("building root")?,
    ));

    let argv = command(run, image_config, user_args);
    if argv.is_empty() {
        bail!(
            "{}: image has no entrypoint or command and none was configured",
            cfg.app.name,
        );
    }
    // Start in the shared working directory when there is one. Without it that path does not exist
    // inside, so use the image's own directory, or /.
    let cwd = match host_dir(run)? {
        Some(_) => std::env::current_dir().context("resolving current directory")?,
        None => image_config
            .and_then(|c| c.working_dir().clone())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/")),
    };
    spec.set_process(Some(
        ProcessBuilder::default()
            .terminal(tty)
            .cwd(cwd)
            .args(argv)
            .env(environment(run, image_config))
            .no_new_privileges(true)
            .build()
            .context("building process")?,
    ));

    let mut mounts = spec.mounts().clone().unwrap_or_default();
    for (dir, mode) in TMPFS_DIRS {
        mounts.push(tmpfs(dir, mode)?);
    }
    if let Some(host) = host_dir(run)? {
        mounts.push(bind(&host, &host, false)?);
    }
    for (source, destination, readonly) in extra_mounts(run)? {
        mounts.push(bind(&source, &destination, readonly)?);
    }
    for file in host_net_files(run) {
        mounts.push(bind(&file, &file, true)?);
    }
    spec.set_mounts(Some(mounts));

    let mut linux = spec
        .linux()
        .clone()
        .expect("rootless spec has a linux object");
    match run.network {
        // With no network namespace listed the container shares the host's.
        Network::Full => {}
        // A private network namespace, holding only a disabled loopback.
        Network::None | Network::Localhost => {
            let mut namespaces = linux.namespaces().clone().unwrap_or_default();
            namespaces.push(
                LinuxNamespaceBuilder::default()
                    .typ(LinuxNamespaceType::Network)
                    .build()
                    .context("building network namespace")?,
            );
            linux.set_namespaces(Some(namespaces));
        }
    }
    spec.set_linux(Some(linux));

    // The loopback interface has to be enabled from inside the new network namespace, which
    // is where the runtime executes hooks. The hook runs this same binary again;
    // see `bring_loopback_up`.
    if run.network == Network::Localhost {
        let exe = std::env::current_exe().context("resolving the climate executable")?;
        let hook = HookBuilder::default()
            .path(exe)
            .args(vec!["climate".to_string(), LOOPBACK_HOOK_ARG.to_string()])
            .build()
            .context("building loopback hook")?;
        spec.set_hooks(Some(
            HooksBuilder::default()
                .create_container(vec![hook])
                .build()
                .context("building hooks")?,
        ));
    }

    Ok(spec)
}

// Enable the container's loopback interface so connections to 127.0.0.1 work. This runs
// as a container hook, inside the container's own network namespace, where the interface exists
// but is still disabled and where switching it on needs no privileges. The ioctl calls read
// its flags and set the "up" bit.
pub fn bring_loopback_up() -> Result<()> {
    let sock = socket(AddressFamily::INET, SocketType::DGRAM, None)
        .context("opening a socket to configure loopback")?;

    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (slot, byte) in req.ifr_name.iter_mut().zip(b"lo") {
        *slot = *byte as libc::c_char;
    }

    let fd = sock.as_raw_fd();
    unsafe {
        if libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut req) < 0 {
            return Err(std::io::Error::last_os_error()).context("reading loopback flags");
        }
        req.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        if libc::ioctl(fd, libc::SIOCSIFFLAGS, &mut req) < 0 {
            return Err(std::io::Error::last_os_error()).context("bringing loopback up");
        }
    }
    Ok(())
}
