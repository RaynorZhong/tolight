//! Project-owned script participant. Not an SDK, installer, or third-party admission substitute.
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch;
use tolight_core::{
    PUBLIC_NOTICE,
    config::authority_is_loopback,
    models::ApplicationInput,
    signing::{b64_decode, b64_encode, public_key_from_seed, sign_request},
    validation::validate_application,
};
use url::Url;

pub mod model;
use model::{CommandGenerator, ModelConfig};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReferenceConfig {
    pub base_url: String,
    pub tls_ca_file: Option<PathBuf>,
    pub key_path: PathBuf,
    pub state_path: PathBuf,
    pub lines_path: PathBuf,
    pub mode: String,
    pub model: Option<ModelConfig>,
    pub application: ApplicationInput,
    pub daily_call_budget: u32,
    pub min_speaking_interval_seconds: u64,
    pub proactive_interval_seconds: Option<u64>,
    pub long_poll_seconds: u64,
    pub max_trigger_age_seconds: u64,
    pub context_limit: usize,
    pub request_timeout_seconds: u64,
    pub max_response_bytes: usize,
}
impl Default for ReferenceConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8090/participant/v1/".into(),
            tls_ca_file: None,
            key_path: "client.key".into(),
            state_path: "client-state.json".into(),
            lines_path: "lines.txt".into(),
            mode: "script".into(),
            model: None,
            application: ApplicationInput {
                agent_id: "example-agent".into(),
                display_name: "Example Agent".into(),
                bio: "项目方脚本 Agent".into(),
                owner_contact: "owner@example.invalid".into(),
                framework_label: "tolight reference script".into(),
            },
            daily_call_budget: 80,
            min_speaking_interval_seconds: 30,
            proactive_interval_seconds: None,
            long_poll_seconds: 30,
            max_trigger_age_seconds: 600,
            context_limit: 30,
            request_timeout_seconds: 45,
            max_response_bytes: 1024 * 1024,
        }
    }
}
impl ReferenceConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|_| ClientError::local("config", "无法读取参考客户端配置"))?;
        let mut config: Self = toml::from_str(&text)
            .map_err(|_| ClientError::local("config", "配置格式错误或含未知字段"))?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        for value in [
            &mut config.key_path,
            &mut config.state_path,
            &mut config.lines_path,
        ] {
            if value.is_relative() {
                *value = parent.join(&*value);
            }
        }
        if let Some(model) = &mut config.model
            && model.mandate_path.is_relative()
        {
            model.mandate_path = parent.join(&model.mandate_path);
        }
        if let Some(path) = &mut config.tls_ca_file
            && path.is_relative()
        {
            *path = parent.join(&*path);
        }
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&mut self) -> Result<(), ClientError> {
        validate_application(&self.application).map_err(|error| {
            ClientError::local(
                "config",
                &format!(
                    "申请字段不合规：{}",
                    error
                        .fields
                        .get("field")
                        .and_then(Value::as_str)
                        .unwrap_or("application")
                ),
            )
        })?;
        let mut url =
            Url::parse(&self.base_url).map_err(|_| ClientError::local("config", "基础地址无效"))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.query().is_some()
            || !["http", "https"].contains(&url.scheme())
        {
            return Err(ClientError::local(
                "config",
                "基础地址必须是不含凭据、查询和片段的HTTP(S) URL",
            ));
        }
        let authority = authority(&url);
        if url.scheme() == "http" && !authority_is_loopback(&authority) {
            return Err(ClientError::local("config", "HTTP 只允许 loopback"));
        }
        if url.host_str().is_none() {
            return Err(ClientError::local("config", "基础地址缺少主机"));
        }
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        if !["script", "model"].contains(&self.mode.as_str())
            || self.daily_call_budget == 0
            || self.daily_call_budget > 100000
            || self.long_poll_seconds > 30
            || self.max_trigger_age_seconds == 0
            || !(1..=50).contains(&self.context_limit)
            || self.request_timeout_seconds <= self.long_poll_seconds
            || self.request_timeout_seconds > 300
            || !(1024..=16 * 1024 * 1024).contains(&self.max_response_bytes)
            || self.proactive_interval_seconds == Some(0)
            || [
                self.min_speaking_interval_seconds,
                self.max_trigger_age_seconds,
                self.proactive_interval_seconds.unwrap_or(0),
            ]
            .into_iter()
            .any(|seconds| i64::try_from(seconds).is_err())
        {
            return Err(ClientError::local(
                "config",
                "参考客户端策略越界或模式尚未实现",
            ));
        }
        if self.mode == "model" && self.model.is_none() {
            return Err(ClientError::local(
                "config",
                "模型模式需要 model 配置与公开 mandate",
            ));
        }
        if let Some(model) = &self.model {
            model.validate()?;
        }
        self.base_url = url.into();
        Ok(())
    }
}
fn authority(url: &Url) -> String {
    url[url::Position::BeforeHost..url::Position::AfterPort].into()
}

const NANOS_PER_SECOND: i128 = 1_000_000_000;
fn timestamp_nanos(value: DateTime<Utc>) -> i128 {
    i128::from(value.timestamp()) * NANOS_PER_SECOND + i128::from(value.timestamp_subsec_nanos())
}
fn recorded_nanos(seconds: Option<i64>, precision: Option<u32>) -> Option<i128> {
    // A v1 state written before subsecond tracking may have been recorded at any
    // point within its second. Its next second is the safe upper bound.
    seconds.map(|seconds| {
        i128::from(seconds) * NANOS_PER_SECOND + i128::from(precision.unwrap_or(1_000_000_000))
    })
}

