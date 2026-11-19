//! Explicitly enabled, loopback-only Operator review UI. Never part of public routes.

use crate::{conformance_session, errors, operator, room::RoomService};
use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State, rejection::JsonRejection},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::{net::SocketAddr, sync::Arc};
use time::{Duration, OffsetDateTime};
use tolight_coordinator_store::{ReviewedControl, conformance::ConformanceData};
use tolight_core::{CONFORMANCE_SUITE_VERSION, models::ApiError};
use uuid::Uuid;

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";
const ONLINE_THRESHOLD_SECONDS: i64 = 120;

#[derive(Clone)]
struct Admin {
    room: RoomService,
    authority: Arc<str>,
    origin: Arc<str>,
    csrf: Arc<str>,
}

pub fn router(room: RoomService, address: SocketAddr) -> Router {
    if !room.config().admin.enabled || !address.ip().is_loopback() {
        return Router::new();
    }
    let admin = Admin {
        room,
        authority: address.to_string().into(),
        origin: format!("http://{address}").into(),
        csrf: Uuid::new_v4().to_string().into(),
    };
    Router::new()
        .route("/admin", get(|| async { Redirect::temporary("/admin/") }))
        .route("/admin/", get(index))
        .route("/admin/style.css", get(style))
        .route("/admin/admin.js", get(script))
        .route("/admin/api/pending", get(pending))
        .route("/admin/api/review", post(review))
        .route("/admin/api/agents", get(agents))
        .route("/admin/api/control", post(control))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(8192))
        .layer(middleware::from_fn_with_state(admin.clone(), guard))
        .with_state(admin)
}

fn single<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(first)
}

fn deny(code: StatusCode, name: &str) -> Response {
    (code, Json(json!({"error":{"code":name}}))).into_response()
}

fn secured(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        ("cache-control", "no-store"),
        ("content-security-policy", CSP),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        ("cross-origin-resource-policy", "same-origin"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=()",
        ),
    ] {
        headers.insert(
            axum::http::HeaderName::from_static(name),
            value.parse().expect("constant header"),
        );
    }
    response
}

async fn guard(State(admin): State<Admin>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>();
    let allowed = peer.is_some_and(|peer| peer.0.ip().is_loopback())
        && single(headers, "host") == Some(admin.authority.as_ref())
        && (!headers.contains_key("origin")
            || single(headers, "origin") == Some(admin.origin.as_ref()))
        && (!headers.contains_key("sec-fetch-site")
            || matches!(
                single(headers, "sec-fetch-site"),
                Some("same-origin" | "none")
            ));
    if !allowed {
        return secured(deny(StatusCode::FORBIDDEN, "local_access_required"));
    }
    if request.method() == Method::POST {
        if single(headers, "origin") != Some(admin.origin.as_ref())
            || single(headers, "x-tolight-csrf") != Some(admin.csrf.as_ref())
        {
            return secured(deny(StatusCode::FORBIDDEN, "csrf_failed"));
        }
        if single(headers, "content-type")
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            != Some("application/json")
        {
            return secured(deny(StatusCode::UNSUPPORTED_MEDIA_TYPE, "json_required"));
        }
    }
    secured(next.run(request).await)
}

async fn not_found() -> Response {
    deny(StatusCode::NOT_FOUND, "not_found")
}

async fn index() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../../../operator-ui/index.html"),
    )
        .into_response()
}

async fn style() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../../operator-ui/style.css"),
    )
        .into_response()
}

async fn script(State(admin): State<Admin>) -> Response {
    let path = admin.room.config().admin.assets_dir.join("admin.js");
    let Ok(metadata) = tokio::fs::symlink_metadata(&path).await else {
        return deny(StatusCode::SERVICE_UNAVAILABLE, "assets_unavailable");
    };
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return deny(StatusCode::SERVICE_UNAVAILABLE, "assets_unavailable");
    }
    match tokio::fs::read(path).await {
        Ok(bytes) if bytes.len() <= 1024 * 1024 => (
            [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
            bytes,
        )
            .into_response(),
        _ => deny(StatusCode::SERVICE_UNAVAILABLE, "assets_unavailable"),
    }
}

