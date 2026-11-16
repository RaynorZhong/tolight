//! One-shot project model boundary. No provider protocol, tools or stored model session.
use crate::{ClientError, Generator, conversation_rules};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::Instant,
};
use tolight_core::PUBLIC_NOTICE;

const MAX_MANDATE_BYTES: u64 = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub command: Vec<String>,
    pub mandate_path: PathBuf,
    pub max_output_tokens: u32,
    pub timeout_seconds: u64,
    pub credential_env: Option<String>,
    pub secret_cmd: Option<Vec<String>>,
}
impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            command: vec![],
            mandate_path: PathBuf::new(),
            max_output_tokens: 256,
            timeout_seconds: 30,
            credential_env: None,
            secret_cmd: None,
        }
    }
}
impl ModelConfig {
    pub fn validate(&self) -> Result<(), ClientError> {
        let invalid =
            || ClientError::local("config", "模型配置缺少必填命令/公开 mandate 或参数越界");
        if self
            .command
            .first()
            .is_none_or(|program| !Path::new(program).is_absolute())
            || self.command.iter().any(|arg| arg.contains('\0'))
            || self.mandate_path.as_os_str().is_empty()
            || !(1..=8192).contains(&self.max_output_tokens)
            || !(1..=300).contains(&self.timeout_seconds)
            || (self.credential_env.is_some() && self.secret_cmd.is_some())
        {
            return Err(invalid());
        }
        if self.credential_env.as_ref().is_some_and(|name| {
            name.is_empty()
                || name.len() > 128
                || !name.bytes().enumerate().all(|(index, c)| {
                    c == b'_' || c.is_ascii_alphabetic() || (index > 0 && c.is_ascii_digit())
                })
        }) {
            return Err(invalid());
        }
        if self.secret_cmd.as_ref().is_some_and(|args| {
            args.first()
                .is_none_or(|program| !Path::new(program).is_absolute())
                || args.iter().any(|arg| arg.contains('\0'))
        }) {
            return Err(invalid());
        }
        Ok(())
    }
}

pub struct CommandGenerator {
    config: ModelConfig,
    mandate: String,
}
impl CommandGenerator {
    pub fn new(config: ModelConfig) -> Result<Self, ClientError> {
        config.validate()?;
        let metadata = std::fs::metadata(&config.mandate_path)
            .map_err(|_| ClientError::local("config", "无法读取公开 mandate 文件"))?;
        if !metadata.is_file() || metadata.len() > MAX_MANDATE_BYTES {
            return Err(ClientError::local(
                "config",
                "公开 mandate 必须为不超过64KiB的文本文件",
            ));
        }
        let mandate = std::fs::read_to_string(&config.mandate_path)
            .map_err(|_| ClientError::local("config", "公开 mandate 不是可读UTF-8文本"))?;
        if mandate.trim().is_empty() {
            return Err(ClientError::local("config", "公开 mandate 不能为空"));
        }
        Ok(Self { config, mandate })
    }
    async fn credential(&self, deadline: Instant) -> Result<Option<String>, String> {
        let raw = if let Some(name) = &self.config.credential_env {
            Some(std::env::var(name).map_err(|_| "模型凭据环境变量缺失".to_owned())?)
        } else if let Some(command) = &self.config.secret_cmd {
            Some(invoke(command, b"", None, deadline, 65536).await?)
        } else {
            None
        };
        raw.map(|value| {
            let text = value.strip_suffix('\n').unwrap_or(&value);
            if text.is_empty() || text.trim() != text || text.contains(['\n', '\r', '\0']) {
                return Err("模型凭据命令未返回有效单行值".into());
            }
            Ok(text.to_owned())
        })
        .transpose()
    }
    async fn generate_once(&self, context: &str) -> Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(self.config.timeout_seconds);
        let credential = self.credential(deadline).await?;
        let request = json!({"schema":"tolight-model-request/1","notice":PUBLIC_NOTICE,"mandate":self.mandate,"conversation_rules":conversation_rules(),"input":context,"max_output_tokens":self.config.max_output_tokens,"timeout_seconds":self.config.timeout_seconds});
        let bytes = serde_json::to_vec(&request).map_err(|_| "模型请求编码失败".to_owned())?;
        invoke(
            &self.config.command,
            &bytes,
            credential.as_deref(),
            deadline,
            MAX_OUTPUT_BYTES,
        )
        .await
    }
}
impl Generator for CommandGenerator {
    fn generate<'a>(
        &'a self,
        context: &'a str,
        _index: u64,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(self.generate_once(context))
    }
}

