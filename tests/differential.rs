#![cfg(unix)]

//! Differential test harness: run `fleets` and the real `ansible-inventory` on each
//! fixture and assert their outputs match semantically (order-independent).
//!
//! Skips gracefully (with a printed note) if `ansible-inventory` is not on `PATH`, so the
//! suite still builds and the non-differential unit tests run in environments without it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;

#[derive(Clone, Copy)]
enum Format {
    Json,
    Yaml,
    Toml,
}

impl Format {
    /// The output-format flag for this format (empty for the default JSON).
    fn flag(self) -> &'static [&'static str] {
        match self {
            Format::Json => &[],
            Format::Yaml => &["-y"],
            Format::Toml => &["--toml"],
        }
    }

    fn parse(self, s: &str) -> Value {
        match self {
            Format::Json => serde_json::from_str(s).expect("parse json"),
            Format::Yaml => noyalib::from_str(s).expect("parse yaml"),
            Format::Toml => toml::from_str(s).expect("parse toml"),
        }
    }
}

/// `--list` is exercised in every format and with/without `--export`.
const FORMATS: &[Format] = &[Format::Json, Format::Yaml, Format::Toml];

/// Fixtures whose `--export` output ansible computes quirkily (a documented ansible
/// behavior we deliberately don't replicate). Their non-export modes are still compared.
const EXPORT_QUARANTINE: &[&str] = &["samename.yml"];

fn quarantined_for_export(fixture: &Path) -> bool {
    let name = fixture.file_name().and_then(|n| n.to_str()).unwrap_or("");
    EXPORT_QUARANTINE.contains(&name)
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn ansible_env(cmd: &mut Command) {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ansible.cfg");
    cmd.env("ANSIBLE_CONFIG", config)
        .env("ANSIBLE_NOCOLOR", "1")
        .env("LC_ALL", "en_US.UTF-8")
        .env("LANG", "en_US.UTF-8")
        .env("ANSIBLE_TRANSFORM_INVALID_GROUP_CHARS", "silently")
        .env("ANSIBLE_DEPRECATION_WARNINGS", "false")
        .env("ANSIBLE_LOCALHOST_WARNING", "false")
        .env("ANSIBLE_INVENTORY_UNPARSED_WARNING", "false")
        // ansible's DEFAULT plugin order. `auto` must precede `ini`: the INI plugin is
        // permissive enough to "claim" a .json/.yaml file and turn stray lines (e.g. `{`)
        // into phantom hosts. The default order lets `auto` dispatch by extension first.
        .env(
            "ANSIBLE_INVENTORY_ENABLED",
            "host_list,script,auto,yaml,ini,toml",
        );
}

fn ansible_available() -> bool {
    let available = Command::new("ansible-inventory")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        available || std::env::var_os("FLEETS_REQUIRE_ANSIBLE").is_none(),
        "ansible-inventory is required when FLEETS_REQUIRE_ANSIBLE is set"
    );
    available
}

/// Ansible cannot represent these inventories as TOML. Keep the list exact so a new oracle
/// failure does not silently reduce compatibility coverage.
fn expected_ansible_failure(source: &Path, flags: &[&str]) -> bool {
    let name = source.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(
        (name, flags),
        ("cyclic.yml", ["--list", "--toml"])
            | ("cyclic.yml", ["--list", "--toml", "--export"])
            | ("ranges.ini", ["--list", "--toml"])
            | ("ranges.ini", ["--list", "--toml", "--export"])
            | ("ranges.ini", ["--host", "coerce-host", "--toml"])
            | ("vartypes.yml", ["--list", "--toml"])
            | ("vartypes.yml", ["--list", "--toml", "--export"])
            | ("vartypes.yml", ["--host", "node1", "--toml"])
    )
}

/// Run ansible-inventory; returns `None` if ansible itself errors (e.g. `--toml` on a
/// cyclic inventory, which ansible cannot serialize). We only compare where ansible
/// produced valid output — its own failures are not part of the compatibility spec.
fn run_ansible(source: &Path, flags: &[&str]) -> Option<String> {
    let mut cmd = Command::new("ansible-inventory");
    ansible_env(&mut cmd);
    cmd.args(flags).arg("-i").arg(source);
    let out = cmd.output().expect("spawn ansible-inventory");
    if !out.status.success() && expected_ansible_failure(source, flags) {
        return None;
    }
    assert!(
        out.status.success(),
        "unexpected ansible-inventory failure on {} {:?}: {}",
        source.display(),
        flags,
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8(out.stdout).expect("ansible stdout utf8"))
}

