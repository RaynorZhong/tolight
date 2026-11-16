//! Fault responses use a tiny local HTTP peer; real signed PostgreSQL flows live in onboarding/two_reference_clients.
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, watch};
use tolight_reference_client::{
    Clock, Generator, LocalState, PendingCall, ReferenceClient, ReferenceConfig, RunOptions, keygen,
};

struct Time {
    now: AtomicI64,
    waits: Mutex<Vec<u64>>,
}
impl Time {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            now: AtomicI64::new(Utc::now().timestamp()),
            waits: Mutex::new(Vec::new()),
        })
    }
}
impl Clock for Time {
    fn now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.now.load(Ordering::SeqCst), 0).unwrap()
    }
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.waits.lock().unwrap().push(duration.as_millis() as u64);
        self.now
            .fetch_add(duration.as_secs() as i64, Ordering::SeqCst);
        Box::pin(async { tokio::task::yield_now().await })
    }
}
struct Gen {
    calls: AtomicUsize,
    result: Result<String, String>,
    entered: Option<Arc<Notify>>,
    release: Option<Arc<Notify>>,
}
impl Gen {
    fn line(value: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            result: Ok(value.into()),
            entered: None,
            release: None,
        })
    }
}
impl Generator for Gen {
    fn generate<'a>(
        &'a self,
        _context: &'a str,
        _index: u64,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(entered) = &self.entered {
                entered.notify_one();
            }
            if let Some(release) = &self.release {
                release.notified().await;
            }
            self.result.clone()
        })
    }
}
type ResponsePlan = (u16, &'static str, Option<u64>);

#[derive(Clone)]
struct Peer {
    posts: Arc<Mutex<Vec<Value>>>,
    plans: Arc<Mutex<VecDeque<ResponsePlan>>>,
    gets: Arc<AtomicUsize>,
    events: Value,
    clock: Arc<Mutex<Option<Arc<dyn Clock>>>>,
    post_times: Arc<Mutex<Vec<DateTime<Utc>>>>,
    self_plans: Arc<Mutex<VecDeque<ResponsePlan>>>,
    request_times: Arc<Mutex<Vec<DateTime<Utc>>>>,
    event_plans: Arc<Mutex<VecDeque<Value>>>,
}
async fn handle(
    State(peer): State<Peer>,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let headers = request.headers().clone();
    let bytes = axum::body::to_bytes(request.into_body(), 32768)
        .await
        .unwrap();
    // Even the fault peer verifies exactly the headers/body the client actually sent.
    verify(&headers, &method, &path, &query, &bytes);
    if let Some(clock) = peer.clock.lock().unwrap().as_ref() {
        peer.request_times.lock().unwrap().push(clock.now());
    }
    if path.ends_with("/messages") {
        if let Some(clock) = peer.clock.lock().unwrap().as_ref() {
            peer.post_times.lock().unwrap().push(clock.now());
        }
        peer.posts
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&bytes).unwrap());
        if let Some((status, body, retry)) = peer.plans.lock().unwrap().pop_front() {
            let mut response = (StatusCode::from_u16(status).unwrap(), body).into_response();
            if let Some(retry) = retry {
                response
                    .headers_mut()
                    .insert("Retry-After", retry.to_string().parse().unwrap());
            }
            if status == 307 {
                response
                    .headers_mut()
                    .insert("Location", "http://127.0.0.1:9/never".parse().unwrap());
            }
            return response;
        }
        return axum::Json(
            json!({"seq":42,"event_id":"published","duplicate":false,"mentions":[]}),
        )
        .into_response();
    }
    peer.gets.fetch_add(1, Ordering::SeqCst);
    if path.ends_with("/self") {
        if let Some((status, body, retry)) = peer.self_plans.lock().unwrap().pop_front() {
            let mut response = (StatusCode::from_u16(status).unwrap(), body).into_response();
            if let Some(retry) = retry {
                response
                    .headers_mut()
                    .insert("Retry-After", retry.to_string().parse().unwrap());
            }
            return response;
        }
        axum::Json(json!({"state":"ACTIVE","state_reason":null,"mute":null,"room_paused":false,"limits":{"max_text_scalars":37}})).into_response()
    } else if path.ends_with("/agents") {
        axum::Json(json!({"agents":[]})).into_response()
    } else {
        let page = peer
            .event_plans
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| peer.events.clone());
        axum::Json(page).into_response()
    }
}
fn verify(headers: &HeaderMap, method: &str, path: &str, query: &str, body: &[u8]) {
    let key = headers.get("Tolight-Key").unwrap().to_str().unwrap();
    let stamp = headers.get("Tolight-Timestamp").unwrap().to_str().unwrap();
    let signature = headers.get("Tolight-Signature").unwrap().to_str().unwrap();
    let authority = headers.get("Host").unwrap().to_str().unwrap();
    tolight_core::signing::verify_request(
        key, stamp, signature, method, authority, "http", path, query, body,
    )
    .unwrap();
}

async fn peer(
    events: Value,
    plans: Vec<ResponsePlan>,
) -> (String, Peer, tokio::task::JoinHandle<()>) {
    let peer = Peer {
        posts: Default::default(),
        plans: Arc::new(Mutex::new(plans.into())),
        gets: Default::default(),
        events,
        clock: Default::default(),
        post_times: Default::default(),
        self_plans: Default::default(),
        request_times: Default::default(),
        event_plans: Default::default(),
    };
    let router = Router::new().fallback(any(handle)).with_state(peer.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}/participant/v1/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (origin, peer, task)
}
fn config(temp: &std::path::Path, base_url: String) -> ReferenceConfig {
    let config = ReferenceConfig {
        base_url,
        key_path: temp.join("key"),
        state_path: temp.join("state.json"),
        lines_path: temp.join("lines.txt"),
        min_speaking_interval_seconds: 0,
        long_poll_seconds: 0,
        ..ReferenceConfig::default()
    };
    std::fs::write(&config.lines_path, "hello\n").unwrap();
    keygen(&config).unwrap();
    config
}
fn page(events: Vec<Value>) -> Value {
    json!({"timeline":"public","events":events,"next_cursor":"next-opaque","head_seq":10,"cursor_reset":false})
}
fn pending(kind: &str) -> PendingCall {
    PendingCall {
        id: "stable-id-for-retry".into(),
        kind: kind.into(),
        trigger_ids: vec!["trigger".into()],
        trigger_events: vec![],
        reply_to: if kind == "reply" {
            Some("trigger".into())
        } else {
            None
        },
        output: Some("generated before crash".into()),
        hint: None,
    }
}

