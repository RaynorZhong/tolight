//! Serial local maintenance owned by the foreground supervisor.
use super::{Result, c};
use clap::Parser;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const SCHEMA: &str = "tolight-local-automation/1";
const LOG_BYTES: u64 = 1024 * 1024;
const LOG_FILES: usize = 3;
const SUMMARY_ITEMS: usize = 8;
const SUMMARY_TEXT_CHARS: usize = 160;

#[derive(clap::Args, Clone)]
pub(super) struct Options {
    /// Disable this supervisor's local checks and daily logical backups.
    #[arg(long)]
    pub no_automation: bool,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=86400))]
    pub check_interval_seconds: u64,
    #[arg(long, default_value_t = 86400, value_parser = clap::value_parser!(u64).range(1..=604800))]
    pub backup_interval_seconds: u64,
}
impl Options {
    pub fn describe(&self, maintenance: bool) -> Value {
        json!({"enabled":!self.no_automation && !maintenance,"check_interval_seconds":self.check_interval_seconds,"backup_interval_seconds":self.backup_interval_seconds,"logical_retention_days":14,"notification_delivery":"private_local_file_only"})
    }
}

#[derive(Debug)]
struct Schedule {
    interval: Duration,
    next: Duration,
    failures: u32,
}
impl Schedule {
    fn new(seconds: u64, delay: Duration) -> Self {
        Self {
            interval: Duration::from_secs(seconds),
            next: delay,
            failures: 0,
        }
    }
    fn complete(&mut self, elapsed: Duration, success: bool) {
        let delay = if success {
            self.failures = 0;
            self.interval
        } else {
            self.failures = self.failures.saturating_add(1);
            let base = self.interval.as_secs().min(60);
            Duration::from_secs(
                base.saturating_mul(1u64 << self.failures.saturating_sub(1).min(16))
                    .min(3600),
            )
        };
        // A missed interval never creates a catch-up queue.
        self.next = elapsed.saturating_add(delay);
    }
}

