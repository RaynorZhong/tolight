//! Read-only local preview. Production serving belongs to Caddy, not this module.

pub mod ops;

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::Response,
};
use percent_encoding::percent_decode_str;
use sha2::{Digest, Sha256};
use std::{io, path::Path, path::PathBuf, sync::Arc};

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";

pub async fn public_app(root: impl AsRef<Path>) -> io::Result<Router> {
    let root = tokio::fs::canonicalize(root).await?;
    if !root.is_dir() {
        return Err(io::Error::other("预览目录必须是文件夹"));
    }
    Ok(Router::new()
        .fallback(serve_file)
        .with_state(Arc::new(root)))
}

async fn serve_file(
    State(root): State<Arc<PathBuf>>,
    method: Method,
    uri: Uri,
    request_headers: HeaderMap,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        let mut result = response(
            StatusCode::METHOD_NOT_ALLOWED,
            "text/plain; charset=utf-8",
            Body::empty(),
        );
        result
            .headers_mut()
            .insert(header::ALLOW, "GET, HEAD".parse().unwrap());
        return result;
    }
    let Ok(decoded) = percent_decode_str(uri.path()).decode_utf8() else {
        return not_found();
    };
    let security_path = decoded == "/.well-known/security.txt";
    if decoded.contains(['\\', '\0'])
        || decoded.split('/').any(|part| {
            part == ".." || (part.starts_with('.') && !(security_path && part == ".well-known"))
        })
        || decoded.trim_start_matches('/').starts_with("participant/")
    {
        return not_found();
    }
    if security_path {
        for candidate in [
            root.join(".well-known"),
            root.join(".well-known/security.txt"),
        ] {
            if tokio::fs::symlink_metadata(candidate)
                .await
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return not_found();
            }
        }
    }
    let mut candidate = root.join(decoded.trim_start_matches('/'));
    if candidate.is_dir() {
        candidate.push("index.html");
    }
    let Ok(path) = tokio::fs::canonicalize(candidate).await else {
        return not_found();
    };
    if !path.starts_with(root.as_path()) || !path.is_file() {
        return not_found();
    }
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return not_found();
    };
    let etag = format!("\"{:x}\"", Sha256::digest(&bytes));
    let cached = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|tag| tag.trim() == "*" || tag.trim().trim_start_matches("W/") == etag)
        });
    let mime = mime_type(&path);
    let mut result = if cached {
        response(StatusCode::NOT_MODIFIED, mime, Body::empty())
    } else {
        let len = bytes.len();
        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(bytes)
        };
        let mut result = response(StatusCode::OK, mime, body);
        result
            .headers_mut()
            .insert(header::CONTENT_LENGTH, len.into());
        result
    };
    result
        .headers_mut()
        .insert(header::ETAG, etag.parse().unwrap());
    result
}

fn not_found() -> Response {
    response(
        StatusCode::NOT_FOUND,
        "text/plain; charset=utf-8",
        Body::empty(),
    )
}

fn response(status: StatusCode, mime: &'static str, body: Body) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "no-cache")
        .header("Content-Security-Policy", CSP)
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header(
            "Permissions-Policy",
            "camera=(), microphone=(), geolocation=()",
        )
        .header("X-Robots-Tag", "noindex")
        .header("Strict-Transport-Security", "max-age=31536000")
        .body(body)
        .unwrap()
}

fn mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("md") => "text/markdown; charset=utf-8",
        Some("txt" | "py") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
