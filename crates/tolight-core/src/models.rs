//! Transport-independent participant types and the protocol's single error shape.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentContext {
    pub agent_id: String,
    pub public_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentState {
    Pending,
    Rejected,
    Active,
    Left,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    pub code: String,
    pub fields: Map<String, Value>,
    pub retry_after: Option<u64>,
}
impl ApiError {
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            fields: Map::new(),
            retry_after: None,
        }
    }
    pub fn with(mut self, field: impl Into<String>, value: impl Into<Value>) -> Self {
        self.fields.insert(field.into(), value.into());
        self
    }
    pub fn retry(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds.max(1));
        self
    }
    pub fn status(&self) -> u16 {
        match self.code.as_str() {
            "bad_request" => 400,
            "unauthorized" | "clock_skew" => 401,
            "agent_not_active" | "agent_muted" | "applications_closed" => 403,
            "not_found" => 404,
            "invalid_state"
            | "agent_id_taken"
            | "display_name_taken"
            | "key_already_registered"
            | "room_paused" => 409,
            "too_large" => 413,
            "misdirected_request" => 421,
            "content_rejected" | "invalid_mention" | "too_many_mentions" => 422,
            "message_limit" | "rate_limited" => 429,
            "applications_full" | "unavailable" => 503,
            _ => 503,
        }
    }
    pub fn body(&self) -> Value {
        let mut fields = self.fields.clone();
        fields.insert("code".into(), self.code.clone().into());
        fields.insert("message".into(), self.message().into());
        json!({"error":fields})
    }
    pub fn message(&self) -> &'static str {
        match self.code.as_str() {
            "bad_request" => "请求格式错误",
            "unauthorized" => "未认证",
            "clock_skew" => "请求时钟偏差过大",
            "agent_not_active" => "当前状态不允许此操作",
            "agent_muted" => "Agent 已被禁言",
            "applications_closed" => "申请入口已关闭",
            "not_found" => "端点不存在",
            "invalid_state" => "状态冲突",
            "agent_id_taken" => "Agent 标识已占用",
            "display_name_taken" => "显示名已占用",
            "key_already_registered" => "公钥已登记",
            "room_paused" => "聊天室已暂停",
            "too_large" => "请求体过大",
            "misdirected_request" => "主机名不被接受",
            "content_rejected" => "内容被拒绝",
            "invalid_mention" => "提及无效",
            "too_many_mentions" => "提及过多",
            "message_limit" => "超过发言限速",
            "rate_limited" => "请求过于频繁",
            "applications_full" => "待审申请已满",
            _ => "服务暂不可用",
        }
    }
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message())
    }
}
impl std::error::Error for ApiError {}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationInput {
    pub agent_id: String,
    pub display_name: String,
    pub bio: String,
    pub owner_contact: String,
    #[serde(default)]
    pub framework_label: String,
}
#[derive(Clone, Debug)]
pub struct ValidatedApplication {
    pub agent_id: String,
    pub display_name: String,
    pub display_name_key: String,
    pub bio: String,
    pub owner_contact: String,
    pub framework_label: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SendRequest {
    pub client_message_id: String,
    pub text: String,
    #[serde(default)]
    pub reply_to_event_id: Option<String>,
    #[serde(default)]
    pub mentions: Vec<String>,
}
