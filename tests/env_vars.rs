//! Integration tests for the ansible-compatible environment variables fleets honors:
//! `ANSIBLE_INVENTORY` (default source), `ANSIBLE_INVENTORY_ENABLED` (enabled source kinds),
//! and `FLEETS_STRICT_COMPATIBILITY` (output pruning). Each case sets the environment on the
//! child process, so it is isolated from the test runner's own environment.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("fleets-{label}-{}-{sequence}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create temporary test directory");
    path
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Build a fleets command with a clean slate for the two variables we care about, so the
/// test runner's own environment can't leak in. Individual tests re-set what they need.
fn fleets() -> Command {
    let mut cmd = Command::cargo_bin("fleets").expect("build fleets binary");
    cmd.env_remove("ANSIBLE_INVENTORY")
        .env_remove("ANSIBLE_INVENTORY_ENABLED")
        .env_remove("FLEETS_STRICT_COMPATIBILITY");
    cmd
}

fn list_json(cmd: &mut Command) -> Value {
    let out = cmd.arg("--list").output().expect("spawn fleets");
    assert!(
        out.status.success(),
        "fleets failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("parse fleets json")
}

/// Names in a group's `hosts` list, or empty if the group is absent.
fn group_hosts<'a>(inv: &'a Value, group: &str) -> Vec<&'a str> {
    inv.get(group)
        .and_then(|g| g.get("hosts"))
        .and_then(|h| h.as_array())
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

// --- ANSIBLE_INVENTORY (default source) ------------------------------------------------

#[test]
fn ansible_inventory_env_supplies_default_source() {
    let basic = fixtures_dir().join("basic.yml");
    let inv = list_json(fleets().env("ANSIBLE_INVENTORY", &basic));
    // basic.yml's webservers group proves the source was loaded.
    assert!(group_hosts(&inv, "webservers").contains(&"web01"));
}

#[test]
fn ansible_inventory_env_is_comma_split() {
    let basic = fixtures_dir().join("basic.yml");
    let ranges = fixtures_dir().join("ranges.ini");
    let joined = format!("{},{}", basic.display(), ranges.display());
    let inv = list_json(fleets().env("ANSIBLE_INVENTORY", joined));
    // A host from each of the two comma-joined sources.
    assert!(group_hosts(&inv, "webservers").contains(&"web01"));
    assert!(
        inv.get("_meta").is_some(),
        "merged inventory should emit _meta"
    );
}

#[test]
fn explicit_inventory_overrides_env() {
    let basic = fixtures_dir().join("basic.yml");
    // env points somewhere bogus; -i must win and the bogus path must never be read.
    let inv = list_json(
        fleets()
            .env("ANSIBLE_INVENTORY", "/no/such/path/nope.yml")
            .arg("-i")
            .arg(&basic),
    );
    assert!(group_hosts(&inv, "webservers").contains(&"web01"));
}

#[test]
fn no_source_and_no_env_errors_helpfully() {
    let out = fleets().arg("--list").output().expect("spawn fleets");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ANSIBLE_INVENTORY"),
        "error should mention the env fallback, got: {stderr}"
    );
}

// --- ANSIBLE_INVENTORY_ENABLED (enabled source kinds) ----------------------------------

#[test]
fn script_kind_runs_when_enabled() {
    let script = fixtures_dir().join("dynamic.sh");
    let inv = list_json(
        fleets()
            .env("ANSIBLE_INVENTORY_ENABLED", "script,auto,yaml,ini,toml")
            .arg("-i")
            .arg(&script),
    );
    assert!(group_hosts(&inv, "appservers").contains(&"app01"));
}

#[test]
fn script_kind_skipped_when_disabled() {
    let script = fixtures_dir().join("dynamic.sh");
    // `script` omitted: the executable must not run, leaving an empty inventory.
    let inv = list_json(
        fleets()
            .env("ANSIBLE_INVENTORY_ENABLED", "yaml,ini,toml,auto")
            .arg("-i")
            .arg(&script),
    );
    assert!(
        group_hosts(&inv, "appservers").is_empty(),
        "appservers should be absent when script is disabled: {inv}"
    );
    assert!(
        inv.get("_meta")
            .and_then(|m| m.get("hostvars"))
            .and_then(Value::as_object)
            .is_none_or(|h| h.is_empty()),
        "no hosts should be loaded when script is disabled: {inv}"
    );
}

#[test]
fn auto_kind_runs_constructed_when_enabled() {
    let dir = fixtures_dir().join("constructed_groups");
    let inv = list_json(
        fleets()
            .env("ANSIBLE_INVENTORY_ENABLED", "auto,yaml,ini,toml,script")
            .arg("-i")
            .arg(&dir),
    );
    // The constructed pass puts mon1 (prometheus is defined) into a `prometheus` group.
    assert!(
        group_hosts(&inv, "prometheus").contains(&"mon1"),
        "constructed should synthesize prometheus->mon1: {inv}"
    );
}

#[test]
fn auto_kind_disabled_skips_constructed() {
    let dir = fixtures_dir().join("constructed_groups");
    // `auto` omitted: the constructed file is parsed as an ordinary document, so the
    // synthesized membership (prometheus -> mon1) must not appear.
    let inv = list_json(
        fleets()
            .env("ANSIBLE_INVENTORY_ENABLED", "yaml,ini,toml,script")
            .arg("-i")
            .arg(&dir),
    );
    assert!(
        !group_hosts(&inv, "prometheus").contains(&"mon1"),
        "constructed must not run when auto is disabled: {inv}"
    );
}

