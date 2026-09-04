use crate::config::{AppConfig, Capability, Entrypoint, Network};
use anyhow::Result;
use std::io::IsTerminal;

const GRAY: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

fn entrypoint_value(entrypoint: &Entrypoint) -> toml::Value {
    match entrypoint {
        Entrypoint::String(program) => toml::Value::from(program.clone()),
        Entrypoint::List(argv) => toml::Value::from(argv.clone()),
    }
}

fn network_value(network: &Network) -> toml::Value {
    let name = match network {
        Network::Full => "full",
        Network::None => "none",
        Network::Localhost => "localhost",
    };
    toml::Value::from(name)
}

// Capability names as a definition writes them, without the "CAP_" prefix.
fn capabilities_value(capabilities: &[Capability]) -> toml::Value {
    let names: Vec<String> = capabilities.iter().map(ToString::to_string).collect();
    toml::Value::from(names)
}

struct Printer {
    // Whether to colour the output. Off when redirected, so a file gets plain TOML.
    color: bool,
    // Whether to include the keys the file leaves out.
    defaults: bool,
}

impl Printer {
    // Print a line the file does not contain, greyed out to set it apart, or nothing at all when
    // defaults are hidden.
    fn defaulted(&self, line: String) {
        if !self.defaults {
            return;
        }
        if self.color {
            println!("{GRAY}{line}{RESET}");
        } else {
            println!("{line}");
        }
    }

    fn key(&self, key: &str, value: toml::Value, defaulted: bool) {
        let line = format!("{key} = {value}");
        if defaulted {
            self.defaulted(line);
        } else {
            println!("{line}");
        }
    }

    // A limit the file leaves out has no default value to show, so say what its absence means
    // in a comment instead of inventing one.
    fn limit(&self, key: &str, value: Option<toml::Value>, unset: &str) {
        match value {
            Some(value) => self.key(key, value, false),
            None => self.defaulted(format!("# {key} is unset: {unset}")),
        }
    }
}

pub fn show(app_name: &str, defaults: bool) -> Result<()> {
    let (config, raw) = AppConfig::load_with_raw(app_name)?;
    let printer = Printer {
        color: std::io::stdout().is_terminal(),
        defaults,
    };
    let table = |name: &str| raw.get(name).and_then(toml::Value::as_table);
    let absent = |name: &str, key: &str| !table(name).is_some_and(|table| table.contains_key(key));

    println!("[app]");
    printer.key("name", config.app.name.into(), false);
    printer.key("description", config.app.description.into(), false);
    printer.key("license", config.app.license.into(), false);

    println!("\n[image]");
    printer.key("reference", config.image.reference.into(), false);
    printer.key("pull", config.image.pull.into(), absent("image", "pull"));

    // Every key of [run] has a default, so with defaults hidden the section can end up empty. Print
    // its header only if something will follow.
    if defaults || table("run").is_some_and(|table| !table.is_empty()) {
        println!("\n[run]");
    }
    // No value stands for "no entrypoint set"; it means the image's own entrypoint runs,
    // so say that in a comment instead of inventing a default.
    match &config.run.entrypoint {
        Some(entrypoint) => printer.key("entrypoint", entrypoint_value(entrypoint), false),
        None => {
            printer.defaulted("# entrypoint is unset: the image's own entrypoint runs".to_string())
        }
    }
    printer.key("args", config.run.args.into(), absent("run", "args"));
    printer.key("env", config.run.env.into(), absent("run", "env"));
    printer.key(
        "mount-cwd",
        config.run.mount_cwd.into(),
        absent("run", "mount-cwd"),
    );
    printer.key("mount", config.run.mount.into(), absent("run", "mount"));
    printer.key(
        "network",
        network_value(&config.run.network),
        absent("run", "network"),
    );
    printer.key(
        "capabilities",
        capabilities_value(&config.run.capabilities),
        absent("run", "capabilities"),
    );

    // Like [run], the header only makes sense if a line follows it.
    if defaults || table("limits").is_some_and(|table| !table.is_empty()) {
        println!("\n[limits]");
    }
    printer.limit(
        "memory",
        config.limits.memory.map(Into::into),
        "the app may use as much memory as it likes",
    );
    printer.limit(
        "swap",
        config.limits.swap.map(Into::into),
        "the app may swap as much as it likes",
    );
    printer.limit(
        "memory-high",
        config.limits.memory_high.map(Into::into),
        "the app is never throttled, only killed at the memory limit",
    );
    printer.limit(
        "cpu",
        config.limits.cpu.map(Into::into),
        "the app may use every core",
    );
    printer.limit(
        "cpu-shares",
        config.limits.cpu_shares.map(Into::into),
        "the app competes for the CPU on equal terms",
    );
    printer.limit(
        "pids",
        config.limits.pids.map(Into::into),
        "the app may start as many processes as it likes",
    );
    Ok(())
}