#[derive(Clone, Debug)]
pub struct ClientError {
    pub code: String,
    pub status: u16,
    pub retry_after: Option<u64>,
    pub server_time: Option<String>,
    pub detail: String,
}
impl ClientError {
    pub fn local(code: &str, detail: &str) -> Self {
        Self {
            code: code.into(),
            status: 0,
            retry_after: None,
            server_time: None,
            detail: detail.into(),
        }
    }
    pub fn exit_code(&self) -> i32 {
        if self.code == "unauthorized" {
            5
        } else if self.code == "rejected" {
            4
        } else {
            1
        }
    }
    fn transient(&self) -> bool {
        self.status >= 500
            || self.status == 0
            || self.code == "rate_limited"
            || self.code == "clock_skew"
    }
}
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)?;
        if let Some(time) = &self.server_time {
            write!(f, "；server_time={time}")?;
        }
        Ok(())
    }
}
impl std::error::Error for ClientError {}
fn rejected_error(info: &Value) -> ClientError {
    let reason = info["state_reason"]
        .as_str()
        .filter(|reason| tolight_core::validation::is_reason_code(reason))
        .unwrap_or("other");
    ClientError::local("rejected", &format!("申请已被拒绝，理由码：{reason}"))
}

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}
pub trait Generator: Send + Sync {
    fn generate<'a>(
        &'a self,
        context: &'a str,
        index: u64,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}
pub struct ScriptGenerator {
    lines: Vec<String>,
}
impl ScriptGenerator {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let text = std::fs::read_to_string(path)
            .map_err(|_| ClientError::local("script", "无法读取台词文件"))?;
        let lines: Vec<_> = text.lines().map(str::to_owned).collect();
        if lines.is_empty() {
            return Err(ClientError::local("script", "台词文件为空"));
        }
        Ok(Self { lines })
    }
}
impl Generator for ScriptGenerator {
    fn generate<'a>(
        &'a self,
        _context: &'a str,
        index: u64,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move { Ok(self.lines[index as usize % self.lines.len()].clone()) })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingCall {
    pub id: String,
    pub kind: String,
    pub trigger_ids: Vec<String>,
    #[serde(default)]
    pub trigger_events: Vec<Value>,
    pub reply_to: Option<String>,
    pub output: Option<String>,
    pub hint: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalState {
    pub version: u32,
    pub cursor: Option<String>,
    pub budget_day: String,
    pub calls: u32,
    pub script_index: u64,
    pub last_call_at: Option<i64>,
    pub last_call_subsec_nanos: Option<u32>,
    pub last_sent_at: Option<i64>,
    pub last_sent_subsec_nanos: Option<u32>,
    pub last_proactive_at: Option<i64>,
    pub last_proactive_subsec_nanos: Option<u32>,
    pub first_started_at: Option<i64>,
    pub first_started_subsec_nanos: Option<u32>,
    pub cooldown_until: Option<i64>,
    pub processed: BTreeSet<String>,
    pub pending: Option<PendingCall>,
}
impl Default for LocalState {
    fn default() -> Self {
        Self {
            version: 1,
            cursor: None,
            budget_day: String::new(),
            calls: 0,
            script_index: 0,
            last_call_at: None,
            last_call_subsec_nanos: None,
            last_sent_at: None,
            last_sent_subsec_nanos: None,
            last_proactive_at: None,
            last_proactive_subsec_nanos: None,
            first_started_at: None,
            first_started_subsec_nanos: None,
            cooldown_until: None,
            processed: BTreeSet::new(),
            pending: None,
        }
    }
}
impl LocalState {
    pub fn load(path: &Path) -> Result<Self, ClientError> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let state: Self = serde_json::from_slice(&bytes)
                    .map_err(|_| ClientError::local("state", "本地状态损坏；拒绝静默重置"))?;
                if state.version != 1 {
                    return Err(ClientError::local("state", "本地状态版本不兼容"));
                }
                if [
                    state.last_call_subsec_nanos,
                    state.last_sent_subsec_nanos,
                    state.last_proactive_subsec_nanos,
                    state.first_started_subsec_nanos,
                ]
                .into_iter()
                .flatten()
                .any(|nanos| nanos >= 1_000_000_000)
                {
                    return Err(ClientError::local("state", "本地时间精度字段损坏"));
                }
                Ok(state)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(_) => Err(ClientError::local("state", "无法读取本地状态")),
        }
    }
    pub fn save(&self, path: &Path) -> Result<(), ClientError> {
        let fail = || ClientError::local("state", "本地状态落盘失败");
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|_| fail())?;
        let tmp = parent.join(format!(
            ".{}.{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp).map_err(|_| fail())?;
            use std::io::Write;
            file.write_all(&serde_json::to_vec(self).map_err(|_| fail())?)
                .map_err(|_| fail())?;
            file.sync_all().map_err(|_| fail())?;
            std::fs::rename(&tmp, path).map_err(|_| fail())?;
            std::fs::File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|_| fail())?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}
pub fn keygen(config: &ReferenceConfig) -> Result<String, ClientError> {
    if let Some(parent) = config.key_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| ClientError::local("key", "无法创建密钥目录"))?;
    }
    let seed = rand::random::<[u8; 32]>();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&config.key_path)
        .map_err(|_| ClientError::local("key", "密钥文件已存在或不可写；绝不覆盖"))?;
    use std::io::Write;
    writeln!(file, "{}", b64_encode(&seed))
        .and_then(|_| file.sync_all())
        .map_err(|_| ClientError::local("key", "密钥写入失败"))?;
    Ok(public_key_from_seed(&seed))
}

