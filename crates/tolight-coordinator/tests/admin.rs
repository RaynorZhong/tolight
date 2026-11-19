mod support;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::net::SocketAddr;
use support::fake_agent::{FakeAgent, TestApp};
use tolight_coordinator::{admin, http, operator};
use tower::ServiceExt;

const AUTHORITY: &str = "127.0.0.1:19091";
const ORIGIN: &str = "http://127.0.0.1:19091";

fn local(method: &str, path: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, AUTHORITY)
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:41000".parse::<SocketAddr>().unwrap(),
    ));
    request
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    bytes: Vec<u8>,
    body: Value,
}
async fn send(router: &Router, request: Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Reply {
        status,
        headers,
        bytes,
        body,
    }
}

async fn fixture(pool: PgPool) -> (TestApp, Router, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("admin.js"),
        "export const privateUi = true;\n",
    )
    .unwrap();
    let app = TestApp::configured(pool, |config| {
        config.admin.enabled = true;
        config.admin.assets_dir = directory.path().into();
    })
    .await;
    let router = admin::router(app.room.clone(), AUTHORITY.parse().unwrap());
    (app, router, directory)
}

async fn list(router: &Router) -> Value {
    let response = send(router, local("GET", "/admin/api/pending")).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body["schema"], "tolight-admin-pending/1");
    response.body
}

fn decision(snapshot: &Value, action: &str) -> Value {
    json!({
        "agent_id":snapshot["agent_id"],
        "application_id":snapshot["application_id"],
        "application_revision":snapshot["application_revision"],
        "public_key":snapshot["public_key"],
        "action":action,
        "reason":"other",
    })
}

fn post(value: &Value, token: &str) -> Request<Body> {
    let mut request = local("POST", "/admin/api/review");
    request
        .headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    request
        .headers_mut()
        .insert("x-tolight-csrf", token.parse().unwrap());
    *request.body_mut() = Body::from(serde_json::to_vec(value).unwrap());
    request
}

