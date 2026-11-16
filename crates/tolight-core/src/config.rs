//! Strict configuration. Secrets are resolved by a noninteractive external command.
use crate::{models::ApiError, signing::canonical_authority};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    pub secret_cmd: Vec<String>,
}
impl Default for SecretRef {
    fn default() -> Self {
        Self {
            secret_cmd: vec!["/usr/bin/printenv".into(), "DATABASE_URL".into()],
        }
    }
}
impl SecretRef {
    pub fn resolve(&self) -> Result<String, ApiError> {
        let fail = || ApiError::new("bad_request").with("field", "database.secret_cmd");
        let Some(program) = self.secret_cmd.first() else {
            return Err(fail());
        };
        if !Path::new(program).is_absolute() {
            return Err(fail());
        }
        let output = Command::new(program)
            .args(&self.secret_cmd[1..])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|_| fail())?;
        if !output.status.success() || output.stdout.len() > 65536 {
            return Err(fail());
        }
        let value = String::from_utf8(output.stdout).map_err(|_| fail())?;
        let value = value.strip_suffix('\n').unwrap_or(&value);
        if value.is_empty() || value.trim() != value || value.contains(['\n', '\r', '\0']) {
            return Err(fail());
        }
        Ok(value.into())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PublicConfig {
    pub dir: PathBuf,
    pub demo_banner: bool,
}
impl Default for PublicConfig {
    fn default() -> Self {
        Self {
            dir: "public-dist".into(),
            demo_banner: false,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub body_read_timeout_ms: u64,
    pub total_request_timeout_ms: u64,
    pub trusted_proxies: Vec<IpAddr>,
    pub allow_direct_loopback: bool,
}
impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            body_read_timeout_ms: 10000,
            total_request_timeout_ms: 40000,
            trusted_proxies: vec![],
            allow_direct_loopback: false,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObservabilityConfig {
    pub listen: SocketAddr,
    pub log_dir: PathBuf,
    pub log_max_bytes: u64,
    pub log_max_files: usize,
}
impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8091".parse().expect("constant socket"),
            log_dir: ".local/logs/service".into(),
            log_max_bytes: 1048576,
            log_max_files: 5,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    pub enabled: bool,
    pub assets_dir: PathBuf,
}
impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            assets_dir: ".local/operator-ui".into(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub network: NetworkConfig,
    pub observability: ObservabilityConfig,
    pub admin: AdminConfig,
    pub database: SecretRef,
    pub public: PublicConfig,
    pub messages_per_minute: usize,
    pub messages_per_day: usize,
    pub max_text_scalars: usize,
    pub max_mentions_per_message: usize,
    pub read_default_limit: usize,
    pub page_default_limit: usize,
    pub read_text_budget_scalars: usize,
    pub long_poll_max_wait: u64,
    pub requests_per_minute: usize,
    pub unauthenticated_requests_per_minute: usize,
    pub signature_max_skew: u64,
    pub participant_hosts: Vec<String>,
    pub participant_scheme: String,
    pub applications_per_ip_per_day: usize,
    pub max_pending_applications: usize,
    pub application_ttl: u64,
    pub room_poll_interval: u64,
    pub publish_coalesce: u64,
    pub conformance_runs_per_day: usize,
    pub conformance_reply_window: u64,
    pub conformance_retry_after: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8090".parse().expect("constant socket"),
            network: NetworkConfig::default(),
            observability: ObservabilityConfig::default(),
            admin: AdminConfig::default(),
            database: SecretRef::default(),
            public: PublicConfig::default(),
            messages_per_minute: 2,
            messages_per_day: 100,
            max_text_scalars: 2000,
            max_mentions_per_message: 3,
            read_default_limit: 20,
            page_default_limit: 50,
            read_text_budget_scalars: 20000,
            long_poll_max_wait: 30,
            requests_per_minute: 120,
            unauthenticated_requests_per_minute: 30,
            signature_max_skew: 300,
            participant_hosts: vec!["127.0.0.1:8090".into()],
            participant_scheme: "http".into(),
            applications_per_ip_per_day: 5,
            max_pending_applications: 50,
            application_ttl: 14 * 86400,
            room_poll_interval: 1,
            publish_coalesce: 1,
            conformance_runs_per_day: 5,
            conformance_reply_window: 600,
            conformance_retry_after: 20,
        }
    }
}
impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ApiError> {
        let text = std::fs::read_to_string(path)
            .map_err(|_| ApiError::new("bad_request").with("field", "config"))?;
        Self::from_toml(&text)
    }
    pub fn from_toml(text: &str) -> Result<Self, ApiError> {
        let value: toml::Value = toml::from_str(text)
            .map_err(|_| ApiError::new("bad_request").with("field", "config"))?;
        let mut config: Self = toml::from_str(text)
            .map_err(|_| ApiError::new("bad_request").with("field", "config"))?;
        if value.get("participant_hosts").is_none() {
            config.participant_hosts = vec![config.listen.to_string()];
        }
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&mut self) -> Result<(), ApiError> {
        let range = |name: &str, value: u64, min: u64, max: u64| {
            if (min..=max).contains(&value) {
                Ok(())
            } else {
                Err(ApiError::new("bad_request").with("field", name))
            }
        };
        for (name, value, min, max) in [
            (
                "messages_per_minute",
                self.messages_per_minute as u64,
                1,
                100000,
            ),
            (
                "messages_per_day",
                self.messages_per_day as u64,
                1,
                10000000,
            ),
            ("max_text_scalars", self.max_text_scalars as u64, 1, 8000),
            (
                "max_mentions_per_message",
                self.max_mentions_per_message as u64,
                1,
                16,
            ),
            ("read_default_limit", self.read_default_limit as u64, 0, 50),
            ("page_default_limit", self.page_default_limit as u64, 1, 200),
            (
                "read_text_budget_scalars",
                self.read_text_budget_scalars as u64,
                1,
                1000000,
            ),
            ("long_poll_max_wait", self.long_poll_max_wait, 0, 30),
            (
                "requests_per_minute",
                self.requests_per_minute as u64,
                1,
                1000000,
            ),
            (
                "unauthenticated_requests_per_minute",
                self.unauthenticated_requests_per_minute as u64,
                1,
                1000000,
            ),
            ("signature_max_skew", self.signature_max_skew, 30, 900),
            (
                "applications_per_ip_per_day",
                self.applications_per_ip_per_day as u64,
                1,
                1000000,
            ),
            (
                "max_pending_applications",
                self.max_pending_applications as u64,
                1,
                1000000,
            ),
            ("application_ttl", self.application_ttl, 60, 365 * 86400),
            ("room_poll_interval", self.room_poll_interval, 1, 60),
            ("publish_coalesce", self.publish_coalesce, 0, 60),
            (
                "conformance_runs_per_day",
                self.conformance_runs_per_day as u64,
                1,
                1000,
            ),
            (
                "conformance_reply_window",
                self.conformance_reply_window,
                2,
                86400,
            ),
            (
                "conformance_retry_after",
                self.conformance_retry_after,
                1,
                3600,
            ),
        ] {
            range(name, value, min, max)?;
        }
        range(
            "network.body_read_timeout_ms",
            self.network.body_read_timeout_ms,
            1,
            60000,
        )?;
        range(
            "network.total_request_timeout_ms",
            self.network.total_request_timeout_ms,
            1,
            300000,
        )?;
        range(
            "observability.log_max_bytes",
            self.observability.log_max_bytes,
            256,
            1024 * 1024 * 1024,
        )?;
        range(
            "observability.log_max_files",
            self.observability.log_max_files as u64,
            1,
            100,
        )?;
        if self.network.total_request_timeout_ms <= self.long_poll_max_wait * 1000
            || self.network.total_request_timeout_ms <= self.network.body_read_timeout_ms
        {
            return Err(
                ApiError::new("bad_request").with("field", "network.total_request_timeout_ms")
            );
        }
        if self
            .network
            .trusted_proxies
            .iter()
            .any(|ip| !ip.is_loopback())
        {
            return Err(ApiError::new("bad_request").with("field", "network.trusted_proxies"));
        }
        if self.network.allow_direct_loopback && !self.listen.ip().is_loopback() {
            return Err(ApiError::new("bad_request").with("field", "network.allow_direct_loopback"));
        }
        if !self.observability.listen.ip().is_loopback()
            || (self.observability.listen == self.listen && self.listen.port() != 0)
            || self.observability.log_dir.as_os_str().is_empty()
        {
            return Err(ApiError::new("bad_request").with("field", "observability"));
        }
        if self.conformance_retry_after >= self.conformance_reply_window {
            return Err(ApiError::new("bad_request").with("field", "conformance_retry_after"));
        }
        if self.participant_hosts.is_empty() {
            return Err(ApiError::new("bad_request").with("field", "participant_hosts"));
        }
        if self.participant_scheme != "http" && self.participant_scheme != "https" {
            return Err(ApiError::new("bad_request").with("field", "participant_scheme"));
        }
        let mut hosts = BTreeSet::new();
        for value in &self.participant_hosts {
            let host = canonical_authority(value, &self.participant_scheme)
                .map_err(|_| ApiError::new("bad_request").with("field", "participant_hosts"))?;
            if self.participant_scheme == "http" && !authority_is_loopback(&host) {
                return Err(ApiError::new("bad_request").with("field", "participant_hosts"));
            }
            hosts.insert(host);
        }
        if self.participant_scheme == "http" && !self.listen.ip().is_loopback() {
            return Err(ApiError::new("bad_request").with("field", "listen"));
        }
        self.participant_hosts = hosts.into_iter().collect();
        if resolved_path(&self.observability.log_dir)?.starts_with(resolved_path(&self.public.dir)?)
        {
            return Err(ApiError::new("bad_request").with("field", "observability.log_dir"));
        }

        if self.public.dir.as_os_str().is_empty() {
            return Err(ApiError::new("bad_request").with("field", "public.dir"));
        }
        if self.admin.enabled
            && (self.admin.assets_dir.as_os_str().is_empty()
                || !self.observability.listen.ip().is_loopback()
                || resolved_path(&self.admin.assets_dir)?
                    .starts_with(resolved_path(&self.public.dir)?))
        {
            return Err(ApiError::new("bad_request").with("field", "admin"));
        }
        if self
            .database
            .secret_cmd
            .first()
            .is_none_or(|path| !Path::new(path).is_absolute())
        {
            return Err(ApiError::new("bad_request").with("field", "database.secret_cmd"));
        }
        Ok(())
    }
}
fn resolved_path(path: &Path) -> Result<PathBuf, ApiError> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|_| ApiError::new("bad_request"))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut ancestor = normalized.clone();
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut resolved) = ancestor.canonicalize() {
            for part in suffix.into_iter().rev() {
                resolved.push(part);
            }
            return Ok(resolved);
        }
        if let Some(name) = ancestor.file_name() {
            suffix.push(name.to_owned());
        }
        if !ancestor.pop() {
            return Ok(normalized);
        }
    }
}

