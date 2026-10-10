use anyhow::{Context, Result, bail};
use oci_client::Reference;
use oci_client::manifest::{IMAGE_LAYER_MEDIA_TYPE, OciDescriptor, OciImageManifest};
use std::path::Path;

use crate::config::{AppConfig, Capability, Entrypoint, LimitsConfig, Network, RunConfig};
use crate::runtime::Mount;
use crate::store;

// Built layers and manifests are not content-addressed, as nothing is ever checked against them,
// so they are filed under an algorithm name of their own.
const BUILD_ALGO: &str = "build";

// Package managers running as root expect to write into directories the image left read-only and to
// hand files to root. Only root is mapped into the container, so these reach no one else's files.
const BUILD_CAPABILITIES: [Capability; 4] = [
    Capability::Chown,
    Capability::DacOverride,
    Capability::Fowner,
    Capability::Fsetid,
];

// The script's output would otherwise mix into the app's when a run builds the image first, so
// it goes to stderr, where the other status messages go.
fn build_config(cfg: &AppConfig, script: &str) -> AppConfig {
    AppConfig {
        app: cfg.app.clone(),
        image: cfg.image.clone(),
        run: RunConfig {
            entrypoint: Some(Entrypoint::List(vec![
                "/bin/sh".to_string(),
                "-ec".to_string(),
                format!("exec 1>&2\n{script}"),
            ])),
            mount_cwd: false,
            network: Network::Full,
            capabilities: BUILD_CAPABILITIES.to_vec(),
            ..RunConfig::default()
        },
        limits: LimitsConfig::default(),
    }
}

fn run_script(
    cfg: &AppConfig,
    script: &str,
    base_key: &str,
    base: &OciImageManifest,
    upper: &Path,
    work: &Path,
) -> Result<()> {
    let build_cfg = build_config(cfg, script);
    let config = store::image_config(base_key, base)?;
    let layers: Vec<String> = base.layers.iter().map(|l| l.digest.clone()).collect();
    let mountpoints = crate::spec::mountpoints(&build_cfg)?;
    let mount = Mount::new(&layers, &mountpoints, Some((upper, work)))?;

    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let spec = crate::spec::build(&build_cfg, &config, mount.root(), &[], uid, gid, false)?;
    let seccomp = crate::seccomp::profile(&build_cfg.run)?;
    let code = crate::runtime::run(spec, seccomp, false, &Network::Full)?;
    drop(mount);
    if code != 0 {
        bail!(
            "{}: install script failed with exit code {code}",
            cfg.app.name
        );
    }
    Ok(())
}

// Runs the install script in a container of the base image whose root is writable, then keeps what
// the script wrote as one more layer on top of the base's. The upper directory serves as a layer as
// it is: fuse-overlayfs reads back the markers it leaves there for deleted files.
pub fn build(cfg: &AppConfig, script: &str) -> Result<()> {
    let reference: Reference = cfg
        .image
        .reference
        .parse()
        .with_context(|| format!("invalid image reference '{}'", cfg.image.reference))?;
    let base_key = reference.whole();
    let Some(base) = store::read_manifest(&base_key)? else {
        bail!(
            "{}: base image '{}' is not in the store; pull it first",
            cfg.app.name,
            cfg.image.reference,
        );
    };

    let id = store::unique_id().replace('-', "");
    let digest = format!("{BUILD_ALGO}:{id}");
    let layer = store::layer_path(&digest)?;
    let parent = layer.parent().expect("layer path has a parent");
    // The ".extract-" prefix has `clean` remove what a killed build leaves behind. The work
    // directory has to share a filesystem with the upper one, so it sits beside it.
    let upper = parent.join(format!(".extract-{id}"));
    let work = parent.join(format!(".extract-{id}-work"));
    for dir in [&upper, &work] {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    eprintln!("building {} on {}", cfg.app.name, cfg.image.reference);
    let result = run_script(cfg, script, &base_key, &base, &upper, &work);
    let _ = store::remove_tree(&work);
    if let Err(err) = result {
        let _ = store::remove_tree(&upper);
        return Err(err);
    }
    std::fs::rename(&upper, &layer).with_context(|| format!("finalising layer {digest}"))?;

    let mut manifest = base;
    manifest.layers.push(OciDescriptor {
        media_type: IMAGE_LAYER_MEDIA_TYPE.to_string(),
        digest: digest.clone(),
        ..Default::default()
    });
    // Blobs and layers live apart, so the manifest can take the layer's name.
    let bytes = serde_json::to_vec(&manifest).context("serialising the built manifest")?;
    store::write_blob(&digest, &bytes)?;

    let key = store::image_key(cfg)?;
    store::record_ref(&key, &digest)?;
    eprintln!("built {key} ({digest})");
    Ok(())
}
