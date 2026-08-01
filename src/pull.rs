use crate::config::{AppConfig, app_names};
use crate::store;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use oci_client::client::{ClientConfig, current_platform_resolver};
use oci_client::manifest::{IMAGE_MANIFEST_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE, OciDescriptor};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use std::io::IsTerminal;
use std::path::Path;
use tokio_util::io::InspectWriter;

// How many layers to download at once, the same number `docker pull` uses.
const MAX_CONCURRENT_DOWNLOADS: usize = 3;

// Progress bars go to stderr when there is a terminal, and are switched off otherwise so that
// redirected output stays clean.
fn draw_target() -> ProgressDrawTarget {
    if std::io::stderr().is_terminal() {
        ProgressDrawTarget::stderr()
    } else {
        ProgressDrawTarget::hidden()
    }
}

// A labelled progress bar for one download, or a spinner when the size is not known. It starts
// hidden because applying a style draws the bar straight away, which would leave a stray line;
// adding it to the MultiProgress shows it.
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

// Download one file to `temp` while updating `bar`. The client checks the content against the
// digest the registry advertised.
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
    let writer = InspectWriter::new(file, |chunk: &[u8]| bar.inc(chunk.len() as u64));
    client
        .pull_blob(reference, descriptor, writer)
        .await
        .with_context(|| format!("pulling blob {}", descriptor.digest))
}

// Download one layer, then unpack it into the store. Unpacking is CPU-bound, so it runs on a
// separate thread where it cannot hold up the other downloads.
async fn fetch_layer(
    client: &Client,
    reference: &Reference,
    layer: &OciDescriptor,
    bar: ProgressBar,
) -> Result<()> {
    let temp = store::temp_path("layer")?;
    let result = async {
        download_blob(client, reference, layer, &temp, &bar).await?;
        bar.finish_and_clear();

        let digest = layer.digest.clone();
        let media_type = layer.media_type.clone();
        let blob = temp.clone();
        tokio::task::spawn_blocking(move || store::extract_layer(&blob, &digest, &media_type))
            .await
            .context("layer extraction task panicked")?
    }
    .await;
    // Delete the temporary file on failure as well, so a broken download does not sit around until
    // the next successful pull cleans it up.
    let _ = std::fs::remove_file(&temp);
    result
}

// Download an image: ask the registry which version matches this machine's OS and CPU architecture,
// store its settings, and unpack the layers not already in the store. Since layers are identified
// by their content, re-pulling an unchanged image downloads nothing and a newer one only the layers
// that differ. Layers are fetched in parallel, each with its own progress bar.
async fn fetch_image(client: &Client, reference: &Reference) -> Result<()> {
    let auth = RegistryAuth::Anonymous;
    let (manifest, manifest_digest) = client
        .pull_image_manifest(reference, &auth)
        .await
        .with_context(|| format!("resolving {reference}"))?;

    // Keep the manifest, the list of the image's layers and settings, so later runs can look them
    // up without contacting the registry. It must be stored exactly as the registry sent it, since
    // writing the parsed form back out would no longer match the digest - and the call above parsed
    // it away.
    let (manifest_raw, _) = client
        .pull_manifest_raw(
            &reference.clone_with_digest(manifest_digest.clone()),
            &auth,
            &[OCI_IMAGE_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE],
        )
        .await
        .with_context(|| format!("fetching the manifest blob for {reference}"))?;
    store::write_blob(&manifest_digest, &manifest_raw)?;

    let multi = MultiProgress::with_draw_target(draw_target());

    // The image's own settings, needed later to build the container.
    let config_digest = &manifest.config.digest;
    if !store::has_blob(config_digest)? {
        let temp = store::temp_path("config")?;
        let bar = multi.add(styled_bar("config", manifest.config.size));
        download_blob(client, reference, &manifest.config, &temp, &bar).await?;
        bar.finish_and_clear();
        store::commit_blob(&temp, config_digest)?;
    }

    let layer_count = manifest.layers.len();
    // Pad the layer number to the width of the total so the bars stay lined up.
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
    // Status messages go to stderr so they never mix into an app's output.
    if fetched == 0 {
        eprintln!("up to date: {reference} ({manifest_digest})");
    } else {
        eprintln!("pulled {reference} ({manifest_digest})");
    }
    Ok(())
}

// Get the app's image into the store. Apps with `pull = false` supply their image some other way,
// so nothing is downloaded for them. With `update` the registry is contacted every time; without it
// an image already in the store is left alone. Only public registries are supported for now - there
// is no login.
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

// The `pull` command. With `update` it refreshes every app that was downloaded before; otherwise it
// downloads the one app that was named.
pub fn pull(update: bool, app: Option<&str>) -> Result<()> {
    let mut failed = Vec::new();

    if update {
        // One unreachable registry must not stop the remaining apps or the cleanup below, so
        // failures are only collected and reported at the end.
        for app_name in app_names() {
            let Some(cfg) = AppConfig::load_or_warn(&app_name) else {
                continue;
            };
            let Ok(reference) = cfg.image.reference.parse::<Reference>() else {
                continue;
            };
            if store::has_ref(reference.whole().as_str())?
                && let Err(err) = ensure(&cfg, true)
            {
                eprintln!("{app_name}: {err:#}");
                failed.push(app_name);
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

    // Free the disk space held by the image versions just replaced.
    crate::clean::gc_images()?;

    if !failed.is_empty() {
        bail!("failed to update: {}", failed.join(", "));
    }
    Ok(())
}