#[tokio::test]
async fn poll_results_recheck_changed_pause_and_mute_states_before_dropping_new_mentions() {
    let ready = r#"{"state":"ACTIVE","state_reason":null,"mute":null,"room_paused":false,"limits":{"max_text_scalars":37}}"#;
    for restricted in [
        r#"{"state":"ACTIVE","state_reason":null,"mute":null,"room_paused":true,"limits":{"max_text_scalars":37}}"#,
        r#"{"state":"ACTIVE","state_reason":null,"mute":{"indefinite":true,"reason_code":"spam"},"room_paused":false,"limits":{"max_text_scalars":37}}"#,
    ] {
        for (before_poll, after_poll, allowed) in [
            (restricted, ready, true),
            (restricted, restricted, false),
            (ready, restricted, false),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (url, peer, server) = peer(page(vec![]), vec![]).await;
            let clock = Time::new();
            *peer.clock.lock().unwrap() = Some(clock.clone());
            peer.self_plans
                .lock()
                .unwrap()
                .push_back((200, before_poll, None));
            peer.self_plans
                .lock()
                .unwrap()
                .push_back((200, after_poll, None));
            // The loop starts with a restricted self snapshot. The read then
            // returns a new mention after the room/agent may have recovered.
            peer.event_plans.lock().unwrap().push_back(page(vec![json!({
                "seq":10,"event_id":"after-resume","kind":"agent_message",
                "mentions_you":true,"from_self":false,"retracted":false,
                "created_at":clock.now().to_rfc3339(),"text":"new mention"
            })]));
            let cfg = config(temp.path(), url);
            LocalState {
                cursor: Some("before-long-poll".into()),
                ..Default::default()
            }
            .save(&cfg.state_path)
            .unwrap();
            let generator = Gen::line("reply to new mention");
            let mut client =
                ReferenceClient::with_dependencies(cfg.clone(), clock.clone(), generator.clone())
                    .unwrap();
            let (_stop, receiver) = watch::channel(false);
            let report = client
                .run(
                    receiver,
                    RunOptions {
                        max_messages: Some(1),
                        run_for_seconds: Some(2),
                    },
                )
                .await
                .unwrap();
            let expected = usize::from(allowed);
            assert_eq!(
                report.messages as usize, expected,
                "allowed={allowed}; before={before_poll}; after={after_poll}"
            );
            assert_eq!(generator.calls.load(Ordering::SeqCst), expected);
            let posts = peer.posts.lock().unwrap();
            assert_eq!(posts.len(), expected);
            if allowed {
                assert_eq!(posts[0]["reply_to_event_id"], "after-resume");
            }
            let saved = LocalState::load(&cfg.state_path).unwrap();
            assert_eq!(saved.cursor.as_deref(), Some("next-opaque"));
            assert!(saved.pending.is_none());
            assert!(saved.processed.contains("after-resume"));
            // Still-restricted messages are dropped immediately. Later healthy
            // polls must not regenerate or send the discarded reply.
            assert!(clock.waits.lock().unwrap().iter().all(|wait| *wait == 1000));
            server.abort();
        }
        // Resuming already-persisted output uses a fresh self response. Once a
        // restriction is observed there, discard it rather than recheck until
        // the restriction disappears and replay the old output.
        let temp = tempfile::tempdir().unwrap();
        let (url, peer, server) = peer(page(vec![]), vec![]).await;
        peer.self_plans
            .lock()
            .unwrap()
            .extend([(200, restricted, None), (200, ready, None)]);
        let cfg = config(temp.path(), url);
        LocalState {
            cursor: Some("old-cursor".into()),
            pending: Some(pending("reply")),
            ..Default::default()
        }
        .save(&cfg.state_path)
        .unwrap();
        let generator = Gen::line("must not regenerate");
        let mut client =
            ReferenceClient::with_dependencies(cfg.clone(), Time::new(), generator.clone())
                .unwrap();
        let (_stop, receiver) = watch::channel(false);
        let report = client
            .run(
                receiver,
                RunOptions {
                    max_messages: Some(1),
                    run_for_seconds: Some(2),
                },
            )
            .await
            .unwrap();
        assert_eq!(report.messages, 0);
        assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
        assert!(peer.posts.lock().unwrap().is_empty());
        assert!(LocalState::load(&cfg.state_path).unwrap().pending.is_none());
        server.abort();
    }
}

#[tokio::test]
async fn restriction_refresh_rate_limit_preserves_pending_reply_id_and_waits_before_generation() {
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(page(vec![]), vec![]).await;
    let started = DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap();
    let waiting = Arc::new(Notify::new());
    let clock = Arc::new(RetryWait {
        now: started,
        waiting: waiting.clone(),
    });
    *peer.clock.lock().unwrap() = Some(clock.clone());
    peer.self_plans.lock().unwrap().extend([
        (
            200,
            r#"{"state":"ACTIVE","mute":null,"room_paused":true}"#,
            None,
        ),
        (429, r#"{"error":{"code":"rate_limited"}}"#, Some(7)),
    ]);
    peer.event_plans.lock().unwrap().push_back(page(vec![json!({
        "seq":10,"event_id":"after-resume","kind":"agent_message",
        "mentions_you":true,"from_self":false,"retracted":false,
        "created_at":started.to_rfc3339(),"text":"new mention"
    })]));
    let cfg = config(temp.path(), url);
    LocalState {
        cursor: Some("before-interruption".into()),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let generator = Gen::line("reply after full cooldown");
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), clock.clone(), generator.clone()).unwrap();
    let (_stop, receiver) = watch::channel(false);
    let task = tokio::spawn(async move { client.run(receiver, RunOptions::default()).await });
    tokio::time::timeout(Duration::from_secs(2), waiting.notified())
        .await
        .unwrap();
    let saved = LocalState::load(&cfg.state_path).unwrap();
    assert_eq!(saved.cursor.as_deref(), Some("before-interruption"));
    assert_eq!(saved.calls, 0);
    let original_id = saved.pending.as_ref().unwrap().id.clone();
    assert_eq!(
        saved.pending.as_ref().unwrap().trigger_ids,
        ["after-resume"]
    );
    assert!(saved.pending.as_ref().unwrap().output.is_none());
    assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
    assert!(peer.posts.lock().unwrap().is_empty());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    server.abort();

    let (url, peer, server) = self::peer(page(vec![]), vec![]).await;
    let clock = Time::new();
    clock.now.store(started.timestamp(), Ordering::SeqCst);
    *peer.clock.lock().unwrap() = Some(clock.clone());
    let mut cfg = cfg;
    cfg.base_url = url;
    let generator = Gen::line("reply after full cooldown");
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), clock.clone(), generator.clone()).unwrap();
    let (_stop, receiver) = watch::channel(false);
    let report = client
        .run(
            receiver,
            RunOptions {
                max_messages: Some(1),
                run_for_seconds: Some(15),
            },
        )
        .await
        .unwrap();
    assert_eq!(report.messages, 1);
    assert_eq!(generator.calls.load(Ordering::SeqCst), 1);
    let posts = peer.posts.lock().unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["client_message_id"], original_id);
    assert_eq!(posts[0]["reply_to_event_id"], "after-resume");
    assert!(clock.waits.lock().unwrap().contains(&7000));
    let times = peer.request_times.lock().unwrap();
    assert!(!times.is_empty());
    assert!(times.iter().all(|at| (*at - started).num_seconds() >= 7));
    let saved = LocalState::load(&cfg.state_path).unwrap();
    assert_eq!(saved.calls, 1);
    assert!(saved.pending.is_none());
    assert!(saved.processed.contains("after-resume"));
    server.abort();
}