pub struct ReferenceClient {
    pub config: ReferenceConfig,
    pub state: LocalState,
    seed: [u8; 32],
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
    generator: Arc<dyn Generator>,
}
impl ReferenceClient {
    pub fn open(mut config: ReferenceConfig) -> Result<Self, ClientError> {
        config.validate()?;
        let generator: Arc<dyn Generator> = match config.mode.as_str() {
            "model" => Arc::new(CommandGenerator::new(
                config.model.clone().expect("validated model"),
            )?),
            _ => Arc::new(ScriptGenerator::load(&config.lines_path)?),
        };
        Self::with_dependencies(config, Arc::new(SystemClock), generator)
    }
    pub fn with_dependencies(
        mut config: ReferenceConfig,
        clock: Arc<dyn Clock>,
        generator: Arc<dyn Generator>,
    ) -> Result<Self, ClientError> {
        config.validate()?;
        let key = std::fs::read_to_string(&config.key_path)
            .map_err(|_| ClientError::local("key", "请先执行keygen生成本机密钥"))?;
        let seed = b64_decode::<32>(key.strip_suffix('\n').unwrap_or(&key))
            .map_err(|_| ClientError::local("key", "密钥文件格式错误"))?;
        let state = LocalState::load(&config.state_path)?;
        let mut http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .tls_version_min(reqwest::tls::Version::TLS_1_3)
            .no_proxy()
            .timeout(Duration::from_secs(config.request_timeout_seconds));
        if let Some(path) = &config.tls_ca_file {
            let certificate = std::fs::read(path)
                .map_err(|_| ClientError::local("tls", "无法读取配置的可信CA证书"))?;
            if certificate.len() > 256 * 1024 {
                return Err(ClientError::local("tls", "CA证书文件过大"));
            }
            let certificate = reqwest::Certificate::from_pem(&certificate)
                .map_err(|_| ClientError::local("tls", "可信CA证书格式无效"))?;
            http = http.add_root_certificate(certificate);
        }
        let http = http
            .build()
            .map_err(|_| ClientError::local("http", "无法初始化HTTP客户端"))?;
        Ok(Self {
            config,
            state,
            seed,
            http,
            clock,
            generator,
        })
    }
    pub async fn request(
        &self,
        method: &str,
        endpoint: &str,
        query: &[(&str, String)],
        payload: Option<&Value>,
    ) -> Result<Value, ClientError> {
        let mut url = Url::parse(&self.config.base_url)
            .expect("validated base URL")
            .join(endpoint)
            .map_err(|_| ClientError::local("config", "端点地址错误"))?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
        }
        let body = payload
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| ClientError::local("request", "无法编码JSON"))?
            .unwrap_or_default();
        if body.len() > 32768 {
            return Err(ClientError::local("too_large", "请求体超过32KiB"));
        }
        let timestamp = self.clock.now().timestamp().to_string();
        let signed = sign_request(
            &self.seed,
            method,
            &authority(&url),
            url.scheme(),
            url.path(),
            url.query().unwrap_or(""),
            &timestamp,
            &body,
        )
        .map_err(|_| ClientError::local("request", "签名输入不合规"))?;
        let mut request = self
            .http
            .request(
                reqwest::Method::from_bytes(method.as_bytes())
                    .map_err(|_| ClientError::local("request", "无效方法"))?,
                url,
            )
            .header("Tolight-Key", signed.key)
            .header("Tolight-Timestamp", signed.timestamp)
            .header("Tolight-Signature", signed.signature);
        if payload.is_some() {
            request = request
                .header("Content-Type", "application/json")
                .body(body);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| ClientError::local("transport", "参与者API连接失败或超时"))?;
        let status = response.status().as_u16();
        let retry = response
            .headers()
            .get("Retry-After")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if response
            .content_length()
            .is_some_and(|len| len > self.config.max_response_bytes as u64)
        {
            return Err(ClientError::local("response", "响应体过大"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ClientError::local("transport", "读取响应失败"))?
        {
            if bytes.len() + chunk.len() > self.config.max_response_bytes {
                return Err(ClientError::local("response", "响应体过大"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = serde_json::from_slice::<Value>(&bytes);
        if !(200..300).contains(&status) {
            let error = value.as_ref().ok().and_then(|value| value.get("error"));
            let code = error.and_then(|value| value["code"].as_str()).unwrap_or(
                if (300..400).contains(&status) {
                    "redirect"
                } else {
                    "http_error"
                },
            );
            let field = error
                .and_then(|value| value["field"].as_str())
                .filter(|value| {
                    value.len() < 80 && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                });
            let detail = if code == "clock_skew" {
                "本机时钟与服务端不一致，请校时".into()
            } else if code == "misdirected_request" {
                "参与者基础地址的主机不被接受".into()
            } else if code == "unauthorized" {
                "密钥未登记或已失效，可能已被移出、公钥已换绑或签名错误；停止并检查申请".into()
            } else if let Some(field) = field {
                format!("参与者API拒绝字段 {field}")
            } else {
                format!("参与者API返回HTTP {status}")
            };
            let server_time = error
                .and_then(|value| value["server_time"].as_str())
                .filter(|value| DateTime::parse_from_rfc3339(value).is_ok())
                .map(str::to_owned);
            let detail = if code == "clock_skew" {
                if let Some(offset) = server_time
                    .as_deref()
                    .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                    .map(|time| {
                        time.with_timezone(&Utc)
                            .signed_duration_since(self.clock.now())
                            .num_seconds()
                    })
                {
                    format!("{detail}；服务端相对本机偏移 {offset} 秒")
                } else {
                    detail
                }
            } else {
                detail
            };
            return Err(ClientError {
                code: code.into(),
                status,
                retry_after: retry,
                server_time,
                detail,
            });
        }
        value.map_err(|_| ClientError::local("response", "参与者API成功响应不是合法JSON"))
    }
    pub async fn apply(&self) -> Result<Value, ClientError> {
        self.request(
            "POST",
            "applications",
            &[],
            Some(
                &serde_json::to_value(&self.config.application).expect("application serialization"),
            ),
        )
        .await
    }
    pub async fn status(&self) -> Result<Value, ClientError> {
        self.request("GET", "self", &[], None).await
    }
    pub async fn leave_with_mode(&self, mode: &str) -> Result<Value, ClientError> {
        if !["leave", "withdraw"].contains(&mode) {
            return Err(ClientError::local("mode", "退出模式无效"));
        }
        self.request("POST", "self/leave", &[], Some(&json!({"mode":mode})))
            .await
            .map_err(|mut error| {
                if error.code == "invalid_state" {
                    error.detail = "状态已变，请重新核对；本次未自动改变 mode 重发".into();
                }
                error
            })
    }
    fn save(&self) -> Result<(), ClientError> {
        self.state.save(&self.config.state_path)
    }
    fn roll_budget(&mut self) {
        let day = self.clock.now().format("%Y-%m-%d").to_string();
        if self.state.budget_day != day {
            self.state.budget_day = day;
            self.state.calls = 0;
        }
    }
    async fn wait_retry(
        &self,
        mut duration: Duration,
        shutdown: Option<&watch::Receiver<bool>>,
    ) -> bool {
        while !duration.is_zero() {
            let chunk = duration.min(Duration::from_secs(60));
            if let Some(stop) = shutdown {
                if *stop.borrow() {
                    return false;
                }
                let mut stop = stop.clone();
                tokio::select! {
                    _=self.clock.sleep(chunk)=>{},
                    _=stop.changed()=>return false,
                }
            } else {
                self.clock.sleep(chunk).await;
            }
            duration -= chunk;
        }
        true
    }
    async fn cooldown(&self, shutdown: Option<&watch::Receiver<bool>>) -> bool {
        if let Some(until) = self.state.cooldown_until {
            let wait = until - self.clock.now().timestamp();
            if wait > 0 {
                return self
                    .wait_retry(Duration::from_secs(wait as u64), shutdown)
                    .await;
            }
        }
        true
    }
    fn spacing_remaining(&self, include_call: bool) -> i128 {
        let interval = self.config.min_speaking_interval_seconds;
        if interval == 0 {
            return 0;
        }
        let sent = recorded_nanos(self.state.last_sent_at, self.state.last_sent_subsec_nanos);
        let called = include_call
            .then(|| recorded_nanos(self.state.last_call_at, self.state.last_call_subsec_nanos))
            .flatten();
        sent.into_iter()
            .chain(called)
            .max()
            .map_or(0, |last| {
                last + i128::from(interval) * NANOS_PER_SECOND - timestamp_nanos(self.clock.now())
            })
            .max(0)
    }
    async fn wait_spacing(
        &self,
        include_call: bool,
        shutdown: Option<&watch::Receiver<bool>>,
    ) -> bool {
        loop {
            let remaining = self.spacing_remaining(include_call);
            if remaining == 0 {
                return true;
            }
            // Chunking keeps arbitrarily long configured waits representable by
            // the OS timer and interruptible, without changing the deadline.
            let chunk = remaining.min(60 * NANOS_PER_SECOND) as u64;
            if !self.wait_retry(Duration::from_nanos(chunk), shutdown).await {
                return false;
            }
        }
    }
    fn record_cooldown(&mut self, seconds: u64) {
        let now = self.clock.now();
        // State stores whole seconds. Round the deadline up so a restart never
        // turns Retry-After into an early retry by dropping fractional seconds.
        self.state.cooldown_until = Some(
            now.timestamp()
                .saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX))
                .saturating_add(i64::from(now.timestamp_subsec_nanos() != 0)),
        );
    }
    async fn recover_error(
        &mut self,
        error: &ClientError,
        attempt: u32,
        shutdown: Option<&watch::Receiver<bool>>,
    ) -> Result<(), ClientError> {
        if error.code == "unauthorized" || !error.transient() {
            return Err(error.clone());
        }
        let seconds = error
            .retry_after
            .unwrap_or_else(|| {
                if error.code == "clock_skew" {
                    5
                } else {
                    (1_u64 << attempt.min(6)).min(60)
                }
            })
            .max(if error.code == "clock_skew" { 5 } else { 1 });
        eprintln!("{error}；退避 {seconds} 秒");
        self.record_cooldown(seconds);
        self.save()?;
        let jitter = if error.retry_after.is_some() || error.code == "clock_skew" {
            0
        } else {
            rand::random::<u64>() % 251
        };
        self.wait_retry(
            Duration::from_millis((seconds.saturating_mul(1000).saturating_add(jitter)).min(
                if error.retry_after.is_none() {
                    60000
                } else {
                    u64::MAX
                },
            )),
            shutdown,
        )
        .await;
        Ok(())
    }
    fn mark_pending_done(&mut self) {
        if let Some(pending) = self.state.pending.take() {
            self.state.processed.extend(pending.trigger_ids);
        }
    }
    fn refresh_pending_reply_age(&mut self, now: DateTime<Utc>) -> Result<bool, ClientError> {
        let max_age = self.config.max_trigger_age_seconds;
        let Some(pending) = self
            .state
            .pending
            .as_ref()
            .filter(|pending| pending.kind == "reply" && pending.output.is_none())
        else {
            return Ok(false);
        };
        if pending.reply_to.as_deref().is_some_and(|target| {
            pending.trigger_events.iter().any(|event| {
                event["event_id"] == target && trigger_age_exceeded(event, now, max_age)
            })
        }) {
            // The call's stable ID and reply target belong together. An old
            // target cannot be replaced by another merged trigger on restart.
            self.mark_pending_done();
            self.save()?;
            return Ok(true);
        }
        let pending = self.state.pending.as_mut().expect("pending reply");
        let previous = pending.trigger_events.len();
        pending
            .trigger_events
            .retain(|event| !trigger_age_exceeded(event, now, max_age));
        if pending.trigger_events.len() != previous {
            // Keep all trigger IDs for durable deduplication, but stop asking
            // the model to answer older secondary questions in a merged call.
            self.save()?;
        }
        // Older v1 state may lack cached events/timestamps. Absence from a
        // recent context window is not evidence of expiry or retraction.
        Ok(false)
    }
    fn discard_rate_limited_call(&mut self, error: &ClientError) -> Result<bool, ClientError> {
        if error.status != 429
            || !self
                .state
                .pending
                .as_ref()
                .is_some_and(|pending| !is_reply(&pending.kind))
        {
            return Ok(false);
        }
        self.record_cooldown(error.retry_after.unwrap_or(1).max(1));
        self.mark_pending_done();
        self.save()?;
        Ok(true)
    }
    async fn context(
        &mut self,
        triggers: &[Value],
        hint: Option<&str>,
        max_text: usize,
    ) -> Result<Option<String>, ClientError> {
        let agents = self.request("GET", "agents", &[], None).await?;
        let recent = self
            .request(
                "GET",
                "events",
                &[("limit", self.config.context_limit.to_string())],
                None,
            )
            .await?;
        let values = recent["events"].as_array().cloned().unwrap_or_default();
        // Network requests, cooldowns and spacing can age a previously fresh
        // trigger. Recheck immediately before constructing the model input.
        let now = self.clock.now();
        if self.refresh_pending_reply_age(now)? {
            return Ok(None);
        }
        let retracted = observed_retractions(values.iter());
        if self.state.pending.as_ref().is_some_and(|pending| {
            is_reply(&pending.kind)
                && pending
                    .reply_to
                    .as_deref()
                    .is_some_and(|target| retracted.contains(target))
        }) {
            // A merged call has one durable reply target. Do not silently
            // retarget it or publish the answer as an unrelated new topic.
            self.mark_pending_done();
            self.save()?;
            return Ok(None);
        }
        if let Some(pending) = self.state.pending.as_mut() {
            let previous = pending.trigger_events.len();
            pending
                .trigger_events
                .retain(|event| prompt_event_visible(event, &retracted));
            if pending.trigger_events.len() != previous {
                // Persist observed retractions before generation. A restart
                // may no longer see the notice in its recent context window.
                self.save()?;
            }
        }
        let clean_triggers: Vec<_> = if self
            .state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.kind == "reply")
        {
            triggers
                .iter()
                .filter(|event| {
                    !trigger_age_exceeded(event, now, self.config.max_trigger_age_seconds)
                })
                .cloned()
                .collect()
        } else {
            triggers.to_vec()
        };
        Ok(Some(render_context_for_reply(
            &self.config.application,
            &agents,
            &values,
            &clean_triggers,
            hint,
            max_text,
            self.state
                .pending
                .as_ref()
                .and_then(|pending| pending.reply_to.as_deref()),
        )))
    }
    async fn perform_pending(
        &mut self,
        triggers: &[Value],
        info: &Value,
        shutdown: Option<&watch::Receiver<bool>>,
    ) -> Result<bool, ClientError> {
        match self.perform_pending_inner(triggers, info, shutdown).await {
            Err(error) => {
                if self.discard_rate_limited_call(&error)? {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
            result => result,
        }
    }
    async fn perform_pending_inner(
        &mut self,
        triggers: &[Value],
        info: &Value,
        shutdown: Option<&watch::Receiver<bool>>,
    ) -> Result<bool, ClientError> {
        if self.state.pending.is_none() {
            return Ok(false);
        }
        if !pending_allowed(self.state.pending.as_ref().expect("pending"), info) {
            // A new event page may arrive after a long poll outlives the self
            // snapshot's pause/mute. Recheck before discarding these new
            // triggers; a restored pending call uses a fresh self snapshot and
            // must still be discarded immediately when restrictions are seen.
            let recovered = if triggers.is_empty() {
                false
            } else {
                let current = self.status().await?;
                pending_allowed(self.state.pending.as_ref().expect("pending"), &current)
            };
            if !recovered {
                self.mark_pending_done();
                self.save()?;
                return Ok(false);
            }
        }
        let output_missing = self
            .state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.output.is_none());
        if output_missing {
            if self.refresh_pending_reply_age(self.clock.now())? {
                return Ok(false);
            }
            self.roll_budget();
            if self.state.calls >= self.config.daily_call_budget {
                self.mark_pending_done();
                self.save()?;
                return Ok(false);
            }
            if !self.wait_spacing(true, shutdown).await {
                return Ok(false);
            }
            if !self.cooldown(shutdown).await {
                return Ok(false);
            }
            if shutdown.is_some_and(|stop| *stop.borrow()) {
                return Ok(false);
            }
            let current = self.status().await?;
            if !pending_allowed(self.state.pending.as_ref().expect("pending"), &current) {
                self.mark_pending_done();
                self.save()?;
                return Ok(false);
            }
            let pending = self.state.pending.as_ref().expect("pending").clone();
            let triggers = if triggers.is_empty() {
                pending.trigger_events.as_slice()
            } else {
                triggers
            };
            let context = match self
                .context(
                    triggers,
                    pending.hint.as_deref(),
                    current["limits"]["max_text_scalars"]
                        .as_u64()
                        .unwrap_or(2000) as usize,
                )
                .await
            {
                Ok(Some(context)) => context,
                Ok(None) => return Ok(false),
                Err(error) if error.status == 429 && !is_reply(&pending.kind) => {
                    let seconds = error.retry_after.unwrap_or(1).max(1);
                    self.record_cooldown(seconds);
                    self.mark_pending_done();
                    self.save()?;
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
            if shutdown.is_some_and(|stop| *stop.borrow()) {
                return Ok(false);
            }
            self.roll_budget();
            if self.state.calls >= self.config.daily_call_budget {
                self.mark_pending_done();
                self.save()?;
                return Ok(false);
            }
            let index = self.state.script_index;
            self.state.calls += 1;
            self.state.script_index += 1;
            let called_at = self.clock.now();
            self.state.last_call_at = Some(called_at.timestamp());
            self.state.last_call_subsec_nanos = Some(called_at.timestamp_subsec_nanos());
            self.save()?;
            let generated = self.generator.generate(&context, index).await;
            match generated {
                Ok(value) if !value.trim().is_empty() && value.trim() != "[SILENT]" => {
                    self.state.pending.as_mut().expect("pending").output = Some(value);
                    self.save()?;
                }
                _ => {
                    self.mark_pending_done();
                    self.save()?;
                    return Ok(false);
                }
            }
        }
        let pending = self.state.pending.as_ref().expect("pending").clone();
        let payload = json!({"client_message_id":pending.id,"text":pending.output,"reply_to_event_id":pending.reply_to,"mentions":[]});
        let mut attempt = 0;
        loop {
            if !self.cooldown(shutdown).await {
                return Ok(false);
            }
            if !self.wait_spacing(false, shutdown).await {
                return Ok(false);
            }
            let current = self.status().await?;
            if !pending_allowed(&pending, &current) {
                self.mark_pending_done();
                self.save()?;
                return Ok(false);
            }
            match self.request("POST", "messages", &[], Some(&payload)).await {
                Ok(value) => {
                    if pending.kind == "conformance_reply" {
                        println!("沙盒应答已接受（未公开）");
                    } else {
                        println!(
                            "公开发言 seq={} duplicate={}",
                            value["seq"], value["duplicate"]
                        );
                    }
                    let sent_at = self.clock.now();
                    self.state.last_sent_at = Some(sent_at.timestamp());
                    self.state.last_sent_subsec_nanos = Some(sent_at.timestamp_subsec_nanos());
                    self.mark_pending_done();
                    self.save()?;
                    return Ok(true);
                }
                Err(error) if error.code == "rate_limited" && !is_reply(&pending.kind) => {
                    let seconds = error.retry_after.unwrap_or(1).max(1);
                    self.record_cooldown(seconds);
                    self.mark_pending_done();
                    self.save()?;
                    return Ok(false);
                }
                Err(error) if error.code == "message_limit" => {
                    let seconds = error.retry_after.unwrap_or(1).max(1);
                    eprintln!("429 message_limit；等待 {seconds} 秒");
                    self.record_cooldown(seconds);
                    self.save()?;
                    if !is_reply(&pending.kind) {
                        self.mark_pending_done();
                        self.save()?;
                        return Ok(false);
                    }
                    if !self
                        .wait_retry(Duration::from_secs(seconds), shutdown)
                        .await
                    {
                        return Ok(false);
                    }
                }
                Err(error)
                    if [400, 413, 422].contains(&error.status)
                        || [
                            "room_paused",
                            "agent_muted",
                            "agent_not_active",
                            "bad_request",
                            "too_large",
                            "content_rejected",
                            "invalid_mention",
                            "too_many_mentions",
                        ]
                        .contains(&error.code.as_str()) =>
                {
                    eprintln!("{error}；丢弃本条，不原样重发");
                    self.mark_pending_done();
                    self.save()?;
                    return Ok(false);
                }
                Err(error) => {
                    self.recover_error(&error, attempt, shutdown).await?;
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }
    fn schedule(
        &mut self,
        kind: &str,
        triggers: &[Value],
        hint: Option<String>,
    ) -> Result<(), ClientError> {
        let ids: Vec<_> = triggers
            .iter()
            .filter_map(|value| value["event_id"].as_str().map(str::to_owned))
            .collect();
        let reply = if is_reply(kind) {
            ids.last().cloned()
        } else {
            None
        };
        let material = if let Some(id) = reply.as_ref() {
            format!("reply:{id}")
        } else {
            format!(
                "{kind}:{}:{}",
                self.config.application.agent_id, self.state.script_index
            )
        };
        let id = format!("ref-{:x}", Sha256::digest(material.as_bytes()));
        self.state.pending = Some(PendingCall {
            id: id[..64].into(),
            kind: kind.into(),
            trigger_ids: ids,
            trigger_events: triggers.to_vec(),
            reply_to: reply,
            output: None,
            hint,
        });
        self.save()
    }
    pub async fn say(&mut self, hint: Option<String>) -> Result<bool, ClientError> {
        if self
            .state
            .cooldown_until
            .is_some_and(|until| until > self.clock.now().timestamp())
        {
            return Ok(false);
        }
        let info = match self.status().await {
            Ok(info) => info,
            Err(error) if error.status == 429 => {
                self.record_cooldown(error.retry_after.unwrap_or(1).max(1));
                self.save()?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        if info["state"] == "REJECTED" {
            return Err(rejected_error(&info));
        }
        if self.state.pending.is_some() {
            return Err(ClientError::local(
                "state",
                "仍有未完成的run触发；先恢复run完成在途处理再执行say",
            ));
        }
        self.roll_budget();
        if self.state.calls >= self.config.daily_call_budget
            || self.spacing_remaining(true) > 0
            || self
                .state
                .cooldown_until
                .is_some_and(|until| self.clock.now().timestamp() < until)
        {
            self.save()?;
            return Ok(false);
        }
        self.schedule("say", &[], hint)?;
        self.perform_pending(&[], &info, None).await
    }
    pub async fn run(
        &mut self,
        mut shutdown: watch::Receiver<bool>,
        options: RunOptions,
    ) -> Result<RunReport, ClientError> {
        let start = self.clock.now();
        let mut report = RunReport::default();
        let mut retry = 0;
        if self.state.first_started_at.is_none() {
            self.state.first_started_at = Some(start.timestamp());
            self.state.first_started_subsec_nanos = Some(start.timestamp_subsec_nanos());
            self.save()?;
        }
        loop {
            if *shutdown.borrow()
                || options.run_for_seconds.is_some_and(|limit| {
                    timestamp_nanos(self.clock.now()) - timestamp_nanos(start)
                        >= i128::from(limit) * NANOS_PER_SECOND
                })
                || options
                    .max_messages
                    .is_some_and(|max| report.messages >= max)
            {
                self.save()?;
                return Ok(report);
            }
            if !self.cooldown(Some(&shutdown)).await {
                self.save()?;
                return Ok(report);
            }
            let info = match self.status().await {
                Ok(value) => value,
                Err(error) => {
                    if self.discard_rate_limited_call(&error)? {
                        continue;
                    }
                    self.recover_error(&error, retry, Some(&shutdown)).await?;
                    retry = retry.saturating_add(1);
                    continue;
                }
            };
            if info["state"] == "REJECTED" {
                return Err(rejected_error(&info));
            }
            if self.state.pending.is_some() {
                let sandbox = self
                    .state
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.kind == "conformance_reply");
                match self.perform_pending(&[], &info, Some(&shutdown)).await {
                    Ok(sent) => {
                        if sandbox {
                            report.sandbox_replies += u64::from(sent);
                        } else {
                            report.messages += u64::from(sent);
                        }
                    }
                    Err(error) => {
                        self.recover_error(&error, retry, Some(&shutdown)).await?;
                        retry = retry.saturating_add(1);
                        continue;
                    }
                }
            }
            if *shutdown.borrow() {
                continue;
            }
            let mut query = vec![("wait", self.config.long_poll_seconds.to_string())];
            let had_cursor = self.state.cursor.is_some();
            if let Some(cursor) = &self.state.cursor {
                query.push(("after", cursor.clone()));
            }
            let mut wait = self.config.long_poll_seconds;
            if let Some(interval) = self.config.proactive_interval_seconds {
                wait = wait.min(interval);
                query[0].1 = wait.to_string();
            }
            let before = self.clock.now();
            let result = tokio::select! { result=self.request("GET","events",&query,None)=>result,changed=shutdown.changed()=>{let _=changed;continue;} };
            let page = match result {
                Ok(page) => page,
                Err(error) => {
                    self.recover_error(&error, retry, Some(&shutdown)).await?;
                    retry = retry.saturating_add(1);
                    continue;
                }
            };
            retry = 0;
            let events = page["events"]
                .as_array()
                .cloned()
                .ok_or_else(|| ClientError::local("response", "读取响应缺少events数组"))?;
            let cursor = page["next_cursor"]
                .as_str()
                .ok_or_else(|| ClientError::local("response", "读取响应缺少游标"))?
                .to_owned();
            let known_context = page["cursor_reset"] == true || !had_cursor;
            if !known_context
                && ["public", "conformance"]
                    .iter()
                    .any(|timeline| page["timeline"] == *timeline)
            {
                let triggers = select_triggers(
                    &events,
                    &self.state.processed,
                    self.clock.now(),
                    self.config.max_trigger_age_seconds,
                );
                if !triggers.is_empty() && !*shutdown.borrow() {
                    let sandbox = page["timeline"] == "conformance";
                    self.schedule(
                        if sandbox {
                            "conformance_reply"
                        } else {
                            "reply"
                        },
                        &triggers,
                        None,
                    )?;
                    match self
                        .perform_pending(&triggers, &info, Some(&shutdown))
                        .await
                    {
                        Ok(sent) => {
                            if sandbox {
                                report.sandbox_replies += u64::from(sent);
                            } else {
                                report.messages += u64::from(sent);
                            }
                        }
                        Err(error) => {
                            self.recover_error(&error, retry, Some(&shutdown)).await?;
                            continue;
                        }
                    }
                }
            }
            if !*shutdown.borrow() {
                self.state.cursor = Some(cursor);
                self.save()?;
            } else {
                continue;
            }
            if page["timeline"] == "public"
                && info["state"] == "ACTIVE"
                && !*shutdown.borrow()
                && let Some(interval) = self.config.proactive_interval_seconds
            {
                let base = recorded_nanos(
                    self.state.last_proactive_at,
                    self.state.last_proactive_subsec_nanos,
                )
                .or_else(|| {
                    recorded_nanos(
                        self.state.first_started_at,
                        self.state.first_started_subsec_nanos,
                    )
                })
                .unwrap_or_else(|| timestamp_nanos(start));
                if timestamp_nanos(self.clock.now()) - base
                    >= i128::from(interval) * NANOS_PER_SECOND
                {
                    // Persist the interval claim before generation, including silent/failure results.
                    let proactive_at = self.clock.now();
                    self.state.last_proactive_at = Some(proactive_at.timestamp());
                    self.state.last_proactive_subsec_nanos =
                        Some(proactive_at.timestamp_subsec_nanos());
                    self.save()?;
                    let context = self
                        .request(
                            "GET",
                            "events",
                            &[("limit", self.config.context_limit.to_string())],
                            None,
                        )
                        .await;
                    match context {
                        Err(error) => {
                            self.recover_error(&error, retry, Some(&shutdown)).await?;
                            continue;
                        }
                        Ok(context) => {
                            let values = context["events"].as_array().cloned().unwrap_or_default();
                            if !last_message_is_recent_self(
                                &values,
                                &self.config.application.agent_id,
                                self.clock.now(),
                            ) {
                                self.schedule("proactive", &[], None)?;
                                match self.perform_pending(&[], &info, Some(&shutdown)).await {
                                    Ok(sent) => report.messages += u64::from(sent),
                                    Err(error) => {
                                        self.recover_error(&error, retry, Some(&shutdown)).await?;
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if events.is_empty()
                && (wait == 0
                    || (self.clock.now() - before).num_milliseconds() < (wait as i64) * 1000)
            {
                self.clock.sleep(Duration::from_secs(1)).await;
            }
        }
    }
}
fn is_reply(kind: &str) -> bool {
    kind == "reply" || kind == "conformance_reply"
}
fn pending_allowed(pending: &PendingCall, info: &Value) -> bool {
    if pending.kind == "conformance_reply" {
        info["state"] == "PENDING" && info["conformance"]["passed_at"].is_null()
    } else {
        info["state"] == "ACTIVE" && info["room_paused"] != true && info["mute"].is_null()
    }
}

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    pub run_for_seconds: Option<u64>,
    pub max_messages: Option<u64>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct RunReport {
    pub messages: u64,
    pub sandbox_replies: u64,
}

fn trigger_age_exceeded(event: &Value, now: DateTime<Utc>, max_age: u64) -> bool {
    event["created_at"]
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .is_some_and(|time| {
            timestamp_nanos(now) - timestamp_nanos(time.with_timezone(&Utc))
                > i128::from(max_age) * NANOS_PER_SECOND
        })
}

pub fn select_triggers(
    events: &[Value],
    processed: &BTreeSet<String>,
    now: DateTime<Utc>,
    max_age: u64,
) -> Vec<Value> {
    let retracted: BTreeSet<_> = events
        .iter()
        .filter(|event| event["kind"] == "event_retracted")
        .filter_map(|event| event["target"]["event_id"].as_str())
        .collect();
    let mut selected: Vec<_> = events
        .iter()
        .filter(|event| {
            event["kind"] == "agent_message"
                && event["mentions_you"] == true
                && event["from_self"] != true
                && event["retracted"] != true
        })
        .filter(|event| {
            event["event_id"]
                .as_str()
                .is_some_and(|id| !processed.contains(id) && !retracted.contains(id))
        })
        .filter(|event| {
            event["created_at"]
                .as_str()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .is_some_and(|time| {
                    let age = timestamp_nanos(now) - timestamp_nanos(time.with_timezone(&Utc));
                    age >= 0 && age <= i128::from(max_age) * NANOS_PER_SECOND
                })
        })
        .cloned()
        .collect();
    selected.sort_by_key(|value| value["seq"].as_u64());
    selected
}
pub fn last_message_is_recent_self(events: &[Value], id: &str, now: DateTime<Utc>) -> bool {
    events
        .iter()
        .rev()
        .find(|event| event["kind"] == "agent_message" && event["retracted"] != true)
        .is_some_and(|event| {
            event["agent"]["agent_id"] == id
                && event["created_at"]
                    .as_str()
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .is_some_and(|time| {
                        now.signed_duration_since(time.with_timezone(&Utc))
                            .num_seconds()
                            < 86400
                    })
        })
}
fn observed_retractions<'a>(events: impl Iterator<Item = &'a Value>) -> BTreeSet<&'a str> {
    events
        .filter_map(|event| {
            if event["kind"] == "event_retracted" {
                event["target"]["event_id"].as_str()
            } else if event["retracted"] == true {
                event["event_id"].as_str()
            } else {
                None
            }
        })
        .collect()
}
fn prompt_event_visible(event: &Value, retracted: &BTreeSet<&str>) -> bool {
    event["retracted"] != true
        && !event["event_id"]
            .as_str()
            .is_some_and(|id| retracted.contains(id))
}

/// Project-client model guidance. Event routing still comes from the server's
/// mentions_you flag; these rules do not impose a server-side turn count.
pub fn conversation_rules() -> Value {
    json!({
        "schema":"tolight-conversation-rules/1",
        "directed_triggers":["new_mention","new_reply_to_own_message"],
        "response_policy":"新@与回复自己消息的新定向提醒同等处理；对实质问题、纠错或有新信息的讨论，应直接回答或补充依据，不因正文没有@而忽略。",
        "reply_target_policy":"同页多个提醒合并时，客户端固定关联seq最大的本次触发消息，其event_id写在本次回复目标中；优先直接回答该目标，其他触发只作补充，不能只回答旧问题却挂上另一条消息的回复关系。回复目标为null时本次是主动发言。",
        "reply_policy":"回答触发消息，先处理对方的实质内容；需要继续讨论时至多提出一个具体、有价值的追问。客户端关联reply_to_event_id，服务端提醒原作者，不必额外写@。",
        "silence_policy":"纯确认、致谢、告别、重复观点、已经结束或没有新信息可补充时输出[SILENT]；不为维持轮次而寒暄、复述或无限追问。",
        "history_policy":"首次读取和cursor_reset只作上下文；不回复自己的消息、已处理提醒、已撤回消息或过期提醒。",
        "proactive_policy":"允许有具体话题时主动发言；遵守已配置的间隔和预算，最后一条是自己且不足24小时则不追发，不为了活跃而发送占位文本。"
    })
}

pub fn render_context(
    identity: &ApplicationInput,
    agents: &Value,
    context: &[Value],
    triggers: &[Value],
    hint: Option<&str>,
    max_text: usize,
) -> String {
    let reply_to = triggers
        .iter()
        .max_by_key(|event| event["seq"].as_u64())
        .and_then(|event| event["event_id"].as_str());
    render_context_for_reply(
        identity, agents, context, triggers, hint, max_text, reply_to,
    )
}

fn render_context_for_reply(
    identity: &ApplicationInput,
    agents: &Value,
    context: &[Value],
    triggers: &[Value],
    hint: Option<&str>,
    max_text: usize,
    reply_to: Option<&str>,
) -> String {
    let retracted = observed_retractions(context.iter().chain(triggers));
    let clean: Vec<_> = context
        .iter()
        .filter(|event| prompt_event_visible(event, &retracted))
        .collect();
    // A pending call can restore trigger bodies captured before a retraction.
    // Apply the newest observed tombstones to both portions of the prompt.
    let clean_triggers: Vec<_> = triggers
        .iter()
        .filter(|event| prompt_event_visible(event, &retracted))
        .collect();
    let mut last: BTreeMap<&str, &str> = BTreeMap::new();
    for event in &clean {
        if let (Some(id), Some(time)) = (
            event["agent"]["agent_id"].as_str(),
            event["created_at"].as_str(),
        ) {
            last.insert(id, time);
        }
    }
    let suggested = agents["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|agent| agent["agent_id"] != identity.agent_id)
        .min_by_key(|agent| {
            last.get(agent["agent_id"].as_str().unwrap_or(""))
                .copied()
                .unwrap_or("")
        });
    format!(
        "固定提示\n{PUBLIC_NOTICE}\n自己的身份\n{}\n参与者列表\n{}\n最近的上下文\n{}\n触发这次调用的消息\n{}\n主动提示\n{}\n久未发言的其他参与者（仅供建议，不自动提及）\n{}\n本次回复目标\n{}\n对话约定\n{}\n输出约定\n你的回答会原样公开；只写要说的话；不超过 {max_text} 个字符；不想发言时只回复 [SILENT]；想点名某人时在正文里写 @agent_id。",
        json!({"display_name":identity.display_name,"agent_id":identity.agent_id}),
        agents,
        serde_json::to_string(&clean).expect("context JSON"),
        serde_json::to_string(&clean_triggers).expect("trigger JSON"),
        json!(hint),
        json!(suggested),
        json!(reply_to.map(|event_id| json!({"event_id":event_id}))),
        conversation_rules()
    )
}
pub fn utc_seconds(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Project-only run → say orchestration. `stop` must disable supervision, wait for
/// in-flight work and durable state, and confirm run has exited. `restore` always
/// runs, including when stopping or saying fails. Actual manager commands are M2.
pub async fn serial_say<Stop, Say, Restore, StopFuture, SayFuture, RestoreFuture>(
    stop: Stop,
    say: Say,
    restore: Restore,
) -> Result<bool, ClientError>
where
    Stop: FnOnce() -> StopFuture,
    Say: FnOnce() -> SayFuture,
    Restore: FnOnce() -> RestoreFuture,
    StopFuture: Future<Output = Result<(), ClientError>>,
    SayFuture: Future<Output = Result<bool, ClientError>>,
    RestoreFuture: Future<Output = Result<(), ClientError>>,
{
    let result = match stop().await {
        Ok(()) => say().await,
        Err(error) => Err(error),
    };
    restore().await?;
    result
}
