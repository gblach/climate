use anyhow::{Context, Result, bail};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::fetch::{NoProgress, fetch_remote};
use grit_lib::objects::parse_commit;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::refs::{resolve_ref, write_ref, write_symbolic_ref};
use grit_lib::repo::{Repository, init_repository};
use grit_lib::transfer::{FetchOptions, FetchOutcome};
use grit_lib::transport::http::http_fetch;
use grit_lib::transport::http::ureq_client::UreqHttpClient;
use grit_lib::transport::{ConnectOptions, Service, SshTransport, Transport, is_ssh_url};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

const SYSTEM_DIR: &str = "/usr/share/climate/apps";

// Git repository holding the app definitions, overridable with $CLIMATE_APPS_URL.
const DEFAULT_APPS_URL: &str = "https://github.com/gblach/climate-apps.git";

// Tells git to fetch every remote branch into refs/remotes/origin/.
const FETCH_REFSPEC: &str = "+refs/heads/*:refs/remotes/origin/*";

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub app: AppMeta,
    pub image: ImageConfig,
    #[serde(default)]
    pub run: RunConfig,
}

#[derive(Debug, Deserialize)]
pub struct AppMeta {
    pub name: String,
    pub description: String,
    // SPDX license identifier of the app itself, e.g. "MIT".
    pub license: String,
}

#[derive(Debug, Deserialize)]
pub struct ImageConfig {
    // Full image reference including registry and tag, e.g. "quay.io/coreos/butane:release".
    pub reference: String,
    // Whether the image may be downloaded from its registry. Set to false for images that are built
    // locally or installed by some other means.
    #[serde(default = "yes")]
    pub pull: bool,
}

// Replacement for the program the image runs by default. A string names a single executable; a list
// is the whole command line.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Entrypoint {
    String(String),
    List(Vec<String>),
}

// How much network the container gets. `Full` uses the host's own network, so the app reaches
// the internet just like the user does. The other two give it a private, empty network: `None` (the
// default) has no working interface at all, `Localhost` enables 127.0.0.1 only, so the app can talk
// just to itself.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Full,
    #[default]
    None,
    Localhost,
}

// How to run the image. The defaults share the current working directory and give the container
// no network; an app can override both.
#[derive(Debug, Deserialize)]
pub struct RunConfig {
    #[serde(default)]
    pub entrypoint: Option<Entrypoint>,
    // Arguments inserted after the entrypoint, before the ones the user typed.
    #[serde(default)]
    pub args: Vec<String>,
    // Environment variables. "NAME" copies the value from the host, "NAME=VALUE" sets it outright.
    #[serde(default)]
    pub env: Vec<String>,

    // Whether the current working directory is shared with the container under the same path. When
    // false nothing is shared and the container starts in the directory the image itself specifies.
    #[serde(rename = "mount-cwd", default = "yes")]
    pub mount_cwd: bool,

    // Extra host paths to share, beyond the working directory, meant above all for a tool's config
    // directory or file. Each entry is "source", "source:destination", or either with a trailing
    // ":ro" or ":rw"; see `parse_mount`.
    #[serde(default)]
    pub mount: Vec<String>,

    #[serde(default)]
    pub network: Network,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            entrypoint: None,
            args: Vec::new(),
            env: Vec::new(),
            mount_cwd: true,
            mount: Vec::new(),
            network: Network::default(),
        }
    }
}

fn yes() -> bool {
    true
}

// Directories searched for app definitions; the first match wins: the override directory
// ($CLIMATE_APPS_DIR) when set, definitions written by the user (~/.config/climate/apps),
// definitions downloaded by `climate sync` (~/.local/share/climate/apps), then the system-wide ones
// (/usr/share/climate/apps).
fn search_dirs() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(apps_dir) = std::env::var_os("CLIMATE_APPS_DIR") {
        paths.push(PathBuf::from(apps_dir));
    }
    if let Some(config_dir) = dirs::config_dir() {
        paths.push(config_dir.join("climate").join("apps"));
    }
    if let Some(data_dir) = dirs::data_dir() {
        paths.push(data_dir.join("climate").join("apps"));
    }
    paths.push(PathBuf::from(SYSTEM_DIR));
    paths
}

// Names of all available apps, taken from the TOML file names. The BTreeSet sorts them and drops
// duplicates when an app exists in several directories.
pub fn app_names() -> Vec<String> {
    let mut app_names = BTreeSet::new();
    for dir in search_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            if let Some(app_name) = path.file_stem().and_then(|s| s.to_str()) {
                app_names.insert(app_name.to_string());
            }
        }
    }
    app_names.into_iter().collect()
}

