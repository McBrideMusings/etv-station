//! `docker/entrypoint.sh` reads `etv-station --list-folders` stdout into an
//! array, one channel folder per element. Nothing else in the suite runs the
//! built binary, so a notice or log line that reached stdout would split or
//! pollute that array with no test failing. This runs the real binary against
//! a station with one good and one broken channel.

use std::fs;
use std::process::Command;

const GOOD: &str = "number: 1\nname: good\nrule:\n  blocks:\n    - mode: all\n      order: manual\n      entries:\n        - kind: item\n          id: x\n          out_point: 30s\n          source:\n            kind: lavfi\n            params: testsrc\n";

#[test]
fn stdout_is_only_the_folder_paths_and_stderr_names_the_broken_channel() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("channels")).unwrap();
    fs::write(dir.path().join("channels/good.yaml"), GOOD).unwrap();
    // No `number:`, so the loader drops it and reports it on stderr.
    fs::write(
        dir.path().join("channels/broken.yaml"),
        GOOD.replace("number: 1\n", ""),
    )
    .unwrap();
    fs::write(
        dir.path().join("station.yaml"),
        "tz: \"UTC\"\noutput_base: out\nchannels:\n  - channels/*.yaml\nnormalization:\n  audio: {}\n  video: {}\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_etv-station"))
        .current_dir(dir.path())
        .env_remove("ETV_STATION_TZ")
        .env_remove("RUST_LOG")
        .args(["--config", "station.yaml", "--list-folders"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr = {stderr}");
    assert_eq!(stdout, "out/good\n", "stdout must carry only folder paths");
    assert!(
        stderr.contains("broken.yaml"),
        "stderr must name the dropped file, got: {stderr}"
    );
}
