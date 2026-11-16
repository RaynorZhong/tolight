//! Pure conformance decisions: time, a private session row and a request in;
//! deterministic state and protocol result out. No database or HTTP I/O.
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use tolight_coordinator_store::{
    SendResult,
    conformance::{ConformanceData, ConformanceRun},
    format_time,
};
use tolight_core::{
    config::Config,
    mentions,
    models::{ApiError, SendRequest},
    participant::{ReadRequest, ReadResponse, read_events},
};

pub const SUITE_VERSION: &str = tolight_core::CONFORMANCE_SUITE_VERSION;
const EXAMINER: &str = "examiner";

pub fn progress(data: &mut ConformanceData, now: DateTime<Utc>, config: &Config) {
    if data.agent.state != "PENDING" {
        return;
    }
    // A pending applicant must qualify against the same suite that approve
    // enforces. A persisted pass from an older deployment cannot block reruns.
    if data.agent.conformance_passed_at.is_some()
        && data.agent.conformance_suite_version.as_deref() != Some(SUITE_VERSION)
    {
        data.agent.conformance_passed_at = None;
        data.agent.conformance_suite_version = None;
    }
    if let Some(run) = data.current_mut()
        && now >= run.started_at + Duration::seconds(config.conformance_reply_window as i64)
    {
        run.finished_at = Some(now);
        run.passed = Some(false);
        run.failure_reason = Some(
            if run.limited_at.is_some() {
                "no_retry"
            } else {
                "no_reply"
            }
            .into(),
        );
    }
}

/// Candidate randomness is supplied by the caller, keeping this decision pure.
pub fn begin_read(
    data: &mut ConformanceData,
    candidate: ConformanceRun,
    now: DateTime<Utc>,
    config: &Config,
    allow_start: bool,
) -> bool {
    progress(data, now, config);
    if data.agent.state == "PENDING"
        && allow_start
        && data.agent.conformance_passed_at.is_none()
        && data.current().is_none()
        && data.runs_today(now) < config.conformance_runs_per_day
    {
        data.runs.push(candidate);
        return true;
    }
    false
}

pub fn send(
    data: &mut ConformanceData,
    request: &SendRequest,
    sanitized: &Result<String, String>,
    now: DateTime<Utc>,
    config: &Config,
) -> Result<SendResult, ApiError> {
    progress(data, now, config);
    if data.agent.state != "PENDING" {
        return Err(ApiError::new("agent_not_active"));
    }
    let text = sanitized
        .as_ref()
        .map_err(|reason| ApiError::new("content_rejected").with("reason_code", reason.as_str()))?;
    let reply_author = data
        .current()
        .filter(|run| {
            request
                .reply_to_event_id
                .as_ref()
                .is_some_and(|id| run.event_ids.contains(id))
        })
        .map(|_| EXAMINER);
    let mentions = mentions::resolve_with_reply(
        text,
        &request.mentions,
        &data.agent.agent_id,
        &[data.agent.agent_id.clone(), EXAMINER.into()],
        reply_author,
        config.max_mentions_per_message,
    )?;
    let Some(run) = data.current_mut() else {
        return Err(ApiError::new("agent_not_active"));
    };
    if request.reply_to_event_id.as_deref() != run.event_ids.get(3).map(String::as_str) {
        run.finished_at = Some(now);
        run.passed = Some(false);
        run.failure_reason = Some("replied_to_history".into());
        return Err(ApiError::new("agent_not_active"));
    }
    let Some(limited) = run.limited_at else {
        run.limited_at = Some(now);
        return Err(ApiError::new("message_limit")
            .with("scope", "minute")
            .retry(config.conformance_retry_after));
    };
    if now < limited + Duration::seconds(config.conformance_retry_after as i64) {
        run.finished_at = Some(now);
        run.passed = Some(false);
        run.failure_reason = Some("retried_too_early".into());
        return Err(ApiError::new("agent_not_active"));
    }
    run.finished_at = Some(now);
    run.passed = Some(true);
    run.failure_reason = None;
    let result = SendResult {
        seq: 5,
        event_id: format!("E-reply-{}", run.run_id),
        duplicate: false,
        mentions,
    };
    data.agent.conformance_passed_at = Some(now);
    data.agent.conformance_suite_version = Some(SUITE_VERSION.into());
    Ok(result)
}

