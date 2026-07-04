use anyhow::{Context, Result};
use libcontainer::container::Container;
use oci_client::Reference;
use oci_client::manifest::OciImageManifest;
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::config::{AppConfig, app_names};
use crate::store;

// Directory entries, treating a missing directory as empty so callers can walk
// store and runtime subdirectories that may not exist yet.
fn entries(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    match fs::read_dir(dir) {
        Ok(reader) => reader
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("reading {}", dir.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("reading {}", dir.display())),
    }
}

// The climate process pid encoded in a runtime artifact name. Container and bundle
// ids are "climate-<pid>-<nanos>"; overlay dirs are "<pid>-<nanos>" (empty prefix).
fn pid_of(name: &str, prefix: &str) -> Option<i32> {
    name.strip_prefix(prefix)?.split('-').next()?.parse().ok()
}

// Whether a process with this pid still exists. Signal 0 probes without
// delivering anything; only ESRCH means the process is gone.
fn alive(pid: i32) -> bool {
    let Some(pid) = Pid::from_raw(pid) else {
        return false;
    };
    !matches!(test_kill_process(pid), Err(Errno::SRCH))
}

// Reclaim store data no longer reachable from any recorded ref. Layers are
// shared across images by digest, so a digest is only removed once no live ref
// uses it; the per-pull caller and the `clean` command share this single pass.
// Interrupted-pull temp files and extraction staging directories are swept too.
pub fn gc_images() -> Result<()> {
    let store = store::dir()?;
    if !store.exists() {
        return Ok(());
    }

    // The live set: every ref's manifest blob, the config it points at, and all
    // its layers. A ref whose manifest is missing leaves the store inconsistent;
    // bail rather than risk deleting layers we cannot prove are unused.
    let mut live_blobs = HashSet::new();
    let mut live_layers = HashSet::new();
    for entry in entries(&store.join("refs"))? {
        let manifest_digest = fs::read_to_string(entry.path())
            .with_context(|| format!("reading ref {}", entry.path().display()))?;
        let manifest_digest = manifest_digest.trim();
        if manifest_digest.is_empty() {
            continue;
        }
        live_blobs.insert(manifest_digest.to_string());
        let manifest: OciImageManifest =
            serde_json::from_slice(&store::read_blob(manifest_digest)?)
                .with_context(|| format!("parsing manifest {manifest_digest}"))?;
        live_blobs.insert(manifest.config.digest.clone());
        for layer in &manifest.layers {
            live_layers.insert(layer.digest.clone());
        }
    }

    for algo in entries(&store.join("blobs"))? {
        for blob in entries(&algo.path())? {
            let digest = format!(
                "{}:{}",
                algo.file_name().display(),
                blob.file_name().display()
            );
            if !live_blobs.contains(&digest) {
                fs::remove_file(blob.path()).with_context(|| format!("removing blob {digest}"))?;
            }
        }
    }

    for algo in entries(&store.join("layers"))? {
        for layer in entries(&algo.path())? {
            let name = layer.file_name();
            let name = name.to_string_lossy();
            // ".extract-<hex>" is staging left by an interrupted extraction.
            let digest = format!("{}:{name}", algo.file_name().display());
            if name.starts_with(".extract-") || !live_layers.contains(&digest) {
                store::remove_tree(&layer.path())?;
            }
        }
    }

    // ".download-*" are partial downloads left in the store root by an aborted pull.
    for entry in entries(&store)? {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".download-")
        {
            fs::remove_file(entry.path())
                .with_context(|| format!("removing {}", entry.path().display()))?;
        }
    }

    Ok(())
}

// Remove ref markers for images whose app is no longer in the apps repo. The
// marker keeps an image reachable, so dropping it lets the following GC reclaim
// the image. A marker's file name is the reference with '/' swapped for '+'.
fn drop_orphan_refs() -> Result<()> {
    let mut live = HashSet::new();
    for app_name in app_names() {
        let Some(cfg) = AppConfig::load_or_warn(&app_name) else {
            continue;
        };
        if let Ok(reference) = cfg.image.reference.parse::<Reference>() {
            live.insert(reference.whole());
        }
    }

    for entry in entries(&store::dir()?.join("refs"))? {
        let reference = entry.file_name().to_string_lossy().replace('+', "/");
        if !live.contains(&reference) {
            fs::remove_file(entry.path())
                .with_context(|| format!("removing ref {}", entry.path().display()))?;
            eprintln!("dropped ref {reference}");
        }
    }
    Ok(())
}

// Whether the path is a current mount point according to /proc/self/mounts.
// Mount points containing characters the kernel octal-escapes (space, tab,
// newline, backslash) won't match, which only makes this conservative.
fn is_mounted(path: &Path) -> bool {
    let Ok(mounts) = fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    let path = path.to_string_lossy();
    mounts
        .lines()
        .filter_map(|line| line.split(' ').nth(1))
        .any(|mount_point| mount_point == path)
}

// Reclaim the bundle, container state, and overlay mount of any run whose climate
// process is gone (a SIGKILLed run). Runs whose climate process is still alive are
// in progress and left untouched.
fn prune_runtime() -> Result<()> {
    let base = crate::runtime::runtime_dir();

    // Containers first: a killed run's container init reparents to PID 1 and
    // keeps running, pinning the overlay mount. Deleting the container (force)
    // kills those processes and removes its cgroup/systemd scope and state.
    for entry in entries(&base.join("containers"))? {
        let id = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = pid_of(&id, "climate-") else {
            continue;
        };
        if alive(pid) {
            continue;
        }
        match Container::load(entry.path()) {
            Ok(mut container) => {
                if let Err(err) = container.delete(true) {
                    eprintln!("deleting container {id}: {err}");
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
            Err(err) => {
                eprintln!("loading container {id}: {err}");
                let _ = fs::remove_dir_all(entry.path());
            }
        }
        eprintln!("pruned container {id}");
    }

    for entry in entries(&base.join("bundles"))? {
        let id = entry.file_name().to_string_lossy().into_owned();
        if pid_of(&id, "climate-").is_some_and(|pid| !alive(pid)) {
            fs::remove_dir_all(entry.path()).with_context(|| format!("removing bundle {id}"))?;
        }
    }

    for entry in entries(&base.join("overlays"))? {
        let id = entry.file_name().to_string_lossy().into_owned();
        if pid_of(&id, "").is_none_or(alive) {
            continue;
        }
        // The fuse-overlayfs daemon may already be gone (killed, or the mount
        // vanished with the run), leaving merged as a plain directory. Only a
        // path that is still mounted needs fusermount3, and only a path that
        // remains mounted after a failed unmount blocks removal; remove_dir_all
        // failing with EBUSY backstops a stale mount table read.
        let merged = entry.path().join("merged");
        if is_mounted(&merged) {
            let status = Command::new("fusermount3")
                .arg("-u")
                .arg(&merged)
                .status()
                .context("running fusermount3")?;
            if !status.success() && is_mounted(&merged) {
                eprintln!("fusermount3 failed to unmount {}", merged.display());
                continue;
            }
        }
        fs::remove_dir_all(entry.path()).with_context(|| format!("removing overlay {id}"))?;
        eprintln!("pruned overlay {id}");
    }

    Ok(())
}

// The `clean` command: drop refs of removed apps, garbage-collect unreachable
// image data, and prune the runtime files of killed containers.
pub fn clean() -> Result<()> {
    drop_orphan_refs()?;
    gc_images()?;
    prune_runtime()
}
