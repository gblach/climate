use crate::config::{AppConfig, app_names};
use crate::store;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use oci_client::client::{ClientConfig, current_platform_resolver};
use oci_client::manifest::OciDescriptor;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use std::io::IsTerminal;
use std::path::Path;
use tokio_util::io::InspectWriter;

// How many layer blobs to download at once, à la `docker pull`.
const MAX_CONCURRENT_DOWNLOADS: usize = 3;

// Where progress bars are drawn: stderr when interactive, hidden otherwise so
// piped output stays clean.
fn draw_target() -> ProgressDrawTarget {
    if std::io::stderr().is_terminal() {
        ProgressDrawTarget::stderr()
    } else {
        ProgressDrawTarget::hidden()
    }
}

// A labelled, byte-sized progress bar for one blob download. It starts on a
// hidden draw target so that styling it (which draws immediately) does not leak
// a stray line; the caller's MultiProgress reassigns the target on `add`.
fn styled_bar(label: &str, size: i64) -> ProgressBar {
    let len = (size > 0).then_some(size as u64);
    let template = if len.is_some() {
        "{msg:<10} [{bar:30}] {bytes:>10}/{total_bytes:<10} {bytes_per_sec}"
    } else {
        "{msg:<10} {spinner} {bytes} {bytes_per_sec}"
    };
    let bar = ProgressBar::with_draw_target(len, ProgressDrawTarget::hidden());
    bar.set_style(
        ProgressStyle::with_template(template)
            .expect("valid template")
            .progress_chars("=> "),
    );
    bar.set_message(label.to_string());
    bar
}

// Stream a blob to `temp`, verifying its digest against the descriptor and
// advancing `bar`. Used for the config and layer blobs.
async fn download_blob(
    client: &Client,
    reference: &Reference,
    descriptor: &OciDescriptor,
    temp: &Path,
    bar: &ProgressBar,
) -> Result<()> {
    let file = tokio::fs::File::create(temp)
        .await
        .with_context(|| format!("creating {}", temp.display()))?;
    // Advance the progress bar by the bytes of each successful write.
    let writer = InspectWriter::new(file, |chunk: &[u8]| bar.inc(chunk.len() as u64));
    client
        .pull_blob(reference, descriptor, writer)
        .await
        .with_context(|| format!("pulling blob {}", descriptor.digest))
}

// Download a layer to a temp file (advancing its own progress line), then
// extract it into the store off-thread so it does not stall the other
// concurrent downloads.
async fn fetch_layer(
    client: &Client,
    reference: &Reference,
    layer: &OciDescriptor,
    bar: ProgressBar,
) -> Result<()> {
    let temp = store::temp_path("layer")?;
    download_blob(client, reference, layer, &temp, &bar).await?;
    bar.finish_and_clear();

    let digest = layer.digest.clone();
    let media_type = layer.media_type.clone();
    let blob = temp.clone();
    let extracted =
        tokio::task::spawn_blocking(move || store::extract_layer(&blob, &digest, &media_type))
            .await
            .context("layer extraction task panicked")?;
    let _ = std::fs::remove_file(&temp);
    extracted
}

// Resolve the reference (narrowing a multi-arch index to the running OS/arch),
// cache the config blob, and extract any layers not already in the store. Only
// missing blobs are downloaded, so re-pulling an up-to-date image is cheap; a
// newer image brings new layer digests, which are fetched and extracted. Layers
// download concurrently, each on its own progress line.
async fn fetch_image(client: &Client, reference: &Reference) -> Result<()> {
    let auth = RegistryAuth::Anonymous;
    let (manifest, manifest_digest) = client
        .pull_image_manifest(reference, &auth)
        .await
        .with_context(|| format!("resolving {reference}"))?;

    // Persist the manifest so a later run can resolve the image's layers and
    // config from the store without contacting the registry.
    let manifest_json = serde_json::to_vec(&manifest).context("encoding manifest")?;
    store::write_blob(&manifest_digest, &manifest_json)?;

    let multi = MultiProgress::with_draw_target(draw_target());

    // The image config (entrypoint/cmd/env/workdir) is kept for the spec stage.
    let config_digest = &manifest.config.digest;
    if !store::has_blob(config_digest)? {
        let temp = store::temp_path("config")?;
        let bar = multi.add(styled_bar("config", manifest.config.size));
        download_blob(client, reference, &manifest.config, &temp, &bar).await?;
        bar.finish_and_clear();
        store::commit_blob(&temp, config_digest)?;
    }

    let layer_count = manifest.layers.len();
    // Pad the index to the width of the total so every label is the same length
    // and the bars stay column-aligned once the count reaches two digits.
    let index_width = layer_count.to_string().len();
    let mut pending = Vec::new();
    for (index, layer) in manifest.layers.iter().enumerate() {
        if !store::has_layer(&layer.digest)? {
            pending.push((index, layer));
        }
    }
    let fetched = pending.len();

    let downloads = pending.into_iter().map(|(index, layer)| {
        let bar = multi.add(styled_bar(
            &format!("layer {:>index_width$}/{layer_count}", index + 1),
            layer.size,
        ));
        fetch_layer(client, reference, layer, bar)
    });
    let mut stream =
        futures_util::stream::iter(downloads).buffer_unordered(MAX_CONCURRENT_DOWNLOADS);
    while let Some(result) = stream.next().await {
        result?;
    }

    store::record_ref(reference.whole().as_str(), &manifest_digest)?;
    // Status goes to stderr so it never mixes with a run's stdout.
    if fetched == 0 {
        eprintln!("up to date: {reference} ({manifest_digest})");
    } else {
        eprintln!("pulled {reference} ({manifest_digest})");
    }
    Ok(())
}

// Make the app's image available in the store. Apps with `pull = false`
// provide their image out of band, so nothing is fetched. With `update`, the
// registry is always re-checked; without it, an image already in the store is
// left untouched (the run path uses this so a running app is reproducible).
// Only anonymous (public) registry access is supported for now.
pub fn ensure(cfg: &AppConfig, update: bool) -> Result<()> {
    if !cfg.image.pull {
        return Ok(());
    }

    let reference: Reference = cfg
        .image
        .reference
        .parse()
        .with_context(|| format!("invalid image reference '{}'", cfg.image.reference))?;

    if !update && store::has_ref(reference.whole().as_str())? {
        return Ok(());
    }

    let config = ClientConfig {
        platform_resolver: Some(Box::new(current_platform_resolver)),
        ..Default::default()
    };
    let client = Client::new(config);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;

    runtime.block_on(fetch_image(&client, &reference))
}

// Handle the `pull` command. With `update`, fetch every app pulled before;
// otherwise fetch the named app, erroring if its image is provided out of band
// (`pull = false`).
pub fn pull(update: bool, app: Option<&str>) -> Result<()> {
    if update {
        for app_name in app_names() {
            let Some(cfg) = AppConfig::load_or_warn(&app_name) else {
                continue;
            };
            let Ok(reference) = cfg.image.reference.parse::<Reference>() else {
                continue;
            };
            if store::has_ref(reference.whole().as_str())? {
                ensure(&cfg, true)?;
            }
        }
    } else {
        let app_name = app.context("pull: specify an app name or -u/--update")?;
        let cfg = AppConfig::load(app_name)?;
        if !cfg.image.pull {
            bail!("{app_name}: image is built locally or provided out of band (pull = false)");
        }
        ensure(&cfg, true)?;
    }

    // Reclaim the layers/config the just-pulled newer images superseded.
    crate::clean::gc_images()
}