#[tokio::test]
async fn crash_saved_output_retries_identical_id_after_5xx_and_rate_limit_without_regeneration() {
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(
        page(vec![]),
        vec![
            (503, "not JSON gateway", None),
            (429, r#"{"error":{"code":"rate_limited"}}"#, Some(7)),
        ],
    )
    .await;
    let cfg = config(temp.path(), url);
    LocalState {
        pending: Some(pending("reply")),
        cursor: Some("old-cursor".into()),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let time = Time::new();
    let generator = Gen::line("must not regenerate");
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), time.clone(), generator.clone()).unwrap();
    let (_tx, rx) = watch::channel(false);
    let report = client
        .run(
            rx,
            RunOptions {
                max_messages: Some(1),
                run_for_seconds: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(report.messages, 1);
    assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
    let posts = peer.posts.lock().unwrap();
    assert_eq!(posts.len(), 3);
    assert!(posts.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(posts[0]["client_message_id"], "stable-id-for-retry");
    drop(posts);
    assert!(time.waits.lock().unwrap().iter().any(|wait| *wait >= 7000));
    let state = LocalState::load(&cfg.state_path).unwrap();
    assert!(state.pending.is_none());
    assert!(state.processed.contains("trigger"));
    server.abort();
}

#[tokio::test]
async fn reply_message_limit_waits_and_retries_while_proactive_is_discarded() {
    for kind in ["reply", "proactive"] {
        let temp = tempfile::tempdir().unwrap();
        let (url, peer, server) = peer(
            page(vec![]),
            vec![(
                429,
                r#"{"error":{"code":"message_limit","scope":"minute"}}"#,
                Some(6),
            )],
        )
        .await;
        let cfg = config(temp.path(), url);
        let time = Time::new();
        let mut client =
            ReferenceClient::with_dependencies(cfg, time.clone(), Gen::line("hello")).unwrap();
        client.state.pending = Some(pending(kind));
        let (_tx, rx) = watch::channel(false);
        let report = client
            .run(
                rx,
                RunOptions {
                    max_messages: if kind == "reply" { Some(1) } else { None },
                    run_for_seconds: Some(8),
                },
            )
            .await
            .unwrap();
        assert_eq!(report.messages, if kind == "reply" { 1 } else { 0 });
        assert_eq!(
            peer.posts.lock().unwrap().len(),
            if kind == "reply" { 2 } else { 1 }
        );
        assert!(client.state.pending.is_none());
        assert!(time.now().timestamp() >= client.state.cooldown_until.unwrap());
        server.abort();
    }
}

#[tokio::test]
async fn nonretryable_content_pause_mute_and_redirect_do_not_repost() {
    for (status, code) in [
        (400, "bad_request"),
        (413, "too_large"),
        (422, "content_rejected"),
        (400, "plain_gateway_error"),
        (413, "plain_gateway_error"),
        (422, "future_content_rule"),
        (409, "room_paused"),
        (403, "agent_muted"),
        (403, "agent_not_active"),
        (307, "redirect"),
    ] {
        let body = match code {
            "bad_request" => r#"{"error":{"code":"bad_request"}}"#,
            "too_large" => r#"{"error":{"code":"too_large"}}"#,
            "content_rejected" => r#"{"error":{"code":"content_rejected"}}"#,
            "future_content_rule" => r#"{"error":{"code":"future_content_rule"}}"#,
            "room_paused" => r#"{"error":{"code":"room_paused"}}"#,
            "agent_muted" => r#"{"error":{"code":"agent_muted"}}"#,
            "agent_not_active" => r#"{"error":{"code":"agent_not_active"}}"#,
            _ => "redirect",
        };
        let temp = tempfile::tempdir().unwrap();
        let (url, peer, server) = peer(page(vec![]), vec![(status, body, None)]).await;
        let cfg = config(temp.path(), url);
        let mut client =
            ReferenceClient::with_dependencies(cfg, Time::new(), Gen::line("hello")).unwrap();
        client.state.pending = Some(pending("reply"));
        let (_tx, rx) = watch::channel(false);
        let result = client
            .run(
                rx,
                RunOptions {
                    run_for_seconds: Some(3),
                    max_messages: None,
                },
            )
            .await;
        if code == "redirect" {
            assert_eq!(result.unwrap_err().code, "redirect");
        } else {
            assert_eq!(result.unwrap().messages, 0);
            assert!(client.state.pending.is_none());
        }
        assert_eq!(peer.posts.lock().unwrap().len(), 1);
        server.abort();
    }
}

#[tokio::test]
async fn budget_silence_failure_spacing_and_utc_reset_are_persistent() {
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(page(vec![]), vec![]).await;
    let mut cfg = config(temp.path(), url);
    cfg.daily_call_budget = 2;
    cfg.min_speaking_interval_seconds = 30;
    let time = Time::new();
    let generator = Gen::line("[SILENT]");
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), time.clone(), generator.clone()).unwrap();
    assert!(!client.say(None).await.unwrap());
    assert_eq!(client.state.calls, 1);
    assert!(!client.say(None).await.unwrap());
    assert_eq!(client.state.calls, 1);
    time.now.fetch_add(30, Ordering::SeqCst);
    assert!(!client.say(None).await.unwrap());
    assert_eq!(client.state.calls, 2);
    time.now.fetch_add(30, Ordering::SeqCst);
    assert!(!client.say(None).await.unwrap());
    assert_eq!(generator.calls.load(Ordering::SeqCst), 2);
    let reloaded = LocalState::load(&cfg.state_path).unwrap();
    assert_eq!(reloaded.calls, 2);
    assert!(peer.posts.lock().unwrap().is_empty());
    time.now.fetch_add(86400, Ordering::SeqCst);
    assert!(!client.say(None).await.unwrap());
    assert_eq!(client.state.calls, 1);
    server.abort();
}

#[tokio::test]
async fn stop_finishes_inflight_generation_and_post_without_advancing_unfinished_page() {
    let time = Time::new();
    let event = json!({"seq":10,"event_id":"fresh-trigger","kind":"agent_message","mentions_you":true,"from_self":false,"retracted":false,"text":"@reference-agent hello","created_at":time.now().to_rfc3339()});
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(page(vec![event]), vec![]).await;
    let cfg = config(temp.path(), url);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let generator = Arc::new(Gen {
        calls: AtomicUsize::new(0),
        result: Ok("finish inflight".into()),
        entered: Some(entered.clone()),
        release: Some(release.clone()),
    });
    let mut client = ReferenceClient::with_dependencies(cfg.clone(), time, generator).unwrap();
    client.state.cursor = Some("previous-cursor".into());
    client.state.save(&cfg.state_path).unwrap();
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(async move { client.run(rx, RunOptions::default()).await });
    entered.notified().await;
    stop.send(true).unwrap();
    release.notify_one();
    task.await.unwrap().unwrap();
    let state = LocalState::load(&cfg.state_path).unwrap();
    assert_eq!(state.cursor.as_deref(), Some("previous-cursor"));
    assert!(state.processed.contains("fresh-trigger"));
    assert!(state.pending.is_none());
    assert_eq!(peer.posts.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn empty_and_failed_generation_consume_budget_without_any_post() {
    for generated in [Ok(String::new()), Err("generation failed".to_owned())] {
        let temp = tempfile::tempdir().unwrap();
        let (url, peer, server) = peer(page(vec![]), vec![]).await;
        let cfg = config(temp.path(), url);
        let generator = Arc::new(Gen {
            calls: AtomicUsize::new(0),
            result: generated,
            entered: None,
            release: None,
        });
        let mut client =
            ReferenceClient::with_dependencies(cfg, Time::new(), generator.clone()).unwrap();
        assert!(!client.say(None).await.unwrap());
        assert_eq!(client.state.calls, 1);
        assert_eq!(generator.calls.load(Ordering::SeqCst), 1);
        assert!(peer.posts.lock().unwrap().is_empty());
        assert!(client.state.pending.is_none());
        server.abort();
    }
}

#[tokio::test]
async fn replayed_page_filters_processed_triggers_and_ignores_unknown_self_and_retracted_events() {
    // The fake clock and protocol event timestamps both use whole seconds.
    let now = DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap();
    let base = json!({"seq":1,"event_id":"old","kind":"agent_message","mentions_you":true,"from_self":false,"retracted":false,"created_at":now.to_rfc3339(),"text":"old"});
    let mut fresh = base.clone();
    fresh["seq"] = 2.into();
    fresh["event_id"] = "fresh".into();
    let mut own = base.clone();
    own["seq"] = 3.into();
    own["event_id"] = "own".into();
    own["from_self"] = true.into();
    let mut removed = base.clone();
    removed["seq"] = 4.into();
    removed["event_id"] = "removed".into();
    removed["retracted"] = true.into();
    let mut unknown = base.clone();
    unknown["seq"] = 5.into();
    unknown["event_id"] = "unknown".into();
    unknown["kind"] = "future_kind".into();
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(page(vec![base, fresh, own, removed, unknown]), vec![]).await;
    let cfg = config(temp.path(), url);
    let mut state = LocalState {
        cursor: Some("old-page-cursor".into()),
        ..Default::default()
    };
    state.processed.insert("old".into());
    state.save(&cfg.state_path).unwrap();
    let mut client =
        ReferenceClient::with_dependencies(cfg, Time::new(), Gen::line("one reply")).unwrap();
    let (_tx, rx) = watch::channel(false);
    client
        .run(
            rx,
            RunOptions {
                max_messages: Some(1),
                run_for_seconds: Some(10),
            },
        )
        .await
        .unwrap();
    let posts = peer.posts.lock().unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["reply_to_event_id"], "fresh");
    assert!(posts[0]["mentions"].as_array().unwrap().is_empty());
    assert!(client.state.processed.contains("fresh"));
    server.abort();
}

#[tokio::test]
async fn serial_say_always_restores_supervision_and_never_says_after_stop_failure() {
    for result in [
        Ok(true),
        Ok(false),
        Err(tolight_reference_client::ClientError::local(
            "message_limit",
            "429",
        )),
        Err(tolight_reference_client::ClientError::local(
            "generation",
            "生成失败",
        )),
        Err(tolight_reference_client::ClientError::local(
            "transport",
            "超时",
        )),
    ] {
        let order = Arc::new(Mutex::new(Vec::new()));
        let stopping = order.clone();
        let saying = order.clone();
        let restoring = order.clone();
        let _ = tolight_reference_client::serial_say(
            move || async move {
                stopping.lock().unwrap().push("stop-and-wait");
                Ok(())
            },
            move || async move {
                saying.lock().unwrap().push("say");
                result
            },
            move || async move {
                restoring.lock().unwrap().push("restore");
                Ok(())
            },
        )
        .await;
        assert_eq!(*order.lock().unwrap(), ["stop-and-wait", "say", "restore"]);
    }
    let order = Arc::new(Mutex::new(Vec::new()));
    let stopping = order.clone();
    let saying = order.clone();
    let restoring = order.clone();
    let result = tolight_reference_client::serial_say(
        move || async move {
            stopping.lock().unwrap().push("stop-failed");
            Err(tolight_reference_client::ClientError::local(
                "state",
                "落盘失败",
            ))
        },
        move || async move {
            saying.lock().unwrap().push("must-not-say");
            Ok(true)
        },
        move || async move {
            restoring.lock().unwrap().push("restore");
            Ok(())
        },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(*order.lock().unwrap(), ["stop-failed", "restore"]);
}

#[tokio::test]
async fn saved_sandbox_output_is_discarded_after_approval_instead_of_published() {
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(page(vec![]), vec![]).await;
    let cfg = config(temp.path(), url);
    LocalState {
        pending: Some(pending("conformance_reply")),
        cursor: Some("sandbox-cursor".into()),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let generator = Gen::line("must not generate");
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), Time::new(), generator.clone()).unwrap();
    let (_tx, rx) = watch::channel(false);
    let report = client
        .run(
            rx,
            RunOptions {
                run_for_seconds: Some(1),
                max_messages: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(report.messages, 0);
    assert_eq!(report.sandbox_replies, 0);
    assert!(peer.posts.lock().unwrap().is_empty());
    assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
    assert!(LocalState::load(&cfg.state_path).unwrap().pending.is_none());
    server.abort();
}

struct RetryWait {
    now: DateTime<Utc>,
    waiting: Arc<Notify>,
}
impl Clock for RetryWait {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }
    fn sleep(&self, _duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {
            self.waiting.notify_one();
            std::future::pending().await
        })
    }
}
#[tokio::test]
async fn normal_stop_does_not_wait_a_whole_day_retry_window_or_advance_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(
        page(vec![]),
        vec![(
            429,
            r#"{"error":{"code":"message_limit","scope":"day"}}"#,
            Some(86400),
        )],
    )
    .await;
    let cfg = config(temp.path(), url);
    LocalState {
        pending: Some(pending("reply")),
        cursor: Some("unconsumed-page".into()),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let waiting = Arc::new(Notify::new());
    let generator = Gen::line("must not regenerate");
    let mut client = ReferenceClient::with_dependencies(
        cfg.clone(),
        Arc::new(RetryWait {
            now: Utc::now(),
            waiting: waiting.clone(),
        }),
        generator.clone(),
    )
    .unwrap();
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(async move { client.run(rx, RunOptions::default()).await });
    tokio::time::timeout(Duration::from_secs(2), waiting.notified())
        .await
        .unwrap();
    stop.send(true).unwrap();
    let report = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(report.messages, 0);
    assert_eq!(peer.posts.lock().unwrap().len(), 1);
    assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
    let state = LocalState::load(&cfg.state_path).unwrap();
    assert_eq!(state.cursor.as_deref(), Some("unconsumed-page"));
    assert_eq!(state.pending.unwrap().id, "stable-id-for-retry");
    assert!(state.cooldown_until.is_some());
    server.abort();
}

struct MillisecondTime {
    now: AtomicI64,
}
impl Clock for MillisecondTime {
    fn now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(self.now.load(Ordering::SeqCst)).unwrap()
    }
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.now
            .fetch_add(duration.as_millis() as i64, Ordering::SeqCst);
        Box::pin(async { tokio::task::yield_now().await })
    }
}

