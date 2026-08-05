use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use oci_client::Reference;
use oci_client::manifest::OciImageManifest;
use oci_spec::image::ImageConfiguration;
use ruzstd::decoding::StreamingDecoder;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::AppConfig;
use crate::pull;

// Local storage for downloaded images, in ~/.local/share/climate/images. It is the data directory
// rather than a cache because images marked `pull = false` come from elsewhere and could
// not be downloaded again if they were dropped.
//
//   blobs/<algo>/<hex>     a downloaded file, e.g. a manifest or image config
//   layers/<algo>/<hex>/   one layer, unpacked into a directory
//
// Names come from the digest (checksum) of the content, so a layer shared by two images is stored
// once and used by both.

pub fn dir() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("resolving the data directory")?
        .join("climate")
        .join("images"))
}

// Turn a digest ("sha256:<hex>") into the relative path "sha256/<hex>". Only letters and digits
// are accepted, so a bad digest cannot point outside the store.
fn digest_path(digest: &str) -> Result<PathBuf> {
    let (algo, hex) = digest
        .split_once(':')
        .with_context(|| format!("malformed digest '{digest}'"))?;
    let is_clean = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric());
    if !is_clean(algo) || !is_clean(hex) {
        bail!("malformed digest '{digest}'");
    }
    Ok(Path::new(algo).join(hex))
}

fn create_parent(path: &Path) -> Result<()> {
    let parent = path.parent().expect("store path has a parent");
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))
}

pub fn blob_path(digest: &str) -> Result<PathBuf> {
    Ok(dir()?.join("blobs").join(digest_path(digest)?))
}

pub fn layer_path(digest: &str) -> Result<PathBuf> {
    Ok(dir()?.join("layers").join(digest_path(digest)?))
}

// Path of the file under refs/ that records which version of an image was downloaded last.
// Its existence answers "do we already have this image?" without asking the registry.
// The '/' of a reference cannot appear in a file name and is replaced by '+', which references
// never contain, so none collide.
pub fn ref_marker(reference: &str) -> Result<PathBuf> {
    Ok(dir()?.join("refs").join(reference.replace('/', "+")))
}

pub fn has_ref(reference: &str) -> Result<bool> {
    Ok(ref_marker(reference)?.exists())
}

pub fn record_ref(reference: &str, manifest_digest: &str) -> Result<()> {
    let path = ref_marker(reference)?;
    create_parent(&path)?;
    fs::write(&path, manifest_digest).with_context(|| format!("recording ref {reference}"))
}

pub fn has_blob(digest: &str) -> Result<bool> {
    Ok(blob_path(digest)?.exists())
}

pub fn read_blob(digest: &str) -> Result<Vec<u8>> {
    let path = blob_path(digest)?;
    std::fs::read(&path).with_context(|| format!("reading blob {}", path.display()))
}

// Store bytes already held in memory. Used for the manifest, which arrives in one small response,
// not through the streaming path layers and configs take.
pub fn write_blob(digest: &str, bytes: &[u8]) -> Result<()> {
    let dest = blob_path(digest)?;
    create_parent(&dest)?;
    fs::write(&dest, bytes).with_context(|| format!("storing blob {digest}"))
}

// Which version of an image was downloaded last, or None if it never was.
pub fn read_ref(reference: &str) -> Result<Option<String>> {
    let path = ref_marker(reference)?;
    match fs::read_to_string(&path) {
        Ok(digest) => Ok(Some(digest)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading ref {reference}")),
    }
}

pub fn has_layer(digest: &str) -> Result<bool> {
    Ok(layer_path(digest)?.exists())
}

// A name no other run will pick: this process's pid plus the current time in nanoseconds. It goes
// into the names of the directories a run creates, so `clean::pid_of` can read the pid back
// and check whether that run is alive.
pub fn unique_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

// A temporary path for a download, inside the store so that it is on the same filesystem
// as its final location: a rename within one filesystem happens in one step, so a half-written file
// is never mistaken for a finished one.
pub fn temp_path(tag: &str) -> Result<PathBuf> {
    let dir = dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(format!(".download-{tag}-{}", unique_id())))
}