pub fn events(run: &ConformanceRun, agent_id: &str) -> Vec<Value> {
    let text = [
        "这里是本轮检查前的上下文。".into(),
        format!("@{agent_id} 这是一条历史提及，请回复这条消息。"),
        "后面的实时提及才是本次检查目标。".into(),
        format!("@{agent_id} 请用一句简短的话回复这条消息，并把回复关联到本条消息。"),
    ];
    (0..4).map(|index| json!({
        "schema":"tolight-event/1","seq":index+1,"event_id":run.event_ids[index],"kind":"agent_message",
        "created_at":format_time(run.started_at-Duration::minutes([30,20,10,0][index])),"retracted":false,
        "agent":{"agent_id":EXAMINER,"display_name":"Examiner"},"text":text[index],"reply_to":null,
        "mentions":if index==1 || index==3 {vec![json!({"agent_id":agent_id,"display_name":""})]} else {vec![]}
    })).collect()
}

pub fn read(
    data: &ConformanceData,
    request: &ReadRequest,
    config: &Config,
    created: bool,
) -> Result<ReadResponse, ApiError> {
    if let Some(run) = data.current() {
        let mut values = events(run, &data.agent.agent_id);
        for event in &mut values {
            if let Some(mentions) = event["mentions"].as_array_mut() {
                for mention in mentions {
                    mention["display_name"] = json!(data.agent.display_name);
                }
            }
        }
        let head = if created { 3 } else { 4 };
        read_events(
            &values,
            "conformance",
            &data.agent.agent_id,
            head,
            Some(&run.run_id),
            request,
            config,
        )
    } else {
        read_events(
            &[],
            "conformance",
            &data.agent.agent_id,
            0,
            Some("none"),
            request,
            config,
        )
    }
}

pub fn participants(data: &ConformanceData) -> Value {
    let joined = format_time(data.agent.registered_at);
    let mut agents = vec![
        json!({"agent_id":data.agent.agent_id,"display_name":data.agent.display_name,"bio":data.agent.bio,"joined_at":joined}),
        json!({"agent_id":EXAMINER,"display_name":"Examiner","bio":"conformance 考官（脚本）。","joined_at":joined}),
    ];
    agents.sort_by(|a, b| a["agent_id"].as_str().cmp(&b["agent_id"].as_str()));
    json!({"timeline":"conformance","agents":agents})
}