#[tokio::test]
async fn restart_preserves_the_full_retry_after_from_a_fractional_second() {
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(
        page(vec![]),
        vec![(
            429,
            r#"{"error":{"code":"message_limit","scope":"minute"}}"#,
            Some(20),
        )],
    )
    .await;
    let cfg = config(temp.path(), url);
    LocalState {
        pending: Some(pending("reply")),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let received_at = DateTime::from_timestamp(1_800_000_000, 900_000_000).unwrap();
    let waiting = Arc::new(Notify::new());
    let mut client = ReferenceClient::with_dependencies(
        cfg.clone(),
        Arc::new(RetryWait {
            now: received_at,
            waiting: waiting.clone(),
        }),
        Gen::line("must not regenerate"),
    )
    .unwrap();
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(async move { client.run(rx, RunOptions::default()).await });
    tokio::time::timeout(Duration::from_secs(2), waiting.notified())
        .await
        .unwrap();
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(peer.posts.lock().unwrap().len(), 1);

    // Restart at an integral second shortly before the original 20.9s deadline.
    // Truncating the persisted deadline to 20s would retry 900ms too early.
    let time = Arc::new(MillisecondTime {
        now: AtomicI64::new(1_800_000_019_000),
    });
    let mut client =
        ReferenceClient::with_dependencies(cfg, time.clone(), Gen::line("must not regenerate"))
            .unwrap();
    let (_stop, rx) = watch::channel(false);
    client
        .run(
            rx,
            RunOptions {
                max_messages: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(peer.posts.lock().unwrap().len(), 2);
    assert!(time.now() >= received_at + chrono::Duration::seconds(20));
    server.abort();
}

#[tokio::test]
async fn fractional_speaking_interval_survives_restart_and_allows_its_exact_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(page(vec![]), vec![]).await;
    let mut cfg = config(temp.path(), url);
    cfg.min_speaking_interval_seconds = 30;
    let time = Arc::new(MillisecondTime {
        now: AtomicI64::new(1_800_000_000_900),
    });
    *peer.clock.lock().unwrap() = Some(time.clone());
    let generator = Gen::line("fractional speaking interval");
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), time.clone(), generator.clone()).unwrap();
    assert!(client.say(None).await.unwrap());
    let state = LocalState::load(&cfg.state_path).unwrap();
    assert_eq!(state.last_call_subsec_nanos, Some(900_000_000));
    assert_eq!(state.last_sent_subsec_nanos, Some(900_000_000));
    drop(client);

    time.now.store(1_800_000_030_000, Ordering::SeqCst);
    let mut client =
        ReferenceClient::with_dependencies(cfg, time.clone(), generator.clone()).unwrap();
    assert!(
        !client.say(None).await.unwrap(),
        "29.1 seconds must not satisfy 30 seconds"
    );
    assert_eq!(generator.calls.load(Ordering::SeqCst), 1);
    time.now.store(1_800_000_030_900, Ordering::SeqCst);
    assert!(client.say(None).await.unwrap());
    let posts = peer.post_times.lock().unwrap();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1] - posts[0], chrono::Duration::seconds(30));
    server.abort();
}

