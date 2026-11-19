//! Read-only checks for a previously verified owned local environment.
use super::{Result, common as c, lan};

use reqwest::{Client, Method, Url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};

const HTTP_CHECKS: [&str; 7] = [
    "health",
    "participant_authentication",
    "index",
    "javascript",
    "stylesheet",
    "manifest",
    "agents",
];
const LAN_CHECKS: [&str; 7] = [
    "lan_index",
    "lan_manifest",
    "lan_agents",
    "lan_participant_authentication",
    "lan_onboarding",
    "lan_certificate",
    "lan_plaintext_api",
];

fn keys(value: &Value, expected: &[&str]) -> bool {
    value.as_object().is_some_and(|fields| {
        fields.keys().map(String::as_str).collect::<BTreeSet<_>>()
            == expected.iter().copied().collect()
    })
}

fn manifest(value: &Value) -> bool {
    if !keys(
        value,
        &[
            "schema",
            "oldest_seq",
            "newest_seq",
            "newest_event_id",
            "chunk_size",
            "paused",
            "demo",
        ],
    ) || value["schema"] != "tolight-manifest/1"
        || value["chunk_size"] != 200
        || !value["paused"].is_boolean()
        || !value["demo"].is_boolean()
    {
        return false;
    }
    match value["newest_seq"].as_i64() {
        Some(0) => value["oldest_seq"] == 0 && value["newest_event_id"].is_null(),
        Some(head) if head > 0 => {
            value["oldest_seq"] == 1
                && value["newest_event_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
        }
        _ => false,
    }
}

fn agents(value: &Value) -> bool {
    if !keys(value, &["schema", "agents"]) || value["schema"] != "tolight-agents/1" {
        return false;
    }
    let Some(agents) = value["agents"].as_array() else {
        return false;
    };
    let mut seen = BTreeSet::new();
    let mut previous = None;
    for agent in agents {
        if !keys(agent, &["agent_id", "display_name", "bio", "joined_at"]) {
            return false;
        }
        let Some(fields) = agent.as_object() else {
            return false;
        };
        if fields
            .values()
            .any(|value| value.as_str().is_none_or(str::is_empty))
        {
            return false;
        }
        let id = agent["agent_id"].as_str().expect("checked public string");
        let joined = agent["joined_at"].as_str().expect("checked public string");
        if !seen.insert(id)
            || !tolight_core::datetime::parse_rfc3339(joined)
                .is_ok_and(|date| tolight_core::datetime::utc_seconds(date) == joined)
        {
            return false;
        }
        let ordering = (joined, id);
        if previous.is_some_and(|last| last > ordering) {
            return false;
        }
        previous = Some(ordering);
    }
    true
}

fn origin(snapshot: &Value, name: &str) -> Result<Url> {
    let value = snapshot["origins"][name]
        .as_str()
        .ok_or("missing owned Origin")?;
    let url = c::checked_url(value, true, true, true)?;
    c::ensure(
        url.scheme() == "http",
        "local checks require an owned HTTP Origin",
    )?;
    Ok(url)
}

enum Body {
    Nonempty,
    Index,
    Health,
    Authentication,
    Manifest,
    Agents,
    CertificateSha256(String),
}

async fn get(client: &Client, url: Url, expected: u16, types: &[&str], body: Body) -> Value {
    let response = match c::fetch(client, url.clone(), Method::GET, None).await {
        Ok(response) => response,
        Err(_) => return json!({"ok":false,"code":"request_failed"}),
    };
    let failure = |code| json!({"ok":false,"code":code,"http_status":response.status});
    if response.status != expected {
        return failure("unexpected_status");
    }
    let mime = response
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !types.is_empty()
        && !mime.is_some_and(|mime| {
            types
                .iter()
                .any(|expected| mime.eq_ignore_ascii_case(expected))
        })
    {
        return failure("content_type");
    }
    let valid = match body {
        Body::Nonempty => !response.body.is_empty(),
        Body::Index => {
            let Ok(index) = std::str::from_utf8(&response.body) else {
                return failure("invalid_body");
            };
            if index.is_empty() {
                return failure("invalid_body");
            }
            for name in tolight_coordinator::publisher::referenced_brand_assets(index) {
                let Ok(asset_url) = url.join(name) else {
                    return failure("brand_asset_invalid");
                };
                let Ok(asset) =
                    c::fetch_limit(client, asset_url, Method::GET, None, 4 * 1024 * 1024).await
                else {
                    return failure("brand_asset_invalid");
                };
                if asset.status != 200
                    || asset
                        .headers
                        .get("content-type")
                        .and_then(|value| value.to_str().ok())
                        != Some("image/jpeg")
                    || !tolight_coordinator::publisher::valid_brand_jpeg(&asset.body)
                {
                    return failure("brand_asset_invalid");
                }
            }
            true
        }
        Body::CertificateSha256(expected) => {
            format!("{:x}", Sha256::digest(&response.body)) == expected
        }
        kind => {
            let Ok(value) = serde_json::from_slice::<Value>(&response.body) else {
                return failure("invalid_json");
            };
            match kind {
                Body::Health => value["status"] == "ok",
                Body::Authentication => value["error"]["code"] == "unauthorized",
                Body::Manifest => manifest(&value),
                Body::Agents => agents(&value),
                Body::Nonempty | Body::Index | Body::CertificateSha256(_) => unreachable!(),
            }
        }
    };
    if !valid {
        return failure("invalid_body");
    }
    json!({"ok":true,"code":"passed","http_status":response.status})
}

async fn check_lan(snapshot: &Value, report: &mut Value) -> Result {
    // The caller already verified this snapshot's marker, managed configuration
    // and supervisor. Derive addresses from its validated private IPv4/ports.
    let settings: lan::Lan = serde_json::from_value(json!({
        "ip":snapshot["lan"]["ip"],
        "web_port":snapshot["lan"]["web_port"],
        "api_port":snapshot["lan"]["api_port"]
    }))?;
    settings.validate()?;
    let directory = Path::new(
        snapshot["directory"]
            .as_str()
            .ok_or("missing owned directory")?,
    );
    let ca = lan::ca_file(directory);
    c::regular(&ca, true)?;
    c::ensure(
        std::fs::metadata(&ca)?.len() <= 256 * 1024,
        "local CA too large",
    )?;
    let hash = format!("{:x}", Sha256::digest(std::fs::read(&ca)?));
    let tls = c::client(Some(&ca), Some(reqwest::tls::Version::TLS_1_3))?;
    let plain = c::client(None, None)?;
    let web = Url::parse(&format!("http://{}:{}/", settings.ip, settings.web_port))?;
    let api = Url::parse(&format!("https://{}:{}/", settings.ip, settings.api_port))?;
    let (index, manifest, agents, authentication, guide, certificate, no_plaintext_api) = tokio::join!(
        get(&plain, web.clone(), 200, &["text/html"], Body::Index),
        get(
            &plain,
            web.join("data/manifest.json")?,
            200,
            &["application/json"],
            Body::Manifest
        ),
        get(
            &plain,
            web.join("data/agents.json")?,
            200,
            &["application/json"],
            Body::Agents
        ),
        get(
            &tls,
            api.join("participant/v1/self")?,
            401,
            &["application/json"],
            Body::Authentication
        ),
        get(
            &tls,
            api.join("lan-join.md")?,
            200,
            &["text/markdown"],
            Body::Nonempty
        ),
        get(
            &plain,
            web.join("local-ca.pem")?,
            200,
            &[],
            Body::CertificateSha256(hash)
        ),
        get(
            &plain,
            web.join("participant/v1/self")?,
            404,
            &[],
            Body::Nonempty
        ),
    );
    for (name, result) in LAN_CHECKS.into_iter().zip([
        index,
        manifest,
        agents,
        authentication,
        guide,
        certificate,
        no_plaintext_api,
    ]) {
        report["checks"][name] = result;
    }
    Ok(())
}

pub(super) async fn run(snapshot: Value) -> Result {
    let mut report = json!({"schema":"tolight-local-check/1","scope":"local","environment_id":snapshot["environment_id"],"mode":snapshot["mode"],"ok":false,"checks":{}});
    for name in HTTP_CHECKS {
        report["checks"][name] = json!({"ok":false,"code":"skipped"});
    }
    let has_lan = snapshot.get("lan").is_some_and(|value| !value.is_null());
    if has_lan {
        for name in LAN_CHECKS {
            report["checks"][name] = json!({"ok":false,"code":"skipped"});
        }
    }
    let code = if snapshot["initialized"] != true {
        "uninitialized"
    } else if snapshot["mode"] == "maintenance" {
        "maintenance"
    } else if snapshot["mode"] == "stopped"
        && snapshot["running"] != true
        && snapshot["runtime_error"].is_null()
    {
        "not_running"
    } else if snapshot["runtime_valid"] != true || snapshot["postgres"]["running"] != true {
        "unverified_environment"
    } else if snapshot["running"] != true || snapshot["mode"] != "normal" {
        "not_running"
    } else {
        "passed"
    };
    report["checks"]["environment"] = json!({"ok":code=="passed","code":code});
    if code == "passed" {
        match (
            origin(&snapshot, "ops"),
            origin(&snapshot, "api"),
            origin(&snapshot, "web"),
        ) {
            (Ok(ops), Ok(api), Ok(web)) => {
                let client = c::client(None, None)?;
                let (health, auth, index, javascript, stylesheet, manifest, agents) = tokio::join!(
                    get(
                        &client,
                        ops.join("healthz")?,
                        200,
                        &["application/json"],
                        Body::Health
                    ),
                    get(
                        &client,
                        api.join("participant/v1/self")?,
                        401,
                        &["application/json"],
                        Body::Authentication
                    ),
                    get(&client, web.clone(), 200, &["text/html"], Body::Index),
                    get(
                        &client,
                        web.join("app.js")?,
                        200,
                        &["text/javascript", "application/javascript"],
                        Body::Nonempty
                    ),
                    get(
                        &client,
                        web.join("style.css")?,
                        200,
                        &["text/css"],
                        Body::Nonempty
                    ),
                    get(
                        &client,
                        web.join("data/manifest.json")?,
                        200,
                        &["application/json"],
                        Body::Manifest
                    ),
                    get(
                        &client,
                        web.join("data/agents.json")?,
                        200,
                        &["application/json"],
                        Body::Agents
                    ),
                );
                for (name, result) in HTTP_CHECKS.into_iter().zip([
                    health, auth, index, javascript, stylesheet, manifest, agents,
                ]) {
                    report["checks"][name] = result;
                }
            }
            _ => report["checks"]["environment"] = json!({"ok":false,"code":"invalid_origins"}),
        }
        if has_lan
            && report["checks"]["environment"]["ok"] == true
            && check_lan(&snapshot, &mut report).await.is_err()
        {
            for name in LAN_CHECKS {
                report["checks"][name] = json!({"ok":false,"code":"configuration_error"});
            }
        }
    }
    report["ok"] = json!(
        report["checks"]
            .as_object()
            .expect("check object")
            .values()
            .all(|check| check["ok"] == true)
    );
    println!("{}", serde_json::to_string(&report)?);
    if report["ok"] == true {
        Ok(())
    } else {
        Err(Box::new(crate::ops::ExitCode(1)))
    }
}
