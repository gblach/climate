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

// Content-addressed store for image blobs and extracted layers, under
// $XDG_DATA_HOME/climate/images (e.g. ~/.local/share/climate/images). It lives in the
// data directory, not the cache, because `pull = false` images are provided out
// of band and cannot be re-fetched.
//
//   blobs/<algo>/<hex>     verified raw blob (the image config)
//   layers/<algo>/<hex>/   layer tarball extracted into a lowerdir
//
// Everything is keyed by digest, so a layer shared between images is stored
// once and reused.

pub fn dir() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("resolving the data directory")?
        .join("climate")
        .join("images"))
}

// Split an OCI digest ("sha256:<hex>") into an "<algo>/<hex>" relative path,
// rejecting anything that could escape the store.
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

// Create the parent directory of a store path so a file or layer can be
// written there.
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

// Marker recording the manifest digest last pulled for an image reference,
// under refs/. The reference's '/' (its only filesystem-unsafe character; the
// rest of the OCI grammar is path-safe) is swapped for '+', which the grammar
// never produces, so the mapping is injective. Its presence answers "has this
// app been pulled before" without hitting the registry.
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

// Read a cached blob's bytes (the manifest or the image config).
pub fn read_blob(digest: &str) -> Result<Vec<u8>> {
    let path = blob_path(digest)?;
    std::fs::read(&path).with_context(|| format!("reading blob {}", path.display()))
}

// Store bytes under their digest. Used for the manifest, whose JSON cannot be
// streamed through the download path the layer/config blobs take.
pub fn write_blob(digest: &str, bytes: &[u8]) -> Result<()> {
    let dest = blob_path(digest)?;
    create_parent(&dest)?;
    fs::write(&dest, bytes).with_context(|| format!("storing blob {digest}"))
}

// The manifest digest last recorded for a reference, or None if never pulled.
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

// A token unique across concurrent runs: this process's pid and a nanosecond
// timestamp. It is embedded in runtime artifact names so `clean::pid_of` can
// recover the owning pid and tell whether the run is still alive.
pub fn unique_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

// A unique temporary path inside the store, on the same filesystem as the
// blob and layer directories so the final rename into place is atomic.
pub fn temp_path(tag: &str) -> Result<PathBuf> {
    let dir = dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(format!(".download-{tag}-{}", unique_id())))
}

// Move an already-downloaded temp file into the content-addressed blob cache.
pub fn commit_blob(temp: &Path, digest: &str) -> Result<()> {
    let dest = blob_path(digest)?;
    create_parent(&dest)?;
    fs::rename(temp, &dest).with_context(|| format!("storing blob {digest}"))
}

// Remove a directory tree that may contain read-only directories. Extracted
// layers preserve their image's permissions, which include directories without
// owner write (e.g. mode 0555); a directory's entries cannot be unlinked until
// it is writable, so grant the owner rwx top-down before removing. Symlinks are
// not followed, so only real directories are touched.
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

// Extract a layer tarball (at `temp`) into its lowerdir, decompressing by media
// type. OCI whiteouts (.wh.<name>, .wh..wh..opq) are left in place: fuse-overlayfs
// reads them natively. The work is staged in a sibling temp directory and
// renamed into place so an interrupted extraction never looks complete.
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

// An image resolved from the local store: its layer digests in OCI order (base
// first) and the parsed image config (entrypoint/cmd/env/workdir).
pub struct Image {
    pub layers: Vec<String>,
    pub config: ImageConfiguration,
}

// Make the app's image available and read back what the run engine needs.
// The image is fetched only when it is absent from the store; an image that is
// already present is never updated here (use `pull` for that). `pull = false`
// apps are provided out of band and require the image to be present already.
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
