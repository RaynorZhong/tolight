use serde_json::Value;
use sqlx::PgPool;
use std::time::Duration;
use tolight_coordinator::{operator, room::RoomService, service};
use tolight_coordinator_store::Store;
use tolight_core::{
    config::Config,
    models::{AgentContext, ApplicationInput, SendRequest},
    participant::ReadRequest,
    signing,
};

async fn manifest(path: &std::path::Path, head: i64) -> Value {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if let Ok(bytes) = tokio::fs::read(path).await {
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                if value["newest_seq"] == head {
                    return value;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn runtime_publishes_external_controls_and_coalesces_messages(pool: PgPool) {
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = Config {
        listen: address,
        participant_hosts: vec![address.to_string()],
        messages_per_minute: 100,
        ..Config::default()
    };
    config.public.dir = directory.path().join("public");
    config.observability.listen = "127.0.0.1:0".parse().unwrap();
    config.observability.log_dir = directory.path().join("logs");
    config.public.demo_banner = true;
    let room = RoomService::new(Store::from_pool(pool), config);
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running = room.clone();
    let task = tokio::spawn(async move {
        service::serve_on(listener, running, async {
            let _ = stopped.await;
        })
        .await
    });
    let path = room.config().public.dir.join("data/manifest.json");
    assert_eq!(manifest(&path, 0).await["demo"], true);
    let key = signing::public_key_from_seed(&[71; 32]);
    operator::add(
        room.store(),
        room.config(),
        ApplicationInput {
            agent_id: "alice".into(),
            display_name: "Alice".into(),
            bio: "脚本测试".into(),
            owner_contact: "owner@example.invalid".into(),
            framework_label: "脚本".into(),
        },
        &key,
    )
    .await
    .unwrap();
    let context = AgentContext {
        agent_id: "alice".into(),
        public_key: key,
    };
    // This runtime test exercises approval switching, not the conformance engine.
    sqlx::query("UPDATE agents SET conformance_passed_at=clock_timestamp(),conformance_suite_version='1',application_revision=application_revision+1 WHERE agent_id='alice'").execute(room.store().pool()).await.unwrap();
    let start = room.read(&context, ReadRequest::default()).await.unwrap();
    let waiting = room.read(
        &context,
        ReadRequest {
            after: Some(start.next_cursor),
            wait: 5,
            ..ReadRequest::default()
        },
    );
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut waiting)
            .await
            .is_err()
    );
    let review = operator::review(room.store(), room.config(), "alice")
        .await
        .unwrap();
    // No room.notify_change(): this is exactly the other-process Operator path.
    operator::approve(room.store(), room.config(), &review)
        .await
        .unwrap();
    let approved = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(approved.cursor_reset);
    assert_eq!(approved.timeline, "public");
    manifest(&path, 1).await;
    let before = room.publication_count();
    for index in 0..10 {
        room.send(
            &context,
            SendRequest {
                client_message_id: format!("rapid-{index}"),
                text: "本地快速发言".into(),
                mentions: vec![],
                reply_to_event_id: None,
            },
        )
        .await
        .unwrap();
    }
    manifest(&path, 11).await;
    assert!(room.publication_count() - before < 10);
    room.store().pause(true, "other").await.unwrap();
    assert_eq!(manifest(&path, 12).await["paused"], true);
    let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let duplicate = service::serve_on(second, room.clone(), std::future::pending())
        .await
        .unwrap_err();
    assert!(duplicate.to_string().contains("already_running"));
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn schema_mismatch_refuses_to_serve(pool: PgPool) {
    sqlx::query("UPDATE schema_version SET version=999")
        .execute(&pool)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.public.dir = directory.path().join("public");
    config.observability.listen = "127.0.0.1:0".parse().unwrap();
    config.observability.log_dir = directory.path().join("logs");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let error = service::serve_on(
        listener,
        RoomService::new(Store::from_pool(pool), config),
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("schema_mismatch"));
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn losing_the_dedicated_serve_lease_terminates_the_runtime(pool: PgPool) {
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = Config {
        listen: address,
        participant_hosts: vec![address.to_string()],
        ..Config::default()
    };
    config.public.dir = directory.path().join("public");
    config.observability.listen = "127.0.0.1:0".parse().unwrap();
    config.observability.log_dir = directory.path().join("logs");
    let store = Store::from_pool(pool.clone());
    let path = config.public.dir.join("data/manifest.json");
    let room = RoomService::new(store, config);
    let task =
        tokio::spawn(
            async move { service::serve_on(listener, room, std::future::pending()).await },
        );
    manifest(&path, 0).await;
    let lock = tolight_coordinator_store::SERVE_LOCK;
    let pid:i32=sqlx::query_scalar("SELECT pid FROM pg_locks WHERE locktype='advisory' AND granted AND classid::bigint=$1 AND objid::bigint=$2 AND database=(SELECT oid FROM pg_database WHERE datname=current_database())")
        .bind(lock>>32).bind(lock & 0xffff_ffff).fetch_one(&pool).await.unwrap();
    let killed: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(killed);
    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
}