fn run_fleets(source: &Path, flags: &[&str]) -> String {
    let mut cmd = Command::cargo_bin("fleets").expect("build fleets binary");
    // fleets now reads ANSIBLE_INVENTORY / ANSIBLE_INVENTORY_ENABLED. Pin them to the same
    // values the ansible side uses (and clear the default-source var, since we always pass
    // an explicit -i) so a developer's shell environment can't make the two sides diverge.
    cmd.env_remove("ANSIBLE_INVENTORY")
        .env_remove("FLEETS_STRICT_COMPATIBILITY")
        .env(
            "ANSIBLE_INVENTORY_ENABLED",
            "host_list,script,auto,yaml,ini,toml",
        );
    // Differential cases assert exact ansible output. Fleets defaults to graph-aware group
    // pruning, so opt back into ansible's shallow pruning for this compatibility suite.
    cmd.arg("--strict-compatibility")
        .args(flags)
        .arg("-i")
        .arg(source);
    let out = cmd.output().expect("spawn fleets");
    assert!(
        out.status.success(),
        "fleets failed on {source:?} {flags:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("fleets stdout utf8")
}

/// Recursively normalize: sort object keys (BTreeMap) and sort any all-string array
/// (covers `hosts`/`children`, which ansible emits in non-deterministic order).
fn normalize(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), normalize(v)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(a) => {
            let mut items: Vec<Value> = a.iter().map(normalize).collect();
            if items.iter().all(|x| x.is_string()) {
                items.sort_by(|x, y| x.as_str().cmp(&y.as_str()));
            }
            Value::Array(items)
        }
        other => other.clone(),
    }
}

/// Compare one (fixture × flags) case. Returns `false` (skipped) if ansible errored.
fn assert_match(fixture: &Path, flags: &[&str], fmt: Format) -> bool {
    let Some(ans_out) = run_ansible(fixture, flags) else {
        eprintln!("SKIP (ansible errored): {} {:?}", fixture.display(), flags);
        return false;
    };
    let ans = fmt.parse(&ans_out);
    let fleets = fmt.parse(&run_fleets(fixture, flags));
    let (na, nf) = (normalize(&ans), normalize(&fleets));
    assert_eq!(
        na,
        nf,
        "\nDIVERGENCE on {} {:?}\n--- ansible ---\n{}\n--- fleets ---\n{}\n",
        fixture.display(),
        flags,
        serde_json::to_string_pretty(&na).unwrap(),
        serde_json::to_string_pretty(&nf).unwrap(),
    );
    true
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn discover_fixtures() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(fixtures_dir()).expect("read fixtures dir") {
        let path = entry.expect("dir entry").path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        // Files with an inventory extension, directory inventories, and executable scripts.
        if path.is_dir()
            || matches!(ext, "yml" | "yaml" | "json" | "toml" | "ini")
            || is_executable(&path)
        {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Host names present in a fixture, via fleets' own JSON `_meta`.
fn hosts_of(fixture: &Path) -> Vec<String> {
    let v: Value = serde_json::from_str(&run_fleets(fixture, &["--list"])).unwrap();
    v["_meta"]["hostvars"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Group names present in a fixture (top-level keys minus `_meta`).
fn groups_of(fixture: &Path) -> Vec<String> {
    let v: Value = serde_json::from_str(&run_fleets(fixture, &["--list"])).unwrap();
    v.as_object()
        .map(|m| m.keys().filter(|k| *k != "_meta").cloned().collect())
        .unwrap_or_default()
}

#[test]
fn differential_list() {
    if !ansible_available() {
        eprintln!("SKIP: ansible-inventory not found on PATH");
        return;
    }
    let fixtures = discover_fixtures();
    assert!(!fixtures.is_empty(), "no fixtures found");

    let mut checked = 0;
    let mut skipped = 0;
    for fixture in &fixtures {
        for &fmt in FORMATS {
            for export in [false, true] {
                if export && quarantined_for_export(fixture) {
                    continue;
                }
                let mut flags: Vec<&str> = vec!["--list"];
                flags.extend_from_slice(fmt.flag());
                if export {
                    flags.push("--export");
                }
                if assert_match(fixture, &flags, fmt) {
                    checked += 1;
                } else {
                    skipped += 1;
                }
            }
        }
    }
    eprintln!("differential_list: {checked} passed, {skipped} expected oracle failures skipped");
}

#[test]
fn differential_limit() {
    if !ansible_available() {
        eprintln!("SKIP: ansible-inventory not found on PATH");
        return;
    }
    let mut checked = 0;
    for fixture in &discover_fixtures() {
        let hosts = hosts_of(fixture);
        let groups = groups_of(fixture);

        // A representative set of limit expressions exercised per fixture.
        let mut limits: Vec<String> = vec!["all".into(), "*".into()];
        if let Some(g) = groups.iter().find(|g| *g != "all" && *g != "ungrouped") {
            limits.push(g.clone());
            limits.push(format!("&{g}")); // intersection with implied 'all'
        }
        if let Some(h) = hosts.first() {
            limits.push(h.clone());
            limits.push(format!("all:!{h}")); // exclusion
        }
        if !hosts.is_empty() {
            limits.push("~.*".into()); // regex matching everything
        }

        for limit in &limits {
            if assert_match(fixture, &["--list", "-l", limit], Format::Json) {
                checked += 1;
            }
        }
    }
    eprintln!("differential_limit: {checked} (fixture × limit) comparisons passed");
}

#[test]
fn differential_host() {
    if !ansible_available() {
        eprintln!("SKIP: ansible-inventory not found on PATH");
        return;
    }
    let mut checked = 0;
    let mut skipped = 0;
    for fixture in &discover_fixtures() {
        for host in hosts_of(fixture) {
            for &fmt in FORMATS {
                let mut flags: Vec<&str> = vec!["--host", &host];
                flags.extend_from_slice(fmt.flag());
                if assert_match(fixture, &flags, fmt) {
                    checked += 1;
                } else {
                    skipped += 1;
                }
            }
        }
    }
    eprintln!("differential_host: {checked} passed, {skipped} expected oracle failures skipped");
}
