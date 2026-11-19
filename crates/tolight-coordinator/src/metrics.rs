//! Loopback-only operational views. No public or Owner-private payloads are logged.
use crate::{logging::Logger, room::RoomService};
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use std::{collections::BTreeMap, sync::Mutex};
use time::OffsetDateTime;
use tolight_core::config::Config;

#[derive(Default)]
struct Counters {
    requests: BTreeMap<String, u64>,
    limits: BTreeMap<String, u64>,
    auth_failures: u64,
    clock_skew: u64,
    lag_since: Option<OffsetDateTime>,
}
pub struct ProcessMetrics {
    counters: Mutex<Counters>,
    pub logger: Logger,
    address: Mutex<Option<std::net::SocketAddr>>,
    started_at_seconds: f64,
}
impl ProcessMetrics {
    pub fn new(config: &Config) -> Self {
        Self {
            counters: Mutex::default(),
            logger: Logger::new(&config.observability),
            address: Mutex::new(None),
            started_at_seconds: (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000) as f64
                / 1_000_000.0,
        }
    }
    pub(crate) fn set_address(&self, address: std::net::SocketAddr) {
        *self.address.lock().unwrap_or_else(|e| e.into_inner()) = Some(address);
    }
    pub fn record_request(&self, endpoint: &str, status: u16) {
        let mut counters = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        *counters.requests.entry(endpoint.into()).or_default() += 1;
        drop(counters);
        let _ = self.logger.event("request", Some(endpoint), Some(status));
    }
    pub fn record_message_limit(&self, agent: &str) {
        let mut c = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        *c.limits.entry(agent.into()).or_default() += 1;
    }
    pub fn record_auth_failure(&self, code: &str) {
        let mut c = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        if code == "clock_skew" {
            c.clock_skew += 1;
        } else if code == "unauthorized" {
            c.auth_failures += 1;
        }
    }
}
pub fn endpoint(path: &str) -> &'static str {
    match path {
        "/participant/v1/applications" => "applications",
        "/participant/v1/events" => "events",
        "/participant/v1/messages" => "messages",
        "/participant/v1/agents" => "agents",
        "/participant/v1/self" => "self",
        "/participant/v1/self/leave" => "self_leave",
        _ => "unknown",
    }
}
fn label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub fn observe_lag(room: &RoomService, last_seq: i64) {
    let published = std::fs::read(room.config().public.dir.join("data/manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value["newest_seq"].as_i64())
        .unwrap_or(0);
    let mut counters = room
        .process_metrics()
        .counters
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if last_seq > published {
        counters.lag_since.get_or_insert(room.now());
    } else {
        counters.lag_since = None;
    }
}
pub fn address(room: &RoomService) -> Option<std::net::SocketAddr> {
    *room
        .process_metrics()
        .address
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
pub fn router(room: RoomService) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/metrics", get(metrics))
        .with_state(room)
}
async fn health(State(room): State<RoomService>) -> Response {
    let result = room.store().check_schema().await;
    let mut response = (
        if result.is_ok() {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(serde_json::json!({"status":if result.is_ok(){"ok"}else{"unavailable"}})),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
async fn metrics(State(room): State<RoomService>) -> Response {
    let result = render(&room).await;
    let mut response = match result {
        Ok(text) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "tolight_database_up 0\n").into_response(),
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
pub async fn render(room: &RoomService) -> Result<String, tolight_core::models::ApiError> {
    let now = room.now();
    let snapshot = room.store().telemetry(now).await?;
    let manifest = std::fs::read(room.config().public.dir.join("data/manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let published = manifest
        .as_ref()
        .and_then(|v| v["newest_seq"].as_i64())
        .unwrap_or(0);
    let lag = (snapshot.room.last_seq - published).max(0);
    let mut counters = room
        .process_metrics()
        .counters
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let lag_seconds = if lag > 0 {
        let start = counters.lag_since.get_or_insert(now);
        (now - *start).whole_milliseconds().max(0) as f64 / 1000.0
    } else {
        counters.lag_since = None;
        0.0
    };
    let mut text = format!(
        "tolight_database_up 1\ntolight_process_start_timestamp_seconds {}\ntolight_room_messages_last_hour {}\ntolight_pending_applications {}\ntolight_oldest_pending_seconds {}\ntolight_latest_pending_timestamp_seconds {}\ntolight_muted_agents {}\ntolight_pending_long_polls {}\ntolight_auth_failures_total {}\ntolight_clock_skew_total {}\ntolight_publish_lag_events {}\ntolight_publish_lag_seconds {}\ntolight_room_paused {}\n",
        room.process_metrics().started_at_seconds,
        snapshot.messages_last_hour,
        snapshot.pending_count,
        snapshot.oldest_pending_seconds,
        snapshot.latest_pending_timestamp,
        snapshot.muted_count,
        room.pending_read_count(),
        counters.auth_failures,
        counters.clock_skew,
        lag,
        lag_seconds,
        u8::from(snapshot.room.paused)
    );
    let rejected_last_hour: BTreeMap<_, _> = snapshot
        .rejections
        .iter()
        .map(|row| {
            (
                (row.agent_id.as_str(), row.reason.as_str()),
                row.count_last_hour,
            )
        })
        .collect();
    for agent in snapshot.agents {
        let id = label(&agent.agent_id);
        text += &format!(
            "tolight_agent_attempts_last_day{{agent_id=\"{id}\"}} {}\ntolight_agent_last_seen_seconds{{agent_id=\"{id}\"}} {}\n",
            agent.attempts, agent.last_seen_seconds
        );
        for reason in [
            "empty",
            "too_long",
            "invalid_chars",
            "secret_shape",
            "invalid_mention",
            "too_many_mentions",
        ] {
            let count = rejected_last_hour
                .get(&(agent.agent_id.as_str(), reason))
                .copied()
                .unwrap_or(0);
            text += &format!(
                "tolight_agent_rejected_attempts_last_hour{{agent_id=\"{id}\",reason_code=\"{reason}\"}} {count}\n"
            );
        }
    }
    for rejected in snapshot.rejections {
        text += &format!(
            "tolight_agent_rejected_attempts{{agent_id=\"{}\",reason_code=\"{}\"}} {}\n",
            label(&rejected.agent_id),
            label(&rejected.reason),
            rejected.count
        );
    }
    for (agent, count) in &counters.limits {
        text += &format!(
            "tolight_message_limit_rejections_total{{agent_id=\"{}\"}} {count}\n",
            label(agent)
        );
    }
    for (endpoint, count) in &counters.requests {
        text += &format!(
            "tolight_requests_total{{endpoint=\"{}\"}} {count}\n",
            label(endpoint)
        );
    }
    Ok(text)
}