// Move a finished download into its final place in the store.
pub fn commit_blob(temp: &Path, digest: &str) -> Result<()> {
    let dest = blob_path(digest)?;
    create_parent(&dest)?;
    fs::rename(temp, &dest).with_context(|| format!("storing blob {digest}"))
}

// Delete a directory tree that may contain read-only directories. Unpacked layers keep
// the permissions the image gave them, and nothing can be deleted from a directory that
// is not writable, so every directory is made writable first. Symlinks are not followed, so only
// real directories are touched.
pub fn remove_tree(path: &Path) -> Result<()> {
    fn grant_writable(dir: &Path) -> std::io::Result<()> {
        let mut perms = fs::symlink_metadata(dir)?.permissions();
        perms.set_mode(perms.mode() | 0o700);
        fs::set_permissions(dir, perms)?;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                grant_writable(&entry.path())?;
            }
        }
        Ok(())
    }

    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => grant_writable(path)
            .with_context(|| format!("preparing {} for removal", path.display()))?,
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    }
    fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

// Unpack a downloaded layer into its own directory, decompressing it by media type. Layers mark
// deleted files with special ".wh." entries; those are left alone, as fuse-overlayfs understands
// them. Unpacking happens in a temporary directory that is renamed at the end, so a broken unpack
// never looks finished.
pub fn extract_layer(temp: &Path, digest: &str, media_type: &str) -> Result<()> {
    let dest = layer_path(digest)?;
    create_parent(&dest)?;
    let parent = dest.parent().expect("layer path has a parent");
    let name = dest.file_name().expect("layer path has a file name");
    let staging = parent.join(format!(".extract-{}", name.to_string_lossy()));
    remove_tree(&staging)?;

    let file = File::open(temp).with_context(|| format!("opening layer blob {digest}"))?;
    let reader: Box<dyn Read> = if media_type.contains("zstd") {
        Box::new(StreamingDecoder::new(file).context("initialising zstd decoder")?)
    } else if media_type.contains("gzip") {
        Box::new(GzDecoder::new(file))
    } else if media_type.ends_with("tar") {
        Box::new(file)
    } else {
        bail!("unsupported layer media type '{media_type}'");
    };

    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);
    archive
        .unpack(&staging)
        .with_context(|| format!("extracting layer {digest}"))?;

    fs::rename(&staging, &dest).with_context(|| format!("finalising layer {digest}"))
}

// What a run needs from an image: the layers of its filesystem, bottom-up, and the settings
// it ships with (the command, environment and working directory).
pub struct Image {
    pub layers: Vec<String>,
    pub config: ImageConfiguration,
}

// Make sure the app's image is in the store and read out what a run needs. An image already there
// is used as it is, never refreshed - that is what `pull` is for. Apps with `pull = false` fail
// here if their image is missing.
pub fn resolve(cfg: &AppConfig) -> Result<Image> {
    let reference: Reference = cfg
        .image
        .reference
        .parse()
        .with_context(|| format!("invalid image reference '{}'", cfg.image.reference))?;
    let key = reference.whole();

    pull::ensure(cfg, false)?;

    let Some(manifest_digest) = read_ref(&key)? else {
        bail!(
            "{}: image '{}' is not in the store; pull it first",
            cfg.app.name,
            cfg.image.reference,
        );
    };

    let manifest: OciImageManifest = serde_json::from_slice(&read_blob(&manifest_digest)?)
        .with_context(|| format!("parsing cached manifest for {key}"))?;
    let config = ImageConfiguration::from_reader(read_blob(&manifest.config.digest)?.as_slice())
        .with_context(|| format!("parsing image config for {key}"))?;
    let layers = manifest.layers.iter().map(|l| l.digest.clone()).collect();

    Ok(Image { layers, config })
}
