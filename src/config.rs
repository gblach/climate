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
use std::path::PathBuf;

const SYSTEM_DIR: &str = "/usr/share/climate/apps";

// Default apps repository, overridable with $CLIMATE_APPS_URL.
const DEFAULT_APPS_URL: &str = "https://github.com/gblach/climate-apps.git";

// The fetch refspec mirroring every remote branch into a tracking ref.
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
    #[serde(default)]
    pub description: String,
    // SPDX license identifier of the main app.
    pub license: String,
}

#[derive(Debug, Deserialize)]
pub struct ImageConfig {
    // Fully qualified reference, e.g. "quay.io/coreos/butane:release".
    pub reference: String,
    // When true (the default) pull newer images only; when false never pull,
    // i.e. the image is built locally or provided out of band.
    #[serde(default = "yes")]
    pub pull: bool,
}

// An image entrypoint override. A string is the single executable to run; a
// list is the full argv. Either replaces the image's entrypoint.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Entrypoint {
    String(String),
    List(Vec<String>),
}

// The container's network access. `Full` shares the host network namespace, so
// the app reaches the internet as the user would. The rest run in an isolated
// network namespace: `None` (the default) has only a down loopback, i.e. no
// connectivity at all; `Localhost` brings that loopback up so the app can talk
// to itself over 127.0.0.1 but nothing else.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Full,
    #[default]
    None,
    Localhost,
}

// How to run the image. By default the current working directory is mounted and
// the container has no network; an app overrides either explicitly.
#[derive(Debug, Deserialize)]
pub struct RunConfig {
    // Override the image entrypoint.
    #[serde(default)]
    pub entrypoint: Option<Entrypoint>,
    // Arguments placed after the entrypoint, before user-supplied ones.
    #[serde(default)]
    pub args: Vec<String>,
    // Environment entries. Either "NAME" (pass through from host) or
    // "NAME=VALUE" (set explicitly).
    #[serde(default)]
    pub env: Vec<String>,

    // Whether to bind-mount the current working directory into the container
    // at the same path. When false, nothing is mounted and the image's own
    // workdir is used.
    #[serde(default = "yes")]
    pub cwd: bool,

    // Which network the container can reach.
    #[serde(default)]
    pub network: Network,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            entrypoint: None,
            args: Vec::new(),
            env: Vec::new(),
            cwd: true,
            network: Network::default(),
        }
    }
}

fn yes() -> bool {
    true
}

// Directories searched for definitions, highest precedence first:
// the override directory ($CLIMATE_APPS_DIR) when set,
// user-authored config ($XDG_CONFIG_HOME/climate/apps or ~/.config/climate/apps),
// the synced apps ($XDG_DATA_HOME/climate/apps or ~/.local/share/climate/apps),
// then the system directory (/usr/share/climate/apps).
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

// Names of all available app definitions (TOML file stems), sorted and
// deduplicated across all search directories.
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

impl AppConfig {
    pub fn load(app_name: &str) -> Result<Self> {
        let filename = format!("{app_name}.toml");
        let path = search_dirs()
            .into_iter()
            .map(|dir| dir.join(&filename))
            .find(|path| path.is_file())
            .with_context(|| format!("unknown app '{app_name}'"))?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if config.app.name != app_name {
            anyhow::bail!(
                "{}: app name '{}' does not match file name '{app_name}'",
                path.display(),
                config.app.name,
            );
        }
        if config.app.license.is_empty() {
            anyhow::bail!("{}: app license must not be empty", path.display());
        }
        Ok(config)
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

    // Pull (if needed), mount the image's layers, build the runtime spec, and
    // run the container to completion. Exits the process with the container's
    // exit code; only returns on a setup failure.
    pub fn run(&self, user_args: &[String]) -> Result<()> {
        let image = crate::store::resolve(self)?;
        let mountpoints = crate::spec::mountpoints(self)?;
        let mount = crate::runtime::Mount::new(&image.layers, &mountpoints)?;

        // Run as the host uid:gid, mapped to container root, so files written
        // through the bind mount keep the user's ownership.
        let (uid, gid) = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        // Allocate a pty only for a fully interactive session: every standard
        // stream must be a terminal. If any is redirected, the container
        // inherits the real fds so programs detect that and emit plain output.
        let tty = std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal();

        let spec = crate::spec::build(self, &image.config, mount.root(), user_args, uid, gid, tty)?;
        let code = crate::runtime::run(spec, tty)?;

        // Drop the overlay mount before exiting so it is unmounted.
        drop(mount);
        std::process::exit(code);
    }
}

// Shallow-fetch the apps repo, dispatching on the URL scheme. ssh runs the
// streaming upload-pack over an ssh subprocess (like git, honouring
// $GIT_SSH_COMMAND); http(s) runs the stateless smart-HTTP RPC through the
// bundled ureq client (rustls TLS). Both request protocol v2.
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

// Shallow-fetch the apps repo and reset HEAD and the worktree to the fetched tip.
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

    // Resolve the fetched tip from the tracking ref the remote's default branch
    // mapped to.
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

    // Resolve the tree currently checked out (the old branch tip, if any) so the
    // checkout below diffs against it; a fresh clone has no branch yet, so the
    // diff is from an empty tree.
    let local_branch = format!("refs/heads/{branch}");
    let from_tree = match resolve_ref(&repo.git_dir, &local_branch) {
        Ok(old_tip) => {
            let old = parse_commit(&repo.odb.read(&old_tip)?.data)
                .context("reading the previous commit")?;
            Some(old.tree)
        }
        Err(_) => None,
    };

    // Fast-forward the local branch and point HEAD at it.
    write_symbolic_ref(&repo.git_dir, "HEAD", &local_branch).context("setting HEAD")?;
    write_ref(&repo.git_dir, &local_branch, &tip)
        .with_context(|| format!("updating {local_branch}"))?;

    // Diff the old tree against the fetched one so files removed upstream are
    // deleted from the worktree, not just additions and updates applied.
    let commit = parse_commit(&repo.odb.read(&tip)?.data).context("reading the fetched commit")?;
    checkout_between_trees(repo, from_tree.as_ref(), &commit.tree)
        .context("checking out the fetched files")?;
    Ok(())
}

// Sync the app definitions into the definition directory. On first run
// the apps repo is shallow-cloned; on later runs the existing clone is fetched
// and the worktree updated. With `--system` the target is the system directory
// (needs root), otherwise the per-user data directory.
pub fn sync(system: bool) -> Result<()> {
    let target = if system {
        PathBuf::from("/usr/share/climate/apps")
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
        // grit-lib has no remote abstraction, so read the URL from config.
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

        // Init the repo and record the remote so later pulls find the URL and
        // refspec; grit-lib has no one-shot clone to do this for us.
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
