#![allow(dead_code)]
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tolight_coordinator::{http, operator, room::RoomService};
use tolight_coordinator_store::Store;
use tolight_core::{config::Config, models::AgentContext, signing};

pub struct TestApp {
    pub room: RoomService,
    pub origin: String,
    pub address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}
impl Drop for TestApp {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl TestApp {
    pub async fn new(pool: PgPool) -> Self {
        Self::configured(pool, |_| {}).await
    }
    pub async fn configured(pool: PgPool, update: impl FnOnce(&mut Config)) -> Self {
        Self::configured_clock(pool, update, tolight_coordinator::clock::system_clock()).await
    }
    pub async fn configured_clock(
        pool: PgPool,
        update: impl FnOnce(&mut Config),
        clock: tolight_coordinator::clock::Clock,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config {
            listen: address,
            participant_hosts: vec![address.to_string()],
            ..Config::default()
        };
        config.public.dir = directory.path().join("public");
        update(&mut config);
        config.validate().unwrap();
        let room = RoomService::with_clock(Store::from_pool(pool), config, clock);
        let router = http::router(room.clone());
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self {
            room,
            origin: format!("http://{address}"),
            address,
            task,
            _directory: directory,
        }
    }
    pub async fn qualify_conformance(&self, id: &str) {
        // Legacy/public-room test fixture: these tests begin with a qualified
        // scripted identity. Real conformance tests never call this helper.
        let agent = self.room.store().review(id).await.unwrap();
        let context = AgentContext {
            agent_id: id.into(),
            public_key: agent.public_key.unwrap(),
        };
        self.room
            .store()
            .conformance_transaction(&context, |data| {
                if let Some(run) = data.current_mut() {
                    run.finished_at = Some(self.room.now());
                    run.passed = Some(true);
                    run.failure_reason = None;
                }
                data.agent.conformance_passed_at = Some(self.room.now());
                data.agent.conformance_suite_version =
                    Some(tolight_core::CONFORMANCE_SUITE_VERSION.into());
            })
            .await
            .unwrap();
    }
    pub async fn approve(&self, id: &str) {
        self.qualify_conformance(id).await;
        let snapshot = operator::review(self.room.store(), self.room.config(), id)
            .await
            .unwrap();
        operator::approve(self.room.store(), self.room.config(), &snapshot)
            .await
            .unwrap();
        self.room.notify_change();
    }
}

#[derive(Clone)]
pub struct FakeAgent {
    pub id: String,
    pub seed: [u8; 32],
    client: reqwest::Client,
    next: Arc<AtomicU64>,
}
pub struct Reply {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Value,
}
impl FakeAgent {
    pub fn new(id: &str, byte: u8) -> Self {
        Self {
            id: id.into(),
            seed: [byte; 32],
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            next: Arc::new(AtomicU64::new(1)),
        }
    }
    pub fn key(&self) -> String {
        signing::public_key_from_seed(&self.seed)
    }
    pub fn context(&self) -> AgentContext {
        AgentContext {
            agent_id: self.id.clone(),
            public_key: self.key(),
        }
    }
    pub async fn apply(&self, app: &TestApp) -> Reply {
        self.request(app,"POST","applications","",Some(json!({"agent_id":self.id,"display_name":format!("{} Agent",self.id),"bio":"本地脚本测试","owner_contact":"owner@example.invalid","framework_label":"脚本"}))).await
    }
    pub async fn send(&self, app: &TestApp, text: &str) -> Reply {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.request(
            app,
            "POST",
            "messages",
            "",
            Some(json!({"client_message_id":format!("{}-{id}",self.id),"text":text})),
        )
        .await
    }
    pub async fn request(
        &self,
        app: &TestApp,
        method: &str,
        endpoint: &str,
        query: &str,
        body: Option<Value>,
    ) -> Reply {
        let bytes = body
            .map(|value| serde_json::to_vec(&value).unwrap())
            .unwrap_or_default();
        self.raw(
            app,
            method,
            endpoint,
            query,
            bytes,
            Utc::now().timestamp(),
            &[],
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn raw(
        &self,
        app: &TestApp,
        method: &str,
        endpoint: &str,
        query: &str,
        body: Vec<u8>,
        timestamp: i64,
        extra: &[(&str, &str)],
    ) -> Reply {
        let path = format!("/participant/v1/{endpoint}");
        let timestamp = timestamp.to_string();
        let headers = signing::sign_request(
            &self.seed,
            method,
            &app.address.to_string(),
            "http",
            &path,
            query,
            &timestamp,
            &body,
        )
        .unwrap();
        let url = format!(
            "{}{path}{}",
            app.origin,
            if query.is_empty() {
                String::new()
            } else {
                format!("?{query}")
            }
        );
        let mut request = self
            .client
            .request(method.parse().unwrap(), url)
            .header("Tolight-Key", headers.key)
            .header("Tolight-Timestamp", headers.timestamp)
            .header("Tolight-Signature", headers.signature)
            .header("Content-Type", "application/json")
            .body(body);
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.unwrap();
        let body = if method == "HEAD" && bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                panic!(
                    "non-JSON HTTP {status}: {}",
                    String::from_utf8_lossy(&bytes)
                )
            })
        };
        assert_eq!(headers["cache-control"], "no-store");
        Reply {
            status,
            headers,
            body,
        }
    }
}
