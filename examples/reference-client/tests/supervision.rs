#![cfg(unix)]
use std::{path::Path, time::Duration};
fn wrapper() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/launchd-run.sh")
}

#[tokio::test]
async fn deterministic_identity_failures_are_successful_to_launchd_but_transient_errors_are_not() {
    let temp = tempfile::tempdir().unwrap();
    for (code, expected) in [(0, 0), (4, 0), (5, 0), (7, 7)] {
        let output = tokio::process::Command::new(wrapper())
            .arg(env!("CARGO_BIN_EXE_reference-fixture"))
            .arg(temp.path().join("config.toml"))
            .env("REFERENCE_FIXTURE_MODE", format!("exit:{code}"))
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(expected));
    }
}

#[tokio::test]
async fn supervisor_stop_forwards_term_and_waits_for_child_state_flush() {
    let temp = tempfile::tempdir().unwrap();
    let ready = temp.path().join("ready");
    let flushed = temp.path().join("flushed");
    let mut process = tokio::process::Command::new(wrapper())
        .arg(env!("CARGO_BIN_EXE_reference-fixture"))
        .arg(temp.path().join("config.toml"))
        .env("REFERENCE_FIXTURE_MODE", "flush")
        .env("REFERENCE_FIXTURE_READY", &ready)
        .env("REFERENCE_FIXTURE_FLUSHED", &flushed)
        .spawn()
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        tokio::process::Command::new("/bin/kill")
            .args(["-TERM", &process.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), process.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert_eq!(std::fs::read_to_string(flushed).unwrap(), "durable");
}
