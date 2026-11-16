//! Real PostgreSQL 18 lifecycle contracts. Every process, path and port belongs
//! to this fixture; neither the default environment nor an existing preview is used.
#![cfg(unix)]

use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt, symlink},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tolight_core::signing;
use uuid::Uuid;

macro_rules! args {
    ($($value:expr),* $(,)?) => {
        vec![$(OsString::from(AsRef::<OsStr>::as_ref(&$value))),*]
    };
}

fn lan_up_args(ports: &[u16]) -> Vec<OsString> {
    assert_eq!(ports.len(), 6);
    args![
        "--api-port",
        ports[0].to_string(),
        "--web-port",
        ports[1].to_string(),
        "--ops-port",
        ports[2].to_string(),
        "--pg-port",
        ports[3].to_string(),
        "--lan-ip",
        "127.0.0.1",
        "--lan-web-port",
        ports[4].to_string(),
        "--lan-api-port",
        ports[5].to_string()
    ]
}

fn lan_client(certificate: &[u8], version: reqwest::tls::Version) -> Client {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .add_root_certificate(reqwest::Certificate::from_pem(certificate).unwrap())
        .tls_version_min(version)
        .tls_version_max(version)
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

async fn lan_signed_bytes(
    http: &Client,
    origin: &str,
    seed: &[u8; 32],
    method: &str,
    endpoint: &str,
    body: Vec<u8>,
) -> (u16, reqwest::header::HeaderMap, Value) {
    let url = Url::parse(&format!("{origin}/participant/v1/{endpoint}")).unwrap();
    let authority = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    let timestamp = Utc::now().timestamp().to_string();
    let headers = signing::sign_request(
        seed,
        method,
        &authority,
        url.scheme(),
        url.path(),
        url.query().unwrap_or(""),
        &timestamp,
        &body,
    )
    .unwrap();
    let response = http
        .request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), url)
        .header("Tolight-Key", headers.key)
        .header("Tolight-Timestamp", headers.timestamp)
        .header("Tolight-Signature", headers.signature)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.bytes().await.unwrap();
    assert!(body.len() <= 2 * 1024 * 1024);
    let body = serde_json::from_slice(&body)
        .unwrap_or_else(|_| json!({"non_json":String::from_utf8_lossy(&body)}));
    (status, headers, body)
}

async fn lan_signed(
    http: &Client,
    origin: &str,
    seed: &[u8; 32],
    method: &str,
    endpoint: &str,
    body: Option<Value>,
) -> (u16, reqwest::header::HeaderMap, Value) {
    lan_signed_bytes(
        http,
        origin,
        seed,
        method,
        endpoint,
        body.map(|value| serde_json::to_vec(&value).unwrap())
            .unwrap_or_default(),
    )
    .await
}