pub(super) struct Worker {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<Value>>,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn start(
        directory: &Path,
        runtime: &Value,
        database: &str,
        options: &Options,
    ) -> Result<Self> {
        let private = directory.join("automation");
        super::private_directory(&private)?;
        super::no_symlinks(&private)?;
        let state_path = private.join("state.json");
        let old = c::load(&state_path)?;
        c::ensure(
            old.as_object().is_some_and(|object| object.is_empty())
                || (old["schema"] == SCHEMA && old["environment_id"] == runtime["environment_id"]),
            "local automation state belongs to a different environment or schema",
        )?;
        let state = json!({"schema":SCHEMA,"environment_id":runtime["environment_id"],"run_id":runtime["run_id"],"configuration":options.describe(false),"running":true,"started_at":c::now(None)?,"current_task":null,"checks":{},"backup":{}});
        c::save(&state_path, &state)?;
        let state = Arc::new(Mutex::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_state = state.clone();
        let worker_stop = stop.clone();
        let directory = directory.to_path_buf();
        let database = database.to_owned();
        let runtime = runtime.clone();
        let options = options.clone();
        let thread = thread::Builder::new().name("tolight-local-maintenance".into()).spawn(move || {
            let result = work(&directory, &runtime, &database, &options, &worker_stop, &worker_state);
            let mut value = worker_state.lock().unwrap_or_else(|error| error.into_inner());
            value["running"] = json!(false);
            value["current_task"] = Value::Null;
            value["stopped_at"] = json!(c::now(None).unwrap_or(0.));
            if result.is_err() { value["worker_error"] = json!("local maintenance failed; inspect private state and restart the owned supervisor"); }
            let _ = c::save(&state_path, &value);
        })?;
        Ok(Self {
            stop,
            state,
            thread: Some(thread),
        })
    }
    pub fn snapshot(&self) -> Value {
        let mut state = summary(&self.state.lock().unwrap_or_else(|error| error.into_inner()));
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            state["running"] = json!(false);
        }
        state
    }
    pub fn shutdown(&mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        // Existing native process guards finish group cleanup before returning.
        crate::ops::storage::request_cancel();
        c::COMMAND_CANCELLED.store(true, Ordering::Release);
        self.thread
            .take()
            .is_none_or(|thread| thread.join().is_ok())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(super) fn stored_status(directory: &Path) -> Value {
    c::load(&directory.join("automation/state.json"))
        .map(|state| summary(&state))
        .unwrap_or_else(|_| json!({"state_error":true}))
}

fn scalar(value: &Value, maximum: usize) -> Value {
    match value {
        Value::String(text) => Value::String(text.chars().take(maximum).collect()),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
        _ => Value::Null,
    }
}

fn list_summary(value: &Value) -> (Value, usize, bool) {
    let Some(items) = value.as_array() else {
        return (json!([]), 0, false);
    };
    let mut truncated = items.len() > SUMMARY_ITEMS;
    let sample: Vec<Value> = items
        .iter()
        .take(SUMMARY_ITEMS)
        .map(|item| {
            if let Some(text) = item.as_str() {
                truncated |= text.chars().count() > SUMMARY_TEXT_CHARS;
            }
            scalar(item, SUMMARY_TEXT_CHARS)
        })
        .collect();
    (json!(sample), items.len(), truncated)
}

/// Keep the control socket independent of the number of Agents and alert series.
/// Full reports stay in the private state file, rather than the lifecycle handshake.
fn summary(state: &Value) -> Value {
    let mut result = json!({"summary":true,"details_file":"automation/state.json"});
    for key in [
        "schema",
        "environment_id",
        "run_id",
        "running",
        "started_at",
        "stopped_at",
        "current_task",
        "worker_error",
    ] {
        if let Some(value) = state.get(key) {
            result[key] = scalar(value, SUMMARY_TEXT_CHARS);
        }
    }
    if let Some(configuration) = state.get("configuration") {
        result["configuration"] = json!({});
        for key in [
            "enabled",
            "check_interval_seconds",
            "backup_interval_seconds",
            "logical_retention_days",
            "notification_delivery",
        ] {
            if let Some(value) = configuration.get(key) {
                result["configuration"][key] = scalar(value, SUMMARY_TEXT_CHARS);
            }
        }
    }
    for name in ["checks", "backup"] {
        let Some(task) = state.get(name) else {
            continue;
        };
        result[name] = json!({});
        for key in [
            "status",
            "last_started_at",
            "last_completed_at",
            "last_success_at",
            "next_due_at",
            "consecutive_failures",
            "error",
        ] {
            if let Some(value) = task.get(key) {
                result[name][key] = scalar(value, SUMMARY_TEXT_CHARS);
            }
        }
        let Some(report) = task.get("report") else {
            continue;
        };
        if name == "checks" {
            let (conditions, total, truncated) = list_summary(&report["conditions"]);
            let mut series = serde_json::Map::new();
            let mut series_total = 0;
            if let Some(values) = report["hourly_counter_estimates"].as_object() {
                series_total = values.len();
                for (key, value) in values.iter().take(SUMMARY_ITEMS) {
                    // Metric keys from the actual checker have a bounded Agent ID.
                    // Refuse to copy malformed oversized keys from stopped state.
                    if key.chars().count() <= SUMMARY_TEXT_CHARS {
                        series.insert(key.clone(), scalar(value, SUMMARY_TEXT_CHARS));
                    }
                }
            }
            let series_truncated = series.len() != series_total;
            result[name]["report"] = json!({"ok":scalar(&report["ok"], SUMMARY_TEXT_CHARS),"conditions":conditions,"conditions_total":total,"conditions_truncated":truncated,"hourly_counter_estimates":series,"hourly_counter_estimates_total":series_total,"hourly_counter_estimates_truncated":series_truncated});
        } else {
            let (removed, total, truncated) = list_summary(&report["removed"]);
            result[name]["report"] = json!({"backup":scalar(&report["backup"],4096),"retention_days":scalar(&report["retention_days"], SUMMARY_TEXT_CHARS),"removed":removed,"removed_total":total,"removed_truncated":truncated});
        }
    }
    result
}

fn append(path: &Path, event: &Value) -> Result {
    let bytes = serde_json::to_vec(event)?;
    c::ensure(bytes.len() <= 65536, "local notification exceeds 64 KiB")?;
    c::regular(path, false)?;
    if path.exists() && fs::metadata(path)?.len() + bytes.len() as u64 + 1 > LOG_BYTES {
        for index in (1..=LOG_FILES).rev() {
            let next = path.with_extension(format!("jsonl.{index}"));
            c::regular(&next, false)?;
            if index == LOG_FILES && next.exists() {
                fs::remove_file(&next)?;
            }
            let previous = if index == 1 {
                path.to_path_buf()
            } else {
                path.with_extension(format!("jsonl.{}", index - 1))
            };
            c::regular(&previous, false)?;
            if previous.exists() {
                fs::rename(previous, next)?;
            }
        }
    }
    let mut file = c::private_open(path, true)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[derive(Parser)]
struct AlertArguments {
    #[command(flatten)]
    args: super::super::alert::Args,
}
fn alert_arguments(directory: &Path, runtime: &Value) -> Result<super::super::alert::Args> {
    let port = runtime["ports"]["ops"]
        .as_u64()
        .ok_or("missing managed operations port")?;
    let config = tolight_core::config::Config::load(directory.join("config.toml"))?;
    let args = vec![
        "local-maintenance".into(),
        "--health-url".into(),
        format!("http://127.0.0.1:{port}/healthz"),
        "--metrics-url".into(),
        format!("http://127.0.0.1:{port}/metrics"),
        "--state-file".into(),
        directory
            .join("automation/alerts.json")
            .to_string_lossy()
            .into_owned(),
        "--backup-dir".into(),
        directory
            .join("backups/logical")
            .to_string_lossy()
            .into_owned(),
        "--filesystem".into(),
        directory.to_string_lossy().into_owned(),
        "--messages-per-day".into(),
        config.messages_per_day.to_string(),
    ];
    Ok(AlertArguments::try_parse_from(args)?.args)
}

fn update(state: &Mutex<Value>, path: &Path, change: impl FnOnce(&mut Value)) -> Result {
    let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
    change(&mut state);
    c::save(path, &state)
}

fn work(
    directory: &Path,
    runtime: &Value,
    database: &str,
    options: &Options,
    stop: &AtomicBool,
    state: &Mutex<Value>,
) -> Result {
    let private = directory.join("automation");
    let state_path = private.join("state.json");
    let notification_path = private.join("notifications.jsonl");
    let now = c::now(None)?;
    let latest = crate::ops::storage::backup::successful_backup_times(
        &directory.join("backups/logical"),
        now,
    )
    .into_iter()
    .reduce(f64::max);
    let delay = latest
        .map(|latest| (options.backup_interval_seconds as f64 - (now - latest).max(0.)).max(0.))
        .unwrap_or(0.);
    let mut backup = Schedule::new(
        options.backup_interval_seconds,
        Duration::from_secs_f64(delay),
    );
    let mut checks = Schedule::new(options.check_interval_seconds, Duration::ZERO);
    let started = Instant::now();
    update(state, &state_path, |state| {
        state["backup"]["status"] = json!("scheduled");
        state["backup"]["next_due_at"] = json!(now + delay);
        state["checks"]["status"] = json!("scheduled");
        state["checks"]["next_due_at"] = json!(now);
    })?;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    while !stop.load(Ordering::Acquire) {
        for (name, schedule) in [("backup", &mut backup), ("checks", &mut checks)] {
            if stop.load(Ordering::Acquire) || started.elapsed() < schedule.next {
                continue;
            }
            update(state, &state_path, |state| {
                state["current_task"] = json!(name);
                state[name]["last_started_at"] = json!(c::now(None).unwrap_or(0.));
            })?;
            let outcome = if name == "backup" {
                (|| -> Result<Value> {
                    let _lock = c::lock(&directory.join("backup-operation"))?;
                    crate::ops::storage::backup::logical_report_with_database(
                        crate::ops::storage::Backup {
                            config: Some(directory.join("config.toml")),
                            output_dir: directory.join("backups/logical"),
                            public_dir: None,
                            retention_days: 14,
                        },
                        database,
                    )
                })()
            } else {
                let mut send = |event: &Value| {
                    append(
                        &notification_path,
                        &json!({"schema":"tolight-local-notification/1","delivery":"private_local_file_only","environment_id":runtime["environment_id"],"notification":event}),
                    )
                };
                executor.block_on(async {
                    // Parsing the current managed configuration belongs to this
                    // attempt, so a temporary read/parse error can be retried.
                    let args = alert_arguments(directory, runtime)?;
                    tokio::select! {
                        result=super::super::alert::report(args, Some(&mut send))=>result,
                        _=async { while !stop.load(Ordering::Acquire) { tokio::time::sleep(Duration::from_millis(50)).await; } }=>Err("local maintenance interrupted".into()),
                    }
                })
            };
            if stop.load(Ordering::Acquire) {
                break;
            }
            schedule.complete(started.elapsed(), outcome.is_ok());
            let completed_at = c::now(None)?;
            update(state, &state_path, |state| {
                state["current_task"] = Value::Null;
                state[name]["last_completed_at"] = json!(completed_at);
                state[name]["next_due_at"] = json!(
                    completed_at
                        + schedule
                            .next
                            .saturating_sub(started.elapsed())
                            .as_secs_f64()
                );
                state[name]["consecutive_failures"] = json!(schedule.failures);
                match &outcome {
                    Ok(report) => {
                        state[name]
                            .as_object_mut()
                            .expect("task state")
                            .remove("error");
                        state[name]["status"] = json!(if report["ok"] == false {
                            "attention"
                        } else {
                            "passed"
                        });
                        state[name]["report"] = report.clone();
                        if name == "backup" {
                            state[name]["last_success_at"] = json!(completed_at);
                        }
                    }
                    Err(_) => {
                        state[name]
                            .as_object_mut()
                            .expect("task state")
                            .remove("report");
                        state[name]["status"] = json!("failed");
                        state[name]["error"] = json!(
                            "local operation failed; private input and tool diagnostics suppressed"
                        );
                    }
                }
            })?;
            if outcome.is_err() {
                append(
                    &notification_path,
                    &json!({"schema":"tolight-local-notification/1","delivery":"private_local_file_only","environment_id":runtime["environment_id"],"notification":{"code":format!("local_{name}_failed"),"at":completed_at,"retry_at":completed_at + schedule.next.saturating_sub(started.elapsed()).as_secs_f64()}}),
                )?;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduling_skips_backlog_and_backs_off_failures_with_a_cap() {
        let mut schedule = Schedule::new(86400, Duration::ZERO);
        schedule.complete(Duration::from_secs(7), false);
        assert_eq!(schedule.next, Duration::from_secs(67));
        schedule.complete(Duration::from_secs(200), false);
        assert_eq!(schedule.next, Duration::from_secs(320));
        for _ in 0..30 {
            schedule.complete(Duration::from_secs(500), false);
        }
        assert_eq!(schedule.next, Duration::from_secs(4100));
        schedule.complete(Duration::from_secs(999999), true);
        assert_eq!(schedule.next, Duration::from_secs(1086399));
        assert_eq!(schedule.failures, 0);
    }
    #[test]
    fn notification_log_rotates_and_never_follows_a_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("notifications.jsonl");
        for index in 0..90 {
            append(&log, &json!({"index":index,"text":"x".repeat(65500)})).unwrap();
        }
        for path in std::iter::once(log.clone())
            .chain((1..=LOG_FILES).map(|index| log.with_extension(format!("jsonl.{index}"))))
        {
            assert!(fs::metadata(&path).unwrap().len() <= LOG_BYTES);
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            for line in fs::read_to_string(path).unwrap().lines() {
                serde_json::from_str::<Value>(line).unwrap();
            }
        }
        let target = directory.path().join("outside");
        fs::write(&target, "untouched").unwrap();
        fs::remove_file(&log).unwrap();
        symlink(&target, &log).unwrap();
        assert!(append(&log, &json!({"ignored":true})).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
    }
}
