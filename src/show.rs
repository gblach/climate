use crate::config::{AppConfig, Entrypoint, Network};
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
    // No value stands for "no entrypoint set"; it means the image's own entrypoint runs, so say
    // that in a comment instead of inventing a default.
    match &config.run.entrypoint {
        Some(entrypoint) => printer.key("entrypoint", entrypoint_value(entrypoint), false),
        None => {
            printer.defaulted("# entrypoint is unset: the image's own entrypoint runs".to_string())
        }
    }
    printer.key("args", config.run.args.into(), absent("run", "args"));
    printer.key("env", config.run.env.into(), absent("run", "env"));
    printer.key("cwd", config.run.cwd.into(), absent("run", "cwd"));
    printer.key(
        "network",
        network_value(&config.run.network),
        absent("run", "network"),
    );
    Ok(())
}
