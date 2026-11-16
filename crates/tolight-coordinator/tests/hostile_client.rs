mod support;
use serde_json::json;
use sqlx::PgPool;
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};
use support::fake_agent::{FakeAgent, TestApp};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tolight_core::signing;

async fn activate(app: &TestApp, agent: &FakeAgent) {
    agent.apply(app).await;
    let review = app.room.store().review(&agent.id).await.unwrap();
    app.room.store().approve(&review, false).await.unwrap();
}
#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn bounded_upload_deadline_and_total_deadline_do_not_block_healthy_requests(pool: PgPool) {
    let app = TestApp::configured(pool, |config| {
        config.network.body_read_timeout_ms = 100;
        config.network.total_request_timeout_ms = 500;
        config.long_poll_max_wait = 0;
    })
    .await;
    let alice = FakeAgent::new("alice", 102);
    activate(&app, &alice).await;
    let huge = alice
        .raw(
            &app,
            "POST",
            "messages",
            "",
            vec![b'a'; 32769],
            chrono::Utc::now().timestamp(),
            &[],
        )
        .await;
    assert_eq!(huge.status, 413);
    let body = br#"{"client_message_id":"slow","text":"drip upload"}"#;
    let timestamp = chrono::Utc::now().timestamp().to_string();
    let headers = signing::sign_request(
        &alice.seed,
        "POST",
        &app.address.to_string(),
        "http",
        "/participant/v1/messages",
        "",
        &timestamp,
        body,
    )
    .unwrap();
    let mut slow = tokio::net::TcpStream::connect(app.address).await.unwrap();
    slow.write_all(format!("POST /participant/v1/messages HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nTolight-Key: {}\r\nTolight-Timestamp: {}\r\nTolight-Signature: {}\r\n\r\n{{",app.address,body.len(),headers.key,headers.timestamp,headers.signature).as_bytes()).await.unwrap();
    let started = Instant::now();
    let healthy = alice.request(&app, "GET", "self", "", None).await;
    assert_eq!(healthy.status, 200);
    assert!(started.elapsed() < Duration::from_secs(1));
    let mut bytes = [0; 4096];
    let length = tokio::time::timeout(Duration::from_secs(1), slow.read(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8_lossy(&bytes[..length]);
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(started.elapsed() >= Duration::from_millis(70));
    // A slow handler/DB acquisition is also capped by the total middleware deadline.
    let pool = app.room.store().pool();
    let mut held = Vec::new();
    for _ in 0..pool.options().get_max_connections() {
        held.push(pool.acquire().await.unwrap());
    }
    let deadline_started = Instant::now();
    let unavailable = alice.request(&app, "GET", "self", "", None).await;
    assert_eq!(unavailable.status, 503);
    assert_eq!(unavailable.headers["retry-after"], "1");
    assert!(deadline_started.elapsed() >= Duration::from_millis(400));
    assert!(deadline_started.elapsed() < Duration::from_secs(2));
    drop(held);
    let sent = alice.send(&app, "healthy after upload failure").await;
    assert_eq!(sent.status, 201);
}
#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn trusted_forwarded_source_requires_an_explicit_local_peer_and_replay_is_idempotent(
    pool: PgPool,
) {
    let app = TestApp::new(pool).await;
    let alice = FakeAgent::new("alice", 103);
    activate(&app, &alice).await;
    let response = alice
        .raw(
            &app,
            "GET",
            "self",
            "",
            vec![],
            chrono::Utc::now().timestamp(),
            &[
                ("X-Forwarded-Proto", "https"),
                ("X-Forwarded-For", "8.8.8.8"),
            ],
        )
        .await;
    assert_eq!(response.status, 200);
    let body =
        serde_json::to_vec(&json!({"client_message_id":"replay","text":"@alice replay"})).unwrap();
    let at = chrono::Utc::now().timestamp();
    let first = alice
        .raw(&app, "POST", "messages", "", body.clone(), at, &[])
        .await;
    assert_eq!(first.status, 201);
    let second = alice.raw(&app, "POST", "messages", "", body, at, &[]).await;
    assert_eq!(second.status, 200);
    assert_eq!(second.body["seq"], first.body["seq"]);
    assert_eq!(second.body["duplicate"], true);
}
#[test]
fn forwarded_source_headers_are_ignored_until_the_exact_loopback_proxy_is_configured() {
    use axum::{body::Body, extract::ConnectInfo, http::Request};
    let mut config = tolight_core::config::Config::default();
    let request = |peer: &str, value: &str| {
        let mut request = Request::builder()
            .header("x-forwarded-for", value)
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        request
    };
    assert_eq!(
        tolight_coordinator::auth::source_address(
            &request("198.51.100.8:50", "203.0.113.9"),
            &config
        )
        .unwrap()
        .to_string(),
        "198.51.100.8"
    );
    config.network.trusted_proxies = vec!["127.0.0.1".parse().unwrap()];
    assert_eq!(
        tolight_coordinator::auth::source_address(&request("127.0.0.1:50", "2001:db8::1"), &config)
            .unwrap()
            .to_string(),
        "2001:db8::1"
    );
    assert!(
        tolight_coordinator::auth::source_address(
            &request("127.0.0.1:50", "1.2.3.4, 8.8.8.8"),
            &config
        )
        .is_err()
    );
    assert_eq!(
        tolight_coordinator::auth::source_address(&request("127.0.0.2:50", "8.8.8.8"), &config)
            .unwrap()
            .to_string(),
        "127.0.0.2"
    );
}