fn private_headers(headers: &HeaderMap) {
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["x-frame-options"], "DENY");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert_eq!(headers["cross-origin-resource-policy"], "same-origin");
    assert!(
        headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    assert!(headers.get("access-control-allow-origin").is_none());
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn pending_reviews_keep_private_details_local_and_approve_only_qualified_snapshots(
    pool: PgPool,
) {
    let (app, router, _directory) = fixture(pool).await;
    let agent = FakeAgent::new("admin-alice", 111);
    agent.apply(&app).await;
    let pending = list(&router).await;
    let view = &pending["agents"][0];
    assert_eq!(view["agent_id"], agent.id);
    assert_eq!(view["owner_contact"], "owner@example.invalid");
    assert_eq!(view["public_key"], agent.key());
    assert_eq!(view["can_approve"], false);
    let token = pending["csrf_token"].as_str().unwrap();
    let denied = send(&router, post(&decision(view, "approve"), token)).await;
    assert_eq!(denied.status, 409);
    assert_eq!(denied.body["error"]["code"], "invalid_state");
    assert_eq!(
        app.room.store().review(&agent.id).await.unwrap().state,
        "PENDING"
    );

    app.qualify_conformance(&agent.id).await;
    let pending = list(&router).await;
    let view = &pending["agents"][0];
    assert_eq!(view["can_approve"], true);
    assert!(view["conformance"]["passed_at"].is_string());
    let token = pending["csrf_token"].as_str().unwrap();
    let approved = send(&router, post(&decision(view, "approve"), token)).await;
    assert_eq!(approved.status, 200, "{}", approved.body);
    assert_eq!(approved.body, json!({"agent_id":agent.id,"state":"ACTIVE"}));
    private_headers(&approved.headers);
    let duplicate = send(&router, post(&decision(view, "approve"), token)).await;
    assert_eq!(duplicate.status, 409);
    assert_eq!(duplicate.body["error"]["code"], "review_changed");
    assert!(list(&router).await["agents"].as_array().unwrap().is_empty());
    let joined: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE kind='agent_joined'")
        .fetch_one(app.room.store().pool())
        .await
        .unwrap();
    assert_eq!(joined, 1);

    let participant = send(
        &http::router(app.room.clone()),
        local("GET", "/admin/api/pending"),
    )
    .await;
    assert_eq!(participant.status, 401);
    assert!(!String::from_utf8_lossy(&participant.bytes).contains("owner@example.invalid"));
    let timestamp = time::OffsetDateTime::now_utc().unix_timestamp().to_string();
    let signature = tolight_core::signing::sign_request(
        &agent.seed,
        "GET",
        &app.address.to_string(),
        "http",
        "/admin/api/pending",
        "",
        &timestamp,
        &[],
    )
    .unwrap();
    let mut request = local("GET", "/admin/api/pending");
    for (name, value) in [
        ("host", app.address.to_string()),
        ("tolight-key", signature.key),
        ("tolight-timestamp", signature.timestamp),
        ("tolight-signature", signature.signature),
    ] {
        request.headers_mut().insert(
            axum::http::HeaderName::from_static(name),
            value.parse().unwrap(),
        );
    }
    let signed_participant = send(&http::router(app.room.clone()), request).await;
    assert_eq!(
        signed_participant.status, 404,
        "even an authenticated Agent cannot reach local review routes"
    );
    assert!(!String::from_utf8_lossy(&signed_participant.bytes).contains("owner@example.invalid"));
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn rejection_checks_reason_and_the_viewed_revision_before_changing_state(pool: PgPool) {
    let (app, router, _directory) = fixture(pool).await;
    let agent = FakeAgent::new("admin-bobby", 112);
    agent.apply(&app).await;
    let pending = list(&router).await;
    let view = &pending["agents"][0];
    let token = pending["csrf_token"].as_str().unwrap();
    let old_snapshot = operator::review(app.room.store(), app.room.config(), &agent.id)
        .await
        .unwrap();
    let mut request = decision(view, "reject");
    request["reason"] = "not-a-reason".into();
    let invalid = send(&router, post(&request, token)).await;
    assert_eq!(invalid.status, 400);
    assert_eq!(
        app.room.store().review(&agent.id).await.unwrap().state,
        "PENDING"
    );
    let rejected = send(&router, post(&decision(view, "reject"), token)).await;
    assert_eq!(rejected.status, 200);
    assert_eq!(rejected.body["state"], "REJECTED");
    agent.apply(&app).await;
    let stale = send(&router, post(&decision(view, "reject"), token)).await;
    assert_eq!(stale.status, 409);
    assert_eq!(stale.body["error"]["code"], "review_changed");
    let error =
        operator::reject_reviewed(app.room.store(), app.room.config(), &old_snapshot, "other")
            .await
            .unwrap_err();
    assert_eq!(
        error.code, "review_changed",
        "the store CAS must protect a race after HTTP review"
    );
    assert_eq!(
        app.room.store().review(&agent.id).await.unwrap().state,
        "PENDING"
    );
    let current = list(&router).await;
    let result = send(
        &router,
        post(&decision(&current["agents"][0], "reject"), token),
    )
    .await;
    assert_eq!(result.status, 200);
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(app.room.store().pool())
        .await
        .unwrap();
    assert_eq!(
        events, 0,
        "rejected applications never join the public timeline"
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn replacement_application_and_old_suite_never_reuse_an_old_approval(pool: PgPool) {
    let (app, router, _directory) = fixture(pool).await;
    let agent = FakeAgent::new("admin-carol", 113);
    agent.apply(&app).await;
    app.qualify_conformance(&agent.id).await;
    let old = list(&router).await;
    let old_snapshot = app.room.store().review(&agent.id).await.unwrap();
    let token = old["csrf_token"].as_str().unwrap();
    app.room
        .store()
        .withdraw(&agent.id, &agent.key())
        .await
        .unwrap();
    agent.apply(&app).await;
    let changed = send(
        &router,
        post(&decision(&old["agents"][0], "approve"), token),
    )
    .await;
    assert_eq!(changed.status, 409);
    assert_eq!(changed.body["error"]["code"], "review_changed");
    let changed_rejection =
        send(&router, post(&decision(&old["agents"][0], "reject"), token)).await;
    assert_eq!(changed_rejection.status, 409);
    assert_eq!(
        operator::reject_reviewed(app.room.store(), app.room.config(), &old_snapshot, "other")
            .await
            .unwrap_err()
            .code,
        "review_changed"
    );
    app.qualify_conformance(&agent.id).await;
    sqlx::query("UPDATE agents SET conformance_suite_version='old-suite' WHERE agent_id=$1")
        .bind(&agent.id)
        .execute(app.room.store().pool())
        .await
        .unwrap();
    let pending = list(&router).await;
    assert_eq!(pending["agents"][0]["can_approve"], false);
    let denied = send(
        &router,
        post(&decision(&pending["agents"][0], "approve"), token),
    )
    .await;
    assert_eq!(denied.status, 409);
    assert_eq!(
        app.room.store().review(&agent.id).await.unwrap().state,
        "PENDING"
    );
    sqlx::query(
        "UPDATE agents SET registered_at=clock_timestamp()-interval '15 days' WHERE agent_id=$1",
    )
    .bind(&agent.id)
    .execute(app.room.store().pool())
    .await
    .unwrap();
    let expired = send(
        &router,
        post(&decision(&pending["agents"][0], "approve"), token),
    )
    .await;
    assert_eq!(
        expired.status, 404,
        "expired applications cannot be approved from an old page"
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn reject_rechecks_the_review_basis_after_waiting_for_the_agent_row_lock(pool: PgPool) {
    let (app, _router, _directory) = fixture(pool).await;
    let agent = FakeAgent::new("admin-race", 116);
    agent.apply(&app).await;
    let snapshot = app.room.store().review(&agent.id).await.unwrap();
    let mut blocker = app.room.store().pool().begin().await.unwrap();
    sqlx::query("SELECT agent_id FROM agents WHERE agent_id=$1 FOR NO KEY UPDATE")
        .bind(&agent.id)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let room = app.room.clone();
    let pending = tokio::spawn(async move {
        operator::reject_reviewed(room.store(), room.config(), &snapshot, "other").await
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FROM agents WHERE agent_id=$1 FOR NO KEY UPDATE%'")
            .fetch_one(app.room.store().pool()).await.unwrap();
        if waiting > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "reviewed reject must actually wait on the held row"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    sqlx::query("UPDATE agents SET application_revision=application_revision+1,bio='申请内容已经变更' WHERE agent_id=$1")
        .bind(&agent.id).execute(&mut *blocker).await.unwrap();
    blocker.commit().await.unwrap();
    assert_eq!(pending.await.unwrap().unwrap_err().code, "review_changed");
    assert_eq!(
        app.room.store().review(&agent.id).await.unwrap().state,
        "PENDING"
    );
    let actions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM operator_actions WHERE kind='reject'")
            .fetch_one(app.room.store().pool())
            .await
            .unwrap();
    assert_eq!(actions, 0);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn host_origin_peer_and_fetch_metadata_block_cross_site_reads_and_rebinding(pool: PgPool) {
    let (app, router, _directory) = fixture(pool).await;
    FakeAgent::new("admin-danny", 114).apply(&app).await;
    for case in [
        "no-peer",
        "remote-peer",
        "host",
        "duplicate-host",
        "origin",
        "duplicate-origin",
        "fetch-site",
        "same-site",
    ] {
        let mut request = local("GET", "/admin/api/pending");
        match case {
            "no-peer" => {
                request.extensions_mut().remove::<ConnectInfo<SocketAddr>>();
            }
            "remote-peer" => {
                request.extensions_mut().insert(ConnectInfo(
                    "192.168.10.22:32000".parse::<SocketAddr>().unwrap(),
                ));
            }
            "host" => {
                request
                    .headers_mut()
                    .insert(header::HOST, "evil.example:19091".parse().unwrap());
            }
            "duplicate-host" => {
                request
                    .headers_mut()
                    .append(header::HOST, AUTHORITY.parse().unwrap());
            }
            "origin" => {
                request
                    .headers_mut()
                    .insert(header::ORIGIN, "http://evil.example".parse().unwrap());
            }
            "duplicate-origin" => {
                request
                    .headers_mut()
                    .append(header::ORIGIN, ORIGIN.parse().unwrap());
                request
                    .headers_mut()
                    .append(header::ORIGIN, ORIGIN.parse().unwrap());
            }
            "fetch-site" => {
                request
                    .headers_mut()
                    .insert("sec-fetch-site", "cross-site".parse().unwrap());
            }
            "same-site" => {
                request
                    .headers_mut()
                    .insert("sec-fetch-site", "same-site".parse().unwrap());
            }
            _ => unreachable!(),
        }
        let response = send(&router, request).await;
        assert_eq!(response.status, 403, "{case}");
        private_headers(&response.headers);
        assert!(response.body.get("agents").is_none());
        assert!(!String::from_utf8_lossy(&response.bytes).contains("owner@example.invalid"));
    }
    let allowed = send(&router, local("GET", "/admin/api/pending")).await;
    assert_eq!(allowed.status, 200);
    private_headers(&allowed.headers);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn write_requires_same_origin_json_and_the_current_process_csrf_token(pool: PgPool) {
    let (app, router, _directory) = fixture(pool).await;
    let agent = FakeAgent::new("admin-edith", 115);
    agent.apply(&app).await;
    let pending = list(&router).await;
    let value = decision(&pending["agents"][0], "reject");
    let token = pending["csrf_token"].as_str().unwrap();
    for case in [
        "missing-token",
        "wrong-token",
        "duplicate-token",
        "missing-origin",
        "wrong-origin",
        "not-json",
        "too-large",
        "unknown-field",
    ] {
        let mut request = post(&value, token);
        let expected = match case {
            "missing-token" => {
                request.headers_mut().remove("x-tolight-csrf");
                403
            }
            "wrong-token" => {
                request
                    .headers_mut()
                    .insert("x-tolight-csrf", "wrong".parse().unwrap());
                403
            }
            "duplicate-token" => {
                request
                    .headers_mut()
                    .append("x-tolight-csrf", token.parse().unwrap());
                403
            }
            "missing-origin" => {
                request.headers_mut().remove(header::ORIGIN);
                403
            }
            "wrong-origin" => {
                request
                    .headers_mut()
                    .insert(header::ORIGIN, "http://127.0.0.1:8080".parse().unwrap());
                403
            }
            "not-json" => {
                request
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
                415
            }
            "too-large" => {
                *request.body_mut() = Body::from(" ".repeat(9000));
                413
            }
            "unknown-field" => {
                let mut extra = value.clone();
                extra["owner_contact"] = "changed".into();
                *request.body_mut() = Body::from(serde_json::to_vec(&extra).unwrap());
                400
            }
            _ => unreachable!(),
        };
        let response = send(&router, request).await;
        assert_eq!(response.status, expected, "{case}: {}", response.body);
        private_headers(&response.headers);
        assert_eq!(
            app.room.store().review(&agent.id).await.unwrap().state,
            "PENDING"
        );
    }
    let restarted = admin::router(app.room.clone(), AUTHORITY.parse().unwrap());
    let new_list = list(&restarted).await;
    assert_ne!(pending["csrf_token"], new_list["csrf_token"]);
    assert_eq!(send(&restarted, post(&value, token)).await.status, 403);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn admin_assets_are_exact_private_paths_and_disabled_by_default(pool: PgPool) {
    let (app, router, directory) = fixture(pool).await;
    for (path, content_type) in [
        ("/admin/", "text/html"),
        ("/admin/style.css", "text/css"),
        ("/admin/admin.js", "text/javascript"),
    ] {
        let response = send(&router, local("GET", path)).await;
        assert_eq!(response.status, 200, "{path}");
        assert!(
            response.headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with(content_type)
        );
        private_headers(&response.headers);
    }
    for path in [
        "/admin/api/unknown",
        "/admin/../private.key",
        "/admin/private.key",
    ] {
        let response = send(&router, local("GET", path)).await;
        assert_eq!(response.status, 404);
        private_headers(&response.headers);
    }
    std::fs::remove_file(directory.path().join("admin.js")).unwrap();
    let missing = send(&router, local("GET", "/admin/admin.js")).await;
    assert_eq!(missing.status, 503);
    private_headers(&missing.headers);
    let disabled = TestApp::new(app.room.store().pool().clone()).await;
    let router = admin::router(disabled.room.clone(), AUTHORITY.parse().unwrap());
    assert_eq!(
        send(&router, local("GET", "/admin/api/pending"))
            .await
            .status,
        404
    );
    let remote_bind = admin::router(app.room.clone(), "192.168.10.66:19091".parse().unwrap());
    assert_eq!(
        send(&remote_bind, local("GET", "/admin/api/pending"))
            .await
            .status,
        404
    );
}