struct Invocation {
    child: tokio::process::Child,
    #[cfg(unix)]
    group: u32,
    finished: bool,
}
impl Invocation {
    #[cfg(unix)]
    fn group_signal(&self, signal: &str) -> bool {
        // Only the group created with this child is addressed, never a PID read
        // from model output, a state file or caller-controlled configuration.
        std::process::Command::new("/bin/kill")
            .args([signal, "--", &format!("-{}", self.group)])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    fn has_descendants(&self) -> bool {
        #[cfg(unix)]
        {
            self.group_signal("-0")
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
    async fn cleanup(&mut self) {
        if self.finished {
            return;
        }
        // A successful wait can reap the leader before inherited pipes close;
        // descendants still belong to its saved process group.
        #[cfg(unix)]
        self.group_signal("-KILL");
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        self.finished = true;
    }
}
impl Drop for Invocation {
    fn drop(&mut self) {
        if !self.finished {
            #[cfg(unix)]
            self.group_signal("-KILL");
            let _ = self.child.start_kill();
        }
    }
}

async fn invoke(
    command: &[String],
    input: &[u8],
    credential: Option<&str>,
    deadline: Instant,
    limit: usize,
) -> Result<String, String> {
    let mut process = Command::new(&command[0]);
    process.args(&command[1..]).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        process.env("PATH", path);
    }
    if let Some(value) = credential {
        process.env("TOLIGHT_MODEL_CREDENTIAL", value);
    }
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);
    let child = process.spawn().map_err(|_| "模型命令无法启动".to_owned())?;
    let mut invocation = Invocation {
        #[cfg(unix)]
        group: child
            .id()
            .ok_or_else(|| "模型命令没有进程标识".to_owned())?,
        child,
        finished: false,
    };
    let stdin = invocation
        .child
        .stdin
        .take()
        .ok_or_else(|| "模型命令缺少输入管道".to_owned())?;
    let stdout = invocation
        .child
        .stdout
        .take()
        .ok_or_else(|| "模型命令缺少输出管道".to_owned())?;
    let operation = async {
        let output = async {
            let mut bytes = Vec::new();
            stdout
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| "模型输出读取失败".to_owned())?;
            if bytes.len() > limit {
                return Err("模型输出超过字节上限".to_owned());
            }
            Ok(bytes)
        };
        let write = async move {
            let mut stdin = stdin;
            stdin
                .write_all(input)
                .await
                .map_err(|_| "模型输入发送失败".to_owned())?;
            // Closing the pipe is the frame boundary; shutdown alone does not
            // necessarily close ChildStdin on every supported OS.
            drop(stdin);
            Ok::<(), String>(())
        };
        let (bytes, ()) = tokio::try_join!(output, write)?;
        let status = invocation
            .child
            .wait()
            .await
            .map_err(|_| "模型命令等待失败".to_owned())?;
        if !status.success() {
            return Err("模型命令执行失败".into());
        }
        if invocation.has_descendants() {
            return Err("模型命令结束后仍有后台子进程".into());
        }
        String::from_utf8(bytes).map_err(|_| "模型输出不是UTF-8文本".into())
    };
    match tokio::time::timeout_at(deadline, operation).await {
        Ok(Ok(output)) => {
            invocation.finished = true;
            Ok(output)
        }
        result => {
            invocation.cleanup().await;
            match result {
                Ok(Err(error)) => Err(error),
                Err(_) => Err("模型调用超时".into()),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}
