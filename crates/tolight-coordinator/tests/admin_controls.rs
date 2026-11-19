mod support;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::BTreeSet,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use support::fake_agent::{FakeAgent, TestApp};
use time::{Duration, OffsetDateTime};
use tolight_coordinator::{admin, http, operator};
use tolight_coordinator_store::ReviewedControl;
use tower::ServiceExt;

const AUTHORITY: &str = "127.0.0.1:19092";
const ORIGIN: &str = "http://127.0.0.1:19092";

struct Fixture {
    app: TestApp,
    router: Router,
    clock: Arc<Mutex<OffsetDateTime>>,
    _assets: tempfile::TempDir,
}

impl Fixture {
    async fn new(pool: PgPool) -> Self {
        let assets = tempfile::tempdir().unwrap();
        std::fs::write(
            assets.path().join("admin.js"),
            "export const privateUi = true;\n",
        )
        .unwrap();
        // PostgreSQL stores microseconds; make exact threshold fixtures equally precise.
        let now = OffsetDateTime::now_utc();
        let now = now
            .replace_nanosecond(now.nanosecond() / 1_000 * 1_000)
            .unwrap();
        let clock = Arc::new(Mutex::new(now));
        let source = clock.clone();
        let app = TestApp::configured_clock(
            pool,
            |config| {
                config.admin.enabled = true;
                config.admin.assets_dir = assets.path().into();
                config.applications_per_ip_per_day = 100;
            },
            Arc::new(move || *source.lock().unwrap()),
        )
        .await;
        let router = admin::router(app.room.clone(), AUTHORITY.parse().unwrap());
        Self {
            app,
            router,
            clock,
            _assets: assets,
        }
    }

    async fn active(&self, id: &str, seed: u8) -> FakeAgent {
        let agent = FakeAgent::new(id, seed);
        assert_eq!(agent.apply(&self.app).await.status, 201);
        self.app.approve(id).await;
        agent
    }
}

fn local(method: &str, path: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, AUTHORITY)
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:41001".parse::<SocketAddr>().unwrap(),
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

async fn roster(router: &Router) -> Value {
    let response = send(router, local("GET", "/admin/api/agents")).await;
    assert_eq!(response.status, 200, "{}", response.body);
    private_headers(&response.headers);
    assert_eq!(response.body["schema"], "tolight-admin-agents/1");
    assert_eq!(response.body["online_threshold_seconds"], 120);
    response.body
}

fn view<'a>(roster: &'a Value, id: &str) -> &'a Value {
    roster["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["agent_id"] == id)
        .unwrap_or_else(|| panic!("missing active Agent {id}"))
}

fn decision(snapshot: &Value, action: &str) -> Value {
    let mut result = json!({
        "agent_id":snapshot["agent_id"],
        "application_id":snapshot["application_id"],
        "application_revision":snapshot["application_revision"],
        "public_key":snapshot["public_key"],
        "muted_until":snapshot["muted_until"],
        "muted_indefinitely":snapshot["muted_indefinitely"],
        "mute_reason":snapshot["mute_reason"],
        "action":action,
    });
    if matches!(action, "mute" | "kick") {
        result["reason"] = "spam".into();
    }
    if action == "mute" {
        result["mute_seconds"] = 60.into();
    }
    result
}

fn post(value: &Value, token: &str) -> Request<Body> {
    let mut request = local("POST", "/admin/api/control");
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

async fn action_count(app: &TestApp) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM operator_actions WHERE kind IN ('mute','unmute','kick')",
    )
    .fetch_one(app.room.store().pool())
    .await
    .unwrap()
}