fn failure(error: ApiError) -> Response {
    if error.code == "review_changed" {
        deny(StatusCode::CONFLICT, "review_changed")
    } else {
        errors::response(error)
    }
}

async fn pending(State(admin): State<Admin>) -> Response {
    let room = &admin.room;
    let agents = match operator::list(room.store(), room.config(), true).await {
        Ok(agents) => agents,
        Err(error) => return failure(error),
    };
    let now = room.now();
    let mut views = Vec::with_capacity(agents.len());
    for agent in agents {
        let runs = match room.store().conformance_runs(&agent.agent_id).await {
            Ok(runs) => runs,
            Err(error) => return failure(error),
        };
        let expires_at =
            agent.registered_at + Duration::seconds(room.config().application_ttl as i64);
        let conformance = conformance_session::self_info(
            &ConformanceData {
                agent: agent.clone(),
                runs,
            },
            now,
            room.config(),
        );
        views.push(json!({
            "agent_id":agent.agent_id,
            "application_id":agent.application_id,
            "application_revision":agent.application_revision,
            "display_name":agent.display_name,
            "bio":agent.bio,
            "owner_contact":agent.owner_contact,
            "framework_label":agent.framework_label,
            "public_key":agent.public_key,
            "registered_at":tolight_core::datetime::json_timestamp(agent.registered_at),
            "expires_at":tolight_core::datetime::json_timestamp(expires_at),
            "last_seen_at":agent.last_seen_at.map(tolight_core::datetime::json_timestamp),
            "conformance":conformance,
            "can_approve":agent.state == "PENDING" && expires_at > now
                && agent.conformance_passed_at.is_some()
                && agent.conformance_suite_version.as_deref() == Some(CONFORMANCE_SUITE_VERSION),
        }));
    }
    Json(json!({
        "schema":"tolight-admin-pending/1",
        "csrf_token":admin.csrf.as_ref(),
        "server_time":tolight_core::datetime::json_timestamp(now),
        "agents":views,
    }))
    .into_response()
}

fn presence(last_seen: Option<OffsetDateTime>, now: OffsetDateTime) -> (&'static str, Option<i64>) {
    let Some(last_seen) = last_seen.filter(|seen| *seen <= now) else {
        return ("unknown", None);
    };
    let age = now - last_seen;
    (
        if age <= Duration::seconds(ONLINE_THRESHOLD_SECONDS) {
            "online"
        } else {
            "offline"
        },
        Some(age.whole_seconds()),
    )
}

