use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tolight_coordinator_store::{
    Agent,
    conformance::{ConformanceData, ConformanceRun},
    controls::ControlSnapshot,
};

fn legacy_agent() -> Value {
    json!({
        "agent_id":"alice",
        "application_id":"12345678-1234-4234-8234-123456789abc",
        "application_revision":1,
        "display_name":"Alice",
        "display_name_key":"alice",
        "bio":"公开简介",
        "framework_label":null,
        "owner_contact":"owner@example.invalid",
        "state":"ACTIVE",
        "state_reason":null,
        "state_changed_at":"2026-11-14T00:00:00.123Z",
        "registered_at":"2026-11-14T00:00:00Z",
        "joined_at":"2026-11-14T00:00:00.123456Z",
        "muted_until":"2026-11-14T00:00:00.123456789Z",
        "muted_indefinitely":false,
        "mute_reason":"spam",
        "conformance_suite_version":"1",
        "conformance_passed_at":"2026-11-14T00:00:00.100Z",
        "last_seen_at":"2026-11-14T00:00:00.000001Z",
        "public_key":null
    })
}

#[test]
fn historical_agent_json_preserves_timestamp_strings_and_missing_optional_fields() {
    let mut legacy = legacy_agent();
    let agent: Agent = serde_json::from_value(legacy.clone()).unwrap();
    assert!(agent.public_key_registered_at.is_none());
    legacy["public_key_registered_at"] = Value::Null;
    assert_eq!(serde_json::to_value(agent).unwrap(), legacy);

    for field in [
        "joined_at",
        "muted_until",
        "conformance_passed_at",
        "last_seen_at",
        "public_key_registered_at",
    ] {
        legacy.as_object_mut().unwrap().remove(field);
    }
    let agent: Agent = serde_json::from_value(legacy).unwrap();
    assert!(agent.joined_at.is_none());
    assert!(agent.muted_until.is_none());
    assert!(agent.conformance_passed_at.is_none());
    assert!(agent.last_seen_at.is_none());
    assert!(agent.public_key_registered_at.is_none());
}

#[test]
fn historical_control_snapshot_and_conformance_json_remain_rfc3339() {
    let legacy = json!({
        "schema":"tolight-control-snapshot/1",
        "export_id":"12345678-1234-4234-8234-123456789abc",
        "exported_at":"2026-11-14T02:00:00.123456789Z",
        "restore_target":"2026-11-14T01:00:00.123456Z",
        "source_anchor":{"seq":0,"event_id":null},
        "room":{"paused":false,"applications_open":true},
        "agents":[],
        "agent_keys":[{
            "public_key":"fixture-public-key",
            "agent_id":"alice",
            "application_id":"12345678-1234-4234-8234-123456789abc",
            "created_at":"2026-11-14T00:00:00.123Z",
            "revoked_at":null
        }],
        "operator_actions":[{
            "action_id":"fixture-action",
            "kind":"approved",
            "target_agent_id":"alice",
            "target_application_id":null,
            "target_event_id":null,
            "target_public_key":null,
            "reason_code":null,
            "detail":null,
            "created_at":"2026-11-14T00:00:00Z"
        }],
        "retracted_event_ids":[]
    });
    let snapshot: ControlSnapshot = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(serde_json::to_value(snapshot).unwrap(), legacy);

    let legacy = json!({
        "run_id":"fixture-run",
        "agent_id":"alice",
        "suite_version":"1",
        "started_at":"2026-11-14T00:00:00.123456Z",
        "event_ids":[],
        "passed":null,
        "failure_reason":null
    });
    let run: ConformanceRun = serde_json::from_value(legacy).unwrap();
    assert!(run.limited_at.is_none());
    assert!(run.finished_at.is_none());
    let encoded = serde_json::to_value(run).unwrap();
    assert_eq!(encoded["started_at"], "2026-11-14T00:00:00.123456Z");
    assert!(encoded["limited_at"].is_null());
    assert!(encoded["finished_at"].is_null());
}

#[test]
fn conformance_daily_limit_uses_utc_day_even_when_input_has_an_offset() {
    let started = OffsetDateTime::parse("2026-11-14T23:30:00-02:00", &Rfc3339).unwrap();
    let now = OffsetDateTime::parse("2026-11-15T02:00:00Z", &Rfc3339).unwrap();
    let data = ConformanceData {
        agent: serde_json::from_value(legacy_agent()).unwrap(),
        runs: vec![ConformanceRun::candidate("alice", started, 1)],
    };
    assert_eq!(data.runs_today(now), 1);
    let now = OffsetDateTime::parse("2026-11-15T00:30:00+02:00", &Rfc3339).unwrap();
    assert_eq!(data.runs_today(now), 0);
}