fn assert_lan_closed(ports: &[u16]) {
    for port in &ports[4..] {
        assert!(
            TcpStream::connect_timeout(
                &SocketAddr::from(([127, 0, 0, 1], *port)),
                Duration::from_millis(200)
            )
            .is_err(),
            "LAN listener {port} remained alive"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lan_tls_onboarding_keeps_trust_authentication_and_operator_approval() {
    let mut fixture = Fixture::new();
    let foreign = fixture.base.join("external-xdg");
    fs::create_dir(&foreign).unwrap();
    let sentinel = foreign.join("must-remain-unchanged");
    fs::write(&sentinel, b"unrelated trust/config fixture\n").unwrap();
    let ports = unused_ports(6);
    let supervisor = fixture.start(
        lan_up_args(&ports),
        &[
            ("XDG_DATA_HOME", foreign.clone().into_os_string()),
            ("XDG_CONFIG_HOME", foreign.clone().into_os_string()),
        ],
    );
    let status = fixture.ready(supervisor, false).await;
    let lan = &status["lan"];
    let web = format!("http://127.0.0.1:{}", ports[4]);
    let api = format!("https://127.0.0.1:{}", ports[5]);
    assert_eq!(lan["web_origin"], web);
    assert_eq!(lan["api_origin"], api);
    assert_eq!(
        lan["participant_base_url"],
        format!("{api}/participant/v1/")
    );
    let certificate = fs::read(fixture.directory.join("public/local-ca.pem")).unwrap();
    let certificate_hash = format!("{:x}", Sha256::digest(&certificate));
    assert_eq!(lan["ca_sha256"], certificate_hash);
    let admin = format!("http://127.0.0.1:{}/admin/", ports[2]);
    assert_eq!(status["admin_url"], admin);
    assert_eq!(fetch(&fixture.http, &admin).await.0, 200);
    let (code, _, script) = fetch(&fixture.http, &format!("{admin}admin.js")).await;
    assert_eq!(code, 200);
    assert_eq!(
        script,
        fs::read(fixture.repo.join(".local/build/operator-ui/admin.js")).unwrap()
    );
    let (code, _, pending) = fetch(&fixture.http, &format!("{admin}api/pending")).await;
    assert_eq!(code, 200);
    let pending: Value = serde_json::from_slice(&pending).unwrap();
    assert_eq!(pending["schema"], "tolight-admin-pending/1");
    assert_eq!(pending["agents"], json!([]));
    let (code, headers, active) = fetch(&fixture.http, &format!("{admin}api/agents")).await;
    assert_eq!(code, 200);
    assert_eq!(headers[reqwest::header::CACHE_CONTROL], "no-store");
    let active: Value = serde_json::from_slice(&active).unwrap();
    assert_eq!(active["schema"], "tolight-admin-agents/1");
    assert_eq!(active["agents"], json!([]));
    assert_eq!(active["online_threshold_seconds"], 120);
    assert_eq!(active["csrf_token"], pending["csrf_token"]);
    assert!(!fixture.directory.join("public/admin.js").exists());
    assert!(!fixture.directory.join("public/operator-ui").exists());
    assert_eq!(fetch(&fixture.http, &format!("{web}/admin/")).await.0, 404);
    assert_eq!(
        fetch(&fixture.http, &format!("{web}/admin/api/pending"))
            .await
            .0,
        404
    );
    for path in ["/admin/api/agents", "/admin/api/control"] {
        assert_eq!(fetch(&fixture.http, &format!("{web}{path}")).await.0, 404);
    }
    let (code, _, downloaded) = fetch(&fixture.http, &format!("{web}/local-ca.pem")).await;
    assert_eq!(code, 200);
    assert_eq!(downloaded, certificate);
    let (code, _, guide) = fetch(&fixture.http, &format!("{web}/lan-join.md")).await;
    assert_eq!(code, 200);
    let guide = String::from_utf8(guide).unwrap();
    assert!(guide.contains(&format!("{api}/participant/v1/")));
    assert!(guide.contains(&certificate_hash));
    assert!(guide.contains("local-ca.pem"));
    let (code, headers, copy_bytes) = fetch(&fixture.http, &format!("{web}/lan-join.json")).await;
    assert_eq!(code, 200);
    assert!(
        headers[reqwest::header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let copy_payload: Value = serde_json::from_slice(&copy_bytes).unwrap();
    assert_eq!(copy_payload["schema"], "tolight-lan-join/1");
    for text in [&guide, copy_payload["prompt"].as_str().unwrap()] {
        assert!(text.contains("https://github.com/RaynorZhong/tolink"));
        assert!(text.contains("825e5424e04465661a0e2a30390534bb2b05965c"));
        assert!(text.contains("离线"));
        assert!(text.contains("不能在线接入") || text.contains("不可在线接入"));
    }
    for field in [
        "web_origin",
        "api_origin",
        "participant_base_url",
        "secure_join_url",
        "ca_url",
        "ca_sha256",
    ] {
        assert_eq!(copy_payload[field], lan[field], "copy payload {field}");
        assert!(
            copy_payload["prompt"]
                .as_str()
                .unwrap()
                .contains(lan[field].as_str().unwrap()),
            "copy prompt must include {field}"
        );
    }
    assert!(fetch(&fixture.http, &format!("{web}/")).await.0 == 200);
    assert!(
        fixture
            .http
            .get(format!("{api}/participant/v1/self"))
            .send()
            .await
            .is_err(),
        "the isolated CA must not become ambient trust"
    );
    let tls = lan_client(&certificate, reqwest::tls::Version::TLS_1_3);
    for path in [
        "/admin/",
        "/admin/api/pending",
        "/admin/api/agents",
        "/admin/api/control",
        "/admin/admin.js",
    ] {
        assert_eq!(fetch(&tls, &format!("{api}{path}")).await.0, 404, "{path}");
        assert_eq!(
            fetch(
                &fixture.http,
                &format!("{}{path}", text(&status["origins"]["api"]))
            )
            .await
            .0,
            401,
            "participant backend {path}"
        );
    }
    let (code, _, secure_guide) = fetch(&tls, &format!("{api}/lan-join.md")).await;
    assert_eq!(code, 200);
    assert_eq!(secure_guide, guide.as_bytes());
    let (code, _, secure_ca) = fetch(&tls, &format!("{api}/local-ca.pem")).await;
    assert_eq!(code, 200);
    assert_eq!(secure_ca, certificate);
    let (code, _, secure_copy) = fetch(&tls, &format!("{api}/lan-join.json")).await;
    assert_eq!(code, 200);
    assert_eq!(secure_copy, copy_bytes);
    for path in [
        "/docs/participant-spec.md",
        "/docs/signing-vectors.json",
        "/examples/minimal_client.py",
    ] {
        assert_eq!(fetch(&tls, &format!("{api}{path}")).await.0, 200, "{path}");
    }
    assert_eq!(
        fetch(
            &fixture.http,
            &format!("{}/participant/v1/self", text(&status["origins"]["api"]))
        )
        .await
        .0,
        401,
        "LAN access must preserve the unsigned loopback client path"
    );
    let old_tls = lan_client(&certificate, reqwest::tls::Version::TLS_1_2);
    assert!(
        old_tls
            .get(format!("{api}/participant/v1/self"))
            .send()
            .await
            .is_err()
    );
    assert_eq!(
        fetch(&tls, &format!("{api}/participant/v1/self")).await.0,
        401
    );
    let wrong_host = tls
        .get(format!("{api}/participant/v1/self"))
        .header("Host", "wrong.invalid")
        .send()
        .await
        .unwrap();
    assert!(matches!(wrong_host.status().as_u16(), 404 | 421));
    for path in ["/participant", "/participant/", "/participant/v1/self"] {
        assert_eq!(fetch(&fixture.http, &format!("{web}{path}")).await.0, 404);
    }
    for path in [
        "/healthz",
        "/metrics",
        "/config.toml",
        "/environment.json",
        "/lan/Caddyfile",
        "/lan/data/caddy/pki/authorities/local/root.key",
        "/.local/dev/config.toml",
        "/local-ca.key",
    ] {
        assert_eq!(
            fetch(&fixture.http, &format!("{web}{path}")).await.0,
            404,
            "{path}"
        );
        assert_eq!(fetch(&tls, &format!("{api}{path}")).await.0, 404, "{path}");
    }
    let seed = [231; 32];
    let unknown = lan_signed(&tls, &api, &seed, "GET", "self", None).await;
    assert_eq!(unknown.0, 401);
    assert_eq!(unknown.2["error"]["code"], "unauthorized");
    let application = lan_signed(&tls, &api, &seed, "POST", "applications", Some(json!({
        "agent_id":"lan-agent", "display_name":"LAN Test Agent", "bio":"Local TLS onboarding fixture",
        "owner_contact":"private-lan-owner@example.invalid", "framework_label":"Rust test fixture"
    }))).await;
    assert_eq!(application.0, 201, "{}", application.2);
    let pending = lan_signed(&tls, &api, &seed, "GET", "self", None).await;
    assert_eq!(pending.0, 200);
    assert_eq!(pending.2["state"], "PENDING");
    assert_eq!(
        lan_signed(
            &fixture.http,
            text(&status["origins"]["api"]),
            &seed,
            "GET",
            "self",
            None
        )
        .await
        .2["state"],
        "PENDING",
        "existing signed loopback clients must remain usable"
    );
    assert_eq!(
        fetch_json(&fixture.http, &format!("{web}/data/agents.json")).await["agents"],
        json!([])
    );
    let refused = fixture
        .local(
            None,
            None,
            args!["operator", "agent", "approve", "lan-agent", "--yes"],
            &[],
            false,
            10,
        )
        .await;
    assert!(
        !refused.status.success(),
        "LAN must not bypass the conformance suite"
    );
    for length in [32768, 32769] {
        let response =
            lan_signed_bytes(&tls, &api, &seed, "POST", "messages", vec![b'a'; length]).await;
        if length == 32769 {
            assert_eq!(response.0, 413, "{}", response.2);
        } else {
            assert_ne!(
                response.0, 413,
                "the full 32 KiB body must reach Coordinator"
            );
        }
    }
    let initial = lan_signed(&tls, &api, &seed, "GET", "events?limit=0", None).await;
    assert_eq!(initial.0, 200, "{}", initial.2);
    assert_eq!(initial.2["head_seq"], 3);
    assert_eq!(initial.2["events"], json!([]));
    let query = format!("events?after={}", text(&initial.2["next_cursor"]));
    let next = lan_signed(&tls, &api, &seed, "GET", &query, None).await;
    assert_eq!(next.0, 200, "{}", next.2);
    let target = &next.2["events"][0];
    assert_eq!(target["seq"], 4);
    assert_eq!(target["mentions_you"], true);
    let reply = json!({"client_message_id":"lan-sandbox-reply", "text":"Local TLS sandbox response",
        "reply_to_event_id":target["event_id"], "mentions":[]});
    let limited = lan_signed(&tls, &api, &seed, "POST", "messages", Some(reply.clone())).await;
    assert_eq!(limited.0, 429, "{}", limited.2);
    assert_eq!(limited.2["error"]["code"], "message_limit");
    let retry = limited.1["retry-after"]
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!((1..=30).contains(&retry));
    tokio::time::sleep(Duration::from_secs(retry)).await;
    let accepted = lan_signed(&tls, &api, &seed, "POST", "messages", Some(reply)).await;
    assert_eq!(accepted.0, 201, "{}", accepted.2);
    let passed = lan_signed(&tls, &api, &seed, "GET", "self", None).await;
    assert_eq!(passed.2["state"], "PENDING");
    assert_eq!(passed.2["conformance"]["last_run"]["passed"], true);
    assert_eq!(
        fetch_json(&fixture.http, &format!("{web}/data/agents.json")).await["agents"],
        json!([])
    );
    fixture
        .run(args!["operator", "agent", "approve", "lan-agent", "--yes"])
        .await;
    let active = lan_signed(&tls, &api, &seed, "GET", "self", None).await;
    assert_eq!(active.2["state"], "ACTIVE");
    let reset = lan_signed(&tls, &api, &seed, "GET", &query, None).await;
    assert_eq!(reset.0, 200);
    assert_eq!(reset.2["cursor_reset"], true);
    let public = lan_signed(&tls, &api, &seed, "POST", "messages", Some(json!({
        "client_message_id":"lan-public-message", "text":"A real TLS request reached the local room",
        "reply_to_event_id":null, "mentions":[]
    }))).await;
    assert_eq!(public.0, 201, "{}", public.2);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let agents = fetch_json(&fixture.http, &format!("{web}/data/agents.json")).await;
        if agents["agents"].as_array().unwrap().len() == 1 {
            assert_eq!(agents["agents"][0]["agent_id"], "lan-agent");
            break;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let (checked, report) = local_check(&mut fixture).await;
    assert!(checked.status.success(), "{report}");
    assert_eq!(report["ok"], true);
    assert!(report["checks"].as_object().unwrap().len() > 8);
    assert_eq!(files(&foreign).as_slice(), std::slice::from_ref(&sentinel));
    assert_eq!(
        fs::read(sentinel).unwrap(),
        b"unrelated trust/config fixture\n"
    );

    let head = lan_signed(&tls, &api, &seed, "GET", "events?limit=0", None).await;
    assert_eq!(head.0, 200);
    let after = text(&head.2["next_cursor"]).to_owned();
    let endpoint = format!("events?after={after}&wait=30");
    let first_poll = {
        let tls = tls.clone();
        let api = api.clone();
        let endpoint = endpoint.clone();
        tokio::spawn(async move { lan_signed(&tls, &api, &seed, "GET", &endpoint, None).await })
    };
    wait_for_pending_long_poll(&fixture.http, text(&status["origins"]["ops"])).await;
    assert!(!first_poll.is_finished());
    let pending_poll = {
        let tls = tls.clone();
        let api = api.clone();
        tokio::spawn(async move { lan_signed(&tls, &api, &seed, "GET", &endpoint, None).await })
    };
    let replaced = tokio::time::timeout(Duration::from_secs(3), first_poll)
        .await
        .expect("a replacement must wake the old long poll")
        .unwrap();
    assert_eq!(replaced.0, 200);
    assert_eq!(replaced.2["events"], json!([]));
    assert_eq!(replaced.2["next_cursor"], after);
    wait_for_pending_long_poll(&fixture.http, text(&status["origins"]["ops"])).await;
    assert!(!pending_poll.is_finished());

    fixture.run(args!["down"]).await;
    assert!(fixture.wait(supervisor, 10).await.success());
    let gateway_log = fs::read_to_string(fixture.directory.join("logs/lan.log")).unwrap();
    assert!(
        !gateway_log.contains("server graceful shutdown"),
        "shutdown must wake the backend read before draining the TLS gateway"
    );
    let stopped_poll = pending_poll
        .await
        .expect("shutdown must return a complete HTTP/JSON response, not cut the TLS connection");
    assert_eq!(stopped_poll.0, 503, "{}", stopped_poll.2);
    assert_eq!(stopped_poll.1["retry-after"], "1");
    assert_eq!(stopped_poll.2["error"]["code"], "unavailable");
    assert_lan_closed(&ports);
    assert_listeners_closed(&status["origins"]);
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lan_settings_ca_and_data_survive_restart_and_explicit_disable() {
    let mut fixture = Fixture::new();
    let ports = unused_ports(6);
    let ordinary = fixture.start(
        args![
            "--api-port",
            ports[0].to_string(),
            "--web-port",
            ports[1].to_string(),
            "--ops-port",
            ports[2].to_string(),
            "--pg-port",
            ports[3].to_string()
        ],
        &[],
    );
    let before = fixture.ready(ordinary, false).await;
    fixture
        .run(args![
            "operator",
            "agent",
            "add",
            "lan-persisted",
            "--display-name",
            "LAN Persisted",
            "--bio",
            "Private persistent LAN fixture",
            "--owner-contact",
            "private-persisted@example.invalid",
            "--public-key",
            signing::public_key_from_seed(&[232; 32])
        ])
        .await;
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(ordinary, 10).await.success());
    let data_before = files(&fixture.directory.join("public/data"))
        .into_iter()
        .filter(|p| p.is_file())
        .map(|p| (p.clone(), fs::read(&p).unwrap()))
        .collect::<Vec<_>>();
    let supervisor = fixture.start(lan_up_args(&ports), &[]);
    let initial = fixture.ready(supervisor, false).await;
    assert_eq!(initial["environment_id"], before["environment_id"]);
    assert_eq!(
        initial["postgres"]["system_identifier"],
        before["postgres"]["system_identifier"]
    );
    let certificate = fs::read(fixture.directory.join("public/local-ca.pem")).unwrap();
    let copy_payload = fs::read(fixture.directory.join("public/lan-join.json")).unwrap();
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(supervisor, 10).await.success());
    assert_lan_closed(&ports);
    // Reproduce an exact pre-admin environment, including existing LAN/CA/DB.
    // The next ordinary up must validate its old bytes before opting it in.
    let config_path = fixture.directory.join("config.toml");
    let current_config = fs::read_to_string(&config_path).unwrap();
    let legacy_config = current_config.split_once("[admin]\n").unwrap().0;
    fs::write(&config_path, legacy_config).unwrap();
    let marker_path = fixture.directory.join("environment.json");
    let mut legacy_marker = read_json(&marker_path);
    legacy_marker.as_object_mut().unwrap().remove("admin_ui");
    fs::write(&marker_path, serde_json::to_vec(&legacy_marker).unwrap()).unwrap();
    let restarted = fixture.start(vec![], &[]);
    let current = fixture.ready(restarted, false).await;
    assert_eq!(current["lan"], initial["lan"]);
    assert_eq!(current["admin_url"], initial["admin_url"]);
    assert_eq!(read_json(&marker_path)["admin_ui"], true);
    assert_eq!(
        current["postgres"]["system_identifier"],
        before["postgres"]["system_identifier"]
    );
    assert_eq!(
        fs::read(fixture.directory.join("public/local-ca.pem")).unwrap(),
        certificate
    );
    assert_eq!(
        fs::read(fixture.directory.join("public/lan-join.json")).unwrap(),
        copy_payload
    );
    for (path, bytes) in &data_before {
        assert_eq!(fs::read(path).unwrap(), *bytes);
    }
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(restarted, 10).await.success());
    let maintenance = fixture.start(args!["--maintenance"], &[]);
    let maintained = fixture.ready(maintenance, true).await;
    assert_eq!(maintained["environment_id"], before["environment_id"]);
    assert_lan_closed(&ports);
    let listeners = ports[4..]
        .iter()
        .map(|port| {
            let listener = TcpListener::bind(("127.0.0.1", *port)).unwrap();
            listener.set_nonblocking(true).unwrap();
            listener
        })
        .collect::<Vec<_>>();
    let (checked, report) = local_check(&mut fixture).await;
    assert_eq!(checked.status.code(), Some(1));
    assert_eq!(report["checks"]["environment"]["code"], "maintenance");
    for listener in listeners {
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(maintenance, 10).await.success());
    let disabled = fixture.start(args!["--no-lan"], &[]);
    let current = fixture.ready(disabled, false).await;
    assert!(current["lan"].is_null());
    assert_eq!(current["environment_id"], before["environment_id"]);
    assert_lan_closed(&ports);
    assert!(!fixture.directory.join("public/local-ca.pem").exists());
    assert!(!fixture.directory.join("public/lan-join.md").exists());
    assert!(!fixture.directory.join("public/lan-join.json").exists());
    assert_eq!(
        fetch(
            &fixture.http,
            &format!("{}/lan-join.json", text(&current["origins"]["web"]))
        )
        .await
        .0,
        404
    );
    for (path, bytes) in &data_before {
        assert_eq!(fs::read(path).unwrap(), *bytes);
    }
    let listing = fixture
        .run(args!["operator", "agent", "list", "--pending"])
        .await;
    assert!(
        String::from_utf8(listing.stdout)
            .unwrap()
            .contains("lan-persisted")
    );
    let (checked, report) = local_check(&mut fixture).await;
    assert!(checked.status.success(), "{report}");
    assert_eq!(report["checks"].as_object().unwrap().len(), 8);
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(disabled, 10).await.success());
    let final_run = fixture.start(vec![], &[]);
    assert!(fixture.ready(final_run, false).await["lan"].is_null());
    assert_lan_closed(&ports);
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(final_run, 10).await.success());
    let reenabled = fixture.start(lan_up_args(&ports), &[]);
    let current = fixture.ready(reenabled, false).await;
    assert_eq!(current["environment_id"], before["environment_id"]);
    assert_eq!(
        fs::read(fixture.directory.join("public/local-ca.pem")).unwrap(),
        certificate,
        "explicit disable must preserve the private CA for later re-enabling"
    );
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(reenabled, 10).await.success());
    assert_lan_closed(&ports);
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lan_rejects_public_bind_conflicting_flags_and_occupied_ports_without_takeover() {
    let mut fixture = Fixture::new();
    let binary = fixture.binary.clone();
    let ports = unused_ports(6);
    let cases = [
        args!["up", "--lan-ip", "0.0.0.0"],
        args!["up", "--lan-ip", "8.8.8.8"],
        args!["up", "--lan-ip", "224.0.0.1"],
        args!["up", "--lan-ip", "255.255.255.255"],
        args!["up", "--no-lan", "--lan-ip", "127.0.0.1"],
        args!["up", "--no-lan", "--lan-web-port", ports[4].to_string()],
        args!["up", "--lan-web-port", ports[4].to_string()],
        args!["up", "--lan-ip", "127.0.0.1", "--api-port", "80"],
        args!["up", "--lan-ip", "127.0.0.1", "--api-port", "443"],
        args![
            "up",
            "--lan-ip",
            "127.0.0.1",
            "--lan-web-port",
            ports[4].to_string(),
            "--lan-api-port",
            ports[4].to_string()
        ],
    ];
    for (index, arguments) in cases.into_iter().enumerate() {
        let directory = fixture.base.join(format!("b{index}"));
        let output = fixture
            .local(Some(&directory), Some(&binary), arguments, &[], false, 10)
            .await;
        assert!(
            !output.status.success(),
            "case {index} unexpectedly started"
        );
        assert!(
            !directory.join("postgres").exists(),
            "case {index} initialized PG"
        );
    }
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut arguments = args!["up"];
    let mut settings = lan_up_args(&ports);
    *settings.last_mut().unwrap() = occupied.local_addr().unwrap().port().to_string().into();
    arguments.extend(settings);
    let directory = fixture.base.join("busy");
    let refused = fixture
        .local(Some(&directory), Some(&binary), arguments, &[], false, 10)
        .await;
    assert!(!refused.status.success());
    assert!(!directory.join("postgres").exists());
    assert!(
        TcpStream::connect_timeout(&occupied.local_addr().unwrap(), Duration::from_secs(1)).is_ok()
    );
    drop(occupied);
    fixture.finish();
}

async fn local_check(fixture: &mut Fixture) -> (Output, Value) {
    let output = fixture
        .local(None, None, args!["check"], &[], false, 30)
        .await;
    let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "local check did not return JSON: {error}; {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, report)
}

async fn diagnostic_business_state(pool: &sqlx::PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
          'agents', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY agent_id), '[]'::jsonb) FROM agents a),
          'keys', (SELECT coalesce(jsonb_agg(to_jsonb(k) ORDER BY public_key), '[]'::jsonb) FROM agent_keys k),
          'events', (SELECT coalesce(jsonb_agg(to_jsonb(e) ORDER BY seq), '[]'::jsonb) FROM events e),
          'attempts', (SELECT count(*) FROM message_attempts),
          'actions', (SELECT count(*) FROM operator_actions),
          'room', (SELECT to_jsonb(r) FROM room_state r))",
    ).fetch_one(pool).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_check_validates_live_assets_and_json_without_changing_business_state() {
    let mut fixture = Fixture::new();
    let ports = unused_ports(4);
    let supervisor = fixture.start(
        args![
            "--api-port",
            ports[0].to_string(),
            "--web-port",
            ports[1].to_string(),
            "--ops-port",
            ports[2].to_string(),
            "--pg-port",
            ports[3].to_string()
        ],
        &[],
    );
    let initial = fixture.ready(supervisor, false).await;
    let vectors = read_json(fixture.repo.join("docs/signing-vectors.json"));
    fixture
        .run(args![
            "operator",
            "agent",
            "add",
            "diagnostic-pending",
            "--display-name",
            "Diagnostic Pending",
            "--bio",
            "Private local diagnostic test identity",
            "--owner-contact",
            "private-diagnostic@example.invalid",
            "--public-key",
            text(&vectors["vectors"][0]["public_key"])
        ])
        .await;
    let mut database = Url::parse("postgresql://tolight@localhost/tolight_dev").unwrap();
    database.set_port(Some(ports[3])).unwrap();
    database
        .query_pairs_mut()
        .append_pair("host", fixture.directory.join("socket").to_str().unwrap());
    let pool = sqlx::PgPool::connect(database.as_str()).await.unwrap();
    let state_before = diagnostic_business_state(&pool).await;
    let public = fixture.directory.join("public");
    let manifest_path = public.join("data/manifest.json");
    let agents_path = public.join("data/agents.json");
    let manifest_bytes = fs::read(&manifest_path).unwrap();
    let agents_bytes = fs::read(&agents_path).unwrap();
    let private_files = ["environment.json", "runtime.json", "config.toml"]
        .map(|name| (name, fs::read(fixture.directory.join(name)).unwrap()));
    let (healthy, report) = local_check(&mut fixture).await;
    assert!(healthy.status.success(), "{report}");
    assert_eq!(report["schema"], "tolight-local-check/1");
    assert_eq!(report["scope"], "local");
    assert_eq!(report["environment_id"], initial["environment_id"]);
    assert_eq!(report["ok"], true);
    assert_eq!(report["checks"].as_object().unwrap().len(), 8);
    assert!(
        report["checks"]
            .as_object()
            .unwrap()
            .values()
            .all(|result| result["ok"] == true)
    );

    let asset = public.join("app.js");
    let saved_asset = fixture.base.join("saved-app.js");
    fs::rename(&asset, &saved_asset).unwrap();
    let (missing, report) = local_check(&mut fixture).await;
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(report["checks"]["javascript"]["ok"], false);
    assert_eq!(report["checks"]["javascript"]["http_status"], 404);
    fs::rename(&saved_asset, &asset).unwrap();

    let mut invalid_manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    invalid_manifest["newest_event_id"] = json!("invalid-empty-anchor");
    fs::write(
        &manifest_path,
        serde_json::to_vec(&invalid_manifest).unwrap(),
    )
    .unwrap();
    let (invalid, report) = local_check(&mut fixture).await;
    assert_eq!(invalid.status.code(), Some(1));
    assert_eq!(report["checks"]["manifest"]["ok"], false);
    fs::write(&manifest_path, &manifest_bytes).unwrap();

    // The publisher may replace agents before manifest during a valid publication.
    let public_agents = json!({
        "schema": "tolight-agents/1",
        "agents": [{
            "agent_id": "future-public",
            "display_name": "Future Public",
            "bio": "Public publication-window fixture",
            "joined_at": "2026-11-15T00:00:00Z"
        }]
    });
    fs::write(&agents_path, serde_json::to_vec(&public_agents).unwrap()).unwrap();
    let (publication_window, report) = local_check(&mut fixture).await;
    assert!(publication_window.status.success(), "{report}");
    let mut invalid_agents = public_agents;
    invalid_agents["agents"][0]["owner_contact"] = json!("private-payload-must-not-echo");
    fs::write(&agents_path, serde_json::to_vec(&invalid_agents).unwrap()).unwrap();
    let (invalid, report) = local_check(&mut fixture).await;
    assert_eq!(invalid.status.code(), Some(1));
    assert_eq!(report["checks"]["agents"]["ok"], false);
    assert!(!String::from_utf8_lossy(&invalid.stdout).contains("private-payload-must-not-echo"));
    fs::write(&agents_path, &agents_bytes).unwrap();

    let (restored, report) = local_check(&mut fixture).await;
    assert!(restored.status.success(), "{report}");
    assert!(
        !String::from_utf8_lossy(&restored.stdout).contains("private-diagnostic@example.invalid")
    );
    assert_eq!(diagnostic_business_state(&pool).await, state_before);
    assert_eq!(fs::read(&manifest_path).unwrap(), manifest_bytes);
    assert_eq!(fs::read(&agents_path).unwrap(), agents_bytes);
    for (name, bytes) in private_files {
        assert_eq!(
            fs::read(fixture.directory.join(name)).unwrap(),
            bytes,
            "{name}"
        );
    }
    assert!(!fixture.directory.join("alert-state.json").exists());
    assert!(!fixture.directory.join("probe-state.json").exists());
    pool.close().await;
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(supervisor, 10).await.success());
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_check_reports_uninitialized_stopped_and_maintenance_without_http_probes() {
    let mut fixture = Fixture::new();
    let (uninitialized, report) = local_check(&mut fixture).await;
    assert_eq!(uninitialized.status.code(), Some(1));
    assert_eq!(report["checks"]["environment"]["code"], "uninitialized");
    assert!(
        !fixture.directory.exists(),
        "check must not initialize the environment"
    );
    let ports = unused_ports(4);
    let supervisor = fixture.start(
        args![
            "--maintenance",
            "--api-port",
            ports[0].to_string(),
            "--web-port",
            ports[1].to_string(),
            "--ops-port",
            ports[2].to_string(),
            "--pg-port",
            ports[3].to_string()
        ],
        &[],
    );
    fixture.ready(supervisor, true).await;
    let listeners = ports[..3]
        .iter()
        .map(|port| {
            let listener = TcpListener::bind(("127.0.0.1", *port)).unwrap();
            listener.set_nonblocking(true).unwrap();
            listener
        })
        .collect::<Vec<_>>();
    let (maintenance, report) = local_check(&mut fixture).await;
    assert_eq!(maintenance.status.code(), Some(1));
    assert_eq!(report["mode"], "maintenance");
    assert_eq!(report["checks"]["environment"]["code"], "maintenance");
    assert!(
        report["checks"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(name, _)| name.as_str() != "environment")
            .all(|(_, result)| result["code"] == "skipped")
    );
    for listener in &listeners {
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "maintenance check connected to a port not owned by the maintenance environment"
        );
    }
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(supervisor, 10).await.success());
    let (stopped, report) = local_check(&mut fixture).await;
    assert_eq!(stopped.status.code(), Some(1));
    assert_eq!(report["ok"], false);
    assert!(
        report["checks"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(name, _)| name.as_str() != "environment")
            .all(|(_, result)| result["code"] == "skipped")
    );
    for listener in listeners {
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "stopped check connected to an unrelated port"
        );
    }
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_local_backup_kills_its_stopped_dump_without_affecting_the_environment() {
    let mut fixture = Fixture::new();
    let ports = unused_ports(4);
    let supervisor = fixture.start(
        args![
            "--no-automation",
            "--api-port",
            ports[0].to_string(),
            "--web-port",
            ports[1].to_string(),
            "--ops-port",
            ports[2].to_string(),
            "--pg-port",
            ports[3].to_string()
        ],
        &[],
    );
    let initial = fixture.ready(supervisor, false).await;
    let mut database = Url::parse("postgresql://tolight@localhost/tolight_dev").unwrap();
    database.set_port(Some(ports[3])).unwrap();
    database
        .query_pairs_mut()
        .append_pair("host", fixture.directory.join("socket").to_str().unwrap());
    let pool = sqlx::PgPool::connect(database.as_str()).await.unwrap();
    let before = diagnostic_business_state(&pool).await;
    let runtime = fs::read(fixture.directory.join("runtime.json")).unwrap();
    let marker = fs::read(fixture.directory.join("environment.json")).unwrap();
    let unrelated = fixture.spawn(Command::new("/bin/sleep").arg("60"), "unrelated");

    // A real table lock holds pg_dump at a deterministic point, independent of
    // database size or disk speed. No replacement executable or production hook.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE agents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let mut command = fixture.local_command(None, None, args!["backup", "--kind", "logical"]);
    let backup = fixture.spawn(&mut command, "cancelled-backup");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(fixture.alive(backup), "{}", fixture.diagnostics());
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT FROM pg_stat_activity WHERE datname=current_database() AND application_name='pg_dump' AND wait_event_type='Lock')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if blocked {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pg_dump did not reach the table lock"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let parent = fixture.children[backup].id();
    let processes = Command::new("ps")
        .args([
            "-ax", "-o", "pid=", "-o", "ppid=", "-o", "pgid=", "-o", "comm=",
        ])
        .output()
        .unwrap();
    assert!(processes.status.success());
    let dumps = String::from_utf8(processes.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let ppid = fields.next()?.parse::<u32>().ok()?;
            let group = fields.next()?.parse::<u32>().ok()?;
            let program = fields.last()?;
            (ppid == parent && Path::new(program).file_name() == Some(OsStr::new("pg_dump")))
                .then_some((pid, group))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        dumps.len(),
        1,
        "the blocked dump must be a direct CLI child"
    );
    let (dump_pid, dump_group) = dumps[0];
    assert_eq!(dump_pid, dump_group, "the dump must own its process group");
    assert!(
        Command::new("/bin/kill")
            .args(["-STOP", &dump_pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let state = Command::new("ps")
            .args(["-p", &dump_pid.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        assert!(state.status.success(), "the blocked owned dump disappeared");
        if state.stdout.trim_ascii().starts_with(b"T") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pg_dump did not enter stopped state"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // SIGSTOP prevents pg_dump from handling TERM itself; cancellation must
    // clean the owned native process group before the CLI exits.
    fixture.signal(backup, "TERM");
    assert!(!fixture.wait(backup, 10).await.success());
    assert!(
        !Command::new("/bin/kill")
            .args(["-0", "--", &format!("-{dump_group}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success(),
        "cancelled pg_dump left its owned process group alive"
    );
    assert!(
        fixture.alive(unrelated),
        "an unrelated process was signalled"
    );
    assert!(
        fixture.alive(supervisor),
        "the local supervisor was signalled"
    );
    blocker.rollback().await.unwrap();

    let (healthy, report) = local_check(&mut fixture).await;
    assert!(healthy.status.success(), "{report}");
    assert_eq!(report["environment_id"], initial["environment_id"]);
    assert_eq!(diagnostic_business_state(&pool).await, before);
    assert_eq!(
        fs::read(fixture.directory.join("runtime.json")).unwrap(),
        runtime
    );
    assert_eq!(
        fs::read(fixture.directory.join("environment.json")).unwrap(),
        marker
    );
    assert!(
        files(&fixture.directory.join("backups/logical")).is_empty(),
        "cancelled backup must leave neither temporary dump nor completion metadata"
    );
    pool.close().await;
    fixture.signal(unrelated, "TERM");
    fixture.wait(unrelated, 5).await;
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(supervisor, 10).await.success());
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_environment_survives_restart_and_produces_both_real_backups() {
    let mut fixture = Fixture::new();
    let ports = unused_ports(4);
    let poisoned = vec![
        (
            "DATABASE_URL",
            "postgresql://must-not-use@127.0.0.1:1/foreign".into(),
        ),
        ("PGHOST", "/not/a/real/socket".into()),
        ("PGPORT", "1".into()),
        ("PGDATABASE", "foreign".into()),
        ("PGUSER", "must-not-use".into()),
        ("PGPASSWORD", "test-only-poison".into()),
        (
            "CARGO_TARGET_DIR",
            fixture
                .base
                .join("ambient-build-must-not-exist")
                .into_os_string(),
        ),
        ("CARGO_BUILD_TARGET", "must-not-use-this-target".into()),
        (
            "SQLX_OFFLINE_DIR",
            fixture
                .base
                .join("ambient-sqlx-must-not-exist")
                .into_os_string(),
        ),
        ("TZ", "Pacific/Honolulu".into()),
    ];
    let supervisor = fixture.start(
        args![
            "--no-automation",
            "--api-port",
            ports[0].to_string(),
            "--web-port",
            ports[1].to_string(),
            "--ops-port",
            ports[2].to_string(),
            "--pg-port",
            ports[3].to_string()
        ],
        &poisoned,
    );
    let initial = fixture.ready(supervisor, false).await;
    assert_eq!(initial["schema"], "tolight-local-status/1");
    assert!(!fixture.base.join("ambient-build-must-not-exist").exists());
    assert_eq!(initial["initialized"], true);
    assert_eq!(initial["postgres"]["major"], 18);
    assert_eq!(initial["postgres"]["running"], true);
    assert_eq!(
        Path::new(text(&initial["postgres"]["data_directory"])),
        fixture.directory.join("postgres")
    );
    assert_eq!(Path::new(text(&initial["tolight_binary"])), fixture.binary);
    assert_eq!(
        initial["binary_mode"],
        if fixture.default_build {
            "source"
        } else {
            "prebuilt"
        }
    );
    let origins = initial["origins"].clone();
    let (code, _, _) = fetch(
        &fixture.http,
        &format!("{}/participant/v1/self", text(&origins["api"])),
    )
    .await;
    assert_eq!(code, 401);
    for path in [
        "/",
        "/join.md",
        "/docs/participant-spec.md",
        "/docs/signing-vectors.json",
        "/examples/minimal_client.py",
    ] {
        let (code, headers, body) =
            fetch(&fixture.http, &format!("{}{path}", text(&origins["web"]))).await;
        assert_eq!(code, 200, "{path}");
        assert!(!body.is_empty(), "{path}");
        assert!(
            headers
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("no-cache"),
            "{path}"
        );
    }

    // Validate the actual downloaded onboarding bundle, including the native
    // schema/HTML checks and the independent public signing-client example.
    let mut verify = fixture.tool(fixture.repo.join("scripts/verify-public.sh"));
    verify.args([text(&origins["web"]), "--allow-http", "--expect-docs"]);
    fixture.execute(verify, "verify-public", true, 30).await;

    // This public test-vector identity is deliberately not third-party evidence.
    let vectors = read_json(fixture.repo.join("docs/signing-vectors.json"));
    let added = fixture
        .run(args![
            "operator",
            "agent",
            "add",
            "persistent-fixture",
            "--display-name",
            "Persistent Fixture",
            "--bio",
            "Private test identity for persistent local development",
            "--owner-contact",
            "persistent@example.invalid",
            "--public-key",
            text(&vectors["vectors"][0]["public_key"])
        ])
        .await;
    let output = String::from_utf8(added.stdout).unwrap();
    let application = output
        .split("application_id=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    Uuid::parse_str(&application).unwrap();
    assert!(output.contains("PENDING"));
    fixture
        .run(args![
            "operator",
            "pause",
            "--reason",
            "persistent local test"
        ])
        .await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let before = loop {
        let manifest = fetch_json(
            &fixture.http,
            &format!("{}/data/manifest.json", text(&origins["web"])),
        )
        .await;
        if manifest["newest_seq"].as_u64().unwrap() >= 1 {
            break manifest;
        }
        assert!(Instant::now() < deadline, "pause event was not published");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let marker_before = fs::read(fixture.directory.join("environment.json")).unwrap();
    let other_binary = fixture.base.join("different-valid-tolight");
    fs::copy(&fixture.binary, &other_binary).unwrap();
    fs::set_permissions(&other_binary, fs::Permissions::from_mode(0o755)).unwrap();
    let swapped = fixture
        .local(
            None,
            Some(&other_binary),
            args!["operator", "agent", "list", "--pending"],
            &[],
            false,
            10,
        )
        .await;
    assert!(
        !swapped.status.success(),
        "a profile cannot silently replace its recorded binary"
    );
    assert_eq!(
        fs::read(fixture.directory.join("environment.json")).unwrap(),
        marker_before
    );

    let runtime_path = fixture.directory.join("runtime.json");
    let original_runtime = fs::read(&runtime_path).unwrap();
    let runtime: Value = serde_json::from_slice(&original_runtime).unwrap();
    let duplicate = fixture.start(vec![], &[]);
    assert!(!fixture.wait(duplicate, 10).await.success());
    assert!(fixture.alive(supervisor));
    assert_eq!(
        read_json(&runtime_path)["supervisor"],
        runtime["supervisor"]
    );

    let unrelated = fixture.spawn(Command::new("/bin/sleep").arg("60"), "unrelated");
    let unrelated_pid = fixture.children[unrelated].id();
    let foreign_identity = process_identity(unrelated_pid);
    for (child, pid, identity) in [
        (false, unrelated_pid, None),
        (true, unrelated_pid, None),
        (true, unrelated_pid, Some(foreign_identity.as_str())),
        (false, 2_147_483_647, None),
    ] {
        let restore = RuntimeRestore {
            path: runtime_path.clone(),
            bytes: original_runtime.clone(),
        };
        let mut tampered = runtime.clone();
        let target = if child {
            &mut tampered["children"]["serve"]
        } else {
            &mut tampered["supervisor"]
        };
        target["pid"] = json!(pid);
        if let Some(identity) = identity {
            target["start_identity"] = json!(identity);
        }
        atomic_write(&runtime_path, &serde_json::to_vec(&tampered).unwrap()).unwrap();
        let refused = fixture
            .local(None, None, args!["down"], &[], false, 10)
            .await;
        assert!(
            !refused.status.success(),
            "unverified runtime must not authorize shutdown"
        );
        assert!(
            fixture.alive(unrelated),
            "an unrelated live process was signalled"
        );
        assert!(fixture.alive(supervisor), "forged runtime must fail closed");
        drop(restore);
    }
    fixture.signal(unrelated, "TERM");
    fixture.wait(unrelated, 5).await;

    fixture
        .local(None, None, args!["backup", "--kind", "both"], &[], true, 45)
        .await;
    let logical_dir = fixture.directory.join("backups/logical");
    let logical = files(&logical_dir)
        .into_iter()
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("tolight-") && name.ends_with(".dump.meta.json")
        })
        .collect::<Vec<_>>();
    assert_eq!(logical.len(), 1);
    let metadata = read_json(&logical[0]);
    let dump = logical_dir.join(text(&metadata["file"]));
    assert!(fs::metadata(&dump).unwrap().len() > 0);
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(dump).unwrap())),
        text(&metadata["sha256"])
    );
    assert_fresh(text(&metadata["created_at"]));
    let physical_dir = fixture.directory.join("backups/physical");
    let physical = files(&physical_dir)
        .into_iter()
        .map(|path| path.join("backup.json"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    assert_eq!(physical.len(), 1);
    let base = read_json(&physical[0]);
    assert_eq!(base["status"], "passed");
    assert_eq!(
        base["system_identifier"],
        initial["postgres"]["system_identifier"]
    );
    assert!(
        physical[0]
            .parent()
            .unwrap()
            .join("data/backup_manifest")
            .is_file()
    );
    assert_fresh(text(&base["completed_at"]));
    // Use the actual monitor CLI as the consumer of each producer's real marker.
    // No temporary Python snippet or imported Python test logic is involved.
    for (kind, directory) in [("logical", logical_dir), ("physical", physical_dir)] {
        let state = fixture.base.join(format!("monitor-{kind}.json"));
        let mut command = fixture.tool(fixture.repo.join("scripts/alert-check.sh"));
        command.args(args![
            "--health-url",
            format!("{}/healthz", text(&origins["ops"])),
            "--metrics-url",
            format!("{}/metrics", text(&origins["ops"])),
            "--state-file",
            state,
            "--backup-dir",
            directory,
            "--backup-max-age",
            "60",
            "--min-free-bytes",
            "0"
        ]);
        let checked = fixture.execute(command, "monitor", true, 15).await;
        let stdout = String::from_utf8(checked.stdout).unwrap();
        let report: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
        assert_eq!(report["conditions"], json!([]), "{stdout}");
        assert_eq!(report["ok"], true);
    }

    fixture.signal(supervisor, "INT");
    assert!(fixture.wait(supervisor, 15).await.success());
    assert_listeners_closed(&origins);
    assert_eq!(
        fs::read_to_string(fixture.directory.join("postgres/PG_VERSION"))
            .unwrap()
            .trim(),
        "18"
    );
    assert!(fixture.directory.join("environment.json").is_file());
    assert!(
        fixture
            .directory
            .join("public/data/manifest.json")
            .is_file()
    );
    assert!(!fixture.directory.join("postgres/postmaster.pid").exists());

    let maintenance = fixture.start(args!["--maintenance"], &[]);
    let maintained = fixture.ready(maintenance, true).await;
    assert_eq!(maintained["postgres"]["running"], true);
    assert_eq!(
        maintained["postgres"]["system_identifier"],
        initial["postgres"]["system_identifier"]
    );
    assert_listeners_closed(&origins);
    let snapshot = fixture.directory.join("maintenance-controls.json");
    fixture
        .run(args![
            "operator",
            "export-controls",
            "--output",
            snapshot,
            "--restore-target",
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        ])
        .await;
    assert_eq!(read_json(&snapshot)["schema"], "tolight-control-snapshot/1");
    assert_eq!(
        fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // This exercises the existing 001 migration, not a future 002 upgrade.
    fixture.run(args!["migrate"]).await;
    fixture.run(args!["publish", "--rebuild"]).await;
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(maintenance, 15).await.success());
    assert!(!fixture.directory.join("postgres/postmaster.pid").exists());

    let restarted = fixture.start(vec![], &[]);
    let current = fixture.ready(restarted, false).await;
    assert_eq!(current["environment_id"], initial["environment_id"]);
    assert_eq!(current["origins"], origins);
    assert_eq!(
        current["postgres"]["system_identifier"],
        initial["postgres"]["system_identifier"]
    );
    assert_eq!(current["tolight_binary"], initial["tolight_binary"]);
    let listing = fixture
        .run(args!["operator", "agent", "list", "--pending"])
        .await;
    let listing = String::from_utf8(listing.stdout).unwrap();
    assert!(listing.contains("persistent-fixture"));
    assert!(listing.contains(&application));
    let after = fetch_json(
        &fixture.http,
        &format!("{}/data/manifest.json", text(&origins["web"])),
    )
    .await;
    assert!(after["newest_seq"].as_u64().unwrap() >= before["newest_seq"].as_u64().unwrap());
    if after["newest_seq"] == before["newest_seq"] {
        assert_eq!(after["newest_event_id"], before["newest_event_id"]);
    }
    assert_eq!(after["paused"], true);
    fixture
        .local(
            None,
            None,
            args!["down"],
            &[("LC_ALL", "C".into()), ("TZ", "Asia/Tokyo".into())],
            true,
            30,
        )
        .await;
    assert!(fixture.wait(restarted, 15).await.success());
    assert_listeners_closed(&origins);
    assert!(!fixture.directory.join("postgres/postmaster.pid").exists());
    assert!(fixture.directory.join("postgres/PG_VERSION").is_file());
    fixture.finish();
}

async fn wait_automation(fixture: &mut Fixture, condition: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let state = read_json(fixture.directory.join("automation/state.json"));
        if condition(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "automation did not reach expected state: {state}; {}",
            fixture.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_automation_makes_a_real_backup_checks_and_private_pending_notification() {
    let mut fixture = Fixture::new();
    let ports = unused_ports(4);
    let supervisor = fixture.start(
        args![
            "--api-port",
            ports[0].to_string(),
            "--web-port",
            ports[1].to_string(),
            "--ops-port",
            ports[2].to_string(),
            "--pg-port",
            ports[3].to_string(),
            "--check-interval-seconds",
            "1"
        ],
        &[],
    );
    let initial = fixture.ready(supervisor, false).await;
    let state = wait_automation(&mut fixture, |state| {
        state["backup"]["status"] == "passed" && state["checks"]["status"] == "passed"
    })
    .await;
    assert_eq!(state["schema"], "tolight-local-automation/1");
    assert_eq!(state["configuration"]["backup_interval_seconds"], 86400);
    assert_eq!(state["configuration"]["logical_retention_days"], 14);
    assert_eq!(
        state["configuration"]["notification_delivery"],
        "private_local_file_only"
    );
    let dump = PathBuf::from(text(&state["backup"]["report"]["backup"]));
    assert!(fs::read(&dump).unwrap().starts_with(b"PGDMP"));
    let metadata = read_json(dump.with_extension("dump.meta.json"));
    assert_eq!(
        metadata["sha256"],
        format!("{:x}", Sha256::digest(fs::read(&dump).unwrap()))
    );
    let vectors = read_json(fixture.repo.join("docs/signing-vectors.json"));
    fixture
        .run(args![
            "operator",
            "agent",
            "add",
            "maintenance-fixture",
            "--display-name",
            "Maintenance Fixture",
            "--bio",
            "Local maintenance fixture",
            "--owner-contact",
            "private-maintenance@example.invalid",
            "--public-key",
            text(&vectors["vectors"][0]["public_key"])
        ])
        .await;
    let notification = fixture.directory.join("automation/notifications.jsonl");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if fs::read_to_string(&notification).is_ok_and(|text| text.contains("new_application")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pending notification missing; {}",
            fixture.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let notices = fs::read_to_string(&notification).unwrap();
    assert!(!notices.contains("private-maintenance@example.invalid"));
    assert_eq!(
        notices
            .lines()
            .filter(|line| line.contains("new_application"))
            .count(),
        1
    );
    for line in notices.lines() {
        let notice: Value = serde_json::from_str(line).unwrap();
        assert_eq!(notice["delivery"], "private_local_file_only");
        assert_eq!(notice["environment_id"], initial["environment_id"]);
    }
    for name in ["state.json", "alerts.json", "notifications.jsonl"] {
        assert_eq!(
            fs::metadata(fixture.directory.join("automation").join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(
        fs::metadata(fixture.directory.join("automation"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fetch(
            &fixture.http,
            &format!("{}/automation/state.json", text(&initial["origins"]["web"]))
        )
        .await
        .0,
        404
    );
    let live: Value = serde_json::from_slice(&fixture.run(args!["status"]).await.stdout).unwrap();
    assert_eq!(live["automation"]["running"], true);
    assert_eq!(live["automation"]["backup"]["status"], "passed");
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(supervisor, 15).await.success());
    assert_eq!(
        read_json(fixture.directory.join("automation/state.json"))["running"],
        false
    );
    let backups_before = files(&fixture.directory.join("backups/logical")).len();
    let restart = fixture.start(args![], &[]);
    fixture.ready(restart, false).await;
    let restarted =
        wait_automation(&mut fixture, |state| state["checks"]["status"] == "passed").await;
    assert_eq!(restarted["configuration"]["check_interval_seconds"], 60);
    assert_eq!(restarted["backup"]["status"], "scheduled");
    assert!(
        restarted["backup"]["next_due_at"].as_f64().unwrap() - Utc::now().timestamp() as f64
            > 86000.
    );
    assert_eq!(
        files(&fixture.directory.join("backups/logical")).len(),
        backups_before
    );
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(restart, 15).await.success());
    let maintenance = fixture.start(args!["--maintenance"], &[]);
    let state = fixture.ready(maintenance, true).await;
    assert_eq!(state["automation"]["configuration"]["enabled"], false);
    assert_eq!(state["automation"]["running"], false);
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(maintenance, 15).await.success());
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_down_sends_stop_directly_when_status_reports_are_unavailable() {
    let mut fixture = Fixture::new();
    let ports = unused_ports(4);
    let supervisor = fixture.start(
        args![
            "--api-port",
            ports[0].to_string(),
            "--web-port",
            ports[1].to_string(),
            "--ops-port",
            ports[2].to_string(),
            "--pg-port",
            ports[3].to_string()
        ],
        &[],
    );
    let initial = fixture.ready(supervisor, false).await;
    wait_automation(&mut fixture, |state| {
        state["backup"]["status"] == "passed" && state["checks"]["status"] == "passed"
    })
    .await;
    let proxy = StopOnlyControlProxy::install(&fixture.directory);
    let unavailable: Value =
        serde_json::from_slice(&fixture.run(args!["status"]).await.stdout).unwrap();
    assert_eq!(unavailable["runtime_valid"], false);
    assert_eq!(*proxy.operations.lock().unwrap(), ["status"]);
    proxy.operations.lock().unwrap().clear();
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(supervisor, 15).await.success());
    assert_eq!(*proxy.operations.lock().unwrap(), ["stop_verified"]);
    assert!(proxy.minimal_stop_ack.load(Ordering::Acquire));
    assert_listeners_closed(&initial["origins"]);
    assert!(!fixture.directory.join("runtime.json").exists());
    drop(proxy);
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automation_retries_a_temporary_config_error_without_losing_checks_or_backups() {
    let mut fixture = Fixture::new();
    let ports = unused_ports(4);
    let supervisor = fixture.start(
        args![
            "--api-port",
            ports[0].to_string(),
            "--web-port",
            ports[1].to_string(),
            "--ops-port",
            ports[2].to_string(),
            "--pg-port",
            ports[3].to_string(),
            "--check-interval-seconds",
            "1",
            "--backup-interval-seconds",
            "2"
        ],
        &[],
    );
    fixture.ready(supervisor, false).await;
    wait_automation(&mut fixture, |state| {
        state["backup"]["status"] == "passed" && state["checks"]["status"] == "passed"
    })
    .await;

    let config_path = fixture.directory.join("config.toml");
    let original = fs::read(&config_path).unwrap();
    let restore = RuntimeRestore {
        path: config_path.clone(),
        bytes: original.clone(),
    };
    atomic_write(&config_path, b"invalid = [\"private-config-sentinel\"\n").unwrap();
    let notification_path = fixture.directory.join("automation/notifications.jsonl");
    let failed = wait_automation(&mut fixture, |state| {
        state["checks"]["status"] == "failed"
            && fs::read_to_string(&notification_path)
                .is_ok_and(|text| text.contains("local_checks_failed"))
    })
    .await;
    assert_eq!(failed["running"], true);
    assert!(failed.get("worker_error").is_none());
    assert!(failed["checks"]["consecutive_failures"].as_u64().unwrap() >= 1);
    assert!(fixture.alive(supervisor));
    let notices = fs::read_to_string(notification_path).unwrap();
    assert!(notices.contains("local_checks_failed"));
    assert!(!notices.contains("private-config-sentinel"));
    assert!(
        !serde_json::to_string(&failed)
            .unwrap()
            .contains("private-config-sentinel")
    );

    let restored_at = Utc::now().timestamp_millis() as f64 / 1000.;
    atomic_write(&config_path, &original).unwrap();
    drop(restore);
    let recovered = wait_automation(&mut fixture, |state| {
        state["checks"]["status"] == "passed"
            && state["backup"]["status"] == "passed"
            && state["backup"]["last_success_at"]
                .as_f64()
                .is_some_and(|at| at >= restored_at)
    })
    .await;
    assert_eq!(recovered["running"], true);
    assert_eq!(recovered["checks"]["consecutive_failures"], 0);
    assert!(recovered.get("worker_error").is_none());
    assert!(recovered["checks"].get("error").is_none());
    let dump = PathBuf::from(text(&recovered["backup"]["report"]["backup"]));
    assert!(fs::read(&dump).unwrap().starts_with(b"PGDMP"));
    let metadata = read_json(dump.with_extension("dump.meta.json"));
    assert_eq!(
        metadata["sha256"],
        format!("{:x}", Sha256::digest(fs::read(dump).unwrap()))
    );
    let live: Value = serde_json::from_slice(&fixture.run(args!["status"]).await.stdout).unwrap();
    assert_eq!(live["automation"]["running"], true);
    assert_eq!(live["runtime_valid"], true);
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(supervisor, 15).await.success());
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_automation_reports_keep_status_and_normal_shutdown_available() {
    let mut fixture = Fixture::new();
    let ports = unused_ports(4);
    let supervisor = fixture.start(
        args![
            "--api-port",
            ports[0].to_string(),
            "--web-port",
            ports[1].to_string(),
            "--ops-port",
            ports[2].to_string(),
            "--pg-port",
            ports[3].to_string(),
            "--check-interval-seconds",
            "1"
        ],
        &[],
    );
    fixture.ready(supervisor, false).await;
    wait_automation(&mut fixture, |state| {
        state["backup"]["status"] == "passed" && state["checks"]["status"] == "passed"
    })
    .await;

    // Seed valid historical rejection buckets in this fixture's private checker
    // state. The actual next check consumes them and emits a large live report.
    let alerts = fixture.directory.join("automation/alerts.json");
    let mut history = read_json(&alerts);
    let now = Utc::now().timestamp() as f64;
    for index in 0..80 {
        let key = format!(
            "tolight_message_limit_rejections_total{{\"agent_id\": \"report-agent-{index:02}\"}}"
        );
        history["counters"][&key] = json!(0);
        history["counter_windows"][&key] = json!([[now, 100]]);
    }
    atomic_write(&alerts, &serde_json::to_vec(&history).unwrap()).unwrap();
    let full = wait_automation(&mut fixture, |state| {
        state["checks"]["report"]["hourly_counter_estimates"]
            .as_object()
            .is_some_and(|series| series.len() >= 80)
    })
    .await;
    assert!(serde_json::to_vec(&full["checks"]["report"]).unwrap().len() > 4096);

    let live: Value = serde_json::from_slice(&fixture.run(args!["status"]).await.stdout).unwrap();
    assert_eq!(live["running"], true);
    assert_eq!(live["runtime_valid"], true);
    let summary = &live["automation"];
    assert_eq!(summary["summary"], true);
    assert_eq!(summary["details_file"], "automation/state.json");
    let report = &summary["checks"]["report"];
    assert_eq!(
        report["conditions_total"],
        full["checks"]["report"]["conditions"]
            .as_array()
            .unwrap()
            .len()
    );
    assert_eq!(report["conditions_truncated"], true);
    assert_eq!(report["conditions"].as_array().unwrap().len(), 8);
    assert_eq!(report["hourly_counter_estimates_total"], 80);
    assert_eq!(report["hourly_counter_estimates_truncated"], true);
    assert_eq!(
        report["hourly_counter_estimates"]
            .as_object()
            .unwrap()
            .len(),
        8
    );

    fixture.run(args!["down"]).await;
    assert!(fixture.wait(supervisor, 15).await.success());
    assert_listeners_closed(&live["origins"]);
    assert_eq!(
        read_json(fixture.directory.join("automation/state.json"))["running"],
        false
    );
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_down_cancels_an_automatic_dump_before_stopping_its_database() {
    let mut fixture = Fixture::new();
    let ports = unused_ports(4);
    let supervisor = fixture.start(
        args![
            "--api-port",
            ports[0].to_string(),
            "--web-port",
            ports[1].to_string(),
            "--ops-port",
            ports[2].to_string(),
            "--pg-port",
            ports[3].to_string(),
            "--backup-interval-seconds",
            "3"
        ],
        &[],
    );
    fixture.ready(supervisor, false).await;
    // Metrics reads agents as well. Finish the serial worker's first full
    // cycle before locking that table, otherwise the check itself blocks the
    // next dump and this test depends on which side reaches the lock first.
    wait_automation(&mut fixture, |state| {
        state["backup"]["status"] == "passed" && state["checks"]["status"] == "passed"
    })
    .await;
    let mut database = Url::parse("postgresql://tolight@localhost/tolight_dev").unwrap();
    database.set_port(Some(ports[3])).unwrap();
    database
        .query_pairs_mut()
        .append_pair("host", fixture.directory.join("socket").to_str().unwrap());
    let pool = sqlx::PgPool::connect(database.as_str()).await.unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE agents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE application_name='pg_dump' AND wait_event_type='Lock')").fetch_one(&pool).await.unwrap();
        if blocked {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "automatic dump did not reach the table lock; {}",
            fixture.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let processes = Command::new("ps")
        .args([
            "-ax", "-o", "pid=", "-o", "ppid=", "-o", "pgid=", "-o", "comm=",
        ])
        .output()
        .unwrap();
    let parent = fixture.children[supervisor].id();
    let dumps = String::from_utf8(processes.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let ppid = fields.next()?.parse::<u32>().ok()?;
            let group = fields.next()?.parse::<u32>().ok()?;
            let program = fields.last()?;
            (ppid == parent && Path::new(program).file_name() == Some(OsStr::new("pg_dump")))
                .then_some((pid, group))
        })
        .collect::<Vec<_>>();
    assert_eq!(dumps.len(), 1);
    let (pid, group) = dumps[0];
    assert_eq!(pid, group);
    assert!(
        Command::new("/bin/kill")
            .args(["-STOP", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    fixture.run(args!["down"]).await;
    assert!(fixture.wait(supervisor, 15).await.success());
    assert!(
        !Command::new("/bin/kill")
            .args(["-0", "--", &format!("-{group}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    assert!(!fixture.directory.join("postgres/postmaster.pid").exists());
    assert_eq!(
        read_json(fixture.directory.join("automation/state.json"))["running"],
        false
    );
    assert!(
        !files(&fixture.directory.join("backups/logical"))
            .iter()
            .any(|path| path.extension().is_some_and(|extension| extension == "tmp"))
    );
    drop(blocker);
    pool.close().await;
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unowned_paths_ports_and_invalid_prebuilt_binaries_are_rejected_without_takeover() {
    let mut fixture = Fixture::new();
    let nonexec = fixture.base.join("nonexecutable");
    fs::write(&nonexec, "not executable").unwrap();
    fs::set_permissions(&nonexec, fs::Permissions::from_mode(0o600)).unwrap();
    let binary_link = fixture.base.join("binary-symlink");
    symlink(&fixture.binary, &binary_link).unwrap();
    for (index, binary) in [
        fixture.base.join("missing-binary"),
        fixture.base.clone(),
        nonexec,
        binary_link,
    ]
    .into_iter()
    .enumerate()
    {
        let directory = fixture.base.join(format!("invalid-binary-{index}"));
        let refused = fixture
            .local(Some(&directory), Some(&binary), args!["up"], &[], false, 10)
            .await;
        assert!(!refused.status.success());
        assert!(
            !directory.exists(),
            "invalid binary must fail before environment initialization"
        );
    }

    let unowned = fixture.base.join("unowned");
    fs::create_dir(&unowned).unwrap();
    let sentinel = unowned.join("someone-elses-data.txt");
    fs::write(&sentinel, "must remain unchanged\n").unwrap();
    fs::set_permissions(&unowned, fs::Permissions::from_mode(0o750)).unwrap();
    let link = fixture.base.join("linked-instance");
    symlink(&unowned, &link).unwrap();
    let outside = tempfile::Builder::new()
        .prefix("tolight-outside-local-")
        .tempdir()
        .unwrap()
        .keep();
    fixture.extra_directories.push(outside.clone());
    for directory in [unowned.clone(), link, outside.join("instance")] {
        for command in ["up", "down"] {
            let binary = (command == "up").then(|| fixture.binary.clone());
            let refused = fixture
                .local(
                    Some(&directory),
                    binary.as_deref(),
                    args![command],
                    &[],
                    false,
                    10,
                )
                .await;
            assert!(
                !refused.status.success(),
                "{} {command}",
                directory.display()
            );
        }
        assert_eq!(
            fs::read_to_string(&sentinel).unwrap(),
            "must remain unchanged\n"
        );
        assert_eq!(files(&unowned), vec![sentinel.clone()]);
        assert_eq!(
            fs::metadata(&unowned).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }
    assert!(!outside.join("instance").exists());

    // Real LISTEN occupation must still fail when TIME_WAIT reuse is permitted.
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();
    let ports = unused_ports(3);
    let busy = fixture.base.join("busy-instance");
    let binary = fixture.binary.clone();
    let refused = fixture
        .local(
            Some(&busy),
            Some(&binary),
            args![
                "up",
                "--api-port",
                port.to_string(),
                "--web-port",
                ports[0].to_string(),
                "--ops-port",
                ports[1].to_string(),
                "--pg-port",
                ports[2].to_string()
            ],
            &[],
            false,
            10,
        )
        .await;
    assert!(!refused.status.success());
    assert!(
        !busy.join("postgres").exists(),
        "port conflict must fail before PostgreSQL starts"
    );
    assert!(
        TcpStream::connect_timeout(&occupied.local_addr().unwrap(), Duration::from_secs(1)).is_ok()
    );
    drop(occupied);
    fixture.finish();
}

fn text(value: &Value) -> &str {
    value.as_str().expect("expected protocol string")
}

fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn files(path: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn unused_ports(count: usize) -> Vec<u16> {
    let listeners = (0..count)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect::<Vec<_>>();
    listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap().port())
        .collect()
}

fn process_identity(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart=", "-o", "command="])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!output.stdout.trim_ascii().is_empty());
    format!("{:x}", Sha256::digest(output.stdout.trim_ascii()))
}

fn assert_fresh(timestamp: &str) {
    let completed = DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc);
    assert!(Utc::now().signed_duration_since(completed).num_seconds() < 60);
}

async fn fetch(http: &Client, url: &str) -> (u16, reqwest::header::HeaderMap, Vec<u8>) {
    let mut response = http.get(url).send().await.unwrap();
    let code = response.status().as_u16();
    let headers = response.headers().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.unwrap() {
        assert!(bytes.len() + chunk.len() <= 2 * 1024 * 1024);
        bytes.extend_from_slice(&chunk);
    }
    (code, headers, bytes)
}

async fn fetch_json(http: &Client, url: &str) -> Value {
    let (code, _, bytes) = fetch(http, url).await;
    assert_eq!(code, 200);
    serde_json::from_slice(&bytes).unwrap()
}

async fn wait_for_pending_long_poll(http: &Client, ops_origin: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (code, _, bytes) = fetch(http, &format!("{ops_origin}/metrics")).await;
        assert_eq!(code, 200);
        if String::from_utf8(bytes)
            .unwrap()
            .lines()
            .any(|line| line == "tolight_pending_long_polls 1")
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the TLS long poll never became pending"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn assert_listeners_closed(origins: &Value) {
    for value in origins.as_object().unwrap().values() {
        let url = Url::parse(text(value)).unwrap();
        let address = SocketAddr::from(([127, 0, 0, 1], url.port().unwrap()));
        assert!(
            TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err(),
            "{url}"
        );
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("{}.test.tmp", Uuid::new_v4()));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    fs::rename(temporary, path)
}

struct RuntimeRestore {
    path: PathBuf,
    bytes: Vec<u8>,
}

/// A test-owned IPC fault: status is unavailable while the same authenticated
/// stop request can still reach the actual supervisor and its PostgreSQL child.
struct StopOnlyControlProxy {
    directory: PathBuf,
    original: PathBuf,
    stop: Arc<AtomicBool>,
    operations: Arc<Mutex<Vec<String>>>,
    minimal_stop_ack: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl StopOnlyControlProxy {
    fn install(directory: &Path) -> Self {
        let path = directory.join("control.sock");
        let original = directory.join("real-control.sock");
        fs::rename(&path, &original).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let operations = Arc::new(Mutex::new(Vec::new()));
        let minimal_stop_ack = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_operations = operations.clone();
        let thread_ack = minimal_stop_ack.clone();
        let real = original.clone();
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                let mut client = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("test control proxy accept failed: {error}"),
                };
                client
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                client
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = String::new();
                BufReader::new(&mut client)
                    .take(4097)
                    .read_line(&mut request)
                    .unwrap();
                let operation: Value = serde_json::from_str(&request).unwrap();
                let operation = operation["operation"].as_str().unwrap();
                thread_operations.lock().unwrap().push(operation.to_owned());
                if operation == "status" {
                    let oversized = json!({"unavailable_status":"x".repeat(64 * 1024)});
                    let _ = serde_json::to_writer(&mut client, &oversized);
                    let _ = client.write_all(b"\n");
                    continue;
                }
                assert_eq!(operation, "stop_verified");
                let mut backend = UnixStream::connect(&real).unwrap();
                backend
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                backend
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                backend.write_all(request.as_bytes()).unwrap();
                let mut response = String::new();
                BufReader::new(backend)
                    .take(64 * 1024 + 1)
                    .read_line(&mut response)
                    .unwrap();
                let ack: Value = serde_json::from_str(&response).unwrap();
                thread_ack.store(
                    ack["ok"] == true && ack.get("automation").is_none(),
                    Ordering::Release,
                );
                let _ = client.write_all(response.as_bytes());
            }
        });
        Self {
            directory: directory.to_path_buf(),
            original,
            stop,
            operations,
            minimal_stop_ack,
            thread: Some(thread),
        }
    }
}
impl Drop for StopOnlyControlProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if self.original.exists() {
            if self.directory.join("runtime.json").exists() {
                // On assertion failure, restore the original endpoint before
                // Fixture's own process cleanup runs.
                let path = self.directory.join("control.sock");
                let _ = fs::remove_file(&path);
                let _ = fs::rename(&self.original, path);
            } else {
                let _ = fs::remove_file(&self.original);
            }
        }
    }
}
impl Drop for RuntimeRestore {
    fn drop(&mut self) {
        if let Err(error) = atomic_write(&self.path, &self.bytes) {
            eprintln!(
                "failed to restore test-owned runtime {}: {error}",
                self.path.display()
            );
        }
    }
}

struct Fixture {
    repo: PathBuf,
    base: PathBuf,
    directory: PathBuf,
    binary: PathBuf,
    default_build: bool,
    environment: BTreeMap<OsString, OsString>,
    children: Vec<Child>,
    instance_directories: BTreeSet<PathBuf>,
    extra_directories: Vec<PathBuf>,
    http: Client,
    completed: bool,
}

impl Fixture {
    fn new() -> Self {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        fs::create_dir_all(repo.join(".local")).unwrap();
        // Keep the directory explicitly: automatic TempDir drop must never
        // delete a PostgreSQL data directory before shutdown is confirmed.
        let base = tempfile::Builder::new()
            .prefix("ldev-rs-")
            .tempdir_in(repo.join(".local"))
            .unwrap()
            .keep();
        // The quality gate executes source mode directly after Cargo exits,
        // allowing real nested source builds without Cargo's parent artifact lock.
        let default_build =
            std::env::var("TOLIGHT_LOCAL_DEV_TEST_DEFAULT_BUILD").as_deref() == Ok("1");
        let binary = if default_build {
            repo.join("target/debug/tolight")
        } else {
            let binary = base.join("prebuilt-tolight");
            fs::copy(env!("CARGO_BIN_EXE_tolight"), &binary).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            binary
        };
        let mut environment = std::env::vars_os()
            .filter(|(name, _)| {
                let name = name.to_string_lossy();
                !name.starts_with("PG") && !name.starts_with("TOLIGHT_") && name != "DATABASE_URL"
            })
            .collect::<BTreeMap<_, _>>();
        let paths = [repo.join(".local/pg18/bin"), repo.join(".local/tools")]
            .into_iter()
            .chain(std::env::split_paths(
                environment
                    .get(OsStr::new("PATH"))
                    .map(OsString::as_os_str)
                    .unwrap_or(OsStr::new("/usr/bin:/bin")),
            ))
            .collect::<Vec<_>>();
        environment.insert("PATH".into(), std::env::join_paths(paths).unwrap());
        environment.insert(
            "CARGO_HOME".into(),
            repo.join(".local/cargo").into_os_string(),
        );
        if !environment.contains_key(OsStr::new("PYTHON"))
            && repo.join(".local/venv/bin/python").is_file()
        {
            environment.insert(
                "PYTHON".into(),
                repo.join(".local/venv/bin/python").into_os_string(),
            );
        }
        let directory = base.join("instance");
        let fixture = Self {
            repo,
            base,
            directory,
            binary,
            default_build,
            environment,
            children: vec![],
            instance_directories: BTreeSet::new(),
            extra_directories: vec![],
            http: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            completed: false,
        };
        assert!(fixture.repo.join("scripts/local-dev.sh").is_file());
        for name in [
            "initdb",
            "pg_ctl",
            "pg_dump",
            "pg_basebackup",
            "pg_verifybackup",
        ] {
            let output = fixture.tool(name).arg("--version").output().unwrap();
            assert!(output.status.success(), "{name} is required");
        }
        fixture
    }

    fn tool(&self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.repo)
            .env_clear()
            .envs(&self.environment)
            .stdin(Stdio::null());
        command
    }

    fn spawn(&mut self, command: &mut Command, label: &str) -> usize {
        let log = self.base.join(format!("{label}-{}.log", Uuid::new_v4()));
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(log)
            .unwrap();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output));
        let child = command.spawn().unwrap();
        let index = self.children.len();
        self.children.push(child); // Register ownership before any await/assert.
        index
    }

    fn alive(&mut self, index: usize) -> bool {
        self.children[index].try_wait().unwrap().is_none()
    }

    fn signal(&mut self, index: usize, signal: &str) {
        if self.alive(index) {
            let status = Command::new("/bin/kill")
                .args([
                    &format!("-{signal}"),
                    &self.children[index].id().to_string(),
                ])
                .status()
                .unwrap();
            assert!(status.success());
        }
    }

    async fn wait(&mut self, index: usize, seconds: u64) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            if let Some(status) = self.children[index].try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "test-owned command timed out; {}",
                self.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn execute(
        &mut self,
        mut command: Command,
        label: &str,
        check: bool,
        seconds: u64,
    ) -> Output {
        let token = Uuid::new_v4();
        let stdout = self.base.join(format!("{label}-{token}.stdout.log"));
        let stderr = self.base.join(format!("{label}-{token}.stderr.log"));
        let out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&stdout)
            .unwrap();
        let err = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&stderr)
            .unwrap();
        command.stdout(Stdio::from(out)).stderr(Stdio::from(err));
        let index = self.children.len();
        self.children.push(command.spawn().unwrap());
        let status = self.wait(index, seconds).await;
        let output = Output {
            status,
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        };
        assert!(
            !check || status.success(),
            "{}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            self.diagnostics()
        );
        output
    }

    fn local_command(
        &mut self,
        directory: Option<&Path>,
        binary: Option<&Path>,
        arguments: Vec<OsString>,
    ) -> Command {
        let directory = directory.unwrap_or(&self.directory).to_path_buf();
        self.instance_directories.insert(directory.clone());
        let mut command = self.tool(self.repo.join("scripts/local-dev.sh"));
        command.arg("--directory").arg(directory);
        if let Some(binary) = binary {
            command.arg("--tolight").arg(binary);
        }
        command.args(arguments);
        command
    }

    fn start(&mut self, arguments: Vec<OsString>, overrides: &[(&str, OsString)]) -> usize {
        let mut args = args!["up"];
        args.extend(arguments);
        let binary = self.binary.clone();
        let mut command = self.local_command(
            None,
            (!self.default_build).then_some(binary.as_path()),
            args,
        );
        command.envs(overrides.iter().map(|(key, value)| (*key, value)));
        self.spawn(&mut command, "supervisor")
    }

    async fn local(
        &mut self,
        directory: Option<&Path>,
        binary: Option<&Path>,
        arguments: Vec<OsString>,
        overrides: &[(&str, OsString)],
        check: bool,
        seconds: u64,
    ) -> Output {
        let mut command = self.local_command(directory, binary, arguments);
        command.envs(overrides.iter().map(|(key, value)| (*key, value)));
        self.execute(command, "command", check, seconds).await
    }

    async fn run(&mut self, arguments: Vec<OsString>) -> Output {
        self.local(None, None, arguments, &[], true, 30).await
    }

    async fn ready(&mut self, index: usize, maintenance: bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            assert!(
                self.alive(index),
                "supervisor exited before readiness; {}",
                self.diagnostics()
            );
            let result = self.local(None, None, args!["status"], &[], false, 5).await;
            if result.status.success() {
                let status: Value = serde_json::from_slice(&result.stdout).unwrap();
                if status["running"] == true && status["runtime_valid"] == true {
                    if maintenance {
                        assert_eq!(status["mode"], "maintenance");
                        return status;
                    }
                    if fetch(
                        &self.http,
                        &format!("{}/healthz", text(&status["origins"]["ops"])),
                    )
                    .await
                    .0 == 200
                    {
                        return status;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "local environment did not become ready; {}",
                self.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn diagnostics(&self) -> String {
        let mut paths = files(&self.base)
            .into_iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
            .collect::<Vec<_>>();
        if self.directory.join("logs").is_dir() {
            paths.extend(files(&self.directory.join("logs")));
        }
        let physical = self.directory.join("backups/physical");
        if physical.is_dir() {
            paths.extend(
                files(&physical)
                    .into_iter()
                    .map(|path| path.join("backup.log")),
            );
        }
        paths
            .into_iter()
            .filter(|path| path.is_file())
            .filter_map(|path| {
                fs::read(&path).ok().map(|bytes| {
                    format!(
                        "{}\n{}",
                        path.display(),
                        String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(8000)..])
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn finish(mut self) {
        self.completed = true;
        let path = self.base.clone();
        drop(self);
        assert!(
            !path.exists(),
            "test-owned processes/data were not safely cleaned: {}",
            path.display()
        );
    }
}

fn wait_owned_child(child: &mut Child, seconds: u64) -> std::io::Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let mut stopped = true;
        for child in self.children.iter_mut().rev() {
            match child.try_wait() {
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(error) => {
                    // An uncertain/reaped handle no longer authorizes a signal.
                    stopped = false;
                    eprintln!("could not inspect owned child {}: {error}", child.id());
                    continue;
                }
            }
            for (signal, seconds) in [("INT", 15), ("TERM", 5), ("KILL", 5)] {
                // Only IDs from unreaped Child handles created by this fixture.
                // No untrusted runtime.json PID ever reaches a signal operation.
                let _ = Command::new("/bin/kill")
                    .args([&format!("-{signal}"), &child.id().to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                match wait_owned_child(child, seconds) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => {
                        stopped = false;
                        eprintln!("could not reap owned child {}: {error}", child.id());
                        break;
                    }
                }
            }
            stopped &= matches!(child.try_wait(), Ok(Some(_)));
        }
        for directory in &self.instance_directories {
            let data = directory.join("postgres");
            if data.is_dir() && !data.is_symlink() {
                match self
                    .tool("pg_ctl")
                    .arg("-D")
                    .arg(&data)
                    .arg("status")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                {
                    // pg_ctl explicitly reserves 3 for "not running". Tool
                    // errors/unknown status never authorize deleting data.
                    Ok(status) => stopped &= status.code() == Some(3),
                    Err(_) => stopped = false,
                }
            }
        }
        if self.completed && stopped && !std::thread::panicking() {
            for directory in &self.extra_directories {
                if let Err(error) = fs::remove_dir_all(directory) {
                    eprintln!(
                        "could not remove test-owned extra directory {}: {error}",
                        directory.display()
                    );
                    return;
                }
            }
            if let Err(error) = fs::remove_dir_all(&self.base) {
                eprintln!(
                    "could not remove test-owned directory {}: {error}",
                    self.base.display()
                );
            }
        } else {
            eprintln!(
                "preserved private local-dev diagnostics at {} (owned children/PG stopped={stopped})\n{}",
                self.base.display(),
                self.diagnostics()
            );
        }
    }
}