async fn await_row_wait(pool: &PgPool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FROM agents WHERE agent_id=$1 FOR NO KEY UPDATE%'")
            .fetch_one(pool).await.unwrap();
        if waiting > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reviewed control must actually wait on the held Agent row"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn active_roster_reports_precise_server_time_activity_without_pending_or_left_agents(
    pool: PgPool,
) {
    let fixture = Fixture::new(pool).await;
    let now = fixture.app.room.now();
    for (id, seed, last_seen, expected) in [
        (
            "control-recent",
            131,
            Some(now - Duration::seconds(1)),
            "online",
        ),
        (
            "control-boundary",
            132,
            Some(now - Duration::seconds(120)),
            "online",
        ),
        (
            "control-idle",
            133,
            Some(now - Duration::seconds(120) - Duration::milliseconds(1)),
            "offline",
        ),
        ("control-unseen", 134, None, "unknown"),
        (
            "control-future",
            135,
            Some(now + Duration::milliseconds(1)),
            "unknown",
        ),
    ] {
        fixture.active(id, seed).await;
        sqlx::query("UPDATE agents SET last_seen_at=$2 WHERE agent_id=$1")
            .bind(id)
            .bind(last_seen)
            .execute(fixture.app.room.store().pool())
            .await
            .unwrap();
        let current = roster(&fixture.router).await;
        assert_eq!(
            current["server_time"],
            tolight_core::datetime::json_timestamp(now),
            "private JSON timestamps retain RFC3339 strings and exact clock precision"
        );
        let agent = view(&current, id);
        assert_eq!(agent["presence"], expected, "{id}");
        if expected == "unknown" {
            assert!(agent["last_seen_age_seconds"].is_null());
        } else {
            assert!(agent["last_seen_age_seconds"].as_u64().is_some());
        }
    }
    FakeAgent::new("control-pending", 136)
        .apply(&fixture.app)
        .await;
    let departed = fixture.active("control-departed", 137).await;
    operator::kick(
        fixture.app.room.store(),
        fixture.app.room.config(),
        &departed.id,
        "other",
    )
    .await
    .unwrap();
    let current = roster(&fixture.router).await;
    assert_eq!(current["agents"].as_array().unwrap().len(), 5);
    assert_eq!(
        current["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|agent| agent["agent_id"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "control-recent",
            "control-boundary",
            "control-idle",
            "control-unseen",
            "control-future",
        ])
    );
    let required = BTreeSet::from([
        "agent_id",
        "application_id",
        "application_revision",
        "display_name",
        "bio",
        "owner_contact",
        "framework_label",
        "public_key",
        "registered_at",
        "joined_at",
        "last_seen_at",
        "muted_until",
        "muted_indefinitely",
        "mute_reason",
        "muted",
        "presence",
        "last_seen_age_seconds",
    ]);
    for agent in current["agents"].as_array().unwrap() {
        let fields = agent
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            required, fields,
            "active roster is an explicit private view"
        );
        assert_eq!(agent["owner_contact"], "owner@example.invalid");
        assert!(agent["joined_at"].is_string());
        assert_eq!(agent["muted"], false);
    }
    let pending = send(&fixture.router, local("GET", "/admin/api/pending")).await;
    assert_eq!(pending.status, 200);
    assert_eq!(
        pending.body["server_time"],
        tolight_core::datetime::json_timestamp(now)
    );
    assert_eq!(pending.body["csrf_token"], current["csrf_token"]);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn finite_and_indefinite_controls_change_real_sending_rules_and_write_audit(pool: PgPool) {
    let fixture = Fixture::new(pool).await;
    let agent = fixture.active("control-muted", 138).await;
    let current = roster(&fixture.router).await;
    let before = OffsetDateTime::now_utc();
    let muted = send(
        &fixture.router,
        post(
            &decision(view(&current, &agent.id), "mute"),
            current["csrf_token"].as_str().unwrap(),
        ),
    )
    .await;
    assert_eq!(muted.status, 200, "{}", muted.body);
    private_headers(&muted.headers);
    assert_eq!(muted.body["state"], "ACTIVE");
    assert_eq!(muted.body["muted_indefinitely"], false);
    assert_eq!(muted.body["mute_reason"], "spam");
    let until =
        tolight_core::datetime::parse_rfc3339(muted.body["muted_until"].as_str().unwrap()).unwrap();
    assert!(until >= before + Duration::seconds(60));
    assert!(until <= OffsetDateTime::now_utc() + Duration::seconds(60));
    let blocked = agent.send(&fixture.app, "一条受限的公开消息").await;
    assert_eq!(blocked.status, 403);
    assert_eq!(blocked.body["error"]["code"], "agent_muted");
    let current = roster(&fixture.router).await;
    assert_eq!(view(&current, &agent.id)["muted"], true);
    let unmuted = send(
        &fixture.router,
        post(
            &decision(view(&current, &agent.id), "unmute"),
            current["csrf_token"].as_str().unwrap(),
        ),
    )
    .await;
    assert_eq!(unmuted.status, 200, "{}", unmuted.body);
    assert!(unmuted.body["muted_until"].is_null());
    assert_eq!(unmuted.body["muted_indefinitely"], false);
    assert!(unmuted.body["mute_reason"].is_null());
    assert_eq!(
        agent
            .send(&fixture.app, "解除限制后的公开消息")
            .await
            .status,
        201
    );
    let current = roster(&fixture.router).await;
    let mut indefinite = decision(view(&current, &agent.id), "mute");
    indefinite["mute_seconds"] = Value::Null;
    indefinite["reason"] = "runaway".into();
    let muted = send(
        &fixture.router,
        post(&indefinite, current["csrf_token"].as_str().unwrap()),
    )
    .await;
    assert_eq!(muted.status, 200, "{}", muted.body);
    assert_eq!(muted.body["muted_indefinitely"], true);
    assert!(muted.body["muted_until"].is_null());
    assert_eq!(
        agent.send(&fixture.app, "无限期禁言中的消息").await.status,
        403
    );
    let current = roster(&fixture.router).await;
    let unmuted = send(
        &fixture.router,
        post(
            &decision(view(&current, &agent.id), "unmute"),
            current["csrf_token"].as_str().unwrap(),
        ),
    )
    .await;
    assert_eq!(unmuted.status, 200);
    let actions: Vec<String> = sqlx::query_scalar("SELECT kind FROM operator_actions WHERE kind IN ('mute','unmute','kick') ORDER BY created_at")
        .fetch_all(fixture.app.room.store().pool()).await.unwrap();
    assert_eq!(actions, ["mute", "unmute", "mute", "unmute"]);
    let public_controls: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE kind NOT IN ('agent_joined','agent_message')",
    )
    .fetch_one(fixture.app.room.store().pool())
    .await
    .unwrap();
    assert_eq!(
        public_controls, 0,
        "mute/unmute do not publish private control details"
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn roster_mute_expiry_uses_effective_state_and_preserves_raw_confirmation_basis(
    pool: PgPool,
) {
    let fixture = Fixture::new(pool).await;
    let agent = fixture.active("control-expiry", 139).await;
    let until = fixture.app.room.now() + Duration::seconds(30);
    operator::mute(
        fixture.app.room.store(),
        fixture.app.room.config(),
        &agent.id,
        Some(until),
        "spam",
    )
    .await
    .unwrap();
    let current = roster(&fixture.router).await;
    let raw_until = view(&current, &agent.id)["muted_until"].clone();
    assert_eq!(view(&current, &agent.id)["muted"], true);
    let exact_until = tolight_core::datetime::parse_rfc3339(raw_until.as_str().unwrap()).unwrap();
    *fixture.clock.lock().unwrap() = exact_until;
    let expired = roster(&fixture.router).await;
    let view = view(&expired, &agent.id);
    assert_eq!(view["muted"], false);
    assert_eq!(view["muted_until"], raw_until);
    assert_eq!(view["mute_reason"], "spam");
    assert_eq!(view["muted_indefinitely"], false);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn kick_revokes_all_keys_and_commits_one_leave_event_with_one_audit(pool: PgPool) {
    let fixture = Fixture::new(pool).await;
    let original = fixture.active("control-kicked", 140).await;
    let current_key = FakeAgent::new(&original.id, 141);
    operator::set_key(
        fixture.app.room.store(),
        fixture.app.room.config(),
        &original.id,
        &current_key.key(),
        "owner_request",
    )
    .await
    .unwrap();
    assert_eq!(
        current_key
            .request(&fixture.app, "GET", "self", "", None)
            .await
            .status,
        200
    );
    let current = roster(&fixture.router).await;
    let body = decision(view(&current, &original.id), "kick");
    let token = current["csrf_token"].as_str().unwrap();
    // Fail the final audit insert after the leave event and key revocations.
    // The HTTP failure must roll every earlier write back, including sequence allocation.
    sqlx::query("ALTER TABLE operator_actions ADD CONSTRAINT test_control_audit_failure CHECK (kind <> 'kick')")
        .execute(fixture.app.room.store().pool()).await.unwrap();
    let before_sequence = fixture
        .app
        .room
        .store()
        .room_state()
        .await
        .unwrap()
        .last_seq;
    let failed = send(&fixture.router, post(&body, token)).await;
    assert_eq!(failed.status, 503);
    private_headers(&failed.headers);
    assert_eq!(
        fixture
            .app
            .room
            .store()
            .review(&original.id)
            .await
            .unwrap()
            .state,
        "ACTIVE"
    );
    assert!(
        fixture
            .app
            .room
            .store()
            .agent_by_key(&current_key.key())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        fixture
            .app
            .room
            .store()
            .room_state()
            .await
            .unwrap()
            .last_seq,
        before_sequence
    );
    assert_eq!(action_count(&fixture.app).await, 0);
    let failed_events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE kind='agent_left'")
            .fetch_one(fixture.app.room.store().pool())
            .await
            .unwrap();
    assert_eq!(failed_events, 0);
    sqlx::query("ALTER TABLE operator_actions DROP CONSTRAINT test_control_audit_failure")
        .execute(fixture.app.room.store().pool())
        .await
        .unwrap();
    let kicked = send(&fixture.router, post(&body, token)).await;
    assert_eq!(kicked.status, 200, "{}", kicked.body);
    assert_eq!(kicked.body["state"], "LEFT");
    assert!(kicked.body["muted_until"].is_null());
    assert_eq!(kicked.body["muted_indefinitely"], false);
    assert_eq!(
        fixture
            .app
            .room
            .store()
            .review(&original.id)
            .await
            .unwrap()
            .state,
        "LEFT"
    );
    assert!(
        roster(&fixture.router).await["agents"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for agent in [&original, &current_key] {
        assert_eq!(
            agent.send(&fixture.app, "被踢出后禁止写入").await.status,
            401
        );
        assert_eq!(
            agent
                .request(&fixture.app, "GET", "self", "", None)
                .await
                .status,
            401
        );
    }
    let (keys, revoked): (i64, i64) = sqlx::query_as("SELECT count(*),count(*) FILTER (WHERE revoked_at IS NOT NULL) FROM agent_keys WHERE agent_id=$1")
        .bind(&original.id).fetch_one(fixture.app.room.store().pool()).await.unwrap();
    assert_eq!(keys, 2);
    assert_eq!(keys, revoked);
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE kind='agent_left' AND agent_id=$1")
            .bind(&original.id)
            .fetch_one(fixture.app.room.store().pool())
            .await
            .unwrap();
    assert_eq!(events, 1);
    assert_eq!(action_count(&fixture.app).await, 1);
    let audit: (String, Option<uuid::Uuid>, Option<String>) = sqlx::query_as(
        "SELECT kind,target_application_id,reason_code FROM operator_actions WHERE kind='kick'",
    )
    .fetch_one(fixture.app.room.store().pool())
    .await
    .unwrap();
    assert_eq!(audit.0, "kick");
    assert_eq!(
        audit.1.unwrap().to_string(),
        current["agents"][0]["application_id"].as_str().unwrap()
    );
    assert_eq!(audit.2.as_deref(), Some("spam"));
    let duplicate = send(&fixture.router, post(&body, token)).await;
    assert_eq!(duplicate.status, 409);
    assert_eq!(duplicate.body["error"]["code"], "review_changed");
    assert_eq!(action_count(&fixture.app).await, 1);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn invalid_controls_cannot_change_state_or_add_control_audit(pool: PgPool) {
    let fixture = Fixture::new(pool).await;
    let agent = fixture.active("control-invalid", 142).await;
    let current = roster(&fixture.router).await;
    let good = decision(view(&current, &agent.id), "mute");
    let token = current["csrf_token"].as_str().unwrap();
    for (field, invalid) in [
        ("mute_seconds", json!(0)),
        ("mute_seconds", json!(31536001_u64)),
        ("mute_seconds", json!(-1)),
        ("mute_seconds", json!(0.5)),
        ("mute_seconds", json!("60")),
        ("reason", json!("unknown-reason")),
        ("action", json!("delete")),
        ("owner_contact", json!("must-not-be-accepted")),
    ] {
        let mut body = good.clone();
        body[field] = invalid;
        let response = send(&fixture.router, post(&body, token)).await;
        assert_eq!(response.status, 400, "{field}: {}", response.body);
        private_headers(&response.headers);
    }
    let agent = fixture.app.room.store().review(&agent.id).await.unwrap();
    assert_eq!(agent.state, "ACTIVE");
    assert!(agent.muted_until.is_none());
    assert!(!agent.muted_indefinitely);
    assert_eq!(action_count(&fixture.app).await, 0);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn controls_reject_wrong_lifecycle_and_every_stale_identity_or_mute_basis(pool: PgPool) {
    let fixture = Fixture::new(pool).await;
    let agent = FakeAgent::new("control-cas", 143);
    agent.apply(&fixture.app).await;
    let empty = roster(&fixture.router).await;
    let token = empty["csrf_token"].as_str().unwrap();
    let pending =
        serde_json::to_value(fixture.app.room.store().review(&agent.id).await.unwrap()).unwrap();
    assert_eq!(
        send(&fixture.router, post(&decision(&pending, "kick"), token))
            .await
            .status,
        409
    );
    fixture.app.approve(&agent.id).await;
    let current = roster(&fixture.router).await;
    let original = view(&current, &agent.id);
    let good = decision(original, "kick");
    for (field, bad) in [
        (
            "application_id",
            json!("00000000-0000-4000-8000-000000000000"),
        ),
        (
            "application_revision",
            json!(original["application_revision"].as_i64().unwrap() + 1),
        ),
        ("public_key", json!(FakeAgent::new("other-key", 144).key())),
    ] {
        let mut body = good.clone();
        body[field] = bad;
        let result = send(&fixture.router, post(&body, token)).await;
        assert_eq!(result.status, 409);
        assert_eq!(result.body["error"]["code"], "review_changed");
    }
    operator::mute(
        fixture.app.room.store(),
        fixture.app.room.config(),
        &agent.id,
        None,
        "spam",
    )
    .await
    .unwrap();
    let muted = roster(&fixture.router).await;
    assert_eq!(
        view(&muted, &agent.id)["application_revision"],
        original["application_revision"],
        "CLI mute changes no application revision; raw control basis must still be checked"
    );
    let before = action_count(&fixture.app).await;
    for action in ["mute", "unmute", "kick"] {
        let result = send(&fixture.router, post(&decision(original, action), token)).await;
        assert_eq!(result.status, 409, "stale unmuted {action}");
        assert_eq!(result.body["error"]["code"], "review_changed");
    }
    operator::mute(
        fixture.app.room.store(),
        fixture.app.room.config(),
        &agent.id,
        None,
        "runaway",
    )
    .await
    .unwrap();
    for action in ["mute", "unmute", "kick"] {
        let result = send(
            &fixture.router,
            post(&decision(view(&muted, &agent.id), action), token),
        )
        .await;
        assert_eq!(result.status, 409, "stale reason {action}");
        assert_eq!(result.body["error"]["code"], "review_changed");
    }
    let new_reason = roster(&fixture.router).await;
    operator::unmute(
        fixture.app.room.store(),
        fixture.app.room.config(),
        &agent.id,
    )
    .await
    .unwrap();
    for action in ["mute", "unmute", "kick"] {
        let result = send(
            &fixture.router,
            post(&decision(view(&new_reason, &agent.id), action), token),
        )
        .await;
        assert_eq!(result.status, 409, "stale muted {action}");
        assert_eq!(result.body["error"]["code"], "review_changed");
    }
    assert_eq!(
        action_count(&fixture.app).await,
        before + 2,
        "only explicit CLI mutations added audit rows"
    );
    let fresh = roster(&fixture.router).await;
    assert_eq!(
        send(
            &fixture.router,
            post(&decision(view(&fresh, &agent.id), "kick"), token)
        )
        .await
        .status,
        200
    );
    let left = fixture.app.room.store().review(&agent.id).await.unwrap();
    assert!(
        left.public_key.is_none(),
        "LEFT no longer has an active key"
    );
    let denied = operator::control_reviewed(
        fixture.app.room.store(),
        fixture.app.room.config(),
        &left,
        ReviewedControl::Unmute,
    )
    .await
    .unwrap_err();
    assert_eq!(denied.code, "invalid_state");
    let mut left_body = decision(&serde_json::to_value(&left).unwrap(), "mute");
    left_body["public_key"] = agent.key().into();
    let denied = send(&fixture.router, post(&left_body, token)).await;
    assert_eq!(denied.status, 409);
    assert_eq!(denied.body["error"]["code"], "review_changed");
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn reviewed_control_wait_rechecks_raw_state_and_starts_duration_after_the_row_lock(
    pool: PgPool,
) {
    let fixture = Fixture::new(pool).await;
    let agent = fixture.active("control-row-lock", 145).await;
    let snapshot = fixture.app.room.store().review(&agent.id).await.unwrap();
    let mut blocker = fixture.app.room.store().pool().begin().await.unwrap();
    sqlx::query("SELECT agent_id FROM agents WHERE agent_id=$1 FOR NO KEY UPDATE")
        .bind(&agent.id)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let room = fixture.app.room.clone();
    let control = tokio::spawn(async move {
        operator::control_reviewed(
            room.store(),
            room.config(),
            &snapshot,
            ReviewedControl::Kick {
                reason: "other".into(),
            },
        )
        .await
    });
    await_row_wait(fixture.app.room.store().pool()).await;
    sqlx::query("UPDATE agents SET muted_indefinitely=true,mute_reason='spam' WHERE agent_id=$1")
        .bind(&agent.id)
        .execute(&mut *blocker)
        .await
        .unwrap();
    blocker.commit().await.unwrap();
    assert_eq!(control.await.unwrap().unwrap_err().code, "review_changed");
    assert_eq!(action_count(&fixture.app).await, 0);
    assert_eq!(
        fixture
            .app
            .room
            .store()
            .review(&agent.id)
            .await
            .unwrap()
            .state,
        "ACTIVE"
    );

    let snapshot = fixture.app.room.store().review(&agent.id).await.unwrap();
    let mut blocker = fixture.app.room.store().pool().begin().await.unwrap();
    sqlx::query("SELECT agent_id FROM agents WHERE agent_id=$1 FOR NO KEY UPDATE")
        .bind(&agent.id)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let room = fixture.app.room.clone();
    let control = tokio::spawn(async move {
        operator::control_reviewed(
            room.store(),
            room.config(),
            &snapshot,
            ReviewedControl::Mute {
                seconds: Some(3),
                reason: "other".into(),
            },
        )
        .await
    });
    await_row_wait(fixture.app.room.store().pool()).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let before_unlock: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    blocker.commit().await.unwrap();
    let controlled = control.await.unwrap().unwrap();
    assert!(
        controlled.muted_until.unwrap() >= before_unlock + Duration::seconds(3),
        "relative mute starts after the row lock, not before waiting"
    );
    assert!(!controlled.muted_indefinitely);
    assert_eq!(controlled.mute_reason.as_deref(), Some("other"));
    assert_eq!(action_count(&fixture.app).await, 1);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn new_control_routes_preserve_local_origin_csrf_and_participant_isolation(pool: PgPool) {
    let fixture = Fixture::new(pool).await;
    let agent = fixture.active("control-guard", 146).await;
    let current = roster(&fixture.router).await;
    let body = decision(view(&current, &agent.id), "kick");
    let token = current["csrf_token"].as_str().unwrap();
    for case in [
        "no-peer",
        "remote-peer",
        "host",
        "origin",
        "csrf",
        "json",
        "large",
    ] {
        let mut request = post(&body, token);
        let expected = match case {
            "no-peer" => {
                request.extensions_mut().remove::<ConnectInfo<SocketAddr>>();
                403
            }
            "remote-peer" => {
                request.extensions_mut().insert(ConnectInfo(
                    "192.168.10.20:42000".parse::<SocketAddr>().unwrap(),
                ));
                403
            }
            "host" => {
                request
                    .headers_mut()
                    .insert(header::HOST, "attacker.invalid:19092".parse().unwrap());
                403
            }
            "origin" => {
                request
                    .headers_mut()
                    .insert(header::ORIGIN, "http://127.0.0.1:8080".parse().unwrap());
                403
            }
            "csrf" => {
                request.headers_mut().remove("x-tolight-csrf");
                403
            }
            "json" => {
                request
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
                415
            }
            "large" => {
                *request.body_mut() = Body::from(" ".repeat(9000));
                413
            }
            _ => unreachable!(),
        };
        let response = send(&fixture.router, request).await;
        assert_eq!(response.status, expected, "{case}: {}", response.body);
        private_headers(&response.headers);
    }
    let mut foreign = local("GET", "/admin/api/agents");
    foreign
        .headers_mut()
        .insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert_eq!(send(&fixture.router, foreign).await.status, 403);
    assert_eq!(action_count(&fixture.app).await, 0);
    let disabled = TestApp::new(fixture.app.room.store().pool().clone()).await;
    let disabled = admin::router(disabled.room.clone(), AUTHORITY.parse().unwrap());
    for (method, path) in [("GET", "/admin/api/agents"), ("POST", "/admin/api/control")] {
        assert_eq!(send(&disabled, local(method, path)).await.status, 404);
        let response = send(&http::router(fixture.app.room.clone()), local(method, path)).await;
        assert_eq!(response.status, 401);
        assert!(!String::from_utf8_lossy(&response.bytes).contains("owner@example.invalid"));
    }
}
