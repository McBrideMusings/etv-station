//! `etv-station --check-config` is what `admin deploy`'s preflight trusts to
//! refuse a broken config, so its exit code and the paths it names on stderr
//! are the contract. These tests run the real binary against a tempdir station.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const VALID: &str = "rule:\n\
                     \x20 blocks:\n\
                     \x20   - mode: all\n\
                     \x20     order: manual\n\
                     \x20     entries:\n\
                     \x20       - kind: item\n\
                     \x20         id: x\n\
                     \x20         out_point: 30s\n\
                     \x20         source:\n\
                     \x20           kind: lavfi\n\
                     \x20           params: testsrc\n";

/// Parses as YAML, fails as a channel config: no `rule:`.
const INVALID: &str = "this_is_not_a_channel: true\n";

/// Writes `station.yaml` plus one `channels/<folder>/channel.yaml` per entry
/// (`true` = valid body, `false` = broken body). Returns the station path and
/// each channel file's path, in order.
fn write_station(dir: &Path, channels: &[(&str, bool)]) -> (PathBuf, Vec<PathBuf>) {
    let station_path = dir.join("station.yaml");
    fs::write(
        &station_path,
        "tz: \"UTC\"\n\
         output_base: out\n\
         channels:\n\
         \x20 - channels/*/channel.yaml\n\
         normalization:\n\
         \x20 audio: {}\n\
         \x20 video: {}\n",
    )
    .unwrap();

    let mut paths = Vec::new();
    for (index, (folder, valid)) in channels.iter().enumerate() {
        let folder_dir = dir.join("channels").join(folder);
        fs::create_dir_all(&folder_dir).unwrap();
        let body = if *valid {
            format!("number: {}\nname: {folder}\n{VALID}", index + 1)
        } else {
            INVALID.to_string()
        };
        let path = folder_dir.join("channel.yaml");
        fs::write(&path, body).unwrap();
        paths.push(path);
    }
    (station_path, paths)
}

fn check_config(station: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_etv-station"))
        .arg("--config")
        .arg(station)
        .arg("--check-config")
        .env_remove("ETV_ACCEL")
        .env_remove("ETV_PORT")
        .output()
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn all_channels_valid_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let (station, _) = write_station(dir.path(), &[("001-a", true), ("002-b", true)]);

    let out = check_config(&station);

    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("2 channel(s) load"), "stdout: {stdout}");
}

#[test]
fn one_invalid_channel_exits_one_and_names_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let (station, paths) = write_station(dir.path(), &[("001-a", true), ("002-bad", false)]);

    let out = check_config(&station);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "stderr: {err}");
    assert!(
        err.contains(&paths[1].display().to_string()),
        "stderr: {err}"
    );
    assert!(
        !err.contains(&paths[0].display().to_string()),
        "the valid channel must not be reported as dropped; stderr: {err}"
    );
}

#[test]
fn no_valid_channel_exits_one_and_names_every_path() {
    let dir = tempfile::tempdir().unwrap();
    let (station, paths) = write_station(dir.path(), &[("001-bad", false), ("002-bad", false)]);

    let out = check_config(&station);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "stderr: {err}");
    for path in &paths {
        assert!(err.contains(&path.display().to_string()), "stderr: {err}");
    }
}
