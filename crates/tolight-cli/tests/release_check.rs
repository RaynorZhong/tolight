use std::{fs, process::Command};
use tempfile::tempdir;

fn check(source: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tolight"))
        .args([
            "--config",
            "/does-not-exist/secret-config.toml",
            "check-security",
            "--source",
        ])
        .arg(source)
        .env_remove("DATABASE_URL")
        .output()
        .unwrap()
}

#[test]
fn offline_security_check_needs_no_configuration_database_or_publication() {
    let directory = tempdir().unwrap();
    let public = directory.path().join("public");
    fs::create_dir(&public).unwrap();
    let output = check(&public);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::Value::Null
    );
    assert_eq!(fs::read_dir(&public).unwrap().count(), 0);
}

#[test]
fn offline_security_check_uses_the_same_contact_and_expiry_contract() {
    let directory = tempdir().unwrap();
    let public = directory.path().join("public");
    fs::create_dir_all(public.join(".well-known")).unwrap();
    fs::write(
        directory.path().join("SECURITY.md"),
        "# Fixture only\nContact: https://security-fixture.invalid/report\n",
    )
    .unwrap();
    let expires =
        tolight_core::datetime::rfc3339(time::OffsetDateTime::now_utc() + time::Duration::days(30));
    let text = format!(
        "Contact: https://security-fixture.invalid/report\nExpires: {expires}\nCanonical: https://security-fixture.invalid/.well-known/security.txt\n"
    );
    fs::write(public.join(".well-known/security.txt"), &text).unwrap();
    let output = check(&public);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        metadata["contacts"][0],
        "https://security-fixture.invalid/report"
    );
    assert_eq!(
        fs::read_to_string(public.join(".well-known/security.txt")).unwrap(),
        text
    );
    fs::write(
        directory.path().join("SECURITY.md"),
        "Contact: https://different-fixture.invalid/report\n",
    )
    .unwrap();
    assert!(!check(&public).status.success());
    fs::write(
        directory.path().join("SECURITY.md"),
        "# Local unpublished draft\n",
    )
    .unwrap();
    fs::remove_file(public.join(".well-known/security.txt")).unwrap();
    assert!(check(&public).status.success());
}