#[test]
fn direct_loopback_source_is_explicit_without_weakening_forwarded_header_validation() {
    use axum::{
        body::Body,
        extract::ConnectInfo,
        http::{HeaderValue, Request},
    };
    let mut config = tolight_core::config::Config::default();
    config.network.trusted_proxies = vec!["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()];
    let request = |peer: &str, values: &[&[u8]]| {
        let mut request = Request::builder().body(Body::empty()).unwrap();
        for value in values {
            request
                .headers_mut()
                .append("x-forwarded-for", HeaderValue::from_bytes(value).unwrap());
        }
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        request
    };
    for peer in ["127.0.0.1:50", "[::1]:50"] {
        let error =
            tolight_coordinator::auth::source_address(&request(peer, &[]), &config).unwrap_err();
        assert_eq!(error.code, "bad_request");
        assert_eq!(error.status(), 400);
    }
    config.network.allow_direct_loopback = true;
    for peer in ["127.0.0.1:50", "[::1]:50"] {
        assert_eq!(
            tolight_coordinator::auth::source_address(&request(peer, &[]), &config).unwrap(),
            peer.parse::<SocketAddr>().unwrap().ip()
        );
        assert_eq!(
            tolight_coordinator::auth::source_address(&request(peer, &[b"192.168.50.20"]), &config)
                .unwrap()
                .to_string(),
            "192.168.50.20"
        );
        for values in [
            vec![&b""[..]],
            vec![&b" "[..]],
            vec![&b"not-an-ip"[..]],
            vec![&b"192.168.50.20, 10.0.0.1"[..]],
            vec![&b"192.168.50.20:1234"[..]],
            vec![&b"\xff"[..]],
            vec![&b"192.168.50.20"[..], &b"10.0.0.1"[..]],
        ] {
            let error = tolight_coordinator::auth::source_address(&request(peer, &values), &config)
                .unwrap_err();
            assert_eq!(error.code, "bad_request");
            assert_eq!(error.status(), 400);
        }
    }
    for peer in ["127.0.0.2:50", "198.51.100.8:50"] {
        assert_eq!(
            tolight_coordinator::auth::source_address(&request(peer, &[b"203.0.113.9"]), &config)
                .unwrap(),
            peer.parse::<SocketAddr>().unwrap().ip()
        );
    }
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn public_scheme_is_configured_and_source_or_agent_floods_do_not_cross_buckets(pool: PgPool) {
    let app = TestApp::configured(pool, |config| {
        config.participant_scheme = "https".into();
        config.participant_hosts = vec!["api.example".into()];
        config.requests_per_minute = 2;
    })
    .await;
    let alice = FakeAgent::new("alice", 113);
    let input = tolight_coordinator_store::ApplicationInput {
        agent_id: "alice".into(),
        display_name: "Alice".into(),
        display_name_key: "alice".into(),
        bio: "public scheme".into(),
        owner_contact: "owner@example.invalid".into(),
        framework_label: None,
        public_key: alice.key(),
    };
    app.room
        .store()
        .apply(input, Default::default())
        .await
        .unwrap();
    let review = app.room.store().review("alice").await.unwrap();
    app.room.store().approve(&review, false).await.unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for index in 0..3 {
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let signed = signing::sign_request(
            &alice.seed,
            "GET",
            "API.EXAMPLE:443",
            "https",
            "/participant/v1/self",
            "",
            &timestamp,
            &[],
        )
        .unwrap();
        let response = client
            .get(format!("{}/participant/v1/self", app.origin))
            .header("Host", "API.EXAMPLE:443")
            .header("X-Forwarded-Proto", "http")
            .header("Tolight-Key", signed.key)
            .header("Tolight-Timestamp", signed.timestamp)
            .header("Tolight-Signature", signed.signature)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            if index < 2 { 200 } else { 429 }
        );
        if index == 2 {
            assert!(response.headers().get("Retry-After").is_some());
            let body: serde_json::Value = response.json().await.unwrap();
            assert_eq!(body["error"]["code"], "rate_limited");
        }
    }
    let timestamp = (chrono::Utc::now().timestamp() - 301).to_string();
    let signed = signing::sign_request(
        &alice.seed,
        "GET",
        "other.example",
        "https",
        "/participant/v1/self",
        "",
        &timestamp,
        &[],
    )
    .unwrap();
    let response = client
        .get(format!("{}/participant/v1/self", app.origin))
        .header("Host", "other.example")
        .header("Tolight-Key", signed.key)
        .header("Tolight-Timestamp", signed.timestamp)
        .header("Tolight-Signature", signed.signature)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 421);
    let timestamp = (chrono::Utc::now().timestamp() - 301).to_string();
    let signed = signing::sign_request(
        &alice.seed,
        "GET",
        "api.example",
        "https",
        "/participant/v1/self",
        "",
        &timestamp,
        &[],
    )
    .unwrap();
    let response = client
        .get(format!("{}/participant/v1/self", app.origin))
        .header("Host", "api.example")
        .header("Tolight-Key", signed.key)
        .header("Tolight-Timestamp", signed.timestamp)
        .header("Tolight-Signature", signed.signature)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "clock_skew"
    );
}
