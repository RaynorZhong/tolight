//! The participant protocol has one error mapping for every transport path.

use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use tolight_core::models::ApiError;

pub fn response(mut error: ApiError) -> Response {
    if matches!(error.code.as_str(), "unavailable" | "applications_full")
        && error.retry_after.is_none()
    {
        error.retry_after = Some(1);
    }
    let mut response = (
        StatusCode::from_u16(error.status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        Json(error.body()),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    if let Some(seconds) = error.retry_after {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, seconds.to_string().parse().unwrap());
    }
    response
}

pub fn unavailable() -> ApiError {
    ApiError::new("unavailable").retry(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_protocol_error_has_its_wire_status_and_no_store() {
        for (code, status) in [
            ("bad_request", 400),
            ("unauthorized", 401),
            ("clock_skew", 401),
            ("agent_not_active", 403),
            ("agent_muted", 403),
            ("applications_closed", 403),
            ("not_found", 404),
            ("invalid_state", 409),
            ("agent_id_taken", 409),
            ("display_name_taken", 409),
            ("key_already_registered", 409),
            ("room_paused", 409),
            ("too_large", 413),
            ("misdirected_request", 421),
            ("content_rejected", 422),
            ("invalid_mention", 422),
            ("too_many_mentions", 422),
            ("message_limit", 429),
            ("rate_limited", 429),
            ("applications_full", 503),
            ("unavailable", 503),
        ] {
            let result = response(ApiError::new(code));
            assert_eq!(result.status().as_u16(), status, "{code}");
            assert_eq!(result.headers()[header::CACHE_CONTROL], "no-store");
        }
        let error = ApiError::new("message_limit")
            .with("scope", "minute")
            .retry(7);
        assert_eq!(error.body()["error"]["scope"], "minute");
        assert!(error.body()["error"].get("retry_after").is_none());
        assert_eq!(response(error).headers()[header::RETRY_AFTER], "7");
    }
}
