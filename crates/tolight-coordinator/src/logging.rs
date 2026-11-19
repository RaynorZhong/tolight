//! Bounded, structured service logs. Callers pass only enumerated metadata.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::Mutex,
};
use tolight_core::config::ObservabilityConfig;

pub struct Logger {
    config: ObservabilityConfig,
    serial: Mutex<()>,
}
impl Logger {
    pub fn new(config: &ObservabilityConfig) -> Self {
        Self {
            config: config.clone(),
            serial: Mutex::new(()),
        }
    }
    pub fn initialize(&self) -> io::Result<()> {
        fs::create_dir_all(&self.config.log_dir)?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(0))?;
        Ok(())
    }
    fn path(&self, index: usize) -> PathBuf {
        self.config.log_dir.join(if index == 0 {
            "service.log".into()
        } else {
            format!("service.log.{index}")
        })
    }
    pub fn event(
        &self,
        event: &str,
        endpoint: Option<&str>,
        status: Option<u16>,
    ) -> io::Result<()> {
        self.write(serde_json::json!({"at":tolight_core::datetime::utc_seconds(time::OffsetDateTime::now_utc()),"event":event,"endpoint":endpoint,"status":status}))
    }
    /// Configuration values only, never the request's Host or forwarded headers.
    pub fn participant_hosts(&self, hosts: &[String]) -> io::Result<()> {
        for host in hosts {
            self.write(serde_json::json!({"at":tolight_core::datetime::utc_seconds(time::OffsetDateTime::now_utc()),"event":"participant_host","host":host}))?;
        }
        Ok(())
    }
    fn write(&self, value: serde_json::Value) -> io::Result<()> {
        let _serial = self
            .serial
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.initialize()?;
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > self.config.log_max_bytes {
            return Err(io::Error::other(
                "structured log record exceeds configured bound",
            ));
        }
        let length = fs::metadata(self.path(0))?.len();
        if length + bytes.len() as u64 > self.config.log_max_bytes {
            if self.config.log_max_files == 1 {
                fs::remove_file(self.path(0))?;
            } else {
                let oldest = self.path(self.config.log_max_files - 1);
                if oldest.exists() {
                    fs::remove_file(oldest)?;
                }
                for index in (0..self.config.log_max_files - 1).rev() {
                    let from = self.path(index);
                    if from.exists() {
                        fs::rename(from, self.path(index + 1))?;
                    }
                }
            }
        }
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.path(0))?;
        file.write_all(&bytes)?;
        file.flush()?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn logs_rotate_with_bounded_count_and_total_size() {
        let directory = tempfile::tempdir().unwrap();
        let config = ObservabilityConfig {
            log_dir: directory.path().into(),
            log_max_bytes: 256,
            log_max_files: 3,
            ..Default::default()
        };
        let logger = Logger::new(&config);
        for _ in 0..100 {
            logger
                .event("request", Some("messages"), Some(201))
                .unwrap();
        }
        let files = fs::read_dir(directory.path())
            .unwrap()
            .map(|e| e.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 3);
        assert!(files.iter().all(|e| e.metadata().unwrap().len() <= 256));
        for entry in files {
            for line in fs::read_to_string(entry.path()).unwrap().lines() {
                serde_json::from_str::<serde_json::Value>(line).unwrap();
            }
        }
    }
}
