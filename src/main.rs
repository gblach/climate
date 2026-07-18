mod clean;
mod config;
mod pull;
mod runtime;
mod spec;
mod store;

use anyhow::{Context, Result, bail};
use argp::FromArgs;
use config::{AppConfig, app_names};
use std::path::Path;

/// Run containerized CLI apps in a self-contained engine, mounting the current user in.
#[derive(FromArgs)]
struct Cli {
    /// print the version and exit
    #[argp(switch)]
    version: bool,
    #[argp(subcommand)]
    command: Option<Command>,
}

#[derive(FromArgs)]
#[argp(subcommand)]
enum Command {
    Clean(CleanCmd),
    Link(LinkCmd),
    List(ListCmd),
    Pull(PullCmd),
    Run(RunCmd),
    Sync(SyncCmd),
}

/// Reclaim orphaned image data and the runtime files of killed containers.
#[derive(FromArgs)]
#[argp(subcommand, name = "clean")]
struct CleanCmd {}

/// Create symlinks next to the climate binary so apps can be invoked directly
/// (e.g. a `dbmate` symlink that runs `climate run dbmate`).
#[derive(FromArgs)]
#[argp(subcommand, name = "link")]
struct LinkCmd {
    /// link every available app
    #[argp(switch, short = 'a')]
    all: bool,
    /// replace existing files or symlinks
    #[argp(switch, short = 'f')]
    force: bool,
    /// apps to link
    #[argp(positional)]
    apps: Vec<String>,
}

/// List the available app definitions.
#[derive(FromArgs)]
#[argp(subcommand, name = "list")]
struct ListCmd {}

/// Fetch (pull) the image for an app.
#[derive(FromArgs)]
#[argp(subcommand, name = "pull")]
struct PullCmd {
    /// pull newer images, but only for apps already present locally
    #[argp(switch, short = 'u')]
    update: bool,
    /// app name (omit with --update)
    #[argp(positional)]
    app: Option<String>,
}

/// Run an app, forwarding any trailing arguments to it.
#[derive(FromArgs)]
#[argp(subcommand, name = "run")]
struct RunCmd {
    /// app name followed by arguments forwarded verbatim to the app.
    /// A single greedy positional so leading-dash args (e.g. --pretty,
    /// --help) reach the app without needing a `--` separator.
    #[argp(positional, greedy)]
    cmd: Vec<String>,
}

/// Download the app definitions from the apps Git repo.
#[derive(FromArgs)]
#[argp(subcommand, name = "sync")]
struct SyncCmd {
    /// write to the system directory (/usr/share/climate/apps) instead of the user one
    #[argp(switch, short = 's')]
    system: bool,
}

// Create a symlink at `link` pointing to `target`. A correct symlink is left
// untouched; any other existing entry is replaced only when `force` is set.
fn create_symlink(target: &Path, link: &Path, force: bool) -> Result<()> {
    if std::fs::read_link(link).is_ok_and(|existing| existing == target) {
        return Ok(());
    }

    if link.symlink_metadata().is_ok() {
        if !force {
            bail!("{} already exists (pass -f to replace)", link.display());
        }
        std::fs::remove_file(link)?;
    }

    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("creating symlink {}", link.display()))
}

fn link(cmd: &LinkCmd) -> Result<()> {
    let app_names = if cmd.all {
        app_names()
    } else if cmd.apps.is_empty() {
        bail!("link: specify one or more app names, or -a/--all");
    } else {
        cmd.apps.clone()
    };

    let exe = std::env::current_exe().context("resolving the climate executable")?;
    let dir = exe.parent().expect("executable path has a parent");
    let exe_name = exe.file_name().expect("executable path has a file name");
    // Point the symlinks at the relative binary name so they keep working if
    // the directory is moved or renamed.
    let target = Path::new(exe_name);

    for app_name in app_names {
        if AppConfig::load_or_warn(&app_name).is_none() {
            continue;
        }
        let link = dir.join(&app_name);
        create_symlink(target, &link, cmd.force)?;
        println!("linked {} -> {}", link.display(), target.display());
    }
    Ok(())
}

fn list() -> Result<()> {
    let configs: Vec<_> = app_names()
        .into_iter()
        .filter_map(|app_name| AppConfig::load_or_warn(&app_name))
        .collect();
    // Pad the name column to the widest name that has a description to align.
    let width = configs
        .iter()
        .filter(|cfg| !cfg.app.description.is_empty())
        .map(|cfg| cfg.app.name.len())
        .max()
        .unwrap_or(0);
    for cfg in configs {
        if cfg.app.description.is_empty() {
            println!("{}", cfg.app.name);
        } else {
            println!("{:<width$}  {}", cfg.app.name, cfg.app.description);
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        )
        .with_writer(std::io::stderr)
        .init();

    // When invoked through a symlink whose name isn't "climate" (e.g. a
    // `dbmate` symlink to the binary), dispatch as `climate run <name> ...`.
    let argv0 = std::env::args().next();
    let app_link = argv0.as_deref().and_then(|argv0| {
        let app_name = Path::new(argv0).file_name()?.to_str()?;
        (app_name != "climate").then(|| app_name.to_string())
    });

    if let Some(app_name) = app_link {
        let args: Vec<String> = std::env::args().skip(1).collect();
        AppConfig::load(&app_name)?.run(&args)?;
        return Ok(());
    }

    // A container hook re-invokes the binary with this internal argument; it runs
    // inside the container's network namespace, not as a user-facing command.
    if std::env::args().nth(1).as_deref() == Some(spec::LOOPBACK_HOOK_ARG) {
        spec::bring_loopback_up()?;
        return Ok(());
    }

    let cli: Cli = argp::parse_args_or_exit(argp::DEFAULT);
    if cli.version {
        println!("climate {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let Some(command) = cli.command else {
        // Reparse with --help to obtain the same help message that argp
        // prints for `climate --help`.
        let Err(argp::EarlyExit::Help(help)) = Cli::from_args(&["climate"], &["--help"]) else {
            unreachable!();
        };
        println!("{}", help.generate_default());
        return Ok(());
    };
    match command {
        Command::Clean(_) => clean::clean()?,
        Command::List(_) => list()?,
        Command::Link(cmd) => link(&cmd)?,
        Command::Pull(cmd) => pull::pull(cmd.update, cmd.app.as_deref())?,
        Command::Run(cmd) => {
            let (app_name, args) = cmd.cmd.split_first().context("run: missing app name")?;
            AppConfig::load(app_name)?.run(args)?;
        }
        Command::Sync(cmd) => config::sync(cmd.system)?,
    }
    Ok(())
}
