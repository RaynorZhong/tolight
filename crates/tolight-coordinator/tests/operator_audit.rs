use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tolight_coordinator::operator;
use tolight_coordinator_store::{ApplicationInput, ApplyLimits, Store};

fn at(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

async fn actions(pool: &PgPool) {
    for (id, target, timestamp) in [
        ("audit-a", Some("alice"), "2026-11-15T01:00:00Z"),
        ("audit-b", Some("bob"), "2026-11-15T01:00:00Z"),
        ("audit-c", None, "2026-11-15T02:00:00Z"),
        ("audit-d", Some("alice"), "2026-11-15T03:00:00Z"),
    ] {
        sqlx::query("INSERT INTO operator_actions(action_id,kind,target_agent_id,target_application_id,target_event_id,target_public_key,reason_code,detail,created_at) VALUES($1,'test_action',$2,$3,'E-test','public-test-key','other',$4,$5)")
            .bind(id)
            .bind(target)
            .bind(uuid::Uuid::from_u128(1))
            .bind(json!({"note":"private diagnostic text"}))
            .bind(at(timestamp))
            .execute(pool)
            .await
            .unwrap();
    }
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn online_audit_filters_orders_and_reports_truncation_without_omissions(pool: PgPool) {
    actions(&pool).await;
    let store = Store::from_pool(pool);
    let page = operator::audit(&store, None, None, 2).await.unwrap();
    assert_eq!(page.schema, "tolight-operator-audit/1");
    assert!(page.truncated);
    assert_eq!(page.limit, 2);
    assert_eq!(page.records[0].action_id, "audit-d");
    assert_eq!(page.records[1].action_id, "audit-c");

    let complete = operator::audit(&store, None, None, 1000).await.unwrap();
    assert!(!complete.truncated);
    assert_eq!(
        complete
            .records
            .iter()
            .map(|row| row.action_id.as_str())
            .collect::<Vec<_>>(),
        ["audit-d", "audit-c", "audit-b", "audit-a"]
    );
    let row = &complete.records[0];
    assert_eq!(row.kind, "test_action");
    assert_eq!(row.target_agent_id.as_deref(), Some("alice"));
    assert_eq!(row.target_application_id, Some(uuid::Uuid::from_u128(1)));
    assert_eq!(row.target_event_id.as_deref(), Some("E-test"));
    assert_eq!(row.target_public_key.as_deref(), Some("public-test-key"));
    assert_eq!(row.reason_code.as_deref(), Some("other"));
    assert_eq!(row.detail, Some(json!({"note":"private diagnostic text"})));
    let filtered = operator::audit(
        &store,
        Some(at("2026-11-15T01:00:00Z")),
        Some("alice"),
        1000,
    )
    .await
    .unwrap();
    assert_eq!(filtered.records.len(), 2);
    assert_eq!(filtered.records[1].action_id, "audit-a");
    let after = operator::audit(
        &store,
        Some(at("2026-11-15T01:00:01Z")),
        Some("alice"),
        1000,
    )
    .await
    .unwrap();
    assert_eq!(after.records.len(), 1);
    assert!(
        operator::audit(&store, None, Some("unknown-agent"), 1000)
            .await
            .unwrap()
            .records
            .is_empty()
    );
}

async fn snapshot(pool: &PgPool) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('agents',(SELECT jsonb_agg(to_jsonb(a) ORDER BY agent_id) FROM agents a),'keys',(SELECT jsonb_agg(to_jsonb(k) ORDER BY public_key) FROM agent_keys k),'events',(SELECT jsonb_agg(to_jsonb(e) ORDER BY seq) FROM events e),'room',(SELECT to_jsonb(r) FROM room_state r WHERE singleton=TRUE),'attempts',(SELECT jsonb_agg(to_jsonb(m) ORDER BY agent_id,client_message_id) FROM message_attempts m),'runs',(SELECT jsonb_agg(to_jsonb(c) ORDER BY run_id) FROM conformance_runs c),'audit',(SELECT jsonb_agg(to_jsonb(o) ORDER BY action_id) FROM operator_actions o))")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn online_audit_preserves_expired_applications_and_business_state_while_serving(
    pool: PgPool,
) {
    let store = Store::from_pool(pool.clone());
    store
        .apply(
            ApplicationInput {
                agent_id: "expired-agent".into(),
                display_name: "Expired Agent".into(),
                display_name_key: "expired agent".into(),
                bio: "Pending diagnostic fixture".into(),
                owner_contact: "private-owner@example.invalid".into(),
                framework_label: None,
                public_key: tolight_core::signing::public_key_from_seed(&[201; 32]),
            },
            ApplyLimits::default(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE agents SET registered_at=clock_timestamp()-INTERVAL '30 days',last_seen_at=clock_timestamp()-INTERVAL '1 day' WHERE agent_id='expired-agent'")
        .execute(&pool).await.unwrap();
    actions(&pool).await;
    let before = snapshot(&pool).await;
    let lease = store.acquire_serve_lock().await.unwrap();
    assert_eq!(
        operator::audit(&store, None, None, 1000)
            .await
            .unwrap()
            .records
            .len(),
        4
    );
    for limit in [0, 10_001] {
        let error = operator::audit(&store, None, None, limit)
            .await
            .unwrap_err();
        assert_eq!(error.code, "bad_request");
        assert_eq!(error.fields["field"], "limit");
    }
    let error = operator::audit(&store, None, Some("bad_actor"), 1000)
        .await
        .unwrap_err();
    assert_eq!(error.fields["field"], "agent_id");
    assert_eq!(snapshot(&pool).await, before);
    assert!(store.agent("expired-agent").await.unwrap().is_some());
    drop(lease);

    sqlx::query("UPDATE schema_version SET version=999 WHERE singleton=TRUE")
        .execute(&pool)
        .await
        .unwrap();
    let before_mismatch = snapshot(&pool).await;
    assert!(operator::audit(&store, None, None, 1000).await.is_err());
    assert_eq!(snapshot(&pool).await, before_mismatch);
}
