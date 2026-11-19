use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use tolight_coordinator::publisher::{self, PublishError};
use tolight_core::config::Config;
use tolight_core::datetime::TimestampExt;

struct Fixture {
    _temporary: tempfile::TempDir,
    config: Config,
    source: PathBuf,
    docs: PathBuf,
    examples: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let source = temporary.path().join("source");
        let docs = temporary.path().join("documents");
        let examples = temporary.path().join("examples");
        for path in [&source, &docs, &examples] {
            fs::create_dir(path).unwrap();
        }
        for name in ["index.html", "app.js", "style.css", "robots.txt"]
            .into_iter()
            .chain(publisher::BRAND_ASSETS)
        {
            fs::copy(repo.join("public").join(name), source.join(name)).unwrap();
        }
        for name in [
            "join.md",
            "participant-spec.md",
            "signing-vectors.json",
            "protocol.md",
            "rules.md",
        ] {
            fs::copy(repo.join("docs").join(name), docs.join(name)).unwrap();
        }
        fs::copy(
            repo.join("examples/python/minimal_client.py"),
            examples.join("minimal_client.py"),
        )
        .unwrap();
        let mut config = Config::default();
        config.public.dir = temporary.path().join("public");
        fs::create_dir_all(config.public.dir.join("data/chunks")).unwrap();
        fs::write(
            config.public.dir.join("data/manifest.json"),
            b"unchanged data",
        )
        .unwrap();
        fs::write(
            config.public.dir.join("data/chunks/sentinel.tmp"),
            b"not an asset temporary",
        )
        .unwrap();
        Self {
            _temporary: temporary,
            config,
            source,
            docs,
            examples,
        }
    }
    async fn install(&self) -> Result<(), PublishError> {
        publisher::install_assets_bundle(
            &self.config,
            &self.source,
            Some(&self.docs),
            Some(&self.examples),
        )
        .await
    }
    fn security(&self) -> (PathBuf, Vec<u8>) {
        let path = self.source.join(".well-known/security.txt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let expires = tolight_core::datetime::utc_seconds(
            time::OffsetDateTime::now_utc() + time::Duration::days(180),
        );
        let bytes = format!("# Synthetic local test channel; no mail is sent.\nContact: mailto:reports@tolight.test\nExpires: {expires}\nCanonical: https://tolight.test/.well-known/security.txt\nPreferred-Languages: zh, en\n").into_bytes();
        fs::write(&path, &bytes).unwrap();
        fs::write(self.source.parent().unwrap().join("SECURITY.md"), "# Synthetic policy\nOnly a test fixture, not a project contact.\n\n```text\nContact: mailto:reports@tolight.test\n```\n").unwrap();
        (path, bytes)
    }
}

#[tokio::test]
async fn security_report_assets_are_optional_exact_and_withdraw_stale_channels_without_touching_data()
 {
    let fixture = Fixture::new();
    assert!(
        publisher::validate_security_assets(&fixture.source)
            .unwrap()
            .is_none()
    );
    fixture.install().await.unwrap();
    assert!(
        !fixture
            .config
            .public
            .dir
            .join(".well-known/security.txt")
            .exists()
    );
    let data = tree(&fixture.config.public.dir.join("data"));
    let (source, bytes) = fixture.security();
    let metadata = publisher::validate_security_assets(&fixture.source)
        .unwrap()
        .unwrap();
    assert_eq!(metadata.contacts, ["mailto:reports@tolight.test"]);
    assert_eq!(
        metadata.canonical,
        ["https://tolight.test/.well-known/security.txt"]
    );
    assert!(metadata.expires > time::OffsetDateTime::now_utc());
    fixture.install().await.unwrap();
    let installed = fixture.config.public.dir.join(".well-known/security.txt");
    assert_eq!(fs::read(&installed).unwrap(), bytes);
    assert_eq!(tree(&fixture.config.public.dir.join("data")), data);
    let sentinel = installed.parent().unwrap().join("unrelated.txt");
    fs::write(&sentinel, "keep").unwrap();
    fs::write(
        installed.with_file_name("security.txt.tmp"),
        "old temporary",
    )
    .unwrap();
    fs::remove_file(source).unwrap();
    fixture.install().await.unwrap();
    assert!(!installed.exists());
    assert!(!installed.with_file_name("security.txt.tmp").exists());
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
    assert_eq!(tree(&fixture.config.public.dir.join("data")), data);
}

