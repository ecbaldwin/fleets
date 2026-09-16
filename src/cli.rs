//! Command-line surface and dispatch. Flags mirror `ansible-inventory`.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{ArgGroup, Parser};

use crate::enabled::EnabledKinds;
use crate::output::{GroupPruning, build_host, build_list, build_toml, build_yaml};
use crate::parse::load_sources;
use crate::serialize::{to_json, to_toml, to_yaml};

#[derive(Parser, Debug)]
#[command(
    name = "fleets",
    version,
    about = "A fast, mostly ansible-inventory-compatible inventory tool",
    group(
        ArgGroup::new("action")
            .required(true)
            .multiple(false)
            .args(["list", "host"])
    )
)]
pub struct Args {
    /// Output all hosts info (the dynamic-inventory `--list` contract).
    #[arg(long)]
    pub list: bool,

    /// Output a single host's variables.
    #[arg(long, value_name = "HOST")]
    pub host: Option<String>,

    /// Inventory source path (repeatable).
    #[arg(short = 'i', long = "inventory", value_name = "PATH")]
    pub inventory: Vec<PathBuf>,

    /// Further limit selected hosts to an additional pattern.
    #[arg(short = 'l', long = "limit", value_name = "PATTERN")]
    pub limit: Option<String>,

    /// Use YAML format instead of the default JSON.
    #[arg(short = 'y', long, conflicts_with = "toml")]
    pub yaml: bool,

    /// Use TOML format instead of the default JSON.
    #[arg(long, conflicts_with = "yaml")]
    pub toml: bool,

    /// Represent the inventory in an export-optimized (round-trippable) way.
    #[arg(long)]
    pub export: bool,

    /// Preserve ansible-inventory's exact output shape, including dangling child groups.
    #[arg(long)]
    pub strict_compatibility: bool,
}

/// Run the CLI, returning the rendered output string.
pub fn run(args: &Args) -> anyhow::Result<String> {
    let sources = resolve_sources(args)?;
    // Exactly one action, mirroring ansible's "ONLY ONE" rule.
    match (args.list, args.host.is_some()) {
        (true, true) => bail!("--list and --host are mutually exclusive"),
        (false, false) => bail!("one of --list or --host is required"),
        _ => {}
    }

    if args.yaml && args.toml {
        bail!("--yaml and --toml are mutually exclusive");
    }

    let pruning = if args.list && strict_compatibility(args)? {
        GroupPruning::AnsibleCompatible
    } else {
        GroupPruning::GraphAware
    };

    let enabled = EnabledKinds::from_env();
    let inv = load_sources(&sources, enabled).context("loading inventory")?;
    let available: HashSet<String> = match &args.limit {
        Some(limit) => crate::pattern::select_hosts(&inv, limit),
        None => inv.hosts.keys().cloned().collect(),
    };

    // --host shares the same single-host var dict across all formats.
    if let Some(host) = &args.host {
        let value = build_host(&inv, host, args.export)
            .with_context(|| format!("host '{host}' not found in inventory"))?;
        return render(&value, args);
    }

    // --list: each format has its own tree shape.
    let value = if args.yaml {
        build_yaml(&inv, &available, args.export, pruning)
    } else if args.toml {
        build_toml(&inv, &available, args.export, pruning)
    } else {
        build_list(&inv, &available, args.export, pruning)
    };
    render(&value, args)
}

/// Resolve the fleets-specific strict-output switch. The flag and environment variable are
/// additive: either can enable compatibility mode, while a false environment value does not
/// override the command-line flag.
fn strict_compatibility(args: &Args) -> anyhow::Result<bool> {
    let from_env = match std::env::var("FLEETS_STRICT_COMPATIBILITY") {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => bail!(
                "invalid FLEETS_STRICT_COMPATIBILITY value {value:?} (expected true, false, 1, or 0)"
            ),
        },
        Err(std::env::VarError::NotPresent) => false,
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("FLEETS_STRICT_COMPATIBILITY must be valid UTF-8")
        }
    };
    Ok(args.strict_compatibility || from_env)
}

/// Resolve the inventory sources: explicit `-i/--inventory` if given, otherwise the
/// `ANSIBLE_INVENTORY` environment variable (comma-separated, like ansible's
/// `DEFAULT_HOST_LIST`). Unlike ansible, fleets does *not* fall back to `/etc/ansible/hosts`
/// when neither is set — it errors instead of scanning a system path.
fn resolve_sources(args: &Args) -> anyhow::Result<Vec<PathBuf>> {
    if !args.inventory.is_empty() {
        return Ok(args.inventory.clone());
    }
    if let Some(val) = std::env::var_os("ANSIBLE_INVENTORY") {
        let sources: Vec<PathBuf> = val
            .to_string_lossy()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if !sources.is_empty() {
            return Ok(sources);
        }
    }
    bail!("no inventory source given (use -i/--inventory or set ANSIBLE_INVENTORY)");
}

/// Serialize a built value in the requested format.
fn render(value: &serde_json::Value, args: &Args) -> anyhow::Result<String> {
    Ok(if args.yaml {
        to_yaml(value)?
    } else if args.toml {
        to_toml(value)?
    } else {
        to_json(value)
    })
}
