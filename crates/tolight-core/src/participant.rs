//! Pure read semantics shared by the public timeline and conformance timelines.
use crate::{
    PUBLIC_NOTICE,
    config::Config,
    models::{AgentContext, ApiError, SendRequest},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReadRequest {
    pub after: Option<String>,
    pub limit: Option<usize>,
    pub wait: u64,
    pub mentions_only: bool,
}
impl ReadRequest {
    pub fn validate(&self, config: &Config) -> Result<(), ApiError> {
        if self.wait > config.long_poll_max_wait
            || self.limit.is_some_and(|limit| {
                if self.after.is_some() {
                    !(1..=200).contains(&limit)
                } else {
                    limit > 50
                }
            })
        {
            return Err(ApiError::new("bad_request"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadResponse {
    pub timeline: String,
    pub notice: String,
    pub events: Vec<Value>,
    pub next_cursor: String,
    pub head_seq: u64,
    pub cursor_reset: bool,
    pub truncated: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub timeline: String,
    pub seq: u64,
    pub event_id: Option<String>,
    pub session: Option<String>,
}
pub fn encode_cursor(
    timeline: &str,
    seq: u64,
    event_id: Option<&str>,
    session: Option<&str>,
) -> String {
    let value = Cursor {
        timeline: timeline.into(),
        seq,
        event_id: event_id.map(str::to_owned),
        session: session.map(str::to_owned),
    };
    format!(
        "c1-{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).expect("serializable cursor"))
    )
}
pub fn decode_cursor(value: &str) -> Option<Cursor> {
    let encoded = value.strip_prefix("c1-")?;
    if encoded.len() > 2048 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    if URL_SAFE_NO_PAD.encode(&bytes) != encoded {
        return None;
    }
    let cursor: Cursor = serde_json::from_slice(&bytes).ok()?;
    if !["public", "conformance"].contains(&cursor.timeline.as_str())
        || (cursor.seq == 0 && cursor.event_id.is_some())
    {
        return None;
    }
    Some(cursor)
}
fn sequence(value: &Value) -> u64 {
    value["seq"].as_u64().unwrap_or(0)
}
fn event_id(value: &Value) -> Option<&str> {
    value["event_id"].as_str()
}
fn mentions_reader(value: &Value, reader: &str) -> bool {
    value["kind"] == "agent_message"
        && value["retracted"] != true
        && value["agent"]["agent_id"] != reader
        && value["mentions"]
            .as_array()
            .is_some_and(|values| values.iter().any(|agent| agent["agent_id"] == reader))
}
fn with_reader_fields(mut event: Value, reader: &str) -> Value {
    let from_self = event["kind"] == "agent_message" && event["agent"]["agent_id"] == reader;
    let mentions_you = mentions_reader(&event, reader);
    event["from_self"] = from_self.into();
    event["mentions_you"] = mentions_you.into();
    event
}
#[allow(clippy::too_many_arguments)]
pub fn read_events(
    events: &[Value],
    timeline: &str,
    reader: &str,
    head_seq: u64,
    session: Option<&str>,
    request: &ReadRequest,
    config: &Config,
) -> Result<ReadResponse, ApiError> {
    request.validate(config)?;
    let mut ordered: Vec<_> = events
        .iter()
        .filter(|value| sequence(value) > 0 && sequence(value) <= head_seq)
        .collect();
    ordered.sort_by_key(|value| sequence(value));
    let find_id = |seq| {
        ordered
            .iter()
            .find(|event| sequence(event) == seq)
            .and_then(|event| event_id(event))
    };
    let cursor = request.after.as_deref().and_then(decode_cursor);
    let cursor_valid = cursor.as_ref().is_some_and(|cursor| {
        cursor.timeline == timeline
            && cursor.seq <= head_seq
            && if timeline == "public" {
                cursor.session.is_none()
                    && if cursor.seq == 0 {
                        cursor.event_id.is_none()
                    } else {
                        cursor.event_id.as_deref() == find_id(cursor.seq)
                    }
            } else {
                cursor.session.as_deref() == session
                    && (cursor.seq == 0 || cursor.event_id.as_deref() == find_id(cursor.seq))
            }
    });
    let reset = request.after.is_some() && !cursor_valid;
    let context = request.after.is_none() || reset;
    let (selected, next_seq, truncated) = if context {
        let limit = request.limit.unwrap_or(config.read_default_limit).min(50);
        let mut selected = Vec::new();
        let mut scalars = 0;
        let mut truncated = false;
        for event in ordered.iter().rev().take(limit) {
            let size = event["text"]
                .as_str()
                .map_or(0, |text| text.chars().count());
            if scalars + size > config.read_text_budget_scalars {
                truncated = true;
                break;
            }
            scalars += size;
            selected.push((*event).clone());
        }
        selected.reverse();
        (selected, head_seq, truncated)
    } else {
        let cursor = cursor.expect("valid cursor");
        let limit = request.limit.unwrap_or(config.page_default_limit);
        let mut selected = Vec::new();
        let mut last = cursor.seq;
        for event in ordered
            .into_iter()
            .filter(|event| sequence(event) > cursor.seq)
        {
            last = sequence(event);
            if !request.mentions_only || mentions_reader(event, reader) {
                selected.push(event.clone());
            }
            if selected.len() == limit {
                break;
            }
        }
        (selected, last, false)
    };
    let next_id = if next_seq == 0 {
        None
    } else {
        events
            .iter()
            .find(|value| sequence(value) == next_seq)
            .and_then(event_id)
    };
    Ok(ReadResponse {
        timeline: timeline.into(),
        notice: PUBLIC_NOTICE.into(),
        events: selected
            .into_iter()
            .map(|event| with_reader_fields(event, reader))
            .collect(),
        next_cursor: encode_cursor(timeline, next_seq, next_id, session),
        head_seq,
        cursor_reset: reset,
        truncated,
    })
}

/// Validate format before the public-text gate, so malformed requests do not consume quota.
pub fn validate_send(request: &SendRequest) -> Result<(), ApiError> {
    if request.client_message_id.is_empty()
        || request.client_message_id.len() > 64
        || !request
            .client_message_id
            .bytes()
            .all(|c| c.is_ascii_graphic())
    {
        return Err(ApiError::new("bad_request").with("field", "client_message_id"));
    }
    crate::mentions::validate_explicit(&request.mentions)
}
// Convenient import path for transport adapters.
pub type Context = AgentContext;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn events() -> Vec<Value> {
        (1..=8).map(|seq|json!({"seq":seq,"event_id":format!("e{seq}"),"kind":"agent_message","agent":{"agent_id":if seq==4 {"bob"}else{"alice"}},"text":"界界界","mentions":if seq%3==0 {vec![json!({"agent_id":"bob","display_name":"Bob"})]}else{vec![]},"retracted":seq==6})).collect()
    }
    fn read(values: &[Value], request: ReadRequest, cfg: &Config) -> ReadResponse {
        read_events(values, "public", "bob", 8, None, &request, cfg).unwrap()
    }
    #[test]
    fn participant_context_budget_zero_and_reader_flags() {
        let events = events();
        let cfg = Config {
            read_text_budget_scalars: 7,
            ..Config::default()
        };
        let result = read(&events, ReadRequest::default(), &cfg);
        assert_eq!(result.events.len(), 2);
        assert!(result.truncated);
        assert_eq!(decode_cursor(&result.next_cursor).unwrap().seq, 8);
        let zero = read(
            &events,
            ReadRequest {
                limit: Some(0),
                ..ReadRequest::default()
            },
            &cfg,
        );
        assert!(zero.events.is_empty());
        assert!(!zero.truncated);
        let result = read(
            &events,
            ReadRequest {
                limit: Some(8),
                ..ReadRequest::default()
            },
            &Config::default(),
        );
        assert_eq!(result.events[3]["from_self"], true);
        assert_eq!(result.events[2]["mentions_you"], true);
        assert_eq!(result.events[5]["mentions_you"], false);
    }
    #[test]
    fn participant_incremental_mentions_and_head_snapshot() {
        let events = events();
        let cfg = Config::default();
        let after = encode_cursor("public", 1, Some("e1"), None);
        let result = read(
            &events,
            ReadRequest {
                after: Some(after),
                limit: Some(1),
                mentions_only: true,
                ..ReadRequest::default()
            },
            &cfg,
        );
        assert_eq!(result.events[0]["seq"], 3);
        assert_eq!(decode_cursor(&result.next_cursor).unwrap().seq, 3);
        let result = read(
            &events,
            ReadRequest {
                after: Some(result.next_cursor),
                mentions_only: true,
                ..ReadRequest::default()
            },
            &cfg,
        );
        assert!(result.events.is_empty());
        assert_eq!(decode_cursor(&result.next_cursor).unwrap().seq, 8);
        let result = read_events(
            &events,
            "public",
            "bob",
            5,
            None,
            &ReadRequest::default(),
            &cfg,
        )
        .unwrap();
        assert_eq!(decode_cursor(&result.next_cursor).unwrap().seq, 5);
        assert_eq!(result.events.len(), 5);
    }
    #[test]
    fn participant_reset_identity_and_request_validation_before_reset() {
        let events = events();
        let cfg = Config::default();
        for cursor in [
            "broken".into(),
            encode_cursor("public", 9, Some("e9"), None),
            encode_cursor("public", 3, Some("restored-old-id"), None),
            encode_cursor("conformance", 0, None, Some("none")),
        ] {
            let result = read(
                &events,
                ReadRequest {
                    after: Some(cursor.clone()),
                    limit: Some(200),
                    mentions_only: true,
                    ..ReadRequest::default()
                },
                &cfg,
            );
            assert!(result.cursor_reset);
            assert_eq!(result.events.len(), 8);
            assert!(
                read_events(
                    &events,
                    "public",
                    "bob",
                    8,
                    None,
                    &ReadRequest {
                        after: Some(cursor),
                        limit: Some(0),
                        ..ReadRequest::default()
                    },
                    &cfg
                )
                .is_err()
            );
        }
        assert!(
            read_events(
                &events,
                "public",
                "bob",
                8,
                None,
                &ReadRequest {
                    limit: Some(51),
                    ..ReadRequest::default()
                },
                &cfg
            )
            .is_err()
        );
    }
    #[test]
    fn participant_empty_conformance_cursor_transitions_to_public() {
        let cfg = Config::default();
        let pending = read_events(
            &[],
            "conformance",
            "alice",
            0,
            Some("none"),
            &ReadRequest::default(),
            &cfg,
        )
        .unwrap();
        let repeated = read_events(
            &[],
            "conformance",
            "alice",
            0,
            Some("none"),
            &ReadRequest {
                after: Some(pending.next_cursor.clone()),
                ..ReadRequest::default()
            },
            &cfg,
        )
        .unwrap();
        assert!(!repeated.cursor_reset);
        let approved = read_events(
            &[],
            "public",
            "alice",
            0,
            None,
            &ReadRequest {
                after: Some(pending.next_cursor),
                ..ReadRequest::default()
            },
            &cfg,
        )
        .unwrap();
        assert!(approved.cursor_reset);
    }
}
