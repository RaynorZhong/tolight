mod support;
use serde_json::json;
use sqlx::PgPool;
use support::fake_agent::{FakeAgent, TestApp};
use tolight_core::{models::SendRequest, participant::ReadRequest};

fn request(id: &str, text: &str) -> SendRequest {
    SendRequest {
        client_message_id: id.into(),
        text: text.into(),
        mentions: vec![],
        reply_to_event_id: None,
    }
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn room_rules_are_identical_without_http_and_idempotency_survives_state_changes(
    pool: PgPool,
) {
    let app = TestApp::configured(pool, |config| config.messages_per_minute = 100).await;
    let alice = FakeAgent::new("alice", 81);
    let bob = FakeAgent::new("bobby", 82);
    alice.apply(&app).await;
    bob.apply(&app).await;
    app.approve("alice").await;
    app.approve("bobby").await;
    let context = alice.context();
    let mut first = request("first", "@bobby 你好 <script>纯文本</script>");
    first.reply_to_event_id = Some("missing".into());
    let published = app.room.send(&context, first).await.unwrap();
    assert_eq!(published.mentions, ["bobby"]);
    let usage = app.room.self_info(&context).await.unwrap();
    assert_eq!(usage["usage"]["minute_used"], 1);
    app.room.store().mute("alice", None, "other").await.unwrap();
    let muted = app.room.self_info(&context).await.unwrap();
    assert_eq!(muted["mute"]["until"], serde_json::Value::Null);
    assert_eq!(
        app.room
            .send(&context, request("muted", "不能公开"))
            .await
            .unwrap_err()
            .code,
        "agent_muted"
    );
    let duplicate = app
        .room
        .send(&context, request("first", "sk-abcdefghijklmnop123456"))
        .await
        .unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.event_id, published.event_id);
    assert_eq!(
        app.room.list_participants(&context).await.unwrap()["agents"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    app.room.store().unmute("alice").await.unwrap();
    app.room.store().pause(true, "other").await.unwrap();
    assert_eq!(
        app.room
            .send(&context, request("paused", "不能公开"))
            .await
            .unwrap_err()
            .code,
        "room_paused"
    );
    assert_eq!(
        app.room.self_info(&context).await.unwrap()["room_paused"],
        true
    );
    app.room.store().pause(false, "other").await.unwrap();
    let mut invalid = request("invalid", "普通正文");
    invalid.mentions = vec!["missing".into()];
    let result = app.room.send(&context, invalid).await.unwrap_err();
    assert_eq!(result.code, "invalid_mention");
    assert_eq!(result.fields["agent_id"], "missing");
    let repeated = app
        .room
        .send(&context, request("invalid", "改了正文仍不能改变首次结果"))
        .await
        .unwrap_err();
    assert_eq!(repeated.code, "invalid_mention");
    assert_eq!(repeated.fields["agent_id"], "missing");
    assert_eq!(
        app.room.self_info(&context).await.unwrap()["usage"]["minute_used"],
        2
    );
    let read = app
        .room
        .read(&bob.context(), ReadRequest::default())
        .await
        .unwrap();
    let message = read
        .events
        .iter()
        .find(|event| event["event_id"] == published.event_id)
        .unwrap();
    assert_eq!(message["reply_to"], serde_json::Value::Null);
    assert_eq!(message["mentions_you"], true);
    assert_eq!(message["from_self"], false);
    assert_eq!(message["text"], "@bobby 你好 <script>纯文本</script>");
    assert!(
        read.events
            .iter()
            .all(|event| event.get("owner_contact").is_none())
    );
    let rotated = FakeAgent::new("rotated", 83);
    app.room
        .store()
        .set_key("alice", &rotated.key())
        .await
        .unwrap();
    assert_eq!(
        app.room
            .send(&context, request("old-key", "不能公开"))
            .await
            .unwrap_err()
            .code,
        "unauthorized"
    );
    assert_eq!(
        app.room
            .read(&context, ReadRequest::default())
            .await
            .unwrap_err()
            .code,
        "unauthorized"
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn pending_reads_are_private_and_retraction_cannot_be_resurrected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let alice = FakeAgent::new("alice", 91);
    let bob = FakeAgent::new("bobby", 92);
    alice.apply(&app).await;
    app.approve("alice").await;
    bob.apply(&app).await;
    let published = app
        .room
        .send(&alice.context(), request("original", "公开内容"))
        .await
        .unwrap();
    let private = app
        .room
        .read(&bob.context(), ReadRequest::default())
        .await
        .unwrap();
    assert_eq!(private.timeline, "conformance");
    assert_eq!(private.head_seq, 3);
    assert_eq!(private.events.len(), 3);
    assert!(
        private
            .events
            .iter()
            .all(|event| event["agent"]["agent_id"] == "examiner")
    );
    let agents = app.room.list_participants(&bob.context()).await.unwrap();
    assert_eq!(agents["timeline"], "conformance");
    assert_eq!(
        agents["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|agent| agent["agent_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["bobby", "examiner"]
    );
    app.room
        .store()
        .retract(&published.event_id, "other")
        .await
        .unwrap();
    let replay = app
        .room
        .send(&alice.context(), request("original", "旧正文不可恢复"))
        .await
        .unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.event_id, published.event_id);
    assert!(replay.mentions.is_empty());
    let snapshot = app
        .room
        .read(&alice.context(), ReadRequest::default())
        .await
        .unwrap();
    let tombstone = snapshot
        .events
        .iter()
        .find(|event| event["event_id"] == published.event_id)
        .unwrap();
    assert_eq!(tombstone["retracted"], true);
    assert!(tombstone.get("text").is_none());
    assert_eq!(tombstone["mentions"], json!([]));
}
