mod support;

use serde_json::json;
use sqlx::PgPool;
use support::fake_agent::{FakeAgent, TestApp};
use time::{Duration, OffsetDateTime};
use tolight_coordinator::publisher;

async fn app(pool: PgPool) -> TestApp {
    TestApp::configured(pool, |config| {
        config.messages_per_minute = 100;
        config.messages_per_day = 1000;
    })
    .await
}

// Only the HTTP adapter is running in these tests. Production serve's offline
// lock and real physical recovery are exercised by separate runtime/PITR tests.
#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn matching_public_anchor_does_not_replace_private_control_verification(pool: PgPool) {
    let app = app(pool.clone()).await;
    let old = FakeAgent::new("alice", 161);
    assert_eq!(old.apply(&app).await.status, 201);
    app.approve("alice").await;
    assert_eq!(old.send(&app, "before recovery point").await.status, 201);
    let baseline = old.request(&app, "GET", "events", "limit=0", None).await;
    let cursor = baseline.body["next_cursor"].as_str().unwrap();
    let store = app.room.store();
    publisher::publish(store, app.room.config(), true)
        .await
        .unwrap();
    let before_agents =
        std::fs::read(app.room.config().public.dir.join("data/agents.json")).unwrap();
    let before_manifest =
        std::fs::read(app.room.config().public.dir.join("data/manifest.json")).unwrap();
    let new = FakeAgent::new("alice", 162);
    store.set_key("alice", &new.key()).await.unwrap();
    let until = OffsetDateTime::now_utc() + Duration::hours(1);
    store.mute("alice", Some(until), "spam").await.unwrap();
    store.applications_open(false).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let private = directory.path().join("current-controls.json");
    store
        .export_controls(
            &private,
            &app.room.config().public.dir,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let bytes = std::fs::read(&private).unwrap();

    // Simulate the older private state at T without moving the public anchor.
    sqlx::query("UPDATE agent_keys SET revoked_at=clock_timestamp() WHERE public_key=$1")
        .bind(new.key())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE agent_keys SET revoked_at=NULL WHERE public_key=$1")
        .bind(old.key())
        .execute(&pool)
        .await
        .unwrap();
    store.unmute("alice").await.unwrap();
    store.applications_open(true).await.unwrap();
    publisher::validate_manifest(store, app.room.config())
        .await
        .unwrap();
    let prepared = store.prepare_restore_controls(&bytes, "1").await.unwrap();
    assert_eq!(prepared.plan().source_anchor, prepared.plan().target_anchor);
    assert!(prepared.plan().required_approvals.is_empty());
    prepared.commit(&[]).await.unwrap();
    publisher::publish(store, app.room.config(), true)
        .await
        .unwrap();

    // Equal public bytes do not mean the old key or old mute were safe.
    assert_eq!(
        std::fs::read(app.room.config().public.dir.join("data/agents.json")).unwrap(),
        before_agents
    );
    assert_eq!(
        std::fs::read(app.room.config().public.dir.join("data/manifest.json")).unwrap(),
        before_manifest
    );
    assert_eq!(old.request(&app, "GET", "self", "", None).await.status, 401);
    let current = new.request(&app, "GET", "self", "", None).await;
    assert_eq!(current.status, 200);
    assert_eq!(
        current.body["mute"]["until"],
        tolight_core::datetime::utc_seconds(until)
    );
    assert!(!store.room_state().await.unwrap().applications_open);
    let query = serde_urlencoded::to_string([("after", cursor), ("limit", "1")]).unwrap();
    let resumed = new.request(&app, "GET", "events", &query, None).await;
    assert_eq!(resumed.status, 200);
    assert_eq!(resumed.body["cursor_reset"], false);
    let attempts = store.attempts("alice").await.unwrap().len();
    let duplicate = new
        .send(&app, "same old ID must not publish new text")
        .await;
    assert_eq!(duplicate.status, 200);
    assert_eq!(duplicate.body["duplicate"], true);
    let blocked = new
        .request(
            &app,
            "POST",
            "messages",
            "",
            Some(json!({
                "client_message_id": "fresh-id-after-restore",
                "text": "must not publish after restore"
            })),
        )
        .await;
    assert_eq!(blocked.status, 403);
    assert_eq!(blocked.body["error"]["code"], "agent_muted");
    assert_eq!(store.attempts("alice").await.unwrap().len(), attempts);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn recovered_timeline_regrown_past_old_head_still_resets_lost_cursors(pool: PgPool) {
    let app = app(pool.clone()).await;
    let alice = FakeAgent::new("alice", 163);
    assert_eq!(alice.apply(&app).await.status, 201);
    app.approve("alice").await;
    let kept = alice.send(&app, "kept before T").await;
    let kept_cursor = alice
        .request(&app, "GET", "events", "limit=0", None)
        .await
        .body["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let lost = alice.send(&app, "unique-public-message-lost-after-T").await;
    let lost_cursor = alice
        .request(&app, "GET", "events", "limit=0", None)
        .await
        .body["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let store = app.room.store();
    publisher::publish(store, app.room.config(), true)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let private = directory.path().join("current-controls.json");
    store
        .export_controls(
            &private,
            &app.room.config().public.dir,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let bytes = std::fs::read(&private).unwrap();
    let at_t = kept.body["seq"].as_i64().unwrap();
    sqlx::query("DELETE FROM message_attempts WHERE event_seq>$1")
        .bind(at_t)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM events WHERE seq>$1")
        .bind(at_t)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE room_state SET last_seq=$1 WHERE singleton")
        .bind(at_t)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        publisher::validate_manifest(store, app.room.config())
            .await
            .is_err()
    );
    store
        .prepare_restore_controls(&bytes, "1")
        .await
        .unwrap()
        .commit(&[])
        .await
        .unwrap();
    let replacement = alice.send(&app, "replacement event after restored T").await;
    let later = alice.send(&app, "later event beyond previous head").await;
    assert_eq!(replacement.body["seq"], lost.body["seq"]);
    assert_ne!(replacement.body["event_id"], lost.body["event_id"]);
    assert!(later.body["seq"].as_i64().unwrap() > lost.body["seq"].as_i64().unwrap());
    // Matching or exceeding the old sequence alone cannot admit this tree.
    assert!(
        publisher::validate_manifest(store, app.room.config())
            .await
            .is_err()
    );
    publisher::publish(store, app.room.config(), true)
        .await
        .unwrap();
    publisher::validate_manifest(store, app.room.config())
        .await
        .unwrap();
    let query =
        serde_urlencoded::to_string([("after", lost_cursor.as_str()), ("limit", "50")]).unwrap();
    let reset = alice.request(&app, "GET", "events", &query, None).await;
    assert_eq!(reset.status, 200);
    assert_eq!(reset.body["cursor_reset"], true);
    let query =
        serde_urlencoded::to_string([("after", kept_cursor.as_str()), ("limit", "50")]).unwrap();
    let continued = alice.request(&app, "GET", "events", &query, None).await;
    assert_eq!(continued.body["cursor_reset"], false);
    assert_eq!(continued.body["events"].as_array().unwrap().len(), 2);
    let visible = serde_json::to_string(&json!({
        "snapshot": store.public_snapshot().await.unwrap().events,
        "continued": continued.body,
        "reset": reset.body,
    }))
    .unwrap();
    assert!(!visible.contains("unique-public-message-lost-after-T"));
    for path in std::fs::read_dir(app.room.config().public.dir.join("data/chunks")).unwrap() {
        assert!(
            !std::fs::read_to_string(path.unwrap().path())
                .unwrap()
                .contains("unique-public-message-lost-after-T")
        );
    }
}
