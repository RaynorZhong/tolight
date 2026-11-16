use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use tolight_core::config::{Config, PublicConfig};
use tolight_reference_client::{
    Clock, Generator, LocalState, ReferenceClient, ReferenceConfig, RunOptions, keygen,
};

struct Time(AtomicI64);
impl Clock for Time {
    fn now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.0.load(Ordering::SeqCst), 0).unwrap()
    }
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.0
            .fetch_add(duration.as_secs() as i64, Ordering::SeqCst);
        Box::pin(async { tokio::task::yield_now().await })
    }
}
struct FirstSilent;
impl Generator for FirstSilent {
    fn generate<'a>(
        &'a self,
        _context: &'a str,
        index: u64,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            Ok(if index == 0 {
                "[SILENT]"
            } else {
                "给考官的简短回复"
            }
            .into())
        })
    }
}

async fn exercise(pool: PgPool, daily_limit: usize) {
    let temp = tempfile::tempdir().unwrap();
    let noon = Utc::now()
        .date_naive()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp();
    let time = Arc::new(Time(AtomicI64::new(noon)));
    let clock = time.clone();
    let config = Config {
        conformance_retry_after: 1,
        conformance_reply_window: 2,
        conformance_runs_per_day: daily_limit,
        requests_per_minute: 10000,
        public: PublicConfig {
            dir: temp.path().join("public"),
            demo_banner: true,
        },
        ..Config::default()
    };
    let server =
        tolight_coordinator::testing::start_with_clock(pool, config, Arc::new(move || clock.now()))
            .await
            .unwrap();
    let mut cfg = ReferenceConfig {
        base_url: format!("{}/participant/v1/", server.origin),
        key_path: temp.path().join("client.key"),
        state_path: temp.path().join("state.json"),
        min_speaking_interval_seconds: 0,
        long_poll_seconds: 1,
        ..ReferenceConfig::default()
    };
    cfg.application.agent_id = "recovery-agent".into();
    cfg.application.display_name = "Recovery".into();
    keygen(&cfg).unwrap();
    let mut client =
        ReferenceClient::with_dependencies(cfg.clone(), time.clone(), Arc::new(FirstSilent))
            .unwrap();
    client.apply().await.unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move { client.run(rx, RunOptions::default()).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(7);
    loop {
        let runs = server
            .room
            .store()
            .conformance_runs("recovery-agent")
            .await
            .unwrap();
        if runs
            .first()
            .is_some_and(|run| run.failure_reason.as_deref() == Some("no_reply"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "first silent run did not expire"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if daily_limit == 1 {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            server
                .room
                .store()
                .conformance_runs("recovery-agent")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            !task.is_finished(),
            "daily exhaustion is not an application rejection"
        );
        time.0.fetch_add(86400, Ordering::SeqCst);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        if server
            .room
            .store()
            .review("recovery-agent")
            .await
            .unwrap()
            .conformance_passed_at
            .is_some()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "client did not retry the next eligible session"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(true).unwrap();
    let report = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(report.messages, 0);
    assert_eq!(report.sandbox_replies, 1);
    let runs = server
        .room
        .store()
        .conformance_runs("recovery-agent")
        .await
        .unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].failure_reason.as_deref(), Some("no_reply"));
    assert_eq!(runs[1].passed, Some(true));
    assert!(
        server
            .room
            .store()
            .public_snapshot()
            .await
            .unwrap()
            .events
            .is_empty()
    );
    let state = LocalState::load(&cfg.state_path).unwrap();
    assert_eq!(state.script_index, 2);
    assert_eq!(state.calls, if daily_limit == 1 { 1 } else { 2 });
    server.shutdown().await.unwrap();
}
#[sqlx::test(migrations = "../../crates/tolight-coordinator-store/migrations")]
async fn failed_conformance_restarts_without_treating_it_as_rejected(pool: PgPool) {
    exercise(pool, 5).await;
}
#[sqlx::test(migrations = "../../crates/tolight-coordinator-store/migrations")]
async fn daily_exhaustion_keeps_waiting_until_next_utc_day(pool: PgPool) {
    exercise(pool, 1).await;
}
