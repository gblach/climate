use anyhow::{Context, Result, bail};
use oci_spec::image::{Config as ImageExecConfig, ImageConfiguration};
use oci_spec::runtime::{
    HookBuilder, HooksBuilder, LinuxNamespaceBuilder, LinuxNamespaceType, Mount, MountBuilder,
    ProcessBuilder, RootBuilder, Spec,
};
use rustix::net::{AddressFamily, SocketType, socket};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::config::{AppConfig, Entrypoint, Network, RunConfig};
use crate::runtime::MountPoint;

// The fallback search path for images whose config carries no PATH.
const DEFAULT_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

// Writable scratch directories mounted as tmpfs over the read-only root.
const TMPFS_DIRS: [(&str, &str); 3] = [("/tmp", "1777"), ("/run", "0755"), ("/var/tmp", "1777")];

// Default rootless-spec mount targets that land directly on the read-only root.
// Minimal images may not contain them, so they are materialised in the stub.
// Nested targets (/dev/pts, /sys/fs/cgroup, ...) are created by youki on the
// writable /dev tmpfs or the /sys bind and need no stub entry.
const DEFAULT_MOUNT_DIRS: [&str; 3] = ["/proc", "/dev", "/sys"];

// Host files bind-mounted read-only when sharing the host network, so DNS and
// host name resolution work as they do outside the container.
const HOST_NET_FILES: [&str; 2] = ["/etc/resolv.conf", "/etc/hosts"];

// The argument that re-invokes this binary as a createContainer hook to bring
// the container's loopback interface up.
pub const LOOPBACK_HOOK_ARG: &str = "__lo-up";

// The argv to exec, following docker/podman semantics: a configured entrypoint
// overrides the image's and drops its default command; otherwise the image
// entrypoint is kept and its command is used only when no arguments are given.
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

// The image's environment, then the app's entries layered on top: "NAME=VALUE"
// is set literally, bare "NAME" is passed through from the host if present. A
// default PATH is added when the image config carries none.
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
    // Type "bind" (not "none") so youki creates a file target for a file source
    // and a directory target otherwise; "none" always makes a directory.
    MountBuilder::default()
        .destination(destination.to_path_buf())
        .typ("bind")
        .source(source.to_path_buf())
        .options(vec!["rbind".to_string(), access.to_string()])
        .build()
        .with_context(|| format!("building bind mount for {}", destination.display()))
}

// The host directory bind-mounted into the container at the same path, or None
// when the app opts out of sharing any host directory.
fn host_dir(run: &RunConfig) -> Result<Option<PathBuf>> {
    Ok(if run.cwd {
        Some(std::env::current_dir().context("resolving current directory")?)
    } else {
        None
    })
}

// Guard against sharing far more of the host than a cwd mount is meant to:
// from `/` the bind mount would cover the entire host filesystem read-write
// (shadowing the image root), so refuse outright; from the home directory
// itself everything in it (~/.ssh, keyrings, ...) becomes writable inside the
// container, so warn. Subdirectories of home are the intended use and pass
// silently; `run.cwd = false` opts out of the mount entirely.
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

// The host network files that exist and so should be bound in, or none unless
// the app shares the host network.
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

// The mount targets the image is not guaranteed to contain, materialised in the
// overlay stub so youki can mount over them without writing to the read-only
// root. Mirrors the extra mounts added in `build`.
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
    for file in host_net_files(run) {
        points.push(MountPoint::file(file));
    }
    Ok(points)
}

// Build the OCI runtime spec for one run: a read-only overlay root, the merged
// command/env/cwd, the rootless user/namespace setup, and the bind/tmpfs mounts
// that make the current user's working directory available inside.
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

    // The root is a fuse-overlayfs merge with no upperdir, so it is already
    // read-only (writes get EROFS). The OCI read-only flag is left off on
    // purpose: it would make youki remount the root with MS_RDONLY, which a
    // rootless user cannot do on a fuse mount (the kernel-locked nosuid/nodev
    // flags cannot be cleared, giving EPERM), and the remount is redundant here.
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
    // With a host directory shared, run in the host cwd; otherwise fall back to
    // the image's own workdir (or the root), since the host cwd is not mounted.
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
    for file in host_net_files(run) {
        mounts.push(bind(&file, &file, true)?);
    }
    spec.set_mounts(Some(mounts));

    let mut linux = spec
        .linux()
        .clone()
        .expect("rootless spec has a linux object");
    match run.network {
        // Rootless spec already omits the network namespace, sharing the host's.
        Network::Full => {}
        // An isolated network namespace: only its own loopback, down by default.
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

    // The namespace's loopback starts down, so bring it up from a hook that runs
    // inside the new network namespace (where we hold CAP_NET_ADMIN). The hook
    // re-invokes this binary; see `bring_loopback_up`.
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

// Bring the container's own loopback interface up so localhost connections work.
// Called from the createContainer hook, i.e. inside the new network namespace: a
// fresh namespace's `lo` exists but starts down, and there it can be set up
// without host privileges.
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
