//! Authentication lives at the unnested router, so the signed URI is untouched.

use crate::{errors, rate_limit::source_bucket, room::RoomService};
use axum::{
    body::{Body, to_bytes},
    extract::{ConnectInfo, Request, State},
    http::header,
    middleware::Next,
    response::Response,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tolight_core::{
    models::{AgentContext, ApiError},
    signing,
};

pub const MAX_REQUEST_BYTES: usize = 32 * 1024;

pub async fn authenticate(
    State(room): State<RoomService>,
    mut request: Request,
    next: Next,
) -> Response {
    let source = match source_address(&request, room.config()) {
        Ok(source) => source_bucket(source),
        Err(error) => return errors::response(error),
    };
    let is_application = request.uri().path() == "/participant/v1/applications"
        && request.method() == axum::http::Method::POST;
    let authentication = authenticate_request(&room, &mut request, &source, is_application).await;
    match authentication {
        Ok(()) => next.run(request).await,
        Err(error) => {
            room.process_metrics().record_auth_failure(&error.code);
            errors::response(error)
        }
    }
}

async fn authenticate_request(
    room: &RoomService,
    request: &mut Request,
    source: &str,
    is_application: bool,
) -> Result<(), ApiError> {
    let config = room.config();
    let now = room.now();
    // Scheme always comes from Config. Only explicitly trusted local proxies choose the source bucket.
    if is_application {
        room.check_source_rate(source, now)?;
    }
    let result = async {
        let key = one(request.headers(), "tolight-key")?;
        let timestamp = one(request.headers(), "tolight-timestamp")?;
        let signature = one(request.headers(), "tolight-signature")?;
        signing::validate_headers(&key, &timestamp, &signature)?;
        let authority = request
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .or_else(|| request.uri().authority().map(|value| value.as_str()))
            .ok_or_else(|| ApiError::new("misdirected_request"))?
            .to_string();
        let canonical = signing::canonical_authority(&authority, &config.participant_scheme)
            .map_err(|_| ApiError::new("misdirected_request"))?;
        if !config.participant_hosts.contains(&canonical) {
            return Err(ApiError::new("misdirected_request"));
        }
        let signed_at = signing::parse_timestamp(&timestamp)?;
        if (i128::from(signed_at) - i128::from(now.unix_timestamp())).abs()
            > i128::from(config.signature_max_skew)
        {
            return Err(ApiError::new("clock_skew")
                .with("server_time", tolight_core::datetime::utc_seconds(now)));
        }
        let context = if is_application {
            None
        } else {
            let agent = room
                .store()
                .agent_by_key(&key)
                .await?
                .ok_or_else(|| ApiError::new("unauthorized"))?;
            Some(AgentContext {
                agent_id: agent.agent_id,
                public_key: key.clone(),
            })
        };
        let method = request.method().as_str().to_string();
        let path = request.uri().path().to_string();
        let query = request.uri().query().unwrap_or("").to_string();
        let body = std::mem::replace(request.body_mut(), Body::empty());
        let body = tokio::time::timeout(
            std::time::Duration::from_millis(config.network.body_read_timeout_ms),
            to_bytes(body, MAX_REQUEST_BYTES),
        )
        .await
        .map_err(|_| ApiError::new("bad_request"))?
        .map_err(|_| ApiError::new("too_large"))?;
        signing::verify_request(
            &key,
            &timestamp,
            &signature,
            &method,
            &authority,
            &config.participant_scheme,
            &path,
            &query,
            &body,
        )?;
        *request.body_mut() = Body::from(body);
        request.extensions_mut().insert(SignedKey(key));
        request
            .extensions_mut()
            .insert(SourceAddress(source.to_string()));
        if let Some(context) = context {
            room.check_agent_rate(&context.agent_id, now)?;
            room.store()
                .touch_last_seen(&context.agent_id, &context.public_key)
                .await?;
            request.extensions_mut().insert(context);
        }
        Ok(())
    }
    .await;
    if !is_application
        && result
            .as_ref()
            .is_err_and(|error| error.code != "rate_limited")
    {
        room.check_source_rate(source, now)?;
    }
    result
}

#[derive(Clone)]
pub struct SignedKey(pub String);

#[derive(Clone)]
pub struct SourceAddress(pub String);

fn one(headers: &axum::http::HeaderMap, name: &str) -> Result<String, ApiError> {
    let mut values = headers.get_all(name).iter();
    let first = values.next().ok_or_else(|| ApiError::new("unauthorized"))?;
    if values.next().is_some() {
        return Err(ApiError::new("unauthorized"));
    }
    Ok(first
        .to_str()
        .map_err(|_| ApiError::new("unauthorized"))?
        .to_string())
}

pub fn source_address(
    request: &Request,
    config: &tolight_core::config::Config,
) -> Result<IpAddr, ApiError> {
    let Some(peer) = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|address| address.0.ip())
    else {
        // In-process transports may omit peer metadata. They never establish proxy trust.
        return Ok(IpAddr::V4(Ipv4Addr::LOCALHOST));
    };
    if !peer.is_loopback() || !config.network.trusted_proxies.contains(&peer) {
        return Ok(peer);
    }
    let values = request
        .headers()
        .get_all("x-forwarded-for")
        .iter()
        .collect::<Vec<_>>();
    if values.is_empty() && config.network.allow_direct_loopback {
        // Local LAN development shares this listener with a trusted proxy and
        // direct loopback clients. Present-but-invalid proxy headers still fail.
        return Ok(peer);
    }
    if values.len() != 1 {
        return Err(ApiError::new("bad_request"));
    }
    // The configured proxy overwrites the header with exactly one actual peer IP.
    values[0]
        .to_str()
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| ApiError::new("bad_request"))
}