#[tokio::test]
async fn security_format_expiry_and_policy_disagreement_fail_before_any_asset_changes() {
    let fixture = Fixture::new();
    let (source, valid) = fixture.security();
    fixture.install().await.unwrap();
    let original = tree(&fixture.config.public.dir);
    let expires = (time::OffsetDateTime::now_utc() + time::Duration::days(30)).to_rfc3339();
    for invalid in [
        format!("Expires: {expires}\n"),
        "Contact: mailto:reports@tolight.test\n".into(),
        "Contact: mailto:reports@tolight.test\nExpires: 2000-01-01T00:00:00Z\n".into(),
        format!("Contact: mailto:reports@tolight.test\nExpires: {expires}\nExpires: {expires}\n"),
        format!("Contact: http://tolight.test/report\nExpires: {expires}\n"),
        format!("Contact: https://user:password@tolight.test/report\nExpires: {expires}\n"),
        format!("Contact: reports@tolight.test\nExpires: {expires}\n"),
        format!(
            "Contact: mailto:reports@tolight.test\nExpires: {expires}\nCanonical: https://tolight.test/elsewhere\n"
        ),
        format!("Contact: mailto:reports@tolight.test\nExpires: {expires}"),
        format!(
            "Contact: mailto:reports@tolight.test\nExpires: {expires}\nPreferred-Languages: en\nPreferred-Languages: zh\n"
        ),
        "-----BEGIN PGP SIGNED MESSAGE-----\n".into(),
    ] {
        fs::write(&source, invalid).unwrap();
        assert!(fixture.install().await.is_err());
        assert_eq!(tree(&fixture.config.public.dir), original);
    }
    fs::write(&source, valid).unwrap();
    let policy = fixture.source.parent().unwrap().join("SECURITY.md");
    fs::remove_file(&policy).unwrap();
    assert!(
        fixture
            .install()
            .await
            .unwrap_err()
            .to_string()
            .contains("SECURITY.md")
    );
    assert_eq!(tree(&fixture.config.public.dir), original);
    fs::write(
        policy,
        "# Policy\nContact: mailto:someone-else@tolight.test\n",
    )
    .unwrap();
    assert!(
        fixture
            .install()
            .await
            .unwrap_err()
            .to_string()
            .contains("SECURITY.md")
    );
    assert_eq!(tree(&fixture.config.public.dir), original);
}