// --- FLEETS_STRICT_COMPATIBILITY -------------------------------------------------------

fn all_children(inv: &Value) -> Vec<&str> {
    inv.get("all")
        .and_then(|all| all.get("children"))
        .and_then(Value::as_array)
        .map(|children| children.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[test]
fn graph_aware_pruning_is_the_default() {
    let edgecases = fixtures_dir().join("edgecases.yml");
    let inv = list_json(fleets().arg("-i").arg(edgecases));
    assert_eq!(all_children(&inv), vec!["populated"]);
}

#[test]
fn strict_compatibility_flag_restores_shallow_pruning() {
    let edgecases = fixtures_dir().join("edgecases.yml");
    let inv = list_json(
        fleets()
            .arg("--strict-compatibility")
            .arg("-i")
            .arg(edgecases),
    );
    assert_eq!(
        all_children(&inv),
        vec!["ungrouped", "emptygroup", "haschild", "populated"]
    );
}

#[test]
fn strict_compatibility_env_accepts_standard_booleans() {
    let edgecases = fixtures_dir().join("edgecases.yml");
    for value in ["true", "TRUE", "1"] {
        let inv = list_json(
            fleets()
                .env("FLEETS_STRICT_COMPATIBILITY", value)
                .arg("-i")
                .arg(&edgecases),
        );
        assert!(all_children(&inv).contains(&"emptygroup"), "value={value}");
    }
    for value in ["false", "FALSE", "0"] {
        let inv = list_json(
            fleets()
                .env("FLEETS_STRICT_COMPATIBILITY", value)
                .arg("-i")
                .arg(&edgecases),
        );
        assert!(!all_children(&inv).contains(&"emptygroup"), "value={value}");
    }
}

#[test]
fn strict_flag_overrides_false_environment_value() {
    let edgecases = fixtures_dir().join("edgecases.yml");
    let inv = list_json(
        fleets()
            .env("FLEETS_STRICT_COMPATIBILITY", "false")
            .arg("--strict-compatibility")
            .arg("-i")
            .arg(edgecases),
    );
    assert!(all_children(&inv).contains(&"emptygroup"));
}

#[test]
fn invalid_strict_compatibility_env_is_an_error() {
    let edgecases = fixtures_dir().join("edgecases.yml");
    let out = fleets()
        .env("FLEETS_STRICT_COMPATIBILITY", "sometimes")
        .arg("--list")
        .arg("-i")
        .arg(edgecases)
        .output()
        .expect("spawn fleets");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected true, false, 1, or 0"));
}

#[cfg(unix)]
#[test]
fn bare_relative_script_path_executes_from_current_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("relative-script");
    let script = dir.join("inventory-script");
    fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' '{\"all\":{\"hosts\":[\"relative-ok\"]},\"_meta\":{\"hostvars\":{}}}'\n",
    )
    .expect("write script");
    let mut permissions = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("make script executable");

    let inv = list_json(
        fleets()
            .current_dir(&dir)
            .env("PATH", "/usr/bin:/bin")
            .arg("-i")
            .arg("inventory-script"),
    );
    assert!(group_hosts(&inv, "ungrouped").contains(&"relative-ok"));
    fs::remove_dir_all(dir).expect("remove temporary test directory");
}

#[test]
fn host_name_cannot_escape_host_vars_directory() {
    let dir = temp_dir("vars-containment");
    fs::create_dir(dir.join("host_vars")).expect("create host_vars");
    fs::write(
        dir.join("inventory.yml"),
        "all:\n  hosts:\n    \"../secrets\":\n",
    )
    .expect("write inventory");
    fs::write(dir.join("secrets.yml"), "leaked_value: proof-only\n").expect("write sentinel");

    let inv = list_json(fleets().arg("-i").arg(dir.join("inventory.yml")));
    assert!(
        inv["_meta"]["hostvars"]["../secrets"]
            .get("leaked_value")
            .is_none()
    );
    fs::remove_dir_all(dir).expect("remove temporary test directory");
}

#[cfg(unix)]
#[test]
fn directory_symlink_cycle_is_rejected() {
    use std::os::unix::fs::symlink;

    let dir = temp_dir("directory-cycle");
    let child = dir.join("child");
    fs::create_dir(&child).expect("create child directory");
    symlink(&dir, child.join("back")).expect("create directory symlink");

    let out = fleets()
        .arg("--list")
        .arg("-i")
        .arg(&dir)
        .output()
        .expect("spawn fleets");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("symlink cycle"));
    fs::remove_dir_all(dir).expect("remove temporary test directory");
}

#[test]
fn version_flag_reports_package_version() {
    let out = fleets().arg("--version").output().expect("spawn fleets");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "fleets 0.1.0");
}

#[cfg(unix)]
#[test]
fn closed_stdout_pipe_exits_successfully() {
    let inventory = fixtures_dir().join("basic.yml");
    let mut child = fleets()
        .arg("--list")
        .arg("-i")
        .arg(inventory)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn fleets");
    drop(child.stdout.take());
    let status = child.wait().expect("wait for fleets");
    assert!(
        status.success(),
        "broken stdout pipe should not be an error"
    );
}