#[tokio::test]
async fn persisted_output_and_message_limit_retry_respect_fractional_speaking_spacing() {
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(
        page(vec![]),
        vec![(429, r#"{"error":{"code":"message_limit"}}"#, Some(1))],
    )
    .await;
    let mut cfg = config(temp.path(), url);
    cfg.min_speaking_interval_seconds = 30;
    let previous = DateTime::from_timestamp(1_800_000_000, 900_000_000).unwrap();
    LocalState {
        last_sent_at: Some(previous.timestamp()),
        last_sent_subsec_nanos: Some(previous.timestamp_subsec_nanos()),
        pending: Some(pending("reply")),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let time = Arc::new(MillisecondTime {
        now: AtomicI64::new(1_800_000_030_000),
    });
    *peer.clock.lock().unwrap() = Some(time.clone());
    let generator = Gen::line("never regenerate persisted output");
    let mut client = ReferenceClient::with_dependencies(cfg, time, generator.clone()).unwrap();
    let (_stop, rx) = watch::channel(false);
    client
        .run(
            rx,
            RunOptions {
                max_messages: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let times = peer.post_times.lock().unwrap();
    assert_eq!(times.len(), 2);
    assert_eq!(times[0] - previous, chrono::Duration::seconds(30));
    assert!(times[1] - times[0] >= chrono::Duration::seconds(1));
    assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
    let posts = peer.posts.lock().unwrap();
    assert_eq!(posts[0], posts[1]);
    server.abort();
}

#[tokio::test]
async fn legacy_second_precision_uses_a_safe_bound_but_zero_spacing_stays_disabled() {
    for interval in [0, 30] {
        let temp = tempfile::tempdir().unwrap();
        let (url, peer, server) = peer(page(vec![]), vec![]).await;
        let mut cfg = config(temp.path(), url);
        cfg.min_speaking_interval_seconds = interval;
        let mut state = serde_json::to_value(LocalState {
            last_call_at: Some(1_800_000_000),
            last_sent_at: Some(1_800_000_000),
            ..Default::default()
        })
        .unwrap();
        for key in [
            "last_call_subsec_nanos",
            "last_sent_subsec_nanos",
            "last_proactive_subsec_nanos",
            "first_started_subsec_nanos",
        ] {
            state.as_object_mut().unwrap().remove(key);
        }
        std::fs::write(&cfg.state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        let time = Arc::new(MillisecondTime {
            now: AtomicI64::new(if interval == 0 {
                1_800_000_000_000
            } else {
                1_800_000_030_999
            }),
        });
        let mut client =
            ReferenceClient::with_dependencies(cfg, time.clone(), Gen::line("legacy interval"))
                .unwrap();
        assert_eq!(client.say(None).await.unwrap(), interval == 0);
        if interval != 0 {
            time.now.store(1_800_000_031_000, Ordering::SeqCst);
            assert!(client.say(None).await.unwrap());
        }
        assert_eq!(peer.posts.lock().unwrap().len(), 1);
        server.abort();
    }
}

#[tokio::test]
async fn first_proactive_interval_uses_persisted_fractional_start() {
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(page(vec![]), vec![]).await;
    let mut cfg = config(temp.path(), url);
    cfg.proactive_interval_seconds = Some(30);
    LocalState {
        first_started_at: Some(1_800_000_000),
        first_started_subsec_nanos: Some(900_000_000),
        cursor: Some("already-bootstrapped".into()),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let time = Arc::new(MillisecondTime {
        now: AtomicI64::new(1_800_000_030_000),
    });
    let generator = Gen::line("proactive fractional start");
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), time.clone(), generator.clone()).unwrap();
    let (_stop, rx) = watch::channel(false);
    client
        .run(
            rx,
            RunOptions {
                run_for_seconds: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
    assert!(peer.posts.lock().unwrap().is_empty());
    time.now.store(1_800_000_030_900, Ordering::SeqCst);
    let mut client = ReferenceClient::with_dependencies(cfg, time, generator.clone()).unwrap();
    let (_stop, rx) = watch::channel(false);
    client
        .run(
            rx,
            RunOptions {
                max_messages: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(generator.calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[test]
fn policy_durations_reject_unsigned_to_signed_overflow_and_trigger_age_is_exact() {
    for value in [i64::MAX as u64 + 1, u64::MAX] {
        for mut cfg in [
            ReferenceConfig {
                min_speaking_interval_seconds: value,
                ..Default::default()
            },
            ReferenceConfig {
                proactive_interval_seconds: Some(value),
                ..Default::default()
            },
            ReferenceConfig {
                max_trigger_age_seconds: value,
                ..Default::default()
            },
        ] {
            assert!(cfg.validate().is_err());
        }
    }
    let mut cfg = ReferenceConfig {
        min_speaking_interval_seconds: i64::MAX as u64,
        proactive_interval_seconds: Some(i64::MAX as u64),
        max_trigger_age_seconds: i64::MAX as u64,
        ..Default::default()
    };
    cfg.validate().unwrap();
    let now = DateTime::from_timestamp(1_800_000_600, 900_000_000).unwrap();
    let event = json!({"event_id":"old-fractional-trigger","seq":1,"kind":"agent_message","mentions_you":true,"from_self":false,"retracted":false,"created_at":DateTime::from_timestamp(1_800_000_000, 0).unwrap().to_rfc3339()});
    let events = [event];
    assert!(
        tolight_reference_client::select_triggers(&events, &Default::default(), now, 600)
            .is_empty()
    );
    assert_eq!(
        tolight_reference_client::select_triggers(&events, &Default::default(), now, u64::MAX)
            .len(),
        1
    );
}

#[tokio::test]
async fn huge_configured_spacing_and_retry_after_remain_interruptible_without_timer_overflow() {
    struct BoundedWait {
        waiting: Arc<Notify>,
        observed: Arc<Mutex<Vec<Duration>>>,
    }
    impl Clock for BoundedWait {
        fn now(&self) -> DateTime<Utc> {
            DateTime::from_timestamp(1_800_000_000, 900_000_000).unwrap()
        }
        fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            self.observed.lock().unwrap().push(duration);
            self.waiting.notify_one();
            Box::pin(std::future::pending())
        }
    }
    for spacing in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let plans = if spacing {
            vec![]
        } else {
            vec![(429, r#"{"error":{"code":"message_limit"}}"#, Some(u64::MAX))]
        };
        let (url, peer, server) = peer(page(vec![]), plans).await;
        let mut cfg = config(temp.path(), url);
        if spacing {
            cfg.min_speaking_interval_seconds = i64::MAX as u64;
        }
        LocalState {
            last_sent_at: spacing.then_some(0),
            last_sent_subsec_nanos: spacing.then_some(0),
            pending: Some(pending("reply")),
            ..Default::default()
        }
        .save(&cfg.state_path)
        .unwrap();
        let waiting = Arc::new(Notify::new());
        let observed = Arc::new(Mutex::new(vec![]));
        let generator = Gen::line("must not regenerate");
        let mut client = ReferenceClient::with_dependencies(
            cfg.clone(),
            Arc::new(BoundedWait {
                waiting: waiting.clone(),
                observed: observed.clone(),
            }),
            generator.clone(),
        )
        .unwrap();
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            client
                .run(
                    rx,
                    RunOptions {
                        run_for_seconds: Some(u64::MAX),
                        ..Default::default()
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), waiting.notified())
            .await
            .unwrap();
        stop.send(true).unwrap();
        assert_eq!(task.await.unwrap().unwrap().messages, 0);
        assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
        assert_eq!(peer.posts.lock().unwrap().len(), usize::from(!spacing));
        assert!(
            observed
                .lock()
                .unwrap()
                .iter()
                .all(|duration| *duration <= Duration::from_secs(60))
        );
        assert!(LocalState::load(&cfg.state_path).unwrap().pending.is_some());
        server.abort();
    }
}

#[test]
fn malformed_fractional_state_is_rejected_without_reset() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    for key in [
        "last_call_subsec_nanos",
        "last_sent_subsec_nanos",
        "last_proactive_subsec_nanos",
        "first_started_subsec_nanos",
    ] {
        let mut state = serde_json::to_value(LocalState::default()).unwrap();
        state[key] = json!(1_000_000_000_u32);
        let bytes = serde_json::to_vec(&state).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(LocalState::load(&path).unwrap_err().code, "state");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn say_status_rate_limits_discard_the_call_and_persist_a_request_wide_cooldown() {
    const ACTIVE: &str =
        r#"{"state":"ACTIVE","mute":null,"room_paused":false,"limits":{"max_text_scalars":37}}"#;
    for status_call in 1..=3 {
        let temp = tempfile::tempdir().unwrap();
        let (url, peer, server) = peer(page(vec![]), vec![]).await;
        peer.self_plans.lock().unwrap().extend(
            std::iter::repeat_n((200, ACTIVE, None), status_call - 1).chain(std::iter::once((
                429,
                r#"{"error":{"code":"rate_limited"}}"#,
                Some(7),
            ))),
        );
        let cfg = config(temp.path(), url);
        let clock = Time::new();
        *peer.clock.lock().unwrap() = Some(clock.clone());
        let generator = Gen::line("fresh model output");
        let mut client =
            ReferenceClient::with_dependencies(cfg.clone(), clock.clone(), generator.clone())
                .unwrap();
        assert!(!client.say(Some("discarded hint".into())).await.unwrap());
        assert!(client.state.pending.is_none(), "status call {status_call}");
        assert!(peer.posts.lock().unwrap().is_empty());
        assert_eq!(
            generator.calls.load(Ordering::SeqCst),
            usize::from(status_call == 3)
        );
        let saved = LocalState::load(&cfg.state_path).unwrap();
        assert_eq!(saved.cooldown_until, Some(clock.now().timestamp() + 7));
        let requests = peer.request_times.lock().unwrap().len();
        let mut restarted =
            ReferenceClient::with_dependencies(cfg, clock.clone(), generator.clone()).unwrap();
        assert!(!restarted.say(None).await.unwrap());
        assert_eq!(peer.request_times.lock().unwrap().len(), requests);
        clock.now.fetch_add(7, Ordering::SeqCst);
        assert!(restarted.say(Some("fresh hint".into())).await.unwrap());
        assert_eq!(peer.posts.lock().unwrap().len(), 1);
        assert_eq!(
            generator.calls.load(Ordering::SeqCst),
            usize::from(status_call == 3) + 1
        );
        server.abort();
    }
}

#[tokio::test]
async fn resumed_proactive_status_rate_limit_discards_saved_output_without_sending_it_later() {
    for kind in ["proactive", "say"] {
        let temp = tempfile::tempdir().unwrap();
        let (url, peer, server) = peer(page(vec![]), vec![]).await;
        peer.self_plans.lock().unwrap().push_back((
            429,
            r#"{"error":{"code":"rate_limited"}}"#,
            Some(7),
        ));
        let cfg = config(temp.path(), url);
        LocalState {
            pending: Some(pending(kind)),
            cursor: Some("preserved-cursor".into()),
            ..Default::default()
        }
        .save(&cfg.state_path)
        .unwrap();
        let clock = Time::new();
        let start = clock.now();
        *peer.clock.lock().unwrap() = Some(clock.clone());
        let generator = Gen::line("must not regenerate");
        let mut client =
            ReferenceClient::with_dependencies(cfg.clone(), clock, generator.clone()).unwrap();
        let (_stop, receiver) = watch::channel(false);
        let report = client
            .run(
                receiver,
                RunOptions {
                    max_messages: Some(1),
                    run_for_seconds: Some(8),
                },
            )
            .await
            .unwrap();
        assert_eq!(report.messages, 0, "saved {kind} must be discarded");
        assert!(peer.posts.lock().unwrap().is_empty());
        assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
        assert!(LocalState::load(&cfg.state_path).unwrap().pending.is_none());
        assert!(
            peer.request_times.lock().unwrap()[1..]
                .iter()
                .all(|time| { time.signed_duration_since(start).num_seconds() >= 7 })
        );
        server.abort();
    }
}

#[tokio::test]
async fn reply_status_rate_limit_keeps_the_original_id_and_waits_before_any_request() {
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(page(vec![]), vec![]).await;
    peer.self_plans.lock().unwrap().push_back((
        429,
        r#"{"error":{"code":"rate_limited"}}"#,
        Some(7),
    ));
    let cfg = config(temp.path(), url);
    LocalState {
        pending: Some(pending("reply")),
        cursor: Some("preserved-cursor".into()),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let clock = Time::new();
    let start = clock.now();
    *peer.clock.lock().unwrap() = Some(clock.clone());
    let generator = Gen::line("must not regenerate");
    let mut client = ReferenceClient::with_dependencies(cfg, clock, generator.clone()).unwrap();
    let (_stop, receiver) = watch::channel(false);
    let report = client
        .run(
            receiver,
            RunOptions {
                max_messages: Some(1),
                run_for_seconds: Some(8),
            },
        )
        .await
        .unwrap();
    assert_eq!(report.messages, 1);
    assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
    let posts = peer.posts.lock().unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["client_message_id"], "stable-id-for-retry");
    assert!(
        peer.request_times.lock().unwrap()[1..]
            .iter()
            .all(|time| { time.signed_duration_since(start).num_seconds() >= 7 })
    );
    server.abort();
}

#[test]
fn retracted_prompt_content_is_removed_from_both_context_and_saved_triggers() {
    let identity = ReferenceConfig::default().application;
    let removed = json!({"seq":1,"event_id":"removed","kind":"agent_message","text":"WITHDRAWN_PROMPT_MARKER","agent":{"agent_id":"other"}});
    let kept = json!({"seq":2,"event_id":"kept","kind":"agent_message","text":"still visible","agent":{"agent_id":"other"}});
    for notice in [
        json!({"seq":3,"event_id":"notice","kind":"event_retracted","target":{"seq":1,"event_id":"removed"}}),
        json!({"seq":1,"event_id":"removed","kind":"agent_message","retracted":true}),
    ] {
        let rendered = tolight_reference_client::render_context(
            &identity,
            &json!({"agents":[]}),
            &[removed.clone(), kept.clone(), notice],
            &[removed.clone(), kept.clone()],
            None,
            2000,
        );
        assert!(!rendered.contains("WITHDRAWN_PROMPT_MARKER"));
        assert_eq!(rendered.matches("still visible").count(), 2);
    }
}

#[tokio::test]
async fn restarted_pending_reply_does_not_reintroduce_retracted_trigger_text_to_the_model() {
    struct Capture {
        prompts: Mutex<Vec<String>>,
        stop: Option<watch::Sender<bool>>,
        entered: Option<Arc<Notify>>,
    }
    impl Generator for Capture {
        fn generate<'a>(
            &'a self,
            context: &'a str,
            _index: u64,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.prompts.lock().unwrap().push(context.into());
                if let Some(entered) = &self.entered {
                    entered.notify_one();
                    std::future::pending::<()>().await;
                }
                if let Some(stop) = &self.stop {
                    stop.send(true).unwrap();
                }
                Ok("[SILENT]".into())
            })
        }
    }
    let removed = json!({"seq":1,"event_id":"removed","kind":"agent_message","text":"PRIVATE_OLD_TRIGGER_MARKER","agent":{"agent_id":"other"}});
    let kept = json!({"seq":2,"event_id":"kept","kind":"agent_message","text":"visible trigger","agent":{"agent_id":"other"}});
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(
        page(vec![
            json!({"seq":1,"event_id":"removed","kind":"agent_message","retracted":true}),
            kept.clone(),
        ]),
        vec![],
    )
    .await;
    let mut cfg = config(temp.path(), url);
    LocalState {
        cursor: Some("before-crash".into()),
        pending: Some(PendingCall {
            id: "original-pending-id".into(),
            kind: "reply".into(),
            trigger_ids: vec!["removed".into(), "kept".into()],
            trigger_events: vec![removed, kept.clone()],
            reply_to: Some("kept".into()),
            output: None,
            hint: None,
        }),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let (_stop, receiver) = watch::channel(false);
    let entered = Arc::new(Notify::new());
    let first_generator = Arc::new(Capture {
        prompts: Default::default(),
        stop: None,
        entered: Some(entered.clone()),
    });
    let mut first =
        ReferenceClient::with_dependencies(cfg.clone(), Time::new(), first_generator.clone())
            .unwrap();
    let task = tokio::spawn(async move { first.run(receiver, RunOptions::default()).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let saved = LocalState::load(&cfg.state_path).unwrap();
    let saved_pending = saved.pending.as_ref().unwrap();
    assert_eq!(saved_pending.id, "original-pending-id");
    assert_eq!(saved_pending.trigger_ids, ["removed", "kept"]);
    assert_eq!(saved_pending.trigger_events, vec![kept.clone()]);
    assert!(saved_pending.output.is_none());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!first_generator.prompts.lock().unwrap()[0].contains("PRIVATE_OLD_TRIGGER_MARKER"));
    assert!(peer.posts.lock().unwrap().is_empty());
    server.abort();

    // On restart the recent window no longer contains the observed retraction.
    // The durable trigger cache must still prevent its old body from returning.
    let (url, peer, server) = self::peer(page(vec![kept]), vec![]).await;
    cfg.base_url = url;
    let (stop, receiver) = watch::channel(false);
    let generator = Arc::new(Capture {
        prompts: Default::default(),
        stop: Some(stop),
        entered: None,
    });
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), Time::new(), generator.clone()).unwrap();
    client.run(receiver, RunOptions::default()).await.unwrap();
    let prompts = generator.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    assert!(!prompts[0].contains("PRIVATE_OLD_TRIGGER_MARKER"));
    assert!(prompts[0].contains("visible trigger"));
    let state = LocalState::load(&cfg.state_path).unwrap();
    assert_eq!(state.cursor.as_deref(), Some("before-crash"));
    assert_eq!(state.calls, 2);
    assert!(state.pending.is_none());
    assert!(peer.posts.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn ungenerated_reply_is_discarded_only_for_an_observed_target_retraction() {
    let now = DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap();
    let trigger = |id: &str, seq| json!({"seq":seq,"event_id":id,"kind":"agent_message","agent":{"agent_id":"other-agent"},"text":"具体问题","mentions_you":true,"from_self":false,"retracted":false,"created_at":now.to_rfc3339()});
    for notice in [
        Some(
            json!({"seq":3,"event_id":"notice","kind":"event_retracted","target":{"seq":2,"event_id":"old-target"}}),
        ),
        Some(json!({"seq":2,"event_id":"old-target","kind":"agent_message","retracted":true})),
        None,
    ] {
        let removed = notice.is_some();
        let temp = tempfile::tempdir().unwrap();
        let (url, peer, server) = peer(page(vec![]), vec![]).await;
        let cfg = config(temp.path(), url);
        LocalState {
            cursor: Some("before-pending".into()),
            pending: Some(PendingCall {
                id: "stable-pending-id".into(),
                kind: "reply".into(),
                trigger_ids: vec!["old-other".into(), "old-target".into()],
                trigger_events: vec![trigger("old-other", 1), trigger("old-target", 2)],
                reply_to: Some("old-target".into()),
                output: None,
                hint: None,
            }),
            ..Default::default()
        }
        .save(&cfg.state_path)
        .unwrap();
        // The pending call's context read sees either a real retraction or a
        // recent window that simply lacks the target. Only the former discards.
        peer.event_plans
            .lock()
            .unwrap()
            .push_back(page(notice.into_iter().collect()));
        if removed {
            peer.event_plans
                .lock()
                .unwrap()
                .push_back(page(vec![trigger("fresh-target", 4)]));
        }
        let generator = Gen::line("直接回答具体问题");
        let mut client =
            ReferenceClient::with_dependencies(cfg.clone(), Time::new(), generator.clone())
                .unwrap();
        let (_stop, receiver) = watch::channel(false);
        let report = tokio::time::timeout(
            Duration::from_secs(3),
            client.run(
                receiver,
                RunOptions {
                    max_messages: Some(1),
                    run_for_seconds: Some(10),
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(report.messages, 1);
        assert_eq!(generator.calls.load(Ordering::SeqCst), 1);
        let posts = peer.posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(
            posts[0]["reply_to_event_id"],
            if removed {
                "fresh-target"
            } else {
                "old-target"
            }
        );
        if !removed {
            assert_eq!(posts[0]["client_message_id"], "stable-pending-id");
        }
        let saved = LocalState::load(&cfg.state_path).unwrap();
        assert_eq!(saved.calls, 1);
        assert!(saved.pending.is_none());
        for id in ["old-other", "old-target"] {
            assert!(saved.processed.contains(id));
        }
        drop(posts);
        server.abort();
    }
}

#[tokio::test]
async fn restored_ungenerated_reply_expires_after_the_boundary_but_keeps_unknown_legacy_age() {
    for (age_millis, legacy, expected) in [
        (600_000, false, 1),
        (600_001, false, 0),
        (86_400_000, false, 0),
        (86_400_000, true, 1),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (url, peer, server) = peer(page(vec![]), vec![]).await;
        let cfg = config(temp.path(), url);
        let time = Arc::new(MillisecondTime {
            now: AtomicI64::new(1_800_000_000_000),
        });
        let trigger = json!({"seq":1,"event_id":"target","kind":"agent_message","mentions_you":true,"from_self":false,"retracted":false,"created_at":(time.now()-chrono::Duration::milliseconds(age_millis)).to_rfc3339(),"text":"一个待回应的问题"});
        let mut state = serde_json::to_value(LocalState {
            cursor: Some("before-pending".into()),
            pending: Some(PendingCall {
                id: "stable-age-reply".into(),
                kind: "reply".into(),
                trigger_ids: vec!["target".into()],
                trigger_events: vec![trigger],
                reply_to: Some("target".into()),
                output: None,
                hint: None,
            }),
            ..Default::default()
        })
        .unwrap();
        if legacy {
            state["pending"]
                .as_object_mut()
                .unwrap()
                .remove("trigger_events");
        }
        std::fs::write(&cfg.state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        let generator = Gen::line("回答当前问题");
        let mut client =
            ReferenceClient::with_dependencies(cfg.clone(), time, generator.clone()).unwrap();
        let (_stop, rx) = watch::channel(false);
        let report = client
            .run(
                rx,
                RunOptions {
                    max_messages: Some(1),
                    run_for_seconds: Some(3),
                },
            )
            .await
            .unwrap();
        assert_eq!(report.messages, expected);
        assert_eq!(generator.calls.load(Ordering::SeqCst), expected as usize);
        let posts = peer.posts.lock().unwrap();
        assert_eq!(posts.len(), expected as usize);
        if let Some(post) = posts.first() {
            assert_eq!(post["client_message_id"], "stable-age-reply");
            assert_eq!(post["reply_to_event_id"], "target");
        }
        let saved = LocalState::load(&cfg.state_path).unwrap();
        assert!(saved.pending.is_none());
        assert!(saved.processed.contains("target"));
        assert_eq!(u64::from(saved.calls), expected);
        server.abort();
    }
}

#[tokio::test]
async fn an_ungenerated_reply_is_rechecked_after_spacing_and_request_cooldown() {
    for (spacing, age_millis, expected) in
        [(true, 599_500, 0), (false, 599_500, 0), (true, 599_000, 1)]
    {
        let temp = tempfile::tempdir().unwrap();
        let (url, peer, server) = peer(page(vec![]), vec![]).await;
        let mut cfg = config(temp.path(), url);
        cfg.min_speaking_interval_seconds = u64::from(spacing);
        let time = Arc::new(MillisecondTime {
            now: AtomicI64::new(1_800_000_000_000),
        });
        let now = time.now();
        LocalState {
            cursor: Some("before-wait".into()),
            last_call_at: spacing.then_some(now.timestamp()),
            last_call_subsec_nanos: spacing.then_some(0),
            cooldown_until: (!spacing).then_some(now.timestamp() + 1),
            pending: Some(PendingCall {
                id: "stable-before-wait".into(),
                kind: "reply".into(),
                trigger_ids: vec!["target".into()],
                trigger_events: vec![json!({"seq":1,"event_id":"target","kind":"agent_message","created_at":(now-chrono::Duration::milliseconds(age_millis)).to_rfc3339(),"text":"等待前仍然新鲜的问题"})],
                reply_to: Some("target".into()),
                output: None,
                hint: None,
            }),
            ..Default::default()
        }
        .save(&cfg.state_path)
        .unwrap();
        let generator = Gen::line("在期限边界回答");
        let mut client =
            ReferenceClient::with_dependencies(cfg.clone(), time.clone(), generator.clone())
                .unwrap();
        let (_stop, rx) = watch::channel(false);
        let report = client
            .run(
                rx,
                RunOptions {
                    max_messages: Some(1),
                    run_for_seconds: Some(3),
                },
            )
            .await
            .unwrap();
        assert_eq!(report.messages, expected);
        assert_eq!(generator.calls.load(Ordering::SeqCst), expected as usize);
        assert_eq!(peer.posts.lock().unwrap().len(), expected as usize);
        assert!(time.now() >= now + chrono::Duration::seconds(1));
        let saved = LocalState::load(&cfg.state_path).unwrap();
        assert!(saved.pending.is_none());
        assert!(saved.processed.contains("target"));
        assert_eq!(u64::from(saved.calls), expected);
        server.abort();
    }
}

#[tokio::test]
async fn expired_secondary_triggers_leave_the_fixed_fresh_reply_target_unchanged() {
    struct Capture {
        state_path: std::path::PathBuf,
        observed: Mutex<Option<(String, LocalState)>>,
    }
    impl Generator for Capture {
        fn generate<'a>(
            &'a self,
            context: &'a str,
            _index: u64,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                *self.observed.lock().unwrap() =
                    Some((context.into(), LocalState::load(&self.state_path).unwrap()));
                Ok("先回答新的具体问题".into())
            })
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let time = Time::new();
    let now = time.now();
    let old = json!({"seq":1,"event_id":"expired-other","kind":"agent_message","created_at":(now-chrono::Duration::seconds(601)).to_rfc3339(),"text":"过期的其他问题"});
    let fresh = json!({"seq":2,"event_id":"fixed-target","kind":"agent_message","created_at":now.to_rfc3339(),"text":"本次固定的新问题"});
    let (url, peer, server) = peer(page(vec![old.clone(), fresh.clone()]), vec![]).await;
    let cfg = config(temp.path(), url);
    LocalState {
        cursor: Some("before-merged".into()),
        pending: Some(PendingCall {
            id: "stable-merged-call".into(),
            kind: "reply".into(),
            trigger_ids: vec!["expired-other".into(), "fixed-target".into()],
            trigger_events: vec![old, fresh.clone()],
            reply_to: Some("fixed-target".into()),
            output: None,
            hint: None,
        }),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let generator = Arc::new(Capture {
        state_path: cfg.state_path.clone(),
        observed: Default::default(),
    });
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), time, generator.clone()).unwrap();
    let (_stop, rx) = watch::channel(false);
    client
        .run(
            rx,
            RunOptions {
                max_messages: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let observed = generator.observed.lock().unwrap();
    let (context, during_call) = observed.as_ref().unwrap();
    let triggers: Value = serde_json::from_str(
        context
            .split("触发这次调用的消息\n")
            .nth(1)
            .unwrap()
            .split("\n主动提示")
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(triggers, json!([fresh.clone()]));
    let during = during_call.pending.as_ref().unwrap();
    assert_eq!(during.trigger_events, vec![fresh]);
    assert_eq!(during.trigger_ids, ["expired-other", "fixed-target"]);
    assert_eq!(during.reply_to.as_deref(), Some("fixed-target"));
    let posts = peer.posts.lock().unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["client_message_id"], "stable-merged-call");
    assert_eq!(posts[0]["reply_to_event_id"], "fixed-target");
    let saved = LocalState::load(&cfg.state_path).unwrap();
    assert!(saved.pending.is_none());
    assert!(saved.processed.contains("expired-other"));
    assert!(saved.processed.contains("fixed-target"));
    server.abort();
}

#[tokio::test]
async fn an_old_generated_reply_keeps_its_body_and_id_across_an_uncertain_post() {
    let temp = tempfile::tempdir().unwrap();
    let (url, peer, server) = peer(
        page(vec![]),
        vec![(503, r#"{"error":{"code":"unavailable"}}"#, None)],
    )
    .await;
    let cfg = config(temp.path(), url);
    let time = Time::new();
    let mut call = pending("reply");
    call.trigger_events = vec![
        json!({"seq":1,"event_id":"trigger","kind":"agent_message","created_at":(time.now()-chrono::Duration::days(2)).to_rfc3339()}),
    ];
    LocalState {
        cursor: Some("before-uncertain-post".into()),
        pending: Some(call),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let generator = Gen::line("不能重新调用");
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), time, generator.clone()).unwrap();
    let (_stop, rx) = watch::channel(false);
    client
        .run(
            rx,
            RunOptions {
                max_messages: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let posts = peer.posts.lock().unwrap();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0], posts[1]);
    assert_eq!(posts[0]["client_message_id"], "stable-id-for-retry");
    assert_eq!(posts[0]["text"], "generated before crash");
    assert_eq!(posts[0]["reply_to_event_id"], "trigger");
    assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
    assert!(LocalState::load(&cfg.state_path).unwrap().pending.is_none());
    server.abort();
}

#[tokio::test]
async fn the_public_reply_age_policy_does_not_shorten_an_open_sandbox_window() {
    let temp = tempfile::tempdir().unwrap();
    let mut events = page(vec![]);
    events["timeline"] = json!("conformance");
    let (url, peer, server) = peer(events, vec![]).await;
    let pending_status = r#"{"state":"PENDING","state_reason":null,"mute":null,"room_paused":false,"limits":{"max_text_scalars":37},"conformance":{"passed_at":null}}"#;
    peer.self_plans
        .lock()
        .unwrap()
        .extend(std::iter::repeat_n((200, pending_status, None), 16));
    let mut cfg = config(temp.path(), url);
    cfg.max_trigger_age_seconds = 1;
    let time = Time::new();
    LocalState {
        cursor: Some("current-sandbox".into()),
        pending: Some(PendingCall {
            id: "stable-sandbox-call".into(),
            kind: "conformance_reply".into(),
            trigger_ids: vec!["examiner-current".into()],
            trigger_events: vec![json!({"seq":4,"event_id":"examiner-current","kind":"agent_message","created_at":(time.now()-chrono::Duration::seconds(2)).to_rfc3339(),"text":"仍在沙盒回复窗口内"})],
            reply_to: Some("examiner-current".into()),
            output: None,
            hint: None,
        }),
        ..Default::default()
    }
    .save(&cfg.state_path)
    .unwrap();
    let generator = Gen::line("回复当前考官问题");
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), time, generator.clone()).unwrap();
    let (_stop, rx) = watch::channel(false);
    let report = client
        .run(
            rx,
            RunOptions {
                run_for_seconds: Some(3),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(report.sandbox_replies, 1);
    assert_eq!(report.messages, 0);
    assert_eq!(generator.calls.load(Ordering::SeqCst), 1);
    let posts = peer.posts.lock().unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["client_message_id"], "stable-sandbox-call");
    assert_eq!(posts[0]["reply_to_event_id"], "examiner-current");
    assert!(LocalState::load(&cfg.state_path).unwrap().pending.is_none());
    server.abort();
}
