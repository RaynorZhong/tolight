//! Download first, validate downloaded bytes, then verify transport boundaries.
use super::{Result, common as c, probe};
use clap::Args as ClapArgs;
use reqwest::{Client, Method, Url};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
#[derive(ClapArgs)]
pub struct Args {
    base_url: String,
    #[arg(long)]
    allow_http: bool,
    #[arg(long)]
    expect_demo: bool,
    #[arg(long)]
    expect_docs: bool,
    #[arg(long, conflicts_with = "expect_no_security")]
    expect_security: bool,
    #[arg(long)]
    expect_no_security: bool,
    #[arg(long, requires = "expect_security")]
    security_source: Option<PathBuf>,
    #[arg(long, requires = "expect_security")]
    security_policy: Option<PathBuf>,
    #[arg(long, requires = "expect_security")]
    tolight_bin: Option<PathBuf>,
    #[arg(long)]
    participant_origin: Option<String>,
    #[arg(long, requires = "participant_origin")]
    reference_bin: Option<PathBuf>,
    #[arg(long, requires = "participant_origin")]
    reference_config: Option<PathBuf>,
    #[arg(long)]
    ca_file: Option<PathBuf>,
    #[arg(long)]
    closed_port: Vec<u16>,
}
fn check(response: &c::Response, status: &[u16], types: &[&str]) -> Result {
    c::ensure(
        status.contains(&response.status),
        &format!("unexpected HTTP {}", response.status),
    )?;
    for (name, expected) in [
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=()",
        ),
        ("x-robots-tag", "noindex"),
        ("cache-control", "no-cache"),
    ] {
        c::ensure(
            response.headers.get(name).and_then(|v| v.to_str().ok()) == Some(expected),
            &format!("invalid {name}"),
        )?;
    }
    let hsts = response
        .headers
        .get("strict-transport-security")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    c::ensure(
        hsts.contains("max-age=31536000")
            && !hsts.to_ascii_lowercase().contains("includesubdomains"),
        "invalid HSTS policy",
    )?;
    let mut directives = BTreeMap::new();
    for directive in response
        .headers
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
    {
        let mut parts = directive.split_whitespace();
        if let Some(name) = parts.next() {
            directives.insert(name, parts.collect::<Vec<_>>());
        }
    }
    for (name, expected) in [
        ("default-src", vec!["'none'"]),
        ("script-src", vec!["'self'"]),
        ("style-src", vec!["'self'"]),
        ("img-src", vec!["'self'", "data:"]),
        ("connect-src", vec!["'self'"]),
        ("object-src", vec!["'none'"]),
        ("base-uri", vec!["'none'"]),
        ("frame-ancestors", vec!["'none'"]),
        ("form-action", vec!["'none'"]),
    ] {
        c::ensure(
            directives.get(name) == Some(&expected),
            &format!("invalid CSP {name}"),
        )?;
    }
    if !types.is_empty() {
        let actual = response
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        c::ensure(types.contains(&actual.as_str()), "unexpected content-type")?;
    }
    Ok(())
}
fn utf8(response: &c::Response, charset: bool) -> Result<&str> {
    if charset {
        c::ensure(
            response
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.to_ascii_lowercase()
                        .replace(' ', "")
                        .contains("charset=utf-8")
                }),
            "UTF-8 content type required",
        )?;
    }
    Ok(std::str::from_utf8(&response.body)?)
}
async fn get(client: &Client, origin: &Url, path: &str, types: &[&str]) -> Result<c::Response> {
    let response = c::fetch_limit(
        client,
        origin.join(path.trim_start_matches('/'))?,
        Method::GET,
        None,
        8 * 1024 * 1024,
    )
    .await?;
    check(&response, &[200], types)?;
    Ok(response)
}
async fn etag(client: &Client, origin: &Url, path: &str, response: &c::Response) -> Result {
    let etag = response
        .headers
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing strong ETag")?;
    c::ensure(
        etag.starts_with('"') && etag.ends_with('"'),
        "missing strong ETag",
    )?;
    let response = c::fetch(
        client,
        origin.join(path.trim_start_matches('/'))?,
        Method::GET,
        Some(etag),
    )
    .await?;
    check(&response, &[304], &[])?;
    println!("PASS GET {path} / conditional 304");
    Ok(())
}
fn html_tags(html: &str) -> Result<Vec<(String, BTreeMap<String, String>)>> {
    let mut tags = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        if rest.starts_with("!--") {
            let close = rest.find("-->").ok_or("unterminated HTML comment")?;
            rest = &rest[close + 3..];
            continue;
        }
        let mut quote = None;
        let mut end = None;
        for (index, ch) in rest.char_indices() {
            if let Some(q) = quote {
                if ch == q {
                    quote = None;
                }
            } else if ch == '\'' || ch == '"' {
                quote = Some(ch);
            } else if ch == '>' {
                end = Some(index);
                break;
            }
        }
        let end = end.ok_or("unterminated HTML element")?;
        let raw = rest[..end].trim();
        rest = &rest[end + 1..];
        if raw.starts_with(['!', '?', '/']) {
            continue;
        }
        let split = raw
            .find(|ch: char| ch.is_whitespace() || ch == '/')
            .unwrap_or(raw.len());
        let tag = raw[..split].to_ascii_lowercase();
        c::ensure(
            !tag.is_empty()
                && tag
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-'),
            "invalid HTML element",
        )?;
        let mut raw = raw[split..].trim();
        let mut attrs = BTreeMap::new();
        while !raw.is_empty() && raw != "/" {
            let split = raw
                .find(|ch: char| ch.is_whitespace() || ch == '=' || ch == '/')
                .unwrap_or(raw.len());
            let name = raw[..split].to_ascii_lowercase();
            c::ensure(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || "-_:".contains(ch)),
                "invalid HTML attribute",
            )?;
            raw = raw[split..].trim_start();
            let mut value = String::new();
            if let Some(tail) = raw.strip_prefix('=') {
                raw = tail.trim_start();
                if raw.starts_with(['\'', '"']) {
                    let q = raw.chars().next().expect("quote");
                    let end = raw[1..].find(q).ok_or("unterminated HTML attribute")? + 1;
                    value = raw[1..end].into();
                    raw = &raw[end + 1..];
                } else {
                    let end = raw.find(char::is_whitespace).unwrap_or(raw.len());
                    value = raw[..end].into();
                    raw = &raw[end..];
                }
            }
            c::ensure(
                attrs.insert(name, value).is_none(),
                "duplicate HTML attribute",
            )?;
            raw = raw.trim_start();
        }
        tags.push((tag, attrs));
    }
    Ok(tags)
}
fn safe_html(text: &str) -> Result {
    for (tag, attrs) in html_tags(text)? {
        c::ensure(
            ![
                "script", "iframe", "object", "embed", "img", "link", "style", "base", "form",
            ]
            .contains(&tag.as_str()),
            "active/resource element in document",
        )?;
        for (name, value) in attrs {
            c::ensure(
                !name.starts_with("on") && !["src", "srcset", "style"].contains(&name.as_str()),
                "active/resource attribute in document",
            )?;
            if name == "href" {
                c::ensure(
                    value.starts_with('#')
                        || [
                            "/",
                            "/join.md",
                            "/docs/participant-spec.html",
                            "/docs/protocol.html",
                            "/docs/rules.html",
                            "/docs/signing-vectors.json",
                            "/examples/minimal_client.py",
                        ]
                        .contains(&value.as_str())
                        || [
                            "/docs/participant-spec.html#",
                            "/docs/protocol.html#",
                            "/docs/rules.html#",
                        ]
                        .iter()
                        .any(|prefix| value.starts_with(prefix)),
                    "non-document or external link",
                )?;
            }
        }
    }
    Ok(())
}
pub async fn run(args: Args) -> Result {
    let origin = c::checked_url(&args.base_url, args.allow_http, false, true)?;
    let client = c::client(args.ca_file.as_deref(), None)?;
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    fs::create_dir_all(root.join("data/chunks"))?;
    let mut brand_assets = Vec::new();
    for (path, types) in [
        ("/", vec!["text/html"]),
        ("/app.js", vec!["text/javascript", "application/javascript"]),
        ("/style.css", vec!["text/css"]),
        ("/robots.txt", vec!["text/plain"]),
        ("/data/manifest.json", vec!["application/json"]),
        ("/data/agents.json", vec!["application/json"]),
    ] {
        let response = get(&client, &origin, path, &types).await?;
        if path == "/" {
            brand_assets =
                tolight_coordinator::publisher::referenced_brand_assets(utf8(&response, false)?);
        }
        if path.starts_with("/data/") {
            fs::write(root.join(path.trim_start_matches('/')), &response.body)?;
        }
        etag(&client, &origin, path, &response).await?;
    }
    for name in brand_assets {
        let path = format!("/{name}");
        let response = get(&client, &origin, &path, &["image/jpeg"]).await?;
        c::ensure(
            tolight_coordinator::publisher::valid_brand_jpeg(&response.body),
            "invalid published brand JPEG",
        )?;
        etag(&client, &origin, &path, &response).await?;
        let head = c::fetch(&client, origin.join(name)?, Method::HEAD, None).await?;
        check(&head, &[200], &["image/jpeg"])?;
        c::ensure(
            head.body.is_empty()
                && head
                    .headers
                    .get("content-length")
                    .and_then(|value| value.to_str().ok())
                    == Some(response.body.len().to_string().as_str()),
            "invalid brand image HEAD",
        )?;
        println!("PASS brand JPEG {path} / exact length HEAD");
    }
    let manifest: Value = serde_json::from_slice(&fs::read(root.join("data/manifest.json"))?)?;
    let head = manifest["newest_seq"]
        .as_i64()
        .filter(|n| *n >= 0)
        .ok_or("invalid manifest head")?;
    c::ensure(manifest["chunk_size"] == 200, "invalid chunk size")?;
    if args.expect_demo {
        c::ensure(manifest["demo"] == true, "M0 demo banner must be enabled")?;
    }
    for first in (1..=head).step_by(200) {
        let path = format!("/data/chunks/{first:012}-{:012}.json", first + 199);
        let response = get(&client, &origin, &path, &["application/json"]).await?;
        fs::write(root.join(path.trim_start_matches('/')), &response.body)?;
        etag(&client, &origin, &path, &response).await?;
    }
    let _tree = crate::ops::storage::public_tree::PublicTree::load(root)?;
    println!("PASS public schemas, allowlists and complete seq=1..{head}");
    let response = c::fetch(
        &client,
        origin.join("participant/v1/self")?,
        Method::GET,
        None,
    )
    .await?;
    check(&response, &[404], &[])?;
    println!("PASS static origin has no participant endpoint");
    let response = c::fetch(&client, origin.clone(), Method::POST, None).await?;
    check(&response, &[404, 405], &[])?;
    println!("PASS static origin rejects POST");
    println!(
        "PASS public verification: {}",
        origin.as_str().trim_end_matches('/')
    );
    if args.expect_security {
        let source = args
            .security_source
            .unwrap_or(c::root().join("public/.well-known/security.txt"));
        let policy = args
            .security_policy
            .unwrap_or(c::root().join("SECURITY.md"));
        let binary = args
            .tolight_bin
            .unwrap_or(c::root().join("target/debug/tolight"));
        c::regular(&source, true)?;
        c::regular(&policy, true)?;
        c::regular(&binary, true)?;
        let response = get(
            &client,
            &origin,
            "/.well-known/security.txt",
            &["text/plain"],
        )
        .await?;
        utf8(&response, true)?;
        c::ensure(
            fs::read(source)? == response.body,
            "security source bytes differ from downloaded bytes",
        )?;
        let public = root.join("security/public");
        fs::create_dir_all(public.join(".well-known"))?;
        fs::write(public.join(".well-known/security.txt"), &response.body)?;
        fs::copy(policy, root.join("security/SECURITY.md"))?;
        let metadata = tolight_coordinator::publisher::validate_security_assets(&public)?
            .ok_or("missing security contact")?;
        if origin.scheme() == "https" && !metadata.canonical.is_empty() {
            c::ensure(
                metadata
                    .canonical
                    .iter()
                    .filter_map(|v| Url::parse(v).ok())
                    .any(|url| {
                        url.origin() == origin.origin() && url.path() == "/.well-known/security.txt"
                    }),
                "security Canonical differs from fetched origin",
            )?;
        }
        etag(&client, &origin, "/.well-known/security.txt", &response).await?;
        println!(
            "PASS security.txt source bytes, UTF-8 MIME, RFC fields, expiry and matching policy contacts (delivery/authorization require separate evidence)"
        );
    }
    if args.expect_no_security {
        let response = c::fetch(
            &client,
            origin.join(".well-known/security.txt")?,
            Method::GET,
            None,
        )
        .await?;
        check(&response, &[404], &[])?;
        println!(
            "PASS no security reporting endpoint advertised before an authorized channel exists"
        );
    }
    if args.expect_docs {
        let docs = root.join("documents");
        fs::create_dir(&docs)?;
        for path in [
            "join.md",
            "docs/participant-spec.md",
            "docs/protocol.md",
            "docs/rules.md",
            "docs/signing-vectors.json",
            "examples/minimal_client.py",
        ] {
            let name = Path::new(path).file_name().ok_or("document filename")?;
            let (types, source, charset) = if path.ends_with(".md") {
                (
                    vec!["text/markdown"],
                    c::root().join("docs").join(name),
                    true,
                )
            } else if path.ends_with(".json") {
                (vec!["application/json"], c::root().join(path), false)
            } else {
                (
                    vec!["text/plain"],
                    c::root().join("examples/python").join(name),
                    true,
                )
            };
            let response = get(&client, &origin, path, &types).await?;
            utf8(&response, charset)?;
            c::ensure(
                fs::read(source)? == response.body,
                "document source bytes differ from downloaded bytes",
            )?;
            fs::write(docs.join(name), &response.body)?;
            println!("PASS document source bytes and MIME /{path}");
        }
        for name in ["participant-spec", "protocol", "rules"] {
            let path = format!("docs/{name}.html");
            let response = get(&client, &origin, &path, &["text/html"]).await?;
            safe_html(utf8(&response, false)?)?;
            println!("PASS safe static HTML /{path}");
        }
        let response = get(&client, &origin, "/", &["text/html"]).await?;
        c::ensure(
            html_tags(utf8(&response, false)?)?
                .iter()
                .any(|(tag, attrs)| {
                    tag == "a" && attrs.get("href").is_some_and(|v| v == "/join.md")
                }),
            "missing same-origin join link",
        )?;
        fs::copy(
            c::root().join("examples/python/check_vectors.py"),
            docs.join("check_vectors.py"),
        )?;
        // The independent public Python client remains the explicit cross-language
        // signing example. It runs only against the downloaded example/vector bytes.
        let python = std::env::var_os("PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let local = c::root().join(".local/venv/bin/python");
                if local.is_file() {
                    local
                } else {
                    PathBuf::from("python3")
                }
            });
        let output = c::capture(
            Command::new(python)
                .arg(docs.join("check_vectors.py"))
                .arg(docs.join("signing-vectors.json")),
            30,
            None,
        )?;
        std::io::Write::write_all(&mut std::io::stdout(), &output.stdout)?;
        c::ensure(
            output.status.success(),
            "downloaded independent signing example failed",
        )?;
        println!(
            "PASS published onboarding bundle and downloaded Python signing vectors (online launch gates remain separate)"
        );
    }
    if let Some(participant) = args.participant_origin {
        let api = c::checked_url(&participant, false, false, true)?;
        c::ensure(
            api.scheme() == "https" && api.origin() != origin.origin(),
            "participant must use a distinct HTTPS Origin",
        )?;
        let reference = args
            .reference_bin
            .ok_or("participant verification requires --reference-bin")?;
        let config = args
            .reference_config
            .ok_or("participant verification requires --reference-config")?;
        let modern = c::client(
            args.ca_file.as_deref(),
            Some(reqwest::tls::Version::TLS_1_3),
        )?;
        let response =
            c::fetch(&modern, api.join("participant/v1/self")?, Method::GET, None).await?;
        let body: Value = serde_json::from_slice(&response.body)?;
        c::ensure(
            response.status == 401
                && body["error"]["code"] == "unauthorized"
                && response
                    .headers
                    .get("cache-control")
                    .and_then(|v| v.to_str().ok())
                    == Some("no-store"),
            "unsigned TLS1.3 participant request must return unauthorized/no-store",
        )?;
        let legacy = c::client(
            args.ca_file.as_deref(),
            Some(reqwest::tls::Version::TLS_1_2),
        )?;
        match legacy.get(api.clone()).send().await {
            Ok(_) => return Err("participant Origin accepted TLS1.2".into()),
            Err(error) => {
                let mut detail = error.to_string();
                let mut source = std::error::Error::source(&error);
                while let Some(value) = source {
                    detail.push_str(&value.to_string());
                    source = value.source();
                }
                let detail = detail.to_ascii_lowercase();
                c::ensure(
                    detail.contains("protocolversion")
                        || detail.contains("protocol version")
                        || detail.contains("handshakefailure")
                        || detail.contains("handshake failure"),
                    "TLS1.2 failed for a reason other than server protocol rejection",
                )?;
            }
        }
        let response = c::fetch(
            &modern,
            api.join("not-a-participant-path")?,
            Method::GET,
            None,
        )
        .await?;
        c::ensure(
            response.status == 404,
            "participant Origin exposed a non-API path",
        )?;
        probe::run(probe::Args {
            static_origin: origin.to_string(),
            participant_origin: api.to_string(),
            reference_bin: reference,
            reference_config: config,
            state_file: root.join("probe-state.json"),
            notify_command: None,
            allow_http: args.allow_http,
            ca_file: args.ca_file,
            max_clock_skew: 60,
            repeat_seconds: 21600,
            now: None,
        })
        .await?;
        for port in args.closed_port {
            c::ensure(port > 0, "closed port must be in 1..65535")?;
            let host = api.host_str().ok_or("participant host")?;
            let addresses = tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
                .await?
                .collect::<Vec<_>>();
            c::ensure(
                !addresses.is_empty(),
                "closed-port resolution returned no addresses",
            )?;
            for address in addresses {
                c::ensure(
                    !tokio::time::timeout(
                        Duration::from_secs(2),
                        tokio::net::TcpStream::connect(address),
                    )
                    .await
                    .is_ok_and(|r| r.is_ok()),
                    &format!("unexpectedly reachable public service port {port}"),
                )?;
            }
            println!("PASS public port {port} is not reachable");
        }
        println!(
            "PASS participant TLS 1.3, TLS 1.2 rejection, route isolation and unregistered signed-host probe"
        );
    }
    Ok(())
}
