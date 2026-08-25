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

// List a directory's entries, treating a missing directory as empty: the store and runtime
// subdirectories are only created when they are first needed.
fn entries(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    match fs::read_dir(dir) {
        Ok(reader) => reader
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("reading {}", dir.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("reading {}", dir.display())),
    }
}

// Read the pid of the CLImate process that created a runtime directory out of its name:
// "climate-<pid>-<nanos>", or "<pid>-<nanos>" for overlay directories.
fn pid_of(name: &str, prefix: &str) -> Option<i32> {
    name.strip_prefix(prefix)?.split('-').next()?.parse().ok()
}

// Whether a process with this pid still exists. Signal 0 delivers nothing and only reports whether
// the process could be signalled at all.
fn alive(pid: i32) -> bool {
    let Some(pid) = Pid::from_raw(pid) else {
        return false;
    };
    !matches!(test_kill_process(pid), Err(Errno::SRCH))
}

// Everything in the store still in use: for each downloaded image its manifest, the settings
// it points at, and its layers. The rest can be deleted.
fn live_set(store: &Path) -> Result<(HashSet<String>, HashSet<String>)> {
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
    Ok((live_blobs, live_layers))
}

// Delete everything in the store that no downloaded image refers to any more. Because images share
// layers, a layer is deleted only once no image needs it. Leftovers from interrupted downloads
// and unpacks are swept up as well.
pub fn gc_images() -> Result<()> {
    let store = store::dir()?;
    if !store.exists() {
        return Ok(());
    }

    // If a manifest is missing or damaged we cannot tell what is still in use, and deleting
    // on a guess could destroy a good image. So warn and skip the deletions; the other clean-up
    // steps are independent and may repair this.
    let (live_blobs, live_layers) = match live_set(&store) {
        Ok(live) => live,
        Err(err) => {
            eprintln!("skipping image GC: {err:#}");
            return Ok(());
        }
    };

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
            // ".extract-<hex>" is a half-unpacked layer from a run that died.
            let digest = format!("{}:{name}", algo.file_name().display());
            if name.starts_with(".extract-") || !live_layers.contains(&digest) {
                store::remove_tree(&layer.path())?;
            }
        }
    }

    // ".download-*" are half-finished downloads from an interrupted pull.
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

// Forget images whose app no longer exists. As long as the record under refs/ is there the image
// counts as in use, so removing it is what lets the pass above delete the image data. The file name
// is the reference, with '/' as '+'.
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

// Whether something is currently mounted at this path, according to the list of mounts the kernel
// exposes in /proc. Paths containing a space, tab, newline or backslash appear escaped there
// and never match, which only makes this cautious.
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

// Clean up after runs whose CLImate process no longer exists, which happens when a run is killed
// outright. Runs still alive are in progress and left alone.
fn prune_runtime() -> Result<()> {
    let base = crate::runtime::runtime_dir();

    // Containers come first: a killed run's container keeps running and holds the mounted image
    // in use. Deleting it with force stops its processes.
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
        // The fuse-overlayfs process may already be gone, leaving an ordinary empty directory that
        // needs no unmounting. Only a path that is still mounted after a failed attempt really
        // blocks removal - and if the mount list was out of date, the removal below fails
        // and reports it.
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

// The `clean` command: forget images of apps that are gone, delete image data nothing uses
// any more, and clean up after killed runs.
pub fn clean() -> Result<()> {
    drop_orphan_refs()?;
    gc_images()?;
    prune_runtime()
}
