//! Deserialize, invoke room/admission, serialize. No timeline rules live here.

use crate::{
    admission,
    auth::{self, SignedKey, SourceAddress},
    errors, metrics,
    room::RoomService,
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Extension, Request, State},
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::de::DeserializeOwned;
use tolight_core::{
    models::{AgentContext, ApiError, ApplicationInput, SendRequest},
    participant::ReadRequest,
};

pub fn router(room: RoomService) -> Router {
    Router::new()
        .route("/participant/v1/applications", post(apply))
        .route("/participant/v1/events", get(events))
        .route("/participant/v1/messages", post(messages))
        .route("/participant/v1/agents", get(agents))
        .route("/participant/v1/self", get(self_info))
        .route("/participant/v1/self/leave", post(self_leave))
        .fallback(|| async { errors::response(ApiError::new("not_found")) })
        .method_not_allowed_fallback(|| async { errors::response(ApiError::new("not_found")) })
        .layer(middleware::from_fn_with_state(
            room.clone(),
            auth::authenticate,
        ))
        .layer(middleware::from_fn_with_state(
            room.clone(),
            deadline_and_record,
        ))
        .layer(middleware::from_fn_with_state(room.clone(), no_store))
        .with_state(room)
}

async fn deadline_and_record(
    State(room): State<RoomService>,
    request: Request,
    next: middleware::Next,
) -> Response {
    let endpoint = metrics::endpoint(request.uri().path());
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(room.config().network.total_request_timeout_ms),
        next.run(request),
    )
    .await;
    let response = match result {
        Ok(response) => response,
        Err(_) => errors::response(ApiError::new("unavailable").retry(1)),
    };
    room.process_metrics()
        .record_request(endpoint, response.status().as_u16());
    response
}

async fn no_store(
    State(room): State<RoomService>,
    request: Request,
    next: middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response.headers_mut().insert(
        header::DATE,
        tolight_core::datetime::http_date(room.now())
            .parse()
            .expect("HTTP date"),
    );
    response
}

fn success(status: StatusCode, value: impl serde::Serialize) -> Response {
    (status, Json(value)).into_response()
}
fn finish<T: serde::Serialize>(result: Result<T, ApiError>) -> Response {
    match result {
        Ok(value) => success(StatusCode::OK, value),
        Err(error) => errors::response(error),
    }
}

async fn json_body<T: DeserializeOwned>(request: Request) -> Result<T, ApiError> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if content_type != "application/json"
        && !(content_type.starts_with("application/") && content_type.ends_with("+json"))
    {
        return Err(ApiError::new("bad_request"));
    }
    if request.uri().query().is_some_and(|value| !value.is_empty()) {
        return Err(ApiError::new("bad_request"));
    }
    let body = to_bytes(request.into_body(), auth::MAX_REQUEST_BYTES)
        .await
        .map_err(|_| ApiError::new("too_large"))?;
    serde_json::from_slice(&body).map_err(|_| ApiError::new("bad_request"))
}

async fn apply(
    State(room): State<RoomService>,
    Extension(key): Extension<SignedKey>,
    Extension(source): Extension<SourceAddress>,
    request: Request,
) -> Response {
    let result = async {
        let input: ApplicationInput = json_body(request).await?;
        admission::apply(&room, input, key.0, &source.0).await
    }
    .await;
    match result {
        Ok((created, value)) => success(
            if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            value,
        ),
        Err(error) => errors::response(error),
    }
}

async fn events(
    State(room): State<RoomService>,
    Extension(context): Extension<AgentContext>,
    request: Request,
) -> Response {
    if request.method() != axum::http::Method::GET {
        return errors::response(ApiError::new("not_found"));
    }
    let params =
        match serde_urlencoded::from_str::<ReadRequest>(request.uri().query().unwrap_or("")) {
            Ok(params) => params,
            Err(_) => return errors::response(ApiError::new("bad_request")),
        };
    finish(room.read(&context, params).await)
}

async fn messages(
    State(room): State<RoomService>,
    Extension(context): Extension<AgentContext>,
    request: Request,
) -> Response {
    let result = async {
        let input: SendRequest = json_body(request).await?;
        room.send(&context, input).await
    }
    .await;
    match result {
        Ok(value) => success(
            if value.duplicate {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            },
            value,
        ),
        Err(error) => errors::response(error),
    }
}

fn no_query(request: &Request) -> Result<(), ApiError> {
    if request.method() != axum::http::Method::GET {
        return Err(ApiError::new("not_found"));
    }
    if request.uri().query().is_some_and(|value| !value.is_empty()) {
        Err(ApiError::new("bad_request"))
    } else {
        Ok(())
    }
}
async fn agents(
    State(room): State<RoomService>,
    Extension(context): Extension<AgentContext>,
    request: Request,
) -> Response {
    if let Err(error) = no_query(&request) {
        return errors::response(error);
    }
    drop(request);
    finish(room.list_participants(&context).await)
}
async fn self_info(
    State(room): State<RoomService>,
    Extension(context): Extension<AgentContext>,
    request: Request,
) -> Response {
    if let Err(error) = no_query(&request) {
        return errors::response(error);
    }
    drop(request);
    finish(room.self_info(&context).await)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaveRequest {
    mode: String,
}
async fn self_leave(
    State(room): State<RoomService>,
    Extension(context): Extension<AgentContext>,
    request: Request,
) -> Response {
    let result=async {
        let input:LeaveRequest=json_body(request).await?;
        if !matches!(input.mode.as_str(),"leave"|"withdraw") {return Err(ApiError::new("bad_request").with("field","mode"));}
        room.store().owner_leave(&context,&input.mode).await?;
        room.notify_change();
        Ok(serde_json::json!({"agent_id":context.agent_id,"state":if input.mode=="leave" {Some("LEFT")}else{None}}))
    }.await;
    finish(result)
}
