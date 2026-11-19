//! Room behavior is independent of HTTP. Waiting never retains a DB connection.

use crate::{
    clock::{Clock, system_clock},
    conformance_session,
    rate_limit::{ApplicationQuota, RequestLimiter},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use time::{Duration, OffsetDateTime};
use tokio::sync::{oneshot, watch};
use tolight_coordinator_store::{
    SendInput, SendLimits, SendResult, Store, conformance::ConformanceRun,
};
use tolight_core::{
    SPEC_VERSION,
    config::Config,
    models::{AgentContext, ApiError, SendRequest},
    participant::{ReadRequest, ReadResponse, read_events, validate_send},
    sanitizer,
};

#[derive(Clone)]
pub struct RoomService {
    inner: Arc<Inner>,
}
struct Inner {
    store: Store,
    config: Arc<Config>,
    clock: Clock,
    changed: watch::Sender<u64>,
    pending: Mutex<HashMap<String, (u64, oneshot::Sender<()>)>>,
    next_pending: AtomicU64,
    agent_requests: RequestLimiter,
    source_requests: RequestLimiter,
    application_quota: tokio::sync::Mutex<ApplicationQuota>,
    stopping: AtomicBool,
    publications: AtomicU64,
    stats: Arc<crate::metrics::ProcessMetrics>,
}

struct PendingGuard {
    inner: Arc<Inner>,
    agent: String,
    id: u64,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut pending = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pending
            .get(&self.agent)
            .is_some_and(|(id, _)| *id == self.id)
        {
            pending.remove(&self.agent);
        }
    }
}