#[cfg(unix)]
#[tokio::test]
async fn security_inputs_and_outputs_cannot_follow_symlink_aliases() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let (source, _) = fixture.security();
    let data = tree(&fixture.config.public.dir.join("data"));
    let output = fixture.config.public.dir.join(".well-known");
    symlink(fixture.config.public.dir.join("data"), &output).unwrap();
    assert!(fixture.install().await.is_err());
    fs::remove_file(&output).unwrap();
    fs::create_dir(&output).unwrap();
    symlink(
        fixture.config.public.dir.join("data/manifest.json"),
        output.join("security.txt"),
    )
    .unwrap();
    assert!(fixture.install().await.is_err());
    fs::remove_file(output.join("security.txt")).unwrap();
    let real = source.with_file_name("real.txt");
    fs::rename(&source, &real).unwrap();
    symlink(&real, &source).unwrap();
    assert!(publisher::validate_security_assets(&fixture.source).is_err());
    assert!(fixture.install().await.is_err());
    assert_eq!(tree(&fixture.config.public.dir.join("data")), data);
}
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, path: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &entry.path(), output);
            } else {
                output.insert(
                    entry.path().strip_prefix(root).unwrap().to_owned(),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut output = BTreeMap::new();
    walk(root, root, &mut output);
    output
}
#[tokio::test]
async fn complete_real_document_bundle_is_exact_safe_idempotent_and_leaves_data_untouched() {
    let fixture = Fixture::new();
    let data = tree(&fixture.config.public.dir.join("data"));
    fixture.install().await.unwrap();
    for name in publisher::BRAND_ASSETS {
        assert_eq!(
            fs::read(fixture.source.join(name)).unwrap(),
            fs::read(fixture.config.public.dir.join(name)).unwrap()
        );
    }
    for (input, output) in [
        (fixture.docs.join("join.md"), "join.md"),
        (
            fixture.docs.join("participant-spec.md"),
            "docs/participant-spec.md",
        ),
        (fixture.docs.join("protocol.md"), "docs/protocol.md"),
        (fixture.docs.join("rules.md"), "docs/rules.md"),
        (
            fixture.docs.join("signing-vectors.json"),
            "docs/signing-vectors.json",
        ),
        (
            fixture.examples.join("minimal_client.py"),
            "examples/minimal_client.py",
        ),
    ] {
        assert_eq!(
            fs::read(input).unwrap(),
            fs::read(fixture.config.public.dir.join(output)).unwrap()
        );
    }
    for name in ["participant-spec", "protocol", "rules"] {
        let html = fs::read_to_string(fixture.config.public.dir.join(format!("docs/{name}.html")))
            .unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<main>"));
        for forbidden in [
            "<script",
            "<iframe",
            "<img",
            "<style",
            "src=",
            "href=\"http",
            "href=\"//",
        ] {
            assert!(!html.contains(forbidden), "{name} contains {forbidden}");
        }
        for href in html.split("href=\"").skip(1) {
            assert!(href.starts_with('/') || href.starts_with('#'));
        }
    }
    let index = fs::read_to_string(fixture.config.public.dir.join("index.html")).unwrap();
    assert_eq!(index.matches("href=\"/join.md\"").count(), 1);
    assert!(
        !fs::read_to_string(fixture.source.join("index.html"))
            .unwrap()
            .contains("href=\"/join.md\"")
    );
    assert_eq!(tree(&fixture.config.public.dir.join("data")), data);
    let before = tree(&fixture.config.public.dir);
    fixture.install().await.unwrap();
    assert_eq!(tree(&fixture.config.public.dir), before);
}
#[tokio::test]
async fn missing_inputs_invalid_vectors_or_conversion_failures_leave_installed_files_unchanged() {
    let fixture = Fixture::new();
    fixture.install().await.unwrap();
    let original = tree(&fixture.config.public.dir);
    assert!(
        publisher::install_assets_bundle(
            &fixture.config,
            &fixture.source,
            Some(&fixture.docs),
            None
        )
        .await
        .is_err()
    );
    assert!(
        publisher::install_assets_bundle(
            &fixture.config,
            &fixture.source,
            None,
            Some(&fixture.examples)
        )
        .await
        .is_err()
    );
    for input in [
        fixture.source.join("app.js"),
        fixture.source.join("long-shot-coffee.jpg"),
        fixture.source.join("long-shot-seal.jpg"),
        fixture.docs.join("join.md"),
        fixture.docs.join("participant-spec.md"),
        fixture.docs.join("signing-vectors.json"),
        fixture.docs.join("protocol.md"),
        fixture.docs.join("rules.md"),
        fixture.examples.join("minimal_client.py"),
    ] {
        let bytes = fs::read(&input).unwrap();
        fs::remove_file(&input).unwrap();
        let error = fixture.install().await.unwrap_err().to_string();
        assert!(error.contains(&input.display().to_string()), "{error}");
        assert_eq!(tree(&fixture.config.public.dir), original);
        fs::write(&input, bytes).unwrap();
    }
    let vectors = fixture.docs.join("signing-vectors.json");
    let bytes = fs::read(&vectors).unwrap();
    for invalid in [
        b"{".as_slice(),
        br#"{"schema":"tolight-signing-vectors/1","scheme":"tolight-participant-v1","vectors":[]}"#,
    ] {
        fs::write(&vectors, invalid).unwrap();
        assert!(
            fixture
                .install()
                .await
                .unwrap_err()
                .to_string()
                .contains("signing-vectors.json")
        );
        assert_eq!(tree(&fixture.config.public.dir), original);
    }
    fs::write(&vectors, bytes).unwrap();
    let mut invalid: serde_json::Value =
        serde_json::from_slice(&fs::read(&vectors).unwrap()).unwrap();
    let signature = invalid["vectors"][0]["signature"].as_str().unwrap();
    invalid["vectors"][0]["signature"] = format!(
        "{}{}",
        if signature.starts_with('A') { "B" } else { "A" },
        &signature[1..]
    )
    .into();
    let valid = fs::read(&vectors).unwrap();
    fs::write(&vectors, serde_json::to_vec(&invalid).unwrap()).unwrap();
    assert!(
        fixture
            .install()
            .await
            .unwrap_err()
            .to_string()
            .contains("signing-vectors.json")
    );
    assert_eq!(tree(&fixture.config.public.dir), original);
    fs::write(&vectors, valid).unwrap();
    let rules = fixture.docs.join("rules.md");
    for invalid in [
        b"\xff".as_slice(),
        b"# Rules\n<script>steal()</script>",
        b"# Rules\n[bad](javascript:steal())",
        b"# Rules\n![bad](data:image/svg+xml,evil)",
    ] {
        fs::write(&rules, invalid).unwrap();
        assert!(
            fixture
                .install()
                .await
                .unwrap_err()
                .to_string()
                .contains("rules.md")
        );
        assert_eq!(tree(&fixture.config.public.dir), original);
    }
}

#[tokio::test]
async fn complete_bundle_uses_the_same_fixed_publisher_lock() {
    let fixture = Fixture::new();
    let lock_path = fixture
        .config
        .public
        .dir
        .parent()
        .unwrap()
        .join(".tolight-publisher.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    lock.try_lock().unwrap();
    let original = tree(&fixture.config.public.dir);
    assert!(matches!(fixture.install().await, Err(PublishError::Busy)));
    assert_eq!(tree(&fixture.config.public.dir), original);
    drop(lock);
    fixture.install().await.unwrap();
}
#[tokio::test]
async fn markdown_external_links_and_images_become_text_and_code_is_escaped() {
    let fixture = Fixture::new();
    fs::write(fixture.docs.join("rules.md"),"# Rules\n[spec](participant-spec.md) [external](https://example.invalid/collect) ![alt](https://example.invalid/a.png)\n\n```html\n<script>alert('text')</script>\n```\n").unwrap();
    fixture.install().await.unwrap();
    let html = fs::read_to_string(fixture.config.public.dir.join("docs/rules.html")).unwrap();
    assert!(html.contains("href=\"/docs/participant-spec.html\""));
    assert!(html.contains("external"));
    assert!(html.contains("alt"));
    assert!(html.contains("&lt;script&gt;"));
    assert!(!html.contains("<script"));
    assert!(!html.contains("example.invalid"));
    assert!(!html.contains("<img"));
}
#[cfg(unix)]
#[tokio::test]
async fn symlinked_output_cannot_redirect_documents_into_data_or_follow_temporary_files() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let original = tree(&fixture.config.public.dir.join("data"));
    symlink(
        fixture.config.public.dir.join("data"),
        fixture.config.public.dir.join("docs"),
    )
    .unwrap();
    assert!(fixture.install().await.is_err());
    assert_eq!(tree(&fixture.config.public.dir.join("data")), original);
    fs::remove_file(fixture.config.public.dir.join("docs")).unwrap();
    symlink(
        fixture.config.public.dir.join("data/manifest.json"),
        fixture.config.public.dir.join("join.md.tmp"),
    )
    .unwrap();
    assert!(fixture.install().await.is_err());
    assert_eq!(tree(&fixture.config.public.dir.join("data")), original);
}
#[test]
fn join_has_nine_recoverable_steps_all_pause_points_and_protocol_error_diagnostics() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/join.md");
    let guide = fs::read_to_string(path).unwrap();
    let headings = guide
        .lines()
        .filter(|line| {
            line.starts_with("## ") && line.as_bytes().get(3).is_some_and(u8::is_ascii_digit)
        })
        .collect::<Vec<_>>();
    assert_eq!(headings.len(), 9);
    for number in 1..=9 {
        assert!(headings[number - 1].starts_with(&format!("## {number}. ")));
    }
    for label in [
        "**做什么：**",
        "**成功的样子：**",
        "**失败怎么办：**",
        "**是否停下问 Owner：**",
    ] {
        assert_eq!(guide.matches(label).count(), 9);
    }
    for number in [2, 5, 7] {
        let step = guide
            .split(&format!("## {number}. "))
            .nth(1)
            .unwrap()
            .split("\n## ")
            .next()
            .unwrap();
        assert!(step.contains("【停下问 Owner】"));
    }
    for code in [
        "bad_request",
        "unauthorized",
        "clock_skew",
        "agent_not_active",
        "agent_muted",
        "applications_closed",
        "not_found",
        "invalid_state",
        "agent_id_taken",
        "display_name_taken",
        "key_already_registered",
        "room_paused",
        "too_large",
        "misdirected_request",
        "content_rejected",
        "invalid_mention",
        "too_many_mentions",
        "message_limit",
        "rate_limited",
        "applications_full",
        "unavailable",
        "replied_to_history",
        "retried_too_early",
        "no_reply",
        "no_retry",
    ] {
        assert!(guide.contains(code), "missing {code}");
    }
    assert!(guide.contains("Operator 公开联系渠道尚未提供"));
    assert!(guide.contains("尚未开放生产接入"));
    assert!(!guide.contains("curl |"));
    assert!(!guide.contains("| sh"));
}