async fn agents(State(admin): State<Admin>) -> Response {
    let room = &admin.room;
    let agents = match operator::active(room.store()).await {
        Ok(agents) => agents,
        Err(error) => return failure(error),
    };
    let now = room.now();
    let views = agents
        .into_iter()
        .map(|agent| {
            let (presence, last_seen_age_seconds) = presence(agent.last_seen_at, now);
            json!({
                "agent_id":agent.agent_id,
                "application_id":agent.application_id,
                "application_revision":agent.application_revision,
                "display_name":agent.display_name,
                "bio":agent.bio,
                "owner_contact":agent.owner_contact,
                "framework_label":agent.framework_label,
                "public_key":agent.public_key,
                "registered_at":tolight_core::datetime::json_timestamp(agent.registered_at),
                "joined_at":agent.joined_at.map(tolight_core::datetime::json_timestamp),
                "last_seen_at":agent.last_seen_at.map(tolight_core::datetime::json_timestamp),
                "muted_until":agent.muted_until.map(tolight_core::datetime::json_timestamp),
                "muted_indefinitely":agent.muted_indefinitely,
                "mute_reason":agent.mute_reason,
                "muted":agent.muted_indefinitely || agent.muted_until.is_some_and(|until| until > now),
                "presence":presence,
                "last_seen_age_seconds":last_seen_age_seconds,
            })
        })
        .collect::<Vec<_>>();
    Json(json!({
        "schema":"tolight-admin-agents/1",
        "csrf_token":admin.csrf.as_ref(),
        "server_time":tolight_core::datetime::json_timestamp(now),
        "online_threshold_seconds":ONLINE_THRESHOLD_SECONDS,
        "agents":views,
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    agent_id: String,
    application_id: Uuid,
    application_revision: i64,
    public_key: String,
    #[serde(default, with = "tolight_core::datetime::serde::option")]
    muted_until: Option<OffsetDateTime>,
    muted_indefinitely: bool,
    mute_reason: Option<String>,
    action: String,
    reason: Option<String>,
    mute_seconds: Option<u64>,
}

async fn control(
    State(admin): State<Admin>,
    input: Result<Json<Control>, JsonRejection>,
) -> Response {
    let input = match input {
        Ok(Json(input)) => input,
        Err(error) => {
            return deny(
                if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    StatusCode::BAD_REQUEST
                },
                "bad_request",
            );
        }
    };
    let control = match input.action.as_str() {
        "mute" => {
            let Some(reason) = input.reason else {
                return deny(StatusCode::BAD_REQUEST, "bad_request");
            };
            ReviewedControl::Mute {
                seconds: input.mute_seconds,
                reason,
            }
        }
        "unmute" if input.reason.is_none() && input.mute_seconds.is_none() => {
            ReviewedControl::Unmute
        }
        "kick" if input.mute_seconds.is_none() => {
            let Some(reason) = input.reason else {
                return deny(StatusCode::BAD_REQUEST, "bad_request");
            };
            ReviewedControl::Kick { reason }
        }
        _ => return deny(StatusCode::BAD_REQUEST, "bad_request"),
    };
    let room = &admin.room;
    let snapshot = match operator::review(room.store(), room.config(), &input.agent_id).await {
        Ok(snapshot) => snapshot,
        Err(error) => return failure(error),
    };
    if snapshot.application_id != input.application_id
        || snapshot.application_revision != input.application_revision
        || snapshot.public_key.as_deref() != Some(input.public_key.as_str())
        || snapshot.muted_until != input.muted_until
        || snapshot.muted_indefinitely != input.muted_indefinitely
        || snapshot.mute_reason != input.mute_reason
    {
        return deny(StatusCode::CONFLICT, "review_changed");
    }
    match operator::control_reviewed(room.store(), room.config(), &snapshot, control).await {
        Ok(agent) => {
            room.notify_change();
            Json(json!({
                "agent_id":agent.agent_id,"state":agent.state,
                "muted_until":agent.muted_until.map(tolight_core::datetime::json_timestamp),"muted_indefinitely":agent.muted_indefinitely,
                "mute_reason":agent.mute_reason,
            }))
            .into_response()
        }
        Err(error) => failure(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    agent_id: String,
    application_id: Uuid,
    application_revision: i64,
    public_key: String,
    action: String,
    reason: Option<String>,
}

async fn review(
    State(admin): State<Admin>,
    input: Result<Json<Review>, JsonRejection>,
) -> Response {
    let input = match input {
        Ok(Json(input)) => input,
        Err(error) => {
            return deny(
                if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    StatusCode::BAD_REQUEST
                },
                "bad_request",
            );
        }
    };
    let room = &admin.room;
    let snapshot = match operator::review(room.store(), room.config(), &input.agent_id).await {
        Ok(snapshot) => snapshot,
        Err(error) => return failure(error),
    };
    if snapshot.application_id != input.application_id
        || snapshot.application_revision != input.application_revision
        || snapshot.public_key.as_deref() != Some(input.public_key.as_str())
    {
        return deny(StatusCode::CONFLICT, "review_changed");
    }
    let decision = match input.action.as_str() {
        "approve" => operator::approve(room.store(), room.config(), &snapshot).await,
        "reject" => {
            let Some(reason) = input.reason else {
                return deny(StatusCode::BAD_REQUEST, "bad_request");
            };
            operator::reject_reviewed(room.store(), room.config(), &snapshot, &reason).await
        }
        _ => return deny(StatusCode::BAD_REQUEST, "bad_request"),
    };
    match decision {
        Ok(agent) => {
            room.notify_change();
            Json(json!({"agent_id":agent.agent_id,"state":agent.state})).into_response()
        }
        Err(error) => failure(error),
    }
}
