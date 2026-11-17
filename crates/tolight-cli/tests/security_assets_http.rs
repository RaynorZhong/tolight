use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tolight_cli::public_app;

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct Caddy(Child);
impl Drop for Caddy {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start_caddy(
    repo: &Path,
    private: &Path,
    public: &Path,
    upstream: &str,
) -> (Caddy, String, String) {
    let binary = std::env::var_os("TOLIGHT_CADDY_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let local = repo.join(".local/tools/caddy");
            if local.is_file() {
                local
            } else {
                PathBuf::from("caddy")
            }
        });
    let port = free_port();
    let api_port = free_port();
    let origin = format!("http://127.0.0.1:{port}");
    let api_origin = format!("https://localhost:{api_port}");
    let config = private.join("Caddyfile");
    let template = fs::read_to_string(repo.join("deploy/Caddyfile.example")).unwrap();
    // Route and response configuration remain exact; bind/test-only redirect
    // settings confine both Origins and the generated local CA to this fixture.
    fs::write(
        &config,
        template.replacen(
            "{\n\tadmin off",
            "{\n\tdefault_bind 127.0.0.1\n\tauto_https disable_redirects\n\tadmin off",
            1,
        ),
    )
    .unwrap();
    let mut command = Command::new(binary);
    command
        .args([
            "run",
            "--config",
            config.to_str().unwrap(),
            "--adapter",
            "caddyfile",
        ])
        .env("TOLIGHT_PUBLIC_DIR", public)
        .env("TOLIGHT_STATIC_ADDRESS", &origin)
        .env("TOLIGHT_PARTICIPANT_ADDRESS", &api_origin)
        .env("TOLIGHT_PARTICIPANT_UPSTREAM", upstream)
        .env("XDG_DATA_HOME", private.join("caddy-data"))
        .env("XDG_CONFIG_HOME", private.join("caddy-config"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            fs::File::create(private.join("caddy.log")).unwrap(),
        ));
    let mut process = Caddy(
        command
            .spawn()
            .expect("Caddy required; provided by flake.nix or .local/tools/caddy"),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "Caddy exited: {}",
            fs::read_to_string(private.join("caddy.log")).unwrap()
        );
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "Caddy did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    (process, origin, api_origin)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_http_security_verifier_uses_downloaded_bytes_and_the_shared_rust_validator() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let temporary = tempfile::tempdir().unwrap();
    let public = temporary.path().join("public");
    fs::create_dir(&public).unwrap();
    for name in [
        "index.html",
        "app.js",
        "style.css",
        "robots.txt",
        "long-shot-coffee.jpg",
        "long-shot-seal.jpg",
    ] {
        fs::copy(repo.join("public").join(name), public.join(name)).unwrap();
    }
    copy_tree(
        &repo.join("tests/fixtures/public-dist-sample/data"),
        &public.join("data"),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = public_app(&public).await.unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let verify = |arguments: &[&str]| {
        Command::new(repo.join("scripts/verify-public.sh"))
            .args([&origin, "--allow-http", "--expect-demo"])
            .args(arguments)
            .output()
            .unwrap()
    };
    let absent = verify(&["--expect-no-security"]);
    assert!(
        absent.status.success(),
        "{}",
        String::from_utf8_lossy(&absent.stderr)
    );
    assert!(
        String::from_utf8_lossy(&absent.stdout)
            .contains("no security reporting endpoint advertised")
    );
    let brand = public.join("long-shot-coffee.jpg");
    let brand_bytes = fs::read(&brand).unwrap();
    fs::remove_file(&brand).unwrap();
    assert!(!verify(&["--expect-no-security"]).status.success());
    fs::write(&brand, b"not a JPEG").unwrap();
    let invalid_brand = verify(&["--expect-no-security"]);
    assert!(!invalid_brand.status.success());
    assert!(
        String::from_utf8_lossy(&invalid_brand.stderr).contains("invalid published brand JPEG")
    );
    fs::write(&brand, brand_bytes).unwrap();
    let known = public.join(".well-known");
    fs::create_dir(&known).unwrap();
    let security = known.join("security.txt");
    let expires = (chrono::Utc::now() + chrono::Duration::days(180)).to_rfc3339();
    fs::write(&security, format!("Contact: mailto:synthetic@tolight.test\nExpires: {expires}\nCanonical: https://tolight.test/.well-known/security.txt\n")).unwrap();
    let policy = temporary.path().join("SECURITY.md");
    fs::write(
        &policy,
        "# Synthetic test policy\nContact: mailto:synthetic@tolight.test\n",
    )
    .unwrap();
    let arguments = [
        "--expect-security",
        "--security-source",
        security.to_str().unwrap(),
        "--security-policy",
        policy.to_str().unwrap(),
        "--tolight-bin",
        env!("CARGO_BIN_EXE_tolight"),
    ];
    let valid = verify(&arguments);
    assert!(
        valid.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&valid.stdout),
        String::from_utf8_lossy(&valid.stderr)
    );
    assert!(
        String::from_utf8_lossy(&valid.stdout)
            .contains("RFC fields, expiry and matching policy contacts")
    );
    fs::write(
        &policy,
        "# Synthetic mismatch\nContact: mailto:other@tolight.test\n",
    )
    .unwrap();
    let invalid = verify(&arguments);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("SECURITY.md"));
    fs::write(
        &policy,
        "# Synthetic test policy\nContact: mailto:synthetic@tolight.test\n",
    )
    .unwrap();
    fs::write(
        &security,
        "Contact: mailto:synthetic@tolight.test\nExpires: 2000-01-01T00:00:00Z\n",
    )
    .unwrap();
    let expired = verify(&arguments);
    assert!(!expired.status.success());
    assert!(String::from_utf8_lossy(&expired.stderr).contains("Expires"));
    server.abort();
    let _ = server.await;

    // Restore the valid synthetic file and exercise the actual Caddy template,
    // including escaped dots/slashes that cannot be covered by Axum routing.
    fs::write(&security, format!("Contact: mailto:synthetic@tolight.test\nExpires: {expires}\nCanonical: https://tolight.test/.well-known/security.txt\n")).unwrap();
    for (name, text) in [
        (".env", "private"),
        (".well-known/other.txt", "unrelated"),
        (".well-known/security.txt.tmp", "temporary"),
    ] {
        fs::write(public.join(name), text).unwrap();
    }
    let (_caddy, caddy_origin, _) = start_caddy(&repo, temporary.path(), &public, "127.0.0.1:1");
    let checked = Command::new(repo.join("scripts/verify-public.sh"))
        .args([&caddy_origin, "--allow-http", "--expect-demo"])
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    for path in [
        "/.env",
        "/%2eenv",
        "/.well-known/other.txt",
        "/%2ewell-known/other.txt",
        "/.well-known%2Fother.txt",
        "/.well-known/security.txt.tmp",
        "/.well-known/security.txt/",
        "/x/.well-known/security.txt",
    ] {
        let output = Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--path-as-is",
                "--max-time",
                "3",
                "--output",
                "/dev/null",
                "--write-out",
                "%{http_code}",
                &format!("{caddy_origin}{path}"),
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "curl failed for {path}");
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "404", "{path}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_tls_proxy_preserves_the_protocol_body_limit_at_the_byte_boundary() {
    use axum::{
        Router,
        body::Body,
        routing::{get, post},
    };
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let temporary = tempfile::tempdir().unwrap();
    let public = temporary.path().join("public");
    fs::create_dir(&public).unwrap();
    let app = Router::new()
        .route("/participant/v1/self", get(|| async { "ready" }))
        .route(
            "/participant/v1/messages",
            post(|body: Body| async move {
                axum::body::to_bytes(body, 65536)
                    .await
                    .unwrap()
                    .len()
                    .to_string()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (_caddy, _, origin) = start_caddy(&repo, temporary.path(), &public, &upstream);
    let client = reqwest::Client::builder()
        .no_proxy()
        .danger_accept_invalid_certs(true) // The fixture owns an isolated, untrusted localhost CA.
        .tls_version_min(reqwest::tls::Version::TLS_1_3)
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if client
            .get(format!("{origin}/participant/v1/self"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Caddy TLS listener did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for length in [32001, 32768, 32769] {
        let response = client
            .post(format!("{origin}/participant/v1/messages"))
            .body(vec![b'a'; length])
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            if length <= 32768 { 200 } else { 413 },
            "body length {length}"
        );
        if length <= 32768 {
            assert_eq!(response.text().await.unwrap(), length.to_string());
        }
    }
    server.abort();
    let _ = server.await;
}
