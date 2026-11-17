use axum::{body::Body, http::Request};
use std::fs;
use tempfile::TempDir;
use tolight_cli::public_app;
use tower::ServiceExt;

#[tokio::test]
async fn supplied_brand_images_have_exact_bytes_jpeg_mime_head_and_conditional_cache() {
    let dir = TempDir::new().unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../public");
    for name in tolight_coordinator::publisher::BRAND_ASSETS {
        let bytes = fs::read(root.join(name)).unwrap();
        fs::write(dir.path().join(name), &bytes).unwrap();
        let app = public_app(dir.path()).await.unwrap();
        let uri = format!("/{name}");
        let response = app
            .clone()
            .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "image/jpeg");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["cache-control"], "no-cache");
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("img-src 'self'")
        );
        let etag = response.headers()["etag"].clone();
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap(),
            bytes
        );
        let cached = app
            .clone()
            .oneshot(
                Request::get(&uri)
                    .header("if-none-match", etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cached.status(), 304);
        let head = app
            .oneshot(Request::head(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(head.status(), 200);
        assert_eq!(head.headers()["content-type"], "image/jpeg");
        assert_eq!(head.headers()["content-length"], bytes.len().to_string());
        assert!(
            axum::body::to_bytes(head.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn security_endpoint_has_exact_bytes_headers_and_no_general_dot_directory_access() {
    let dir = TempDir::new().unwrap();
    let known = dir.path().join(".well-known");
    fs::create_dir_all(known.join("nested")).unwrap();
    let bytes = b"Contact: mailto:synthetic@tolight.test\nExpires: 2099-01-01T00:00:00Z\n";
    fs::write(known.join("security.txt"), bytes).unwrap();
    for name in [
        "index.html",
        "other.txt",
        "security.txt.tmp",
        "nested/security.txt",
    ] {
        fs::write(known.join(name), "unrelated").unwrap();
    }
    let app = public_app(dir.path()).await.unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::get("/.well-known/security.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; charset=utf-8"
    );
    assert_eq!(response.headers()["cache-control"], "no-cache");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert!(
        response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'none'")
    );
    let etag = response.headers()["etag"].clone();
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
        bytes.as_slice()
    );
    let cached = app
        .clone()
        .oneshot(
            Request::get("/.well-known/security.txt")
                .header("if-none-match", etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cached.status(), 304);
    let head = app
        .clone()
        .oneshot(
            Request::head("/.well-known/security.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-length"], bytes.len().to_string());
    assert!(
        axum::body::to_bytes(head.into_body(), 65536)
            .await
            .unwrap()
            .is_empty()
    );
    for path in [
        "/.well-known",
        "/.well-known/",
        "/.well-known/other.txt",
        "/.well-known/security.txt.tmp",
        "/.well-known/nested/security.txt",
        "/.well-known/security.txt/",
        "/.well-known/%2e%2e/.env",
        "/x/.well-known/security.txt",
    ] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn security_endpoint_rejects_even_inside_root_symlink_aliases() {
    use std::os::unix::fs::symlink;
    let dir = TempDir::new().unwrap();
    let known = dir.path().join(".well-known");
    fs::create_dir(&known).unwrap();
    fs::write(dir.path().join("other.txt"), "not a security policy").unwrap();
    symlink(dir.path().join("other.txt"), known.join("security.txt")).unwrap();
    let app = public_app(dir.path()).await.unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::get("/.well-known/security.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    fs::remove_file(known.join("security.txt")).unwrap();
    fs::remove_dir(&known).unwrap();
    fs::create_dir(dir.path().join("another")).unwrap();
    fs::write(
        dir.path().join("another/security.txt"),
        "not a security policy",
    )
    .unwrap();
    symlink(dir.path().join("another"), &known).unwrap();
    let response = app
        .oneshot(
            Request::get("/.well-known/security.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn preview_is_read_only_and_uses_content_etags() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("index.html"), "first").unwrap();
    let app = public_app(dir.path()).await.unwrap();

    let first = app
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["cache-control"], "no-cache");
    assert_eq!(first.headers()["x-content-type-options"], "nosniff");
    let etag = first.headers()["etag"].clone();
    let cached = app
        .clone()
        .oneshot(
            Request::get("/")
                .header("if-none-match", &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cached.status(), 304);
    assert_eq!(cached.headers()["cache-control"], "no-cache");

    // Same length, no sleep: freshness must not depend on second-resolution mtime.
    fs::write(dir.path().join("index.html"), "other").unwrap();
    let changed = app
        .clone()
        .oneshot(
            Request::get("/")
                .header("if-none-match", &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(changed.status(), 200);
    assert_ne!(changed.headers()["etag"], etag);

    for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 405, "{method}");
    }
    let missing = app
        .oneshot(
            Request::get("/participant/v1/self")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn preview_does_not_follow_paths_outside_its_root() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("public");
    fs::create_dir(&root).unwrap();
    fs::write(parent.path().join("private.txt"), "private").unwrap();
    let app = public_app(&root).await.unwrap();
    for path in [
        "/../private.txt",
        "/%2e%2e/private.txt",
        "/%2e%2e%2fprivate.txt",
        "/.env",
    ] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path}");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(parent.path().join("private.txt"), root.join("index.html"))
            .unwrap();
        let response = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }
}

#[test]
fn command_help_exposes_the_planned_entry_points() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tolight"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in ["serve", "serve-public", "migrate", "publish", "operator"] {
        assert!(help.contains(command));
    }
}