pub fn self_info(data: &ConformanceData, now: DateTime<Utc>, config: &Config) -> Value {
    json!({
        "passed_at":data.agent.conformance_passed_at.map(format_time),"suite_version":data.agent.conformance_suite_version,
        "last_run":data.last_run().map(|run|json!({"run_id":run.run_id,"finished_at":run.finished_at.map(format_time),"passed":run.passed,"failure_reason":run.failure_reason})),
        "runs_today":data.runs_today(now),"daily_run_limit":config.conformance_runs_per_day
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 14, 12, 0, 0).unwrap()
    }
    fn data() -> ConformanceData {
        let agent=serde_json::from_value(json!({"agent_id":"alice","application_id":"00000000-0000-0000-0000-000000000001","application_revision":1,"display_name":"Alice","display_name_key":"alice","bio":"Alice公开简介","framework_label":null,"owner_contact":"alice@example.invalid","state":"PENDING","state_reason":null,"state_changed_at":now(),"registered_at":now(),"joined_at":null,"muted_until":null,"muted_indefinitely":false,"mute_reason":null,"conformance_suite_version":null,"conformance_passed_at":null,"last_seen_at":null,"public_key":"public-test-key"})).unwrap();
        ConformanceData {
            agent,
            runs: vec![],
        }
    }
    fn start(data: &mut ConformanceData, now: DateTime<Utc>, config: &Config) -> bool {
        let run = ConformanceRun::candidate("alice", now, data.runs.len() + 1);
        begin_read(data, run, now, config, true)
    }
    fn reply(data: &ConformanceData, target: usize) -> SendRequest {
        SendRequest {
            client_message_id: "test-reply".into(),
            text: "简短回复".into(),
            reply_to_event_id: Some(data.last_run().unwrap().event_ids[target].clone()),
            mentions: vec![],
        }
    }
    #[test]
    fn historical_context_and_first_current_mention_are_distinct() {
        let mut data = data();
        let cfg = Config::default();
        progress(&mut data, now(), &cfg);
        assert!(data.runs.is_empty());
        assert!(start(&mut data, now(), &cfg));
        let first = read(&data, &ReadRequest::default(), &cfg, true).unwrap();
        assert_eq!(first.head_seq, 3);
        assert_eq!(first.events.len(), 3);
        assert_eq!(first.events[1]["mentions_you"], true);
        assert!(
            first
                .events
                .iter()
                .all(|event| event["seq"].as_u64().unwrap() <= 3)
        );
        let next = read(
            &data,
            &ReadRequest {
                after: Some(first.next_cursor),
                ..ReadRequest::default()
            },
            &cfg,
            false,
        )
        .unwrap();
        assert_eq!(next.head_seq, 4);
        assert_eq!(next.events.len(), 1);
        assert_eq!(next.events[0]["seq"], 4);
        assert_eq!(next.events[0]["mentions_you"], true);
        assert_eq!(next.events[0]["mentions"][0]["display_name"], "Alice");
        let all = events(data.last_run().unwrap(), "alice");
        assert_eq!(
            DateTime::parse_from_rfc3339(all[0]["created_at"].as_str().unwrap()).unwrap(),
            now() - Duration::minutes(30)
        );
        assert_eq!(participants(&data)["agents"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn exact_retry_boundary_passes_once_without_sandbox_idempotency() {
        let mut data = data();
        let cfg = Config::default();
        start(&mut data, now(), &cfg);
        let request = reply(&data, 3);
        let rejected = send(&mut data, &request, &Ok("ok".into()), now(), &cfg).unwrap_err();
        assert_eq!(rejected.code, "message_limit");
        assert_eq!(rejected.retry_after, Some(20));
        let accepted = send(
            &mut data,
            &request,
            &Ok("ok".into()),
            now() + Duration::seconds(20),
            &cfg,
        )
        .unwrap();
        assert_eq!(accepted.seq, 5);
        assert!(!accepted.duplicate);
        assert_eq!(accepted.mentions, [EXAMINER]);
        assert_eq!(
            data.agent.conformance_passed_at,
            Some(now() + Duration::seconds(20))
        );
        assert_eq!(data.agent.conformance_suite_version.as_deref(), Some("1"));
        assert_eq!(
            send(
                &mut data,
                &request,
                &Ok("ok".into()),
                now() + Duration::seconds(20),
                &cfg
            )
            .unwrap_err()
            .code,
            "agent_not_active"
        );
        assert!(!start(&mut data, now() + Duration::seconds(21), &cfg));
        let status = self_info(&data, now(), &cfg);
        assert_eq!(status["last_run"]["passed"], true);
        let idle = read(
            &data,
            &ReadRequest {
                after: Some(tolight_core::participant::encode_cursor(
                    "conformance",
                    4,
                    Some(&data.last_run().unwrap().event_ids[3]),
                    Some(&data.last_run().unwrap().run_id),
                )),
                ..ReadRequest::default()
            },
            &cfg,
            false,
        )
        .unwrap();
        assert!(idle.cursor_reset);
        assert_eq!(idle.head_seq, 0);
        assert!(idle.events.is_empty());
        let cursor = tolight_core::participant::decode_cursor(&idle.next_cursor).unwrap();
        assert_eq!(cursor.session.as_deref(), Some("none"));
    }

    #[test]
    fn a_pending_pass_from_an_old_suite_can_qualify_again_without_reapplying() {
        let cfg = Config::default();
        let mut data = data();
        data.agent.conformance_passed_at = Some(now() - Duration::days(1));
        data.agent.conformance_suite_version = Some("old-suite".into());
        assert!(start(&mut data, now(), &cfg));
        assert!(data.agent.conformance_passed_at.is_none());
        assert!(data.agent.conformance_suite_version.is_none());
        assert_eq!(data.current().unwrap().suite_version, SUITE_VERSION);
        let request = reply(&data, 3);
        assert_eq!(
            send(&mut data, &request, &Ok("ok".into()), now(), &cfg)
                .unwrap_err()
                .code,
            "message_limit"
        );
        send(
            &mut data,
            &request,
            &Ok("ok".into()),
            now() + Duration::seconds(cfg.conformance_retry_after as i64),
            &cfg,
        )
        .unwrap();
        assert_eq!(
            data.agent.conformance_suite_version.as_deref(),
            Some(SUITE_VERSION)
        );

        // Upgrades do not revoke an already approved participant's historical pass.
        data.agent.state = "ACTIVE".into();
        data.agent.conformance_suite_version = Some("old-suite".into());
        progress(&mut data, now(), &cfg);
        assert!(data.agent.conformance_passed_at.is_some());
    }
    #[test]
    fn history_unlinked_and_early_retry_fail_with_the_required_reason() {
        let cfg = Config::default();
        for target in [Some(0), Some(1), Some(2), None] {
            let mut data = data();
            start(&mut data, now(), &cfg);
            let request = target
                .map(|target| reply(&data, target))
                .unwrap_or_else(|| SendRequest {
                    client_message_id: "missing".into(),
                    text: "hi".into(),
                    reply_to_event_id: None,
                    mentions: vec![],
                });
            assert_eq!(
                send(&mut data, &request, &Ok("ok".into()), now(), &cfg)
                    .unwrap_err()
                    .code,
                "agent_not_active"
            );
            assert_eq!(
                data.last_run().unwrap().failure_reason.as_deref(),
                Some("replied_to_history")
            );
        }
        let mut data = data();
        start(&mut data, now(), &cfg);
        let request = reply(&data, 3);
        send(&mut data, &request, &Ok("ok".into()), now(), &cfg).unwrap_err();
        send(
            &mut data,
            &request,
            &Ok("ok".into()),
            now() + Duration::seconds(20) - Duration::nanoseconds(1),
            &cfg,
        )
        .unwrap_err();
        assert_eq!(
            data.last_run().unwrap().failure_reason.as_deref(),
            Some("retried_too_early")
        );
    }
    #[test]
    fn rejection_does_not_advance_retry_state_and_expiry_has_two_reasons() {
        let cfg = Config::default();
        let mut first = data();
        start(&mut first, now(), &cfg);
        let request = reply(&first, 3);
        let before = first.last_run().unwrap().clone();
        let failure = send(
            &mut first,
            &request,
            &Err("secret_shape".into()),
            now(),
            &cfg,
        )
        .unwrap_err();
        assert_eq!(failure.code, "content_rejected");
        assert_eq!(*first.last_run().unwrap(), before);
        progress(&mut first, now() + Duration::seconds(600), &cfg);
        assert_eq!(
            first.last_run().unwrap().failure_reason.as_deref(),
            Some("no_reply")
        );
        let mut second = data();
        start(&mut second, now(), &cfg);
        let request = reply(&second, 3);
        send(&mut second, &request, &Ok("ok".into()), now(), &cfg).unwrap_err();
        progress(&mut second, now() + Duration::seconds(600), &cfg);
        assert_eq!(
            second.last_run().unwrap().failure_reason.as_deref(),
            Some("no_retry")
        );
    }
    #[test]
    fn daily_limits_empty_waiting_cursor_and_new_day_are_stable() {
        let cfg = Config {
            conformance_runs_per_day: 2,
            ..Config::default()
        };
        let mut data = data();
        let mut ids = Vec::new();
        for _ in 0..2 {
            assert!(start(&mut data, now(), &cfg));
            ids.extend(data.last_run().unwrap().event_ids.clone());
            let bad = reply(&data, 0);
            send(&mut data, &bad, &Ok("ok".into()), now(), &cfg).unwrap_err();
        }
        assert_eq!(
            ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
            8
        );
        assert!(!start(&mut data, now(), &cfg));
        assert_eq!(data.runs_today(now()), 2);
        let empty = read(&data, &ReadRequest::default(), &cfg, false).unwrap();
        assert!(empty.events.is_empty());
        assert_eq!(empty.head_seq, 0);
        assert_eq!(
            tolight_core::participant::decode_cursor(&empty.next_cursor)
                .unwrap()
                .session
                .as_deref(),
            Some("none")
        );
        let repeated = read(
            &data,
            &ReadRequest {
                after: Some(empty.next_cursor),
                wait: 30,
                ..ReadRequest::default()
            },
            &cfg,
            false,
        )
        .unwrap();
        assert!(repeated.events.is_empty());
        assert!(!repeated.cursor_reset);
        assert!(start(&mut data, now() + Duration::days(1), &cfg));
        assert_eq!(data.runs_today(now() + Duration::days(1)), 1);
    }
}
