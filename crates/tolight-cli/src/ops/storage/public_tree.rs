use super::{Result, common::*};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};
pub struct PublicTree {
    pub manifest: Value,
    pub agents: Value,
    pub events: BTreeMap<i64, Value>,
    pub chunks: BTreeMap<String, Value>,
}
fn keys(value: &Value, expected: &[&str]) -> bool {
    value.as_object().is_some_and(|o| {
        o.keys().map(String::as_str).collect::<BTreeSet<_>>() == expected.iter().copied().collect()
    })
}
impl PublicTree {
    pub fn load(root: &Path) -> Result<Self> {
        let data = root.join("data");
        let manifest = json_file(&data.join("manifest.json"))?;
        if !keys(
            &manifest,
            &[
                "schema",
                "oldest_seq",
                "newest_seq",
                "newest_event_id",
                "chunk_size",
                "paused",
                "demo",
            ],
        ) || manifest["schema"] != "tolight-manifest/1"
            || manifest["chunk_size"] != 200
        {
            return Err("invalid manifest allowlist/schema".into());
        }
        let head = manifest["newest_seq"]
            .as_i64()
            .filter(|n| *n >= 0)
            .ok_or("invalid manifest anchor")?;
        if manifest["oldest_seq"] != if head == 0 { 0 } else { 1 }
            || !manifest["paused"].is_boolean()
            || !manifest["demo"].is_boolean()
        {
            return Err("invalid manifest state".into());
        }
        let agents = json_file(&data.join("agents.json"))?;
        if !keys(&agents, &["schema", "agents"]) || agents["schema"] != "tolight-agents/1" {
            return Err("invalid public agents schema".into());
        }
        let mut seen = BTreeSet::new();
        let mut previous = None;
        for agent in agents["agents"].as_array().ok_or("public agents list")? {
            if !keys(agent, &["agent_id", "display_name", "bio", "joined_at"])
                || agent
                    .as_object()
                    .is_none_or(|o| o.values().any(|v| v.as_str().is_none_or(str::is_empty)))
                || !seen.insert(agent["agent_id"].as_str().ok_or("agent ID")?)
            {
                return Err("invalid public agent fields".into());
            }
            let key = (
                agent["joined_at"].as_str().unwrap(),
                agent["agent_id"].as_str().unwrap(),
            );
            if previous.is_some_and(|p| p > key) {
                return Err("public agents are not sorted".into());
            }
            previous = Some(key);
        }
        let mut events = BTreeMap::new();
        let mut chunks = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for first in (1..=head).step_by(200) {
            let name = format!("{first:012}-{:012}.json", first + 199);
            let path = data.join("chunks").join(&name);
            regular(&path)?;
            let chunk = json_file(&path)?;
            if !keys(&chunk, &["schema", "events"]) || chunk["schema"] != "tolight-chunk/1" {
                return Err("invalid chunk schema".into());
            }
            let values = chunk["events"].as_array().ok_or("chunk events")?;
            let last = (first + 199).min(head);
            if values.len() as i64 != last - first + 1 {
                return Err("invalid chunk sequence".into());
            }
            chunks.insert(
                name,
                json!({"sha256":sha256(&path)?,"first":first,"last":last}),
            );
            for (index, event) in values.iter().enumerate() {
                let seq = first + index as i64;
                let kind = event["kind"].as_str().ok_or("event kind")?;
                let extra: &[&str] = match kind {
                    "agent_message" => {
                        if event["retracted"] == true {
                            &["agent", "reply_to", "mentions"]
                        } else {
                            &["agent", "reply_to", "mentions", "text"]
                        }
                    }
                    "agent_joined" | "agent_left" => &["agent"],
                    "room_paused" | "room_resumed" => &[],
                    "event_retracted" => &["target", "reason_code"],
                    _ => return Err("unknown public event".into()),
                };
                let mut allowed = vec![
                    "schema",
                    "seq",
                    "event_id",
                    "kind",
                    "created_at",
                    "retracted",
                ];
                allowed.extend(extra);
                if !keys(event, &allowed)
                    || event["seq"] != seq
                    || event["schema"] != "tolight-event/1"
                    || !event["retracted"].is_boolean()
                {
                    return Err("invalid public event fields".into());
                }
                let id = event["event_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or("event id")?;
                if !ids.insert(id.to_string()) {
                    return Err("duplicate public event ID".into());
                }
                let stamp = event["created_at"].as_str().ok_or("event timestamp")?;
                time::PrimitiveDateTime::parse(
                    stamp,
                    &time::format_description::parse_borrowed::<1>(
                        "[year]-[month]-[day]T[hour]:[minute]:[second]Z",
                    )?,
                )?;
                if event.get("agent").is_some()
                    && !keys(&event["agent"], &["agent_id", "display_name"])
                {
                    return Err("invalid public event agent".into());
                }
                if kind == "agent_message" {
                    let mentions = event["mentions"].as_array().ok_or("mentions")?;
                    if mentions
                        .iter()
                        .any(|a| !keys(a, &["agent_id", "display_name"]))
                        || event["retracted"] == true && !mentions.is_empty()
                        || event["retracted"] != true && !event["text"].is_string()
                    {
                        return Err("invalid public message".into());
                    }
                }
                events.insert(seq, event.clone());
            }
        }
        let actual = if data.join("chunks").exists() {
            fs::read_dir(data.join("chunks"))?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|s| s.ends_with(".json"))
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        if actual != chunks.keys().cloned().collect() {
            return Err("unexpected or missing public chunks".into());
        }
        for event in events.values() {
            let target = if event["kind"] == "agent_message" {
                event.get("reply_to")
            } else {
                event.get("target")
            };
            if let Some(target) = target.filter(|v| !v.is_null()) {
                let seq = target["seq"].as_i64().ok_or("target seq")?;
                if !keys(target, &["seq", "event_id"])
                    || seq >= event["seq"].as_i64().unwrap()
                    || events.get(&seq).map(|e| &e["event_id"]) != Some(&target["event_id"])
                    || event["kind"] == "event_retracted" && events[&seq]["retracted"] != true
                {
                    return Err("broken public event target".into());
                }
            }
        }
        if events
            .get(&head)
            .map(|e| e["event_id"].clone())
            .unwrap_or(Value::Null)
            != manifest["newest_event_id"]
        {
            return Err("manifest does not match last event".into());
        }
        Ok(Self {
            manifest,
            agents,
            events,
            chunks,
        })
    }
}
fn tombstone(event: &mut Value) {
    if let Some(object) = event.as_object_mut() {
        object.remove("text");
        object.insert("mentions".into(), json!([]));
        object.insert("retracted".into(), true.into());
    }
}
pub fn compare(source: &PublicTree, restored: &PublicTree, plan: &Value) -> Result<Value> {
    let anchor = plan["target_anchor"]["seq"]
        .as_i64()
        .ok_or("backup anchor")?;
    if restored.manifest["newest_seq"].as_i64().unwrap() < anchor
        || source.manifest["newest_seq"].as_i64().unwrap() < anchor
    {
        return Err("public timeline is older than backup anchor".into());
    }
    let mut exceptions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for item in plan["retractions"].as_array().ok_or("retractions plan")? {
        if let Some(seq) = item["seq"]
            .as_i64()
            .filter(|_| item["already_retracted"] != true)
        {
            let first = (seq - 1) / 200 * 200 + 1;
            let id = item["event_id"].as_str().ok_or("retracted ID")?;
            exceptions
                .entry(format!("{first:012}-{:012}.json", first + 199))
                .or_default()
                .push(id.into());
            let event = restored
                .events
                .get(&seq)
                .ok_or("restored retraction target")?;
            if event["event_id"] != id || event["retracted"] != true || event.get("text").is_some()
            {
                return Err("restored retraction did not remove original text".into());
            }
        }
    }
    let mut full = vec![];
    let mut differences = vec![];
    for (name, chunk) in &restored.chunks {
        let other = source.chunks.get(name);
        let equal = other.is_some_and(|o| o["sha256"] == chunk["sha256"]);
        let first = chunk["first"].as_i64().unwrap();
        if first + 199 <= anchor {
            if other.is_none() || !equal && !exceptions.contains_key(name) {
                return Err(
                    "a full backup chunk changed outside the retraction exception list".into(),
                );
            }
            if !equal {
                for seq in first..first + 200 {
                    let mut left = source.events.get(&seq).ok_or("source event")?.clone();
                    let mut right = restored.events.get(&seq).ok_or("restored event")?.clone();
                    if exceptions
                        .get(name)
                        .is_some_and(|ids| ids.iter().any(|id| left["event_id"] == *id))
                    {
                        tombstone(&mut left);
                        tombstone(&mut right)
                    }
                    if left != right {
                        return Err(
                            "unrelated event changed inside a retraction exception chunk".into(),
                        );
                    }
                }
            }
            full.push(
                json!({"file":name,"equal":equal,"retraction_exception":exceptions.get(name)}),
            );
        } else if !equal {
            differences
                .push(json!({"file":name,"kind":"tail_or_new","source_present":other.is_some()}));
        }
    }
    for name in source
        .chunks
        .keys()
        .filter(|name| !restored.chunks.contains_key(*name))
    {
        differences.push(json!({"file":name,"kind":"source_only"}));
    }
    let changed: Vec<_> = source
        .manifest
        .as_object()
        .unwrap()
        .keys()
        .filter(|key| source.manifest[*key] != restored.manifest[*key])
        .collect();
    let agent_ids = |tree: &PublicTree| {
        tree.agents["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["agent_id"].clone())
            .collect::<Vec<_>>()
    };
    Ok(
        json!({"backup_anchor_seq":anchor,"full_chunks":full,"chunk_differences":differences,"manifest_equal":source.manifest==restored.manifest,"manifest_differences":changed,"agents_equal":source.agents==restored.agents,"source_agent_ids":agent_ids(source),"restored_agent_ids":agent_ids(restored),"source_head":source.manifest["newest_seq"],"restored_head":restored.manifest["newest_seq"],"all_public_schemas_valid":true}),
    )
}
pub fn verify_prefix(published: &PublicTree, reference: &PublicTree) -> Result<()> {
    for (seq, event) in &published.events {
        let other = reference
            .events
            .get(seq)
            .ok_or("published prefix event missing from database")?;
        let mut left = event.clone();
        if other["kind"] == "agent_message" && other["retracted"] == true {
            tombstone(&mut left)
        }
        if left != *other {
            return Err(
                "unrelated event differs between the published prefix and current database".into(),
            );
        }
    }
    Ok(())
}
