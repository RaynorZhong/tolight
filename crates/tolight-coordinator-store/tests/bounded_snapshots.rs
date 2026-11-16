mod support;
use sqlx::PgPool;
use support::*;
use tolight_coordinator_store::Store;
use tolight_core::{
    config::Config,
    participant::{ReadRequest, encode_cursor, read_events},
};

#[sqlx::test(migrations = "./migrations")]
async fn bounded_participant_candidates_preserve_context_cursor_and_mention_semantics(
    pool: PgPool,
) {
    let store = Store::from_pool(pool);
    let alice = active(&store, "alice", 1).await;
    let bobby = active(&store, "bobby", 2).await;
    for n in 0..430 {
        let sender = if n % 11 == 0 { &bobby } else { &alice };
        let mentions = if n % 7 == 0 && sender.agent_id != "bobby" {
            vec!["bobby"]
        } else {
            vec![]
        };
        store
            .send(
                message(sender, format!("m-{n}"), "分页正文", &mentions),
                limits(),
            )
            .await
            .unwrap();
    }
    let full = store.public_snapshot().await.unwrap();
    store
        .retract(full.events[8]["event_id"].as_str().unwrap(), "other")
        .await
        .unwrap();
    let full = store.public_snapshot().await.unwrap();
    let cursor = |seq: usize| {
        encode_cursor(
            "public",
            seq as u64,
            full.events[seq - 1]["event_id"].as_str(),
            None,
        )
    };
    let config = Config {
        read_default_limit: 9,
        page_default_limit: 13,
        read_text_budget_scalars: 17,
        ..Config::default()
    };
    let mut requests = vec![
        ReadRequest::default(),
        ReadRequest {
            limit: Some(0),
            ..ReadRequest::default()
        },
        ReadRequest {
            after: Some(cursor(1)),
            ..ReadRequest::default()
        },
        ReadRequest {
            after: Some(cursor(1)),
            limit: Some(200),
            ..ReadRequest::default()
        },
        ReadRequest {
            after: Some(cursor(1)),
            limit: Some(3),
            mentions_only: true,
            ..ReadRequest::default()
        },
        ReadRequest {
            after: Some(cursor(420)),
            limit: Some(50),
            mentions_only: true,
            ..ReadRequest::default()
        },
        ReadRequest {
            after: Some(cursor(full.last_seq as usize)),
            mentions_only: true,
            ..ReadRequest::default()
        },
    ];
    for after in [
        "malformed".into(),
        encode_cursor("public", 1, None, None),
        encode_cursor("public", 1, Some("wrong-id"), None),
        encode_cursor("conformance", 1, Some("old-exam"), Some("old-run")),
        encode_cursor("public", 999, Some("ahead"), None),
    ] {
        requests.push(ReadRequest {
            after: Some(after),
            limit: Some(200),
            mentions_only: true,
            ..ReadRequest::default()
        });
    }
    for limit in [1, 50] {
        requests.push(ReadRequest {
            limit: Some(limit),
            ..ReadRequest::default()
        });
        requests.push(ReadRequest {
            after: Some(cursor(1)),
            limit: Some(limit),
            ..ReadRequest::default()
        });
    }
    for budget in [0, 16, 17, 20] {
        let config = Config {
            read_text_budget_scalars: budget,
            ..config.clone()
        };
        for request in &requests {
            let bounded = store
                .participant_read_snapshot(
                    "bobby",
                    bobby.public_key.as_deref().unwrap(),
                    request,
                    &config,
                )
                .await
                .unwrap();
            assert!(
                bounded.events.len() <= 202,
                "unbounded candidates for {request:?}"
            );
            let actual = read_events(
                &bounded.events,
                "public",
                "bobby",
                bounded.last_seq as u64,
                None,
                request,
                &config,
            )
            .unwrap();
            let expected = read_events(
                &full.events,
                "public",
                "bobby",
                full.last_seq as u64,
                None,
                request,
                &config,
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "{request:?}"
            );
        }
    }
    let metadata = store
        .metadata_snapshot("bobby", bobby.public_key.as_deref().unwrap())
        .await
        .unwrap();
    assert!(metadata.events.is_empty());
    assert_eq!(metadata.last_seq, full.last_seq);
    assert_eq!(
        store
            .metadata_snapshot("bobby", "wrong")
            .await
            .unwrap_err()
            .code,
        "unauthorized"
    );
    let pending = pending(&store, "charlie", 3).await;
    assert!(
        store
            .participant_read_snapshot(
                "charlie",
                pending.public_key.as_deref().unwrap(),
                &ReadRequest::default(),
                &config
            )
            .await
            .unwrap()
            .events
            .is_empty()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn incremental_public_snapshot_contains_complete_changed_chunks_and_identity_anchor(
    pool: PgPool,
) {
    let store = Store::from_pool(pool);
    let alice = active(&store, "alice", 1).await;
    for n in 0..429 {
        store
            .send(message(&alice, format!("m-{n}"), "完整分片", &[]), limits())
            .await
            .unwrap();
    }
    let full = store.public_snapshot().await.unwrap();
    let initial = store.public_snapshot_since(0, 200).await.unwrap();
    assert_eq!(initial.events, full.events);
    let caught_up = store
        .public_snapshot_since(full.last_seq, 200)
        .await
        .unwrap();
    assert_eq!(caught_up.events, vec![full.events.last().unwrap().clone()]);
    let no_anchor = store
        .public_snapshot_since(full.last_seq + 1, 200)
        .await
        .unwrap();
    assert!(no_anchor.events.is_empty());
    store
        .retract(full.events[8]["event_id"].as_str().unwrap(), "other")
        .await
        .unwrap();
    let changed = store.public_snapshot_since(410, 200).await.unwrap();
    let full = store.public_snapshot().await.unwrap();
    let expected: Vec<_> = full
        .events
        .iter()
        .filter(|event| {
            event["seq"]
                .as_i64()
                .is_some_and(|seq| seq <= 200 || seq >= 401)
        })
        .cloned()
        .collect();
    assert_eq!(changed.events, expected);
    assert_eq!(changed.agents, full.agents);
    assert_eq!(changed.last_seq, full.last_seq);
    assert!(store.public_snapshot_since(-1, 200).await.is_err());
    assert!(store.public_snapshot_since(0, 0).await.is_err());
}