// App names end up as file names (`<name>.toml`) and as symlink names, so only harmless characters
// are allowed. A name containing '/' could point outside the search directories, and one starting
// with '.' would create a hidden file.
fn validate_app_name(app_name: &str) -> Result<()> {
    let valid = !app_name.is_empty()
        && !app_name.starts_with('.')
        && app_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+._-".contains(&b));
    if !valid {
        bail!(
            "invalid app name '{app_name}' (allowed: A-Z a-z 0-9 + . _ -, not starting with '.')"
        );
    }
    Ok(())
}

// Find an app definition in the search directories and read it. The path is returned alongside
// the text so error messages can name the file.
fn read(app_name: &str) -> Result<(PathBuf, String)> {
    validate_app_name(app_name)?;
    let filename = format!("{app_name}.toml");
    let path = search_dirs()
        .into_iter()
        .map(|dir| dir.join(&filename))
        .find(|path| path.is_file())
        .with_context(|| format!("unknown app '{app_name}'"))?;
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok((path, text))
}

impl AppConfig {
    fn parse(app_name: &str, path: &Path, text: &str) -> Result<Self> {
        let config: Self =
            toml::from_str(text).with_context(|| format!("parsing {}", path.display()))?;
        if config.app.name != app_name {
            anyhow::bail!(
                "{}: app name '{}' does not match file name '{app_name}'",
                path.display(),
                config.app.name,
            );
        }
        if config.app.description.is_empty() {
            anyhow::bail!("{}: app description must not be empty", path.display());
        }
        if config.app.license.is_empty() {
            anyhow::bail!("{}: app license must not be empty", path.display());
        }
        Ok(config)
    }

    pub fn load(app_name: &str) -> Result<Self> {
        let (path, text) = read(app_name)?;
        Self::parse(app_name, &path, &text)
    }

    // Load a definition twice: once into the struct, once as a plain TOML table. The struct fills
    // in defaults for missing keys, so only the table still shows which keys the file itself
    // states.
    pub fn load_with_raw(app_name: &str) -> Result<(Self, toml::Table)> {
        let (path, text) = read(app_name)?;
        let config = Self::parse(app_name, &path, &text)?;
        let raw = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok((config, raw))
    }

    pub fn load_or_warn(app_name: &str) -> Option<Self> {
        match Self::load(app_name) {
            Ok(config) => Some(config),
            Err(err) => {
                eprintln!("skipping {app_name}: {err:#}");
                None
            }
        }
    }

    // Download the image if it is missing, stack its layers into a root filesystem, describe
    // the container, and run it. Ends this process with the container's exit code, so it only
    // returns when the setup fails.
    pub fn run(&self, user_args: &[String]) -> Result<()> {
        crate::spec::check_host_dir(&self.run)?;
        let image = crate::store::resolve(self)?;
        let mountpoints = crate::spec::mountpoints(self)?;
        let mount = crate::runtime::Mount::new(&image.layers, &mountpoints)?;

        // Inside the container the app appears to run as root, but the kernel maps that back
        // to the real user, so files it writes into the shared directory stay owned by that user.
        let (uid, gid) = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        // Give the app a terminal only when all three standard streams really are one.
        // If any of them is piped or redirected, the container gets them as they are,
        // so the app notices and prints plain output.
        let tty = std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal();

        let spec = crate::spec::build(self, &image.config, mount.root(), user_args, uid, gid, tty)?;
        let code = crate::runtime::run(spec, tty)?;

        // Dropping the mount unmounts it; process::exit below would skip that.
        drop(mount);
        std::process::exit(code);
    }
}

// Download the apps repository. An ssh URL talks to the server through an ssh subprocess,
// the way git does (so $GIT_SSH_COMMAND applies); an http(s) URL uses the built-in HTTP client.
// Both speak version 2 of the git protocol.
fn fetch(repo: &Repository, url: &str, opts: &FetchOptions) -> Result<FetchOutcome> {
    if is_ssh_url(url) {
        let conn_opts = ConnectOptions {
            protocol_version: 2,
            server_options: Vec::new(),
        };
        let mut conn = SshTransport::new()
            .connect(url, Service::UploadPack, &conn_opts)
            .with_context(|| format!("connecting to {url}"))?;
        fetch_remote(&repo.git_dir, conn.as_mut(), opts, &mut NoProgress)
            .with_context(|| format!("fetching {url}"))
    } else if url.starts_with("http://") || url.starts_with("https://") {
        let client = UreqHttpClient::new().with_git_protocol("version=2");
        http_fetch(&client, &repo.git_dir, url, opts, &mut NoProgress)
            .with_context(|| format!("fetching {url}"))
    } else {
        bail!("unsupported apps URL scheme: {url}");
    }
}