pub fn authority_is_loopback(value: &str) -> bool {
    let host = if value.starts_with('[') {
        value
            .split_once(']')
            .map(|(host, _)| &host[1..])
            .unwrap_or("")
    } else {
        value.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_defaults_hosts_and_strict_keys() {
        let cfg = Config::from_toml("").unwrap();
        assert_eq!(cfg.listen.to_string(), "127.0.0.1:8090");
        assert!(!cfg.public.demo_banner);
        assert_eq!(
            Config::from_toml("listen='127.0.0.1:8877'")
                .unwrap()
                .participant_hosts,
            ["127.0.0.1:8877"]
        );
        for text in [
            "token='secret'",
            "database_url='postgres://u:password@host/db'",
            "unknown=1",
            "[public]\ntoken='x'",
            "[database]\npassword='x'",
        ] {
            assert!(Config::from_toml(text).is_err());
        }
        assert_eq!(Config::from_toml("participant_scheme='https'\nparticipant_hosts=['API.EXAMPLE:443','api.example','api.example:8443']").unwrap().participant_hosts,["api.example","api.example:8443"]);
        assert_eq!(
            Config::from_toml(
                "listen='[::1]:80'\nparticipant_hosts=['[::1]:00080','[0:0:0:0:0:0:0:1]']"
            )
            .unwrap()
            .participant_hosts,
            ["[0000:0000:0000:0000:0000:0000:0000:0001]"]
        );
        for text in [
            "participant_scheme='ftp'",
            "participant_hosts=[]",
            "participant_hosts=['example.org']",
            "participant_hosts=['127.0.0.1:']",
            "listen='0.0.0.0:8090'",
            "participant_scheme='https'\nparticipant_hosts=['user@host']",
        ] {
            assert!(Config::from_toml(text).is_err(), "{text}");
        }
    }
    #[test]
    fn direct_loopback_option_is_explicit_and_requires_a_loopback_listener() {
        assert!(!Config::from_toml("").unwrap().network.allow_direct_loopback);
        for listen in ["127.0.0.1:8090", "[::1]:8090"] {
            let text = format!(
                "listen='{listen}'\n[network]\nallow_direct_loopback=true\ntrusted_proxies=['127.0.0.1','::1']"
            );
            assert!(
                Config::from_toml(&text)
                    .unwrap()
                    .network
                    .allow_direct_loopback
            );
        }
        for listen in ["0.0.0.0:8090", "192.168.50.54:8090", "[::]:8090"] {
            let text = format!(
                "listen='{listen}'\nparticipant_scheme='https'\nparticipant_hosts=['api.example']\n[network]\nallow_direct_loopback=true"
            );
            let error = Config::from_toml(&text).unwrap_err();
            assert_eq!(error.code, "bad_request");
            assert_eq!(error.fields["field"], "network.allow_direct_loopback");
            // The option constrains local mixed access, not ordinary HTTPS deployment.
            assert!(
                Config::from_toml(
                    &text.replace("allow_direct_loopback=true", "allow_direct_loopback=false")
                )
                .is_ok()
            );
        }
    }
    #[test]
    fn config_all_numeric_bounds() {
        let values = [
            ("messages_per_minute", 1, 100000),
            ("messages_per_day", 1, 10000000),
            ("max_text_scalars", 1, 8000),
            ("max_mentions_per_message", 1, 16),
            ("read_default_limit", 0, 50),
            ("page_default_limit", 1, 200),
            ("read_text_budget_scalars", 1, 1000000),
            ("long_poll_max_wait", 0, 30),
            ("requests_per_minute", 1, 1000000),
            ("unauthenticated_requests_per_minute", 1, 1000000),
            ("signature_max_skew", 30, 900),
            ("applications_per_ip_per_day", 1, 1000000),
            ("max_pending_applications", 1, 1000000),
            ("application_ttl", 60, 365 * 86400),
            ("room_poll_interval", 1, 60),
            ("publish_coalesce", 0, 60),
            ("conformance_runs_per_day", 1, 1000),
            ("conformance_reply_window", 21, 86400),
            ("conformance_retry_after", 1, 599),
        ];
        for (name, min, max) in values {
            assert!(
                Config::from_toml(&format!("{name}={min}")).is_ok(),
                "{name} lower"
            );
            assert!(
                Config::from_toml(&format!("{name}={max}")).is_ok(),
                "{name} upper"
            );
            if min > 0 {
                assert!(
                    Config::from_toml(&format!("{name}={}", min - 1)).is_err(),
                    "{name} below"
                );
            }
            assert!(
                Config::from_toml(&format!("{name}={}", max + 1)).is_err(),
                "{name} above"
            );
        }
        assert!(
            Config::from_toml("conformance_reply_window=20\nconformance_retry_after=20").is_err()
        );
    }
    #[test]
    fn config_network_and_private_observability_bounds() {
        for text in [
            "[network]\nbody_read_timeout_ms=0",
            "[network]\ntotal_request_timeout_ms=30000",
            "[network]\ntrusted_proxies=['203.0.113.1']",
            "[observability]\nlisten='0.0.0.0:8091'",
            "[observability]\nlog_dir='public-dist/private-logs'",
            "[observability]\nlog_max_files=0",
            "[observability]\nlog_max_bytes=255",
        ] {
            assert!(Config::from_toml(text).is_err(), "{text}");
        }
        assert!(Config::from_toml("[observability]\nlisten='127.0.0.1:0'").is_ok());
        let directory = tempfile::tempdir().unwrap();
        let public = directory.path().join("public");
        std::fs::create_dir(&public).unwrap();
        #[cfg(unix)]
        {
            let alias = directory.path().join("alias");
            std::os::unix::fs::symlink(&public, &alias).unwrap();
            let text = format!(
                "[public]\ndir={:?}\n[observability]\nlog_dir={:?}",
                public,
                alias.join("logs")
            );
            assert!(Config::from_toml(&text).is_err());
        }
    }

    #[test]
    fn admin_is_opt_in_and_its_assets_cannot_be_public() {
        assert!(!Config::default().admin.enabled);
        assert!(Config::from_toml("[admin]\nenabled=true").is_ok());
        for text in [
            "[admin]\nenabled=true\nassets_dir=''",
            "[admin]\nenabled=true\nassets_dir='public-dist'",
            "[admin]\nenabled=true\nassets_dir='public-dist/admin'",
            "[admin]\nenabled=true\nunknown=true",
        ] {
            assert!(Config::from_toml(text).is_err(), "{text}");
        }
        let directory = tempfile::tempdir().unwrap();
        let public = directory.path().join("public");
        std::fs::create_dir(&public).unwrap();
        #[cfg(unix)]
        {
            let alias = directory.path().join("alias");
            std::os::unix::fs::symlink(&public, &alias).unwrap();
            let text = format!(
                "[public]\ndir={public:?}\n[admin]\nenabled=true\nassets_dir={:?}",
                alias.join("admin")
            );
            assert!(Config::from_toml(&text).is_err());
        }
    }

    #[test]
    fn config_secret_command_fails_closed() {
        let command = |shell: &str| SecretRef {
            secret_cmd: vec!["/bin/sh".into(), "-c".into(), shell.into()],
        };
        assert_eq!(
            command("printf 'postgresql:///local\\n'")
                .resolve()
                .unwrap(),
            "postgresql:///local"
        );
        for shell in [
            "printf ''",
            "printf 'a\\nb\\n'",
            "printf 'secret'; exit 1",
            "printf ' leading'",
            "printf 'one\\r\\n'",
        ] {
            assert!(command(shell).resolve().is_err());
        }
        assert!(
            SecretRef {
                secret_cmd: vec!["relative".into()]
            }
            .resolve()
            .is_err()
        );
    }
}
