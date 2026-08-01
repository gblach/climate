mod clean;
mod config;
mod pull;
mod runtime;
mod show;
mod spec;
mod store;

use anyhow::{Context, Result, bail};
use argp::FromArgs;
use config::{AppConfig, app_names};
use std::path::Path;

/// Run containerized command-line tools as if they were installed on your system.
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
    Show(ShowCmd),
    Sync(SyncCmd),
}

/// Free the disk space of images no app needs any more, and clean up after killed runs.
#[derive(FromArgs)]
#[argp(subcommand, name = "clean")]
struct CleanCmd {}

/// Create symlinks next to the climate binary so apps can be started by their own name (e.g. an
/// `ffmpeg` symlink that runs `climate run ffmpeg`).
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

/// List the apps you can run.
#[derive(FromArgs)]
#[argp(subcommand, name = "list")]
struct ListCmd {}

/// Download an app's image.
#[derive(FromArgs)]
#[argp(subcommand, name = "pull")]
struct PullCmd {
    /// download newer images for apps you already pulled
    #[argp(switch, short = 'u')]
    update: bool,
    /// app name (omit with --update)
    #[argp(positional)]
    app: Option<String>,
}

/// Run an app. Everything after the app name is passed on to the app itself, options like --help
/// included.
#[derive(FromArgs)]
#[argp(subcommand, name = "run")]
struct RunCmd {
    // A single greedy positional so leading-dash args (e.g. --pretty, --help) reach the app
    // without needing a `--` separator. argp prints no help for a greedy positional, so the
    // description below never reaches the user.
    /// app name, followed by the arguments passed on to it
    #[argp(positional, greedy)]
    cmd: Vec<String>,
}

/// Print an app's settings, including the ones left at their default.
#[derive(FromArgs)]
#[argp(subcommand, name = "show")]
struct ShowCmd {
    /// print only the settings the app sets itself
    #[argp(switch, short = 'n')]
    no_defaults: bool,
    /// app name
    #[argp(positional)]
    app: String,
}

/// Download or update the list of available apps.
#[derive(FromArgs)]
#[argp(subcommand, name = "sync")]
struct SyncCmd {
    /// install for all users, in /usr/share/climate/apps (needs root)
    #[argp(switch, short = 's')]
    system: bool,
}

// Create a symlink at `link` pointing to `target`. If it already points there nothing happens;
// anything else in the way is replaced only with `force`.
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
    // Point the symlinks at the bare binary name instead of a full path, so they keep working if
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
    // Pad the name column to the longest name so the descriptions line up.
    let width = configs
        .iter()
        .map(|cfg| cfg.app.name.len())
        .max()
        .unwrap_or(0);
    for cfg in configs {
        println!("{:<width$}  {}", cfg.app.name, cfg.app.description);
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

    // argv[0] is the name the binary was invoked under. Anything other than "climate" is a symlink
    // created by `climate link`, so run that app.
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

    // Not a user-facing command: the container runtime re-runs this binary with this argument from
    // inside the container to bring the loopback up.
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
        // No subcommand given: parse again with --help so argp produces the exact help text
        // `climate --help` would print.
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
        Command::Show(cmd) => show::show(&cmd.app, !cmd.no_defaults)?,
        Command::Sync(cmd) => config::sync(cmd.system)?,
    }
    Ok(())
}