// Fetch the newest commit of the apps repository (depth 1, so no history is downloaded) and update
// the checked-out files to match it.
fn fetch_and_checkout(repo: &Repository, url: &str) -> Result<()> {
    let opts = FetchOptions {
        refspecs: vec![FETCH_REFSPEC.to_string()],
        depth: Some(1),
        ..Default::default()
    };
    let outcome = fetch(repo, url, &opts)?;
    if outcome.updates.is_empty() {
        bail!("the apps remote {url} advertised no branches (does it exist and is it accessible?)");
    }

    // Look up the commit the remote's default branch now points at.
    let branch = outcome
        .default_branch
        .context("the apps remote advertised no default branch")?;
    let tracking = format!("refs/remotes/origin/{branch}");
    let tip = outcome
        .updates
        .iter()
        .find(|u| u.local_ref.as_deref() == Some(tracking.as_str()))
        .and_then(|u| u.new_oid)
        .with_context(|| format!("fetch did not update {tracking}"))?;

    // The file listing of the commit checked out now, for the checkout below to compare against.
    // A fresh clone has no local branch and nothing to compare.
    let local_branch = format!("refs/heads/{branch}");
    let from_tree = match resolve_ref(&repo.git_dir, &local_branch) {
        Ok(old_tip) => {
            let old = parse_commit(&repo.odb.read(&old_tip)?.data)
                .context("reading the previous commit")?;
            Some(old.tree)
        }
        Err(_) => None,
    };

    // Move the local branch to the fetched commit and check that branch out.
    write_symbolic_ref(&repo.git_dir, "HEAD", &local_branch).context("setting HEAD")?;
    write_ref(&repo.git_dir, &local_branch, &tip)
        .with_context(|| format!("updating {local_branch}"))?;

    // Comparing the two file listings, rather than just unpacking the new one, means apps deleted
    // upstream are also deleted locally.
    let commit = parse_commit(&repo.odb.read(&tip)?.data).context("reading the fetched commit")?;
    checkout_between_trees(repo, from_tree.as_ref(), &commit.tree)
        .context("checking out the fetched files")?;
    Ok(())
}

// Download the app definitions: the first run clones the repository, later runs update
// it. `--system` writes to the system-wide directory, which needs root, otherwise they
// go to the user's data directory.
pub fn sync(system: bool) -> Result<()> {
    let target = if system {
        PathBuf::from(SYSTEM_DIR)
    } else {
        dirs::data_dir()
            .context("resolving the user data directory")?
            .join("climate")
            .join("apps")
    };

    let git_dir = target.join(".git");
    if git_dir.is_dir() {
        let repo = Repository::open(&git_dir, Some(&target))
            .with_context(|| format!("opening {}", target.display()))?;
        // grit-lib offers no remote API, so read the URL out of the git config.
        let cfg = ConfigSet::load(Some(&git_dir), false).context("reading repository config")?;
        let url = cfg
            .get("remote.origin.url")
            .context("the apps repository has no remote.origin.url")?;
        fetch_and_checkout(&repo, &url)?;
    } else {
        if let Ok(mut entries) = std::fs::read_dir(&target)
            && entries.next().is_some()
        {
            bail!("{} already exists and is not empty", target.display());
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let url =
            std::env::var("CLIMATE_APPS_URL").unwrap_or_else(|_| DEFAULT_APPS_URL.to_string());

        // grit-lib has no single clone call, so create the repository by hand and store the remote
        // URL and refspec that later syncs read back.
        let repo = init_repository(&target, false, "main", None, "files")
            .with_context(|| format!("initializing {}", target.display()))?;
        let mut cfg = ConfigFile::from_path(&git_dir.join("config"), ConfigScope::Local)
            .context("reading repository config")?
            .context("the freshly initialized repository has no config file")?;
        cfg.set("remote.origin.url", &url)
            .context("recording remote.origin.url")?;
        cfg.set("remote.origin.fetch", FETCH_REFSPEC)
            .context("recording remote.origin.fetch")?;
        cfg.write().context("writing repository config")?;

        fetch_and_checkout(&repo, &url)?;
    }

    println!("synced apps into {}", target.display());
    Ok(())
}
