use super::{Result, common as c};
use clap::Args as ClapArgs;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, process::Command};
#[derive(ClapArgs)]
#[command(args_override_self = true)]
pub struct Args {
    #[arg(long)]
    pub static_origin: String,
    #[arg(long)]
    pub participant_origin: String,
    #[arg(long)]
    pub reference_bin: PathBuf,
    #[arg(long)]
    pub reference_config: PathBuf,
    #[arg(long, default_value = ".local/probe-state.json")]
    pub state_file: PathBuf,
    #[arg(long)]
    pub notify_command: Option<String>,
    #[arg(long)]
    pub allow_http: bool,
    #[arg(long)]
    pub ca_file: Option<PathBuf>,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    pub max_clock_skew: u64,
    #[arg(long, default_value_t = 21600, value_parser = clap::value_parser!(u64).range(1..))]
    pub repeat_seconds: u64,
    #[arg(long)]
    pub now: Option<f64>,
}
pub async fn run(args: Args) -> Result {
    c::ensure(
        args.max_clock_skew >= 1 && args.repeat_seconds >= 1,
        "thresholds must be positive",
    )?;
    let _lock = c::lock(&args.state_file)?;
    let now = c::now(args.now)?;
    let static_url = c::checked_url(&args.static_origin, args.allow_http, false, true)?;
    let participant = c::checked_url(&args.participant_origin, args.allow_http, false, true)?;
    let client = c::client(args.ca_file.as_deref(), None)?;
    let mut problems = BTreeMap::new();
    match c::fetch(
        &client,
        static_url.join("data/manifest.json")?,
        reqwest::Method::GET,
        None,
    )
    .await
    {
        Ok(response)
            if response.status == 200
                && serde_json::from_slice::<Value>(&response.body)
                    .ok()
                    .is_some_and(|v| v["schema"] == "tolight-manifest/1") => {}
        _ => {
            problems.insert(
                "static_failed".into(),
                "manifest fetch or parsing failed".into(),
            );
        }
    }
    let mut date_ok = false;
    match c::fetch(
        &client,
        participant.join("participant/v1/self")?,
        reqwest::Method::GET,
        None,
    )
    .await
    {
        Ok(response) => {
            if response.status != 401 {
                problems.insert(
                    "participant_liveness_failed".into(),
                    "unsigned participant request did not return 401".into(),
                );
            }
            match response.headers.get("date") {
                None => {
                    problems.insert(
                        "missing_date".into(),
                        "participant Date header is missing".into(),
                    );
                }
                Some(value) => match value.to_str().ok().and_then(|value| {
                    time::OffsetDateTime::parse(
                        value,
                        &time::format_description::well_known::Rfc2822,
                    )
                    .ok()
                }) {
                    Some(date) => {
                        let offset = date.unix_timestamp() as f64 - now;
                        if offset.abs() > args.max_clock_skew as f64 {
                            problems.insert(
                                "clock_skew".into(),
                                format!("participant clock differs by {offset:.0} seconds"),
                            );
                        } else {
                            date_ok = true;
                        }
                    }
                    None => {
                        problems.insert(
                            "invalid_date".into(),
                            "participant Date header is malformed".into(),
                        );
                    }
                },
            }
        }
        Err(_) => {
            problems.insert(
                "participant_liveness_failed".into(),
                "participant liveness request failed".into(),
            );
        }
    }
    match c::capture(
        Command::new(&args.reference_bin)
            .arg("--config")
            .arg(&args.reference_config)
            .arg("status"),
        20,
        None,
    ) {
        Ok(output) if output.status.code() == Some(5) => {}
        Ok(output) => {
            problems.insert("signed_probe_failed".into(),if date_ok{format!("signed status failed; check host or request configuration (exit {:?} does not identify a specific HTTP error)",output.status.code())}else{"signed status was not unauthorized/exit 5; check clock first, then host or request configuration".into()});
        }
        Err(_) => {
            problems.insert(
                "signed_probe_failed".into(),
                "signed status could not execute or timed out".into(),
            );
        }
    }
    let mut state = c::load(&args.state_file)?;
    c::notify_conditions(
        &mut state,
        &problems,
        now,
        args.repeat_seconds as f64,
        args.notify_command.as_deref(),
        &args.state_file,
    )?;
    c::save(&args.state_file, &state)?;
    println!(
        "{}",
        json!({"ok":problems.is_empty(),"checks":["manifest","unsigned_401","signed_exit_5","date"],"problems":problems.keys().collect::<Vec<_>>()})
    );
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Box::new(crate::ops::ExitCode(1)))
    }
}