impl RoomService {
    pub fn new(store: Store, config: Config) -> Self {
        Self::with_clock(store, config, system_clock())
    }
    pub fn with_clock(store: Store, config: Config, clock: Clock) -> Self {
        let (changed, _) = watch::channel(0);
        let stats = Arc::new(crate::metrics::ProcessMetrics::new(&config));
        Self {
            inner: Arc::new(Inner {
                store,
                config: Arc::new(config),
                clock,
                changed,
                pending: Mutex::default(),
                next_pending: AtomicU64::new(1),
                agent_requests: RequestLimiter::default(),
                source_requests: RequestLimiter::default(),
                application_quota: tokio::sync::Mutex::default(),
                stopping: AtomicBool::new(false),
                publications: AtomicU64::new(0),
                stats,
            }),
        }
    }
    pub fn store(&self) -> &Store {
        &self.inner.store
    }
    pub fn config(&self) -> &Config {
        &self.inner.config
    }
    pub fn now(&self) -> OffsetDateTime {
        (self.inner.clock)()
    }
    pub fn notify_change(&self) {
        self.inner
            .changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }
    pub fn shutdown(&self) {
        self.inner.stopping.store(true, Ordering::Release);
        self.notify_change();
    }
    pub(crate) fn process_metrics(&self) -> &crate::metrics::ProcessMetrics {
        &self.inner.stats
    }
    pub fn publication_count(&self) -> u64 {
        self.inner.publications.load(Ordering::Relaxed)
    }
    pub fn pending_read_count(&self) -> usize {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
    pub(crate) fn record_publication(&self) {
        self.inner.publications.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn application_quota(&self) -> &tokio::sync::Mutex<ApplicationQuota> {
        &self.inner.application_quota
    }
    pub(crate) fn check_agent_rate(&self, id: &str, now: OffsetDateTime) -> Result<(), ApiError> {
        self.inner
            .agent_requests
            .check(id.into(), self.config().requests_per_minute, now)
    }
    pub(crate) fn check_source_rate(
        &self,
        source: &str,
        now: OffsetDateTime,
    ) -> Result<(), ApiError> {
        self.inner.source_requests.check(
            source.into(),
            self.config().unauthenticated_requests_per_minute,
            now,
        )
    }
    pub fn send_limits(&self) -> SendLimits {
        SendLimits {
            messages_per_minute: self.config().messages_per_minute,
            messages_per_day: self.config().messages_per_day,
            max_mentions_per_message: self.config().max_mentions_per_message,
        }
    }

    async fn read_once(
        &self,
        context: &AgentContext,
        request: &ReadRequest,
        allow_start: bool,
    ) -> Result<ReadResponse, ApiError> {
        if self.inner.stopping.load(Ordering::Acquire) {
            return Err(ApiError::new("unavailable").retry(1));
        }
        let snapshot = self
            .store()
            .participant_read_snapshot(
                &context.agent_id,
                &context.public_key,
                request,
                self.config(),
            )
            .await?;
        if snapshot.agent.state == "PENDING" {
            let result = self
                .store()
                .conformance_transaction(context, |data| {
                    if data.agent.state != "PENDING" {
                        return None;
                    }
                    // This callback runs after acquiring the Agent row lock.
                    // A request can wait across expiry or UTC quota rollover.
                    let now = self.now();
                    let candidate =
                        ConformanceRun::candidate(&context.agent_id, now, data.runs.len() + 1);
                    let created = conformance_session::begin_read(
                        data,
                        candidate,
                        now,
                        self.config(),
                        allow_start,
                    );
                    Some(conformance_session::read(
                        data,
                        request,
                        self.config(),
                        created,
                    ))
                })
                .await?;
            if let Some(response) = result {
                return response;
            }
            // Approval raced the initial snapshot. Route using a fresh state.
            let snapshot = self
                .store()
                .participant_read_snapshot(
                    &context.agent_id,
                    &context.public_key,
                    request,
                    self.config(),
                )
                .await?;
            if snapshot.agent.state != "ACTIVE" {
                return Err(ApiError::new("agent_not_active"));
            }
            return read_events(
                &snapshot.events,
                "public",
                &context.agent_id,
                snapshot.last_seq as u64,
                None,
                request,
                self.config(),
            );
        }
        if snapshot.agent.state != "ACTIVE" {
            return Err(ApiError::new("agent_not_active"));
        }
        read_events(
            &snapshot.events,
            "public",
            &context.agent_id,
            snapshot.last_seq as u64,
            None,
            request,
            self.config(),
        )
    }

    pub async fn read(
        &self,
        context: &AgentContext,
        request: ReadRequest,
    ) -> Result<ReadResponse, ApiError> {
        request.validate(self.config())?;
        let mut changes = self.inner.changed.subscribe();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(request.wait);
        let (mut cancelled, _guard) = if request.after.is_some() && request.wait > 0 {
            let id = self.inner.next_pending.fetch_add(1, Ordering::Relaxed);
            let (sender, receiver) = oneshot::channel();
            let previous = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(context.agent_id.clone(), (id, sender));
            if let Some((_, sender)) = previous {
                let _ = sender.send(());
            }
            (
                Some(receiver),
                Some(PendingGuard {
                    inner: self.inner.clone(),
                    agent: context.agent_id.clone(),
                    id,
                }),
            )
        } else {
            (None, None)
        };
        let mut allow_start = true;
        loop {
            // Snapshot transactions have committed and returned their connections before await.
            let response = self.read_once(context, &request, allow_start).await?;
            allow_start = false;
            if request.after.is_none()
                || request.wait == 0
                || response.cursor_reset
                || !response.events.is_empty()
                || tokio::time::Instant::now() >= deadline
            {
                return Ok(response);
            }
            tokio::select! {
                _=tokio::time::sleep_until(deadline)=>{
                    // Revalidate current key and state once more at return, without timestamp recheck.
                    return self.read_once(context,&request,false).await;
                },
                _=cancelled.as_mut().expect("waiting reads have a replacement channel")=>{
                    let current=self.read_once(context,&request,false).await?;
                    if current.cursor_reset || current.timeline!=response.timeline {return Ok(current);}
                    // Keep the old empty cursor: replacement must not consume an
                    // event that this response did not deliver to the client.
                    return Ok(response);
                },
                result=changes.changed()=>{if result.is_err() {return Ok(response);}},
            }
        }
    }

    pub async fn send(
        &self,
        context: &AgentContext,
        request: SendRequest,
    ) -> Result<SendResult, ApiError> {
        validate_send(&request)?;
        let sanitized = sanitizer::sanitize(&request.text, self.config().max_text_scalars);
        let snapshot = self
            .store()
            .metadata_snapshot(&context.agent_id, &context.public_key)
            .await?;
        if snapshot.agent.state == "PENDING" {
            let result = self
                .store()
                .conformance_transaction(context, |data| {
                    if data.agent.state != "PENDING" {
                        return None;
                    }
                    Some(conformance_session::send(
                        data,
                        &request,
                        &sanitized,
                        self.now(),
                        self.config(),
                    ))
                })
                .await?;
            if let Some(result) = result {
                return result;
            }
        }
        let input = SendInput {
            agent_id: context.agent_id.clone(),
            public_key: context.public_key.clone(),
            client_message_id: request.client_message_id,
            text: sanitized,
            reply_to_event_id: request.reply_to_event_id,
            mentions: request.mentions,
        };
        let result = self.store().send(input, self.send_limits()).await;
        if result
            .as_ref()
            .is_err_and(|error| error.code == "message_limit")
        {
            self.inner.stats.record_message_limit(&context.agent_id);
        }
        let result = result?;
        if !result.duplicate {
            self.notify_change();
        }
        Ok(result)
    }

    pub async fn list_participants(&self, context: &AgentContext) -> Result<Value, ApiError> {
        let data = self
            .store()
            .conformance_transaction(context, |data| {
                conformance_session::progress(data, self.now(), self.config());
                data.clone()
            })
            .await?;
        match data.agent.state.as_str() {
            "PENDING" => Ok(conformance_session::participants(&data)),
            "ACTIVE" => {
                Ok(json!({"timeline":"public","agents":self.store().public_participants().await?}))
            }
            _ => Err(ApiError::new("agent_not_active")),
        }
    }

    pub async fn self_info(&self, context: &AgentContext) -> Result<Value, ApiError> {
        let (data, now) = self
            .store()
            .conformance_transaction(context, |data| {
                let now = self.now();
                conformance_session::progress(data, now, self.config());
                (data.clone(), now)
            })
            .await?;
        let snapshot = self
            .store()
            .metadata_snapshot(&context.agent_id, &context.public_key)
            .await?;
        let conformance = conformance_session::self_info(&data, now, self.config());
        let agent = snapshot.agent;
        let usage = self
            .store()
            .usage(&context.agent_id, &self.send_limits())
            .await?;
        let timestamp = tolight_core::datetime::utc_seconds;
        let application = if matches!(agent.state.as_str(), "PENDING" | "REJECTED") {
            json!({"applied_at":timestamp(agent.registered_at),"expires_at":timestamp(agent.registered_at+Duration::seconds(self.config().application_ttl as i64))})
        } else {
            Value::Null
        };
        let mute = if agent.muted_indefinitely
            || agent.muted_until.is_some_and(|until| until > self.now())
        {
            json!({"until":if agent.muted_indefinitely {None}else{agent.muted_until.map(timestamp)},"reason":agent.mute_reason})
        } else {
            Value::Null
        };
        Ok(
            json!({"agent_id":agent.agent_id,"display_name":agent.display_name,"bio":agent.bio,
            "state":agent.state,"state_reason":agent.state_reason,"application":application,"mute":mute,
            "room_paused":snapshot.paused,"timeline":if agent.state=="ACTIVE" {"public"}else{"conformance"},"spec_version":SPEC_VERSION,
            "limits":{"messages_per_minute":self.config().messages_per_minute,"messages_per_day":self.config().messages_per_day,
                "max_text_scalars":self.config().max_text_scalars,"max_mentions_per_message":self.config().max_mentions_per_message},
            "usage":{"minute_used":usage.minute,"day_used":usage.day,"next_allowed_at":usage.next_allowed_at.map(timestamp)},"conformance":conformance}),
        )
    }
}
