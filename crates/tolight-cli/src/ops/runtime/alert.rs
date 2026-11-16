use super::{Result, common as c};
use clap::Args as ClapArgs;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    process::Command,
};
const REASONS: [&str; 6] = [
    "empty",
    "too_long",
    "invalid_chars",
    "secret_shape",
    "invalid_mention",
    "too_many_mentions",
];
const GLOBAL: [&str; 9] = [
    "tolight_database_up",
    "tolight_process_start_timestamp_seconds",
    "tolight_clock_skew_total",
    "tolight_pending_applications",
    "tolight_oldest_pending_seconds",
    "tolight_latest_pending_timestamp_seconds",
    "tolight_publish_lag_events",
    "tolight_publish_lag_seconds",
    "tolight_room_messages_last_hour",
];
const AGENT: [&str; 4] = [
    "tolight_agent_attempts_last_day",
    "tolight_agent_last_seen_seconds",
    "tolight_message_limit_rejections_total",
    "tolight_agent_rejected_attempts_last_hour",
];
const COUNTER: &str = "tolight_message_limit_rejections_total";
const SKEW: &str = "tolight_clock_skew_total{}";
#[derive(ClapArgs)]
#[command(args_override_self = true)]
pub struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8091/healthz")]
    health_url: String,
    #[arg(long, default_value = "http://127.0.0.1:8091/metrics")]
    metrics_url: String,
    #[arg(long)]
    metrics_file: Option<PathBuf>,
    #[arg(long)]
    health_status: Option<u16>,
    #[arg(long, default_value = ".local/alert-state.json")]
    state_file: PathBuf,
    #[arg(long)]
    notify_command: Option<String>,
    #[arg(long, default_value = ".local/backups")]
    backup_dir: PathBuf,
    #[arg(long, default_value = ".")]
    filesystem: PathBuf,
    #[arg(long)]
    free_bytes: Option<u64>,
    #[arg(long, default_value_t = 1073741824)]
    min_free_bytes: u64,
    #[arg(long, default_value_t = 93600)]
    backup_max_age: u64,
    #[arg(long, default_value_t = 300)]
    publish_lag_seconds: u64,
    #[arg(long, default_value_t = 100)]
    messages_per_day: u64,
    #[arg(long, default_value_t = 0.8)]
    day_alert_ratio: f64,
    #[arg(long, default_value_t = 60)]
    messages_per_hour: u64,
    #[arg(long, default_value_t = 30)]
    message_limit_per_hour: u64,
    #[arg(long, default_value_t = 10)]
    sanitizer_rejections_per_hour: u64,
    #[arg(long, default_value_t = 5)]
    clock_skew_per_hour: u64,
    #[arg(long, default_value_t = 259200)]
    review_reminder: u64,
    #[arg(long, default_value_t = 21600)]
    repeat_seconds: u64,
    #[arg(long)]
    reference_agent: Option<String>,
    #[arg(long, default_value_t = 300)]
    reference_offline_seconds: u64,
    #[arg(long)]
    now: Option<f64>,
}
#[derive(Clone)]
struct Sample {
    name: String,
    labels: BTreeMap<String, String>,
    value: f64,
}
fn metrics(text: &str) -> Result<Vec<Sample>> {
    c::ensure(text.len() <= 1024 * 1024, "metrics exceed 1 MiB")?;
    let mut samples = Vec::new();
    let mut seen = BTreeSet::new();
    let mut globals = BTreeSet::new();
    let mut agents = BTreeSet::new();
    let mut reasons: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let end = line
            .find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != ':')
            .unwrap_or(line.len());
        let name = &line[..end];
        c::ensure(
            !name.is_empty() && !name.starts_with(|ch: char| ch.is_ascii_digit()),
            "malformed metrics",
        )?;
        if !GLOBAL.contains(&name) && !AGENT.contains(&name) {
            continue;
        }
        let mut rest = &line[end..];
        let mut labels = BTreeMap::new();
        if rest.starts_with('{') {
            let close = rest.rfind('}').ok_or("malformed metric labels")?;
            let mut raw = &rest[1..close];
            while !raw.is_empty() {
                let (key, tail) = raw.split_once('=').ok_or("malformed label")?;
                c::ensure(
                    !key.is_empty()
                        && key
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                        && tail.starts_with('"'),
                    "malformed label",
                )?;
                let mut escaped = false;
                let mut boundary = None;
                for (index, ch) in tail.char_indices().skip(1) {
                    if escaped {
                        escaped = false;
                        continue;
                    }
                    if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        boundary = Some(index + 1);
                        break;
                    }
                }
                let boundary = boundary.ok_or("unterminated label")?;
                let value: String = serde_json::from_str(&tail[..boundary])?;
                c::ensure(
                    labels.insert(key.into(), value).is_none(),
                    "duplicate label",
                )?;
                let tail = &tail[boundary..];
                raw = if tail.is_empty() {
                    ""
                } else {
                    tail.strip_prefix(',').ok_or("malformed label separator")?
                };
            }
            rest = &rest[close + 1..];
        }
        c::ensure(
            rest.starts_with(char::is_whitespace),
            "metric value separator missing",
        )?;
        let value: f64 = rest.trim().parse()?;
        c::ensure(
            value.is_finite()
                && (value >= 0. || (name == "tolight_agent_last_seen_seconds" && value == -1.)),
            "nonfinite or negative used metric",
        )?;
        if GLOBAL.contains(&name) {
            c::ensure(labels.is_empty(), "global metric has labels")?;
            globals.insert(name.to_owned());
        }
        if AGENT.contains(&name) {
            let expected = if name == "tolight_agent_rejected_attempts_last_hour" {
                2
            } else {
                1
            };
            c::ensure(
                labels.len() == expected
                    && labels.contains_key("agent_id")
                    && (expected == 1 || labels.contains_key("reason_code")),
                "invalid used metric labels",
            )?;
            let agent = &labels["agent_id"];
            c::ensure(
                tolight_core::mentions::valid_agent_id(agent),
                "invalid metric agent",
            )?;
            if name == "tolight_agent_attempts_last_day" {
                agents.insert(agent.clone());
            }
            if expected == 2 {
                let reason = &labels["reason_code"];
                c::ensure(
                    REASONS.contains(&reason.as_str()),
                    "unknown sanitation reason",
                )?;
                reasons
                    .entry(agent.clone())
                    .or_default()
                    .insert(reason.clone());
            }
        }
        if name.ends_with("_total")
            || [
                "tolight_database_up",
                "tolight_pending_applications",
                "tolight_publish_lag_events",
                "tolight_room_messages_last_hour",
                "tolight_agent_attempts_last_day",
                "tolight_agent_rejected_attempts_last_hour",
            ]
            .contains(&name)
        {
            c::ensure(value.fract() == 0., "fractional event count")?;
        }
        c::ensure(
            seen.insert((name.to_owned(), labels.clone())),
            "duplicate used metric sample",
        )?;
        samples.push(Sample {
            name: name.into(),
            labels,
            value,
        });
    }
    c::ensure(
        GLOBAL.iter().all(|name| globals.contains(*name)),
        "required global metric missing",
    )?;
    c::ensure(
        agents.iter().all(|agent| {
            reasons
                .get(agent)
                .is_some_and(|values| REASONS.iter().all(|reason| values.contains(*reason)))
        }),
        "six-reason sanitation series missing",
    )?;
    c::ensure(samples.len() <= 4096 * 8, "too many metric samples")?;
    Ok(samples)
}
fn counter_key(sample: &Sample) -> String {
    if sample.labels.is_empty() {
        format!("{}{{}}", sample.name)
    } else {
        format!(
            "{}{{\"agent_id\": {}}}",
            sample.name,
            serde_json::to_string(&sample.labels["agent_id"]).expect("JSON string")
        )
    }
}
fn finite(value: &Value) -> Result<f64> {
    let n = value.as_f64().ok_or("invalid numeric state")?;
    c::ensure(n.is_finite() && n >= 0., "invalid numeric state")?;
    Ok(n)
}
fn windows(
    samples: &[Sample],
    state: &mut Value,
    now: f64,
    conditions: &mut BTreeMap<String, String>,
) -> Result<BTreeMap<String, f64>> {
    let mut current: BTreeMap<String, f64> = samples
        .iter()
        .filter(|s| s.name == "tolight_clock_skew_total" || s.name == COUNTER)
        .map(|s| (counter_key(s), s.value))
        .collect();
    c::ensure(current.len() <= 4096, "too many counters")?;
    let previous = c::object(state.get("counters"))?;
    c::ensure(previous.len() <= 4096, "too many baseline counters")?;
    for v in previous.values() {
        finite(v)?;
    }
    let sampled = state.get("counter_sampled_at").map(finite).transpose()?;
    let epoch = samples
        .iter()
        .find(|s| s.name == "tolight_process_start_timestamp_seconds")
        .ok_or("missing process epoch")?
        .value;
    let old = c::object(state.get("counter_windows"))?;
    c::ensure(old.len() <= 4096, "too much counter history")?;
    let mut history: BTreeMap<String, Vec<[f64; 2]>> = BTreeMap::new();
    for (key, value) in old {
        if key != SKEW {
            let labels: Value =
                serde_json::from_str(key.strip_prefix(COUNTER).ok_or("unknown counter history")?)?;
            let labels = labels.as_object().ok_or("invalid counter labels")?;
            c::ensure(
                labels.len() == 1 && labels.contains_key("agent_id"),
                "invalid counter labels",
            )?;
        }
        let rows = value.as_array().ok_or("invalid counter buckets")?;
        c::ensure(rows.len() <= 61, "too many counter buckets")?;
        let mut buckets = Vec::new();
        for row in rows {
            let row = row.as_array().ok_or("invalid counter bucket")?;
            c::ensure(row.len() == 2, "invalid counter bucket")?;
            buckets.push([finite(&row[0])?, finite(&row[1])?]);
        }
        history.insert(key, buckets);
    }
    let baseline = sampled.is_none() || state["counter_window_version"] != 1;
    let gap = sampled.is_some_and(|t| !baseline && now - t > 3600.);
    let regression = sampled.is_some_and(|t| !baseline && now < t);
    let restarted = !baseline && state["counter_process_start"].as_f64() != Some(epoch);
    if gap {
        conditions.insert(
            "counter_sampling_gap".into(),
            "counter sampling gap exceeds one hour; unknown interval rebaselined".into(),
        );
    }
    if regression {
        conditions.insert(
            "counter_clock_regression".into(),
            "checker clock moved backwards; hourly estimates rebaselined".into(),
        );
    }
    if baseline || gap || regression {
        history.clear();
    } else {
        for buckets in history.values_mut() {
            buckets.retain(|row| row[0] > now - 3600.);
        }
    }
    let mut deltas = BTreeMap::new();
    for (key, value) in &current {
        let old = previous.get(key).and_then(Value::as_f64).unwrap_or(0.);
        let delta = if restarted || *value < old {
            *value
        } else {
            value - old
        };
        deltas.insert(key.clone(), delta);
        if !(baseline || gap || regression) && delta != 0. {
            let rows = history.entry(key.clone()).or_default();
            if let Some(last) = rows
                .last_mut()
                .filter(|row| (row[0] / 60.).floor() == (now / 60.).floor())
            {
                *last = [now, last[1] + delta];
            } else {
                rows.push([now, delta]);
            }
        }
    }
    if !(baseline || restarted || gap || regression) {
        let mut missing = false;
        for (key, value) in &previous {
            if !current.contains_key(key) {
                missing = true;
                current.insert(key.clone(), finite(value)?);
            }
        }
        if missing {
            conditions.insert(
                "metrics_missing_series".into(),
                "previously observed counter series disappeared without a process restart".into(),
            );
        }
    }
    history.retain(|_, rows| !rows.is_empty());
    c::ensure(
        current.len() <= 4096
            && history.len() <= 4096
            && history.values().all(|rows| rows.len() <= 61),
        "counter history exceeds bounded storage",
    )?;
    state["counters"] = json!(current);
    state["counter_deltas"] = json!(deltas);
    state["counter_windows"] = json!(history);
    state["counter_sampled_at"] = json!(now);
    state["counter_process_start"] = json!(epoch);
    state["counter_window_version"] = json!(1);
    Ok(history
        .iter()
        .map(|(key, rows)| (key.clone(), rows.iter().map(|r| r[1]).sum()))
        .collect())
}
pub async fn run(args: Args) -> Result {
    let report = report(args, None).await?;
    println!("{report}");
    if report["ok"] == true {
        Ok(())
    } else {
        Err(Box::new(crate::ops::ExitCode(1)))
    }
}
pub(super) async fn report(
    args: Args,
    mut local_notify: Option<&mut (dyn FnMut(&Value) -> Result + Send)>,
) -> Result<Value> {
    c::ensure(
        [
            args.backup_max_age,
            args.publish_lag_seconds,
            args.messages_per_day,
            args.messages_per_hour,
            args.message_limit_per_hour,
            args.sanitizer_rejections_per_hour,
            args.clock_skew_per_hour,
            args.review_reminder,
            args.repeat_seconds,
            args.reference_offline_seconds,
        ]
        .iter()
        .all(|n| *n >= 1)
            && args.day_alert_ratio > 0.
            && args.day_alert_ratio <= 1.,
        "invalid positive threshold",
    )?;
    let _lock = c::lock(&args.state_file)?;
    let now = c::now(args.now)?;
    let mut state = c::load(&args.state_file)?;
    let mut conditions = BTreeMap::new();
    let client = c::client(None, None)?;
    let status = if let Some(status) = args.health_status {
        Some(status)
    } else {
        match c::checked_url(&args.health_url, true, true, false) {
            Ok(url) => c::fetch(&client, url, reqwest::Method::GET, None)
                .await
                .ok()
                .map(|r| r.status),
            Err(_) => None,
        }
    };
    if status != Some(200) {
        conditions.insert(
            "database_unavailable".into(),
            "loopback health is not HTTP 200".into(),
        );
    }
    let sampled = async {
        let bytes = if let Some(path) = &args.metrics_file {
            fs::read(path)?
        } else {
            let response = c::fetch(
                &client,
                c::checked_url(&args.metrics_url, true, true, false)?,
                reqwest::Method::GET,
                None,
            )
            .await?;
            c::ensure(response.status == 200, "metrics failed")?;
            response.body
        };
        metrics(std::str::from_utf8(&bytes)?)
    }
    .await;
    let (samples, hourly) = match sampled.and_then(|samples| {
        windows(&samples, &mut state, now, &mut conditions).map(|history| (samples, history))
    }) {
        Ok(value) => value,
        Err(_) => {
            conditions.insert(
                "metrics_unavailable".into(),
                "metrics fetch, used samples or hourly state validation failed".into(),
            );
            (vec![], BTreeMap::new())
        }
    };
    let values: BTreeMap<_, _> = samples
        .iter()
        .filter(|s| s.labels.is_empty())
        .map(|s| (s.name.as_str(), s.value))
        .collect();
    let get = |name: &str| values.get(name).copied().unwrap_or(0.);
    if values
        .get("tolight_database_up")
        .is_some_and(|value| *value != 1.)
    {
        conditions.insert(
            "database_unavailable".into(),
            "database metric is down".into(),
        );
    }
    if get("tolight_publish_lag_events") > 0.
        && get("tolight_publish_lag_seconds") > args.publish_lag_seconds as f64
    {
        conditions.insert(
            "publish_lag".into(),
            "public tree remains behind the database".into(),
        );
    }
    if get("tolight_room_messages_last_hour") > args.messages_per_hour as f64 {
        conditions.insert(
            "hourly_volume".into(),
            "room messages exceed the hourly alert threshold; no automatic blocking".into(),
        );
    }
    let threshold = (args.messages_per_day as f64 * args.day_alert_ratio).ceil();
    let mut sanitation: BTreeMap<String, f64> = BTreeMap::new();
    let mut reference_observed = false;
    for sample in &samples {
        let agent = sample
            .labels
            .get("agent_id")
            .map(String::as_str)
            .unwrap_or("");
        if sample.name == "tolight_agent_attempts_last_day" && sample.value >= threshold {
            conditions.insert(
                format!("daily_attempts:{agent}"),
                format!("Agent {agent} has at least {threshold} attempts in 24 hours"),
            );
        }
        if args.reference_agent.as_deref() == Some(agent)
            && sample.name == "tolight_agent_last_seen_seconds"
        {
            reference_observed = true;
            if sample.value < 0. || sample.value > args.reference_offline_seconds as f64 {
                conditions.insert(
                    format!("reference_offline:{agent}"),
                    "reference Agent has not been observed recently".into(),
                );
            }
        }
        if sample.name == "tolight_agent_rejected_attempts_last_hour" {
            *sanitation.entry(agent.into()).or_default() += sample.value;
        }
    }
    for (key, count) in &hourly {
        if let Some(labels) = key.strip_prefix(COUNTER)
            && *count > args.message_limit_per_hour as f64
        {
            let labels: Value = serde_json::from_str(labels)?;
            let agent = labels["agent_id"].as_str().ok_or("invalid counter agent")?;
            conditions.insert(
                format!("message_limit_hourly:{agent}"),
                format!(
                    "Agent {agent} exceeds the estimated hourly message_limit rejection threshold"
                ),
            );
        }
    }
    for (agent, count) in sanitation {
        if count > args.sanitizer_rejections_per_hour as f64 {
            conditions.insert(
                format!("sanitizer_rejections_hourly:{agent}"),
                format!("Agent {agent} exceeds the exact hourly sanitation rejection threshold"),
            );
        }
    }
    if hourly.get(SKEW).copied().unwrap_or(0.) > args.clock_skew_per_hour as f64 {
        conditions.insert(
            "clock_skew_hourly".into(),
            "clock_skew increments exceed the estimated hourly threshold".into(),
        );
    }
    if let Some(agent) = &args.reference_agent
        && !samples.is_empty()
        && !reference_observed
    {
        conditions.insert(
            format!("reference_offline:{agent}"),
            "configured reference Agent has no last-seen metric sample".into(),
        );
    }
    let pending = get("tolight_pending_applications");
    let latest = get("tolight_latest_pending_timestamp_seconds");
    c::save(&args.state_file, &state)?;
    if !samples.is_empty() {
        if pending > 0. && latest > state["latest_pending"].as_f64().unwrap_or(0.) {
            let event = json!({"code":"new_application","at":now,"pending":pending,"detail":"Run tolight operator agent list --pending"});
            if let Some(send) = local_notify.as_mut() {
                send(&event)?;
            } else {
                c::notify(args.notify_command.as_deref(), &event)?;
            }
            state["latest_pending"] = json!(latest);
            c::save(&args.state_file, &state)?;
        }
        state["latest_pending"] = json!(latest);
    }
    if pending > 0. && get("tolight_oldest_pending_seconds") > args.review_reminder as f64 {
        conditions.insert(
            "review_waiting".into(),
            "oldest pending application exceeds the review reminder".into(),
        );
    }
    let free = if let Some(free) = args.free_bytes {
        free
    } else {
        let output = c::capture(Command::new("df").arg("-Pk").arg(&args.filesystem), 5, None)?;
        c::ensure(
            output.status.success(),
            "cannot inspect filesystem free space",
        )?;
        String::from_utf8(output.stdout)?
            .lines()
            .last()
            .ok_or("missing df output")?
            .split_whitespace()
            .nth(3)
            .ok_or("invalid df output")?
            .parse::<u64>()?
            .saturating_mul(1024)
    };
    if free < args.min_free_bytes {
        conditions.insert(
            "disk_low".into(),
            "filesystem free space is below the configured threshold".into(),
        );
    }
    let backups = crate::ops::storage::backup::successful_backup_times(&args.backup_dir, now);
    if backups
        .iter()
        .copied()
        .reduce(f64::max)
        .is_none_or(|stamp| now - stamp > args.backup_max_age as f64)
    {
        conditions.insert(
            "backup_stale".into(),
            "no successful logical or verified physical backup within the configured age".into(),
        );
    }
    c::notify_conditions_with(
        &mut state,
        &conditions,
        now,
        args.repeat_seconds as f64,
        &args.state_file,
        |event| {
            if let Some(send) = local_notify.as_mut() {
                send(event)
            } else {
                c::notify(args.notify_command.as_deref(), event)
            }
        },
    )?;
    c::save(&args.state_file, &state)?;
    Ok(
        json!({"ok":conditions.is_empty(),"conditions":conditions.keys().collect::<Vec<_>>(),"hourly_counter_estimates":hourly}),
    )
}
