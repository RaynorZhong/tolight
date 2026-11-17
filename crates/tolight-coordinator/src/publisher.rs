//! Deterministic public projections and a local-filesystem publication barrier.
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;
use tolight_coordinator_store::{PublicSnapshot, Store};
use tolight_core::config::Config;

const CHUNK_SIZE: i64 = 200;
#[path = "publisher_assets.rs"]
mod assets;
pub use assets::{BRAND_ASSETS, SecurityAssetMetadata, referenced_brand_assets, valid_brand_jpeg};

/// Validate the optional public security.txt and its repository SECURITY.md policy.
/// No endpoint is fabricated when the source has no authorized reporting file.
pub fn validate_security_assets(
    source: &Path,
) -> Result<Option<SecurityAssetMetadata>, PublishError> {
    assets::security_metadata(source)
}

#[derive(Debug, Error)]
pub enum PublishError {
    #[error("另一个 Publisher 正持有公共树文件锁")]
    Busy,
    #[error(
        "公共树与数据库的时间线不符；请先按运维手册核对恢复后的管控状态，再执行 publish --rebuild"
    )]
    TimelineMismatch,
    #[error("公共数据无效：{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("数据库快照失败：{0}")]
    Store(String),
    #[error("发布任务失败：{0}")]
    Task(String),
}

struct TreeLock {
    root: PathBuf,
    _file: File,
}

impl TreeLock {
    fn acquire(root: &Path) -> Result<Self, PublishError> {
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        let parent = root
            .parent()
            .ok_or_else(|| PublishError::Invalid("公共目录不能是文件系统根".into()))?;
        let path = parent.join(".tolight-publisher.lock");
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(PublishError::Invalid(
                "Publisher 锁文件不能是符号链接".into(),
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(0o600);
            let file = options.open(path)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.try_lock().map_err(|_| PublishError::Busy)?;
            Ok(Self { root, _file: file })
        }
        #[cfg(not(unix))]
        {
            let file = options.open(path)?;
            file.try_lock().map_err(|_| PublishError::Busy)?;
            Ok(Self { root, _file: file })
        }
    }
}

pub async fn publish(store: &Store, config: &Config, rebuild: bool) -> Result<(), PublishError> {
    store
        .check_schema()
        .await
        .map_err(|e| PublishError::Store(e.to_string()))?;
    let guard = TreeLock::acquire(&config.public.dir)?;
    let start = if rebuild {
        0
    } else {
        manifest_sequence(&guard.root)?
    };
    let snapshot = store
        .public_snapshot_since(start, CHUNK_SIZE)
        .await
        .map_err(|e| PublishError::Store(e.to_string()))?;
    let demo = config.public.demo_banner;
    // The blocking task owns the FD. Dropping/cancelling this future cannot release
    // the lock while a detached filesystem task is still writing a .tmp file.
    tokio::task::spawn_blocking(move || write_tree(&guard, &snapshot, demo, rebuild, |_| Ok(())))
        .await
        .map_err(|e| PublishError::Task(e.to_string()))?
}

pub async fn validate_manifest(store: &Store, config: &Config) -> Result<(), PublishError> {
    let Some(manifest) = read_manifest(&config.public.dir)? else {
        return Ok(());
    };
    let start = manifest["newest_seq"].as_i64().expect("validated sequence");
    let snapshot = store
        .public_snapshot_since(start, CHUNK_SIZE)
        .await
        .map_err(|e| PublishError::Store(e.to_string()))?;
    check_anchor(&config.public.dir, &snapshot).map(|_| ())
}

pub async fn install_assets(config: &Config, source: &Path) -> Result<(), PublishError> {
    install_assets_bundle(config, source, None, None).await
}

pub async fn install_assets_bundle(
    config: &Config,
    source: &Path,
    docs: Option<&Path>,
    examples: Option<&Path>,
) -> Result<(), PublishError> {
    if docs.is_some() != examples.is_some() {
        return Err(PublishError::Invalid(
            "--docs 与 --examples 必须同时提供".into(),
        ));
    }
    let root = config.public.dir.clone();
    let source = source.to_owned();
    let docs = docs.map(Path::to_owned);
    let examples = examples.map(Path::to_owned);
    tokio::task::spawn_blocking(move || {
        assets::install(&root, &source, docs.as_deref(), examples.as_deref())
    })
    .await
    .map_err(|e| PublishError::Task(e.to_string()))?
}

fn read_manifest(root: &Path) -> Result<Option<Value>, PublishError> {
    let path = root.join("data/manifest.json");
    let bytes = match fs::read(path) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let manifest: Value = serde_json::from_slice(&bytes)?;
    manifest["newest_seq"]
        .as_i64()
        .filter(|v| *v >= 0)
        .ok_or_else(|| PublishError::Invalid("manifest newest_seq".into()))?;
    if manifest["schema"] != "tolight-manifest/1" {
        return Err(PublishError::Invalid("manifest schema".into()));
    }
    Ok(Some(manifest))
}

fn manifest_sequence(root: &Path) -> Result<i64, PublishError> {
    Ok(read_manifest(root)?
        .map(|manifest| manifest["newest_seq"].as_i64().expect("validated sequence"))
        .unwrap_or(0))
}

fn check_anchor(root: &Path, snapshot: &PublicSnapshot) -> Result<i64, PublishError> {
    let Some(manifest) = read_manifest(root)? else {
        return Ok(0);
    };
    let seq = manifest["newest_seq"].as_i64().expect("validated sequence");
    if seq > 0
        && snapshot
            .events
            .iter()
            .find(|e| e["seq"] == seq)
            .map(|e| &e["event_id"])
            != Some(&manifest["newest_event_id"])
    {
        return Err(PublishError::TimelineMismatch);
    }
    if seq == 0 && !manifest["newest_event_id"].is_null() {
        return Err(PublishError::TimelineMismatch);
    }
    Ok(seq)
}

fn public_event(event: &Value) -> Result<Value, PublishError> {
    let kind = event["kind"]
        .as_str()
        .ok_or_else(|| PublishError::Invalid("event kind".into()))?;
    let mut keys = vec![
        "schema",
        "seq",
        "event_id",
        "kind",
        "created_at",
        "retracted",
    ];
    keys.extend(match kind {
        "agent_message" => vec!["agent", "reply_to", "mentions"],
        "agent_joined" | "agent_left" => vec!["agent"],
        "room_paused" | "room_resumed" => vec![],
        "event_retracted" => vec!["target", "reason_code"],
        _ => return Err(PublishError::Invalid(format!("unknown kind {kind}"))),
    });
    if kind == "agent_message" && event["retracted"] == false {
        keys.push("text");
    }
    let mut output = serde_json::Map::new();
    for key in keys {
        let value = event
            .get(key)
            .ok_or_else(|| PublishError::Invalid(format!("event missing {key}")))?;
        output.insert(key.into(), value.clone());
    }
    // Nested projections are explicit too: a future storage field must not leak.
    if let Some(agent) = output.get_mut("agent") {
        *agent = identity(agent)?;
    }
    if let Some(mentions) = output.get_mut("mentions") {
        let values = mentions
            .as_array()
            .ok_or_else(|| PublishError::Invalid("mentions".into()))?;
        *mentions = Value::Array(values.iter().map(identity).collect::<Result<_, _>>()?);
    }
    for name in ["reply_to", "target"] {
        if let Some(reference) = output.get_mut(name).filter(|v| !v.is_null()) {
            *reference = json!({"seq":reference["seq"],"event_id":reference["event_id"]});
        }
    }
    if output.get("retracted") == Some(&Value::Bool(true)) && kind == "agent_message" {
        output.insert("mentions".into(), json!([]));
    }
    Ok(Value::Object(output))
}

fn identity(agent: &Value) -> Result<Value, PublishError> {
    if !agent["agent_id"].is_string() || !agent["display_name"].is_string() {
        return Err(PublishError::Invalid("agent identity".into()));
    }
    Ok(json!({"agent_id":agent["agent_id"],"display_name":agent["display_name"]}))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Stage {
    Chunks,
    Agents,
    Manifest,
}

fn write_tree(
    guard: &TreeLock,
    snapshot: &PublicSnapshot,
    demo: bool,
    rebuild: bool,
    mut after: impl FnMut(Stage) -> Result<(), PublishError>,
) -> Result<(), PublishError> {
    let start = if rebuild {
        0
    } else {
        check_anchor(&guard.root, snapshot)?
    };
    let mut previous_seq = 0;
    let mut expected_new = start + 1;
    let mut groups = BTreeMap::<i64, Vec<&Value>>::new();
    for event in &snapshot.events {
        let seq = event["seq"]
            .as_i64()
            .filter(|seq| *seq > previous_seq && *seq <= snapshot.last_seq)
            .ok_or_else(|| PublishError::Invalid("数据库公共事件序号无效".into()))?;
        if seq > start {
            if seq != expected_new {
                return Err(PublishError::Invalid("数据库新增公共事件序号不连续".into()));
            }
            expected_new += 1;
        }
        groups
            .entry((seq - 1) / CHUNK_SIZE * CHUNK_SIZE + 1)
            .or_default()
            .push(event);
        previous_seq = seq;
    }
    if expected_new != snapshot.last_seq + 1 {
        return Err(PublishError::Invalid("数据库公共事件序号不连续".into()));
    }
    let data = guard.root.join("data");
    let chunks = data.join("chunks");
    fs::create_dir_all(&chunks)?;
    cleanup_temps(&data)?;
    let result = (|| {
        let mut affected = BTreeSet::new();
        for event in snapshot
            .events
            .iter()
            .filter(|event| event["seq"].as_i64().unwrap_or(0) > start)
        {
            if event["kind"] == "event_retracted" {
                let seq = event["target"]["seq"]
                    .as_i64()
                    .ok_or_else(|| PublishError::Invalid("retraction target".into()))?;
                affected.insert((seq - 1) / CHUNK_SIZE * CHUNK_SIZE + 1);
            }
        }
        if affected.iter().any(|first| !groups.contains_key(first)) {
            return Err(PublishError::Invalid("数据库缺少撤回目标分片".into()));
        }
        let mut expected = BTreeSet::new();
        for (first, group) in groups {
            let last = first + CHUNK_SIZE - 1;
            let name = format!("{first:012}-{last:012}.json");
            expected.insert(name.clone());
            if rebuild
                || affected.contains(&first)
                || group
                    .iter()
                    .any(|event| event["seq"].as_i64().unwrap_or(0) > start)
            {
                let expected_last = last.min(snapshot.last_seq);
                if group.len() as i64 != expected_last - first + 1
                    || group
                        .iter()
                        .enumerate()
                        .any(|(index, event)| event["seq"] != first + index as i64)
                {
                    return Err(PublishError::Invalid("数据库发布分片不完整".into()));
                }
                let events = group
                    .iter()
                    .map(|event| public_event(event))
                    .collect::<Result<Vec<_>, _>>()?;
                atomic_json(
                    &chunks.join(name),
                    &json!({"schema":"tolight-chunk/1","events":events}),
                )?;
            }
        }
        if rebuild {
            for entry in fs::read_dir(&chunks)? {
                let entry = entry?;
                if !expected.contains(&entry.file_name().to_string_lossy().into_owned())
                    && entry.file_type()?.is_file()
                {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        sync_directory(&chunks)?;
        after(Stage::Chunks)?;
        let mut agents = snapshot.agents.iter().map(|a| json!({"agent_id":a["agent_id"],"display_name":a["display_name"],"bio":a["bio"],"joined_at":a["joined_at"]})).collect::<Vec<_>>();
        agents.sort_by(|a, b| {
            a["joined_at"]
                .as_str()
                .cmp(&b["joined_at"].as_str())
                .then_with(|| a["agent_id"].as_str().cmp(&b["agent_id"].as_str()))
        });
        atomic_json(
            &data.join("agents.json"),
            &json!({"schema":"tolight-agents/1","agents":agents}),
        )?;
        after(Stage::Agents)?;
        let head_id = snapshot
            .events
            .last()
            .map(|e| e["event_id"].clone())
            .unwrap_or(Value::Null);
        atomic_json(
            &data.join("manifest.json"),
            &json!({"schema":"tolight-manifest/1","oldest_seq":if snapshot.last_seq > 0 {1} else {0},"newest_seq":snapshot.last_seq,"newest_event_id":head_id,"chunk_size":CHUNK_SIZE,"paused":snapshot.paused,"demo":demo}),
        )?;
        sync_directory(&data)?;
        after(Stage::Manifest)?;
        Ok(())
    })();
    // The FD remains held during cleanup even when a filesystem step failed.
    let cleanup = cleanup_temps(&data);
    result.and(cleanup)
}

fn atomic_json(path: &Path, value: &Value) -> Result<(), PublishError> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    atomic_write(path, &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), PublishError> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    let tmp = PathBuf::from(name);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn sync_directory(path: &Path) -> Result<(), PublishError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn cleanup_temps(data: &Path) -> Result<(), PublishError> {
    for directory in [data.to_path_buf(), data.join("chunks")] {
        if !directory.exists() {
            continue;
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_file() && entry.file_name().to_string_lossy().ends_with(".tmp")
            {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        time::{Duration, Instant},
    };

    fn snapshot(count: i64) -> PublicSnapshot {
        PublicSnapshot {
            events: (1..=count).map(|seq| json!({"schema":"tolight-event/1","seq":seq,"event_id":format!("E-{seq}"),"kind":"agent_message","created_at":"2026-11-14T00:00:00Z","retracted":false,"agent":{"agent_id":"alice","display_name":"Alice"},"text":format!("消息 {seq}"),"reply_to":null,"mentions":[]})).collect(),
            agents: vec![json!({"agent_id":"alice","display_name":"Alice","bio":"脚本 Agent","joined_at":"2026-11-14T00:00:00Z"})],
            last_seq: count, paused: false,
        }
    }
    fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        for dir in [root.join("data"), root.join("data/chunks")] {
            if dir.exists() {
                for entry in fs::read_dir(dir).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_file() {
                        files.insert(
                            entry.path().strip_prefix(root).unwrap().to_owned(),
                            fs::read(entry.path()).unwrap(),
                        );
                    }
                }
            }
        }
        files
    }
    fn write(guard: &TreeLock, data: &PublicSnapshot, rebuild: bool) {
        write_tree(guard, data, true, rebuild, |_| Ok(())).unwrap();
    }

    #[test]
    fn chunk_boundaries_empty_manifest_and_deterministic_rebuild() {
        for count in [0, 1, 199, 200, 201, 400, 401] {
            let tmp = tempfile::tempdir().unwrap();
            let guard = TreeLock::acquire(&tmp.path().join("public")).unwrap();
            let data = snapshot(count);
            write(&guard, &data, false);
            let original = tree(&guard.root);
            let manifest: Value =
                serde_json::from_slice(&original[Path::new("data/manifest.json")]).unwrap();
            assert_eq!(manifest["newest_seq"], count);
            assert_eq!(
                manifest["newest_event_id"],
                data.events
                    .last()
                    .map(|e| e["event_id"].clone())
                    .unwrap_or(Value::Null)
            );
            assert_eq!(manifest["oldest_seq"], if count == 0 { 0 } else { 1 });
            assert_eq!(original.len(), 2 + ((count + 199) / 200) as usize);
            for (index, group) in data.events.chunks(200).enumerate() {
                let first = index * 200 + 1;
                let last = first + 199;
                let chunk: Value = serde_json::from_slice(
                    &original[&PathBuf::from(format!("data/chunks/{first:012}-{last:012}.json"))],
                )
                .unwrap();
                assert_eq!(chunk["schema"], "tolight-chunk/1");
                assert_eq!(chunk["events"].as_array().unwrap(), group);
            }
            fs::remove_dir_all(guard.root.join("data")).unwrap();
            write(&guard, &data, true);
            assert_eq!(tree(&guard.root), original);
        }
    }

    #[test]
    fn incremental_publish_preserves_full_chunks_and_unchanged_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let guard = TreeLock::acquire(&tmp.path().join("public")).unwrap();
        write(&guard, &snapshot(400), false);
        let full = guard
            .root
            .join("data/chunks/000000000001-000000000200.json");
        let agents = guard.root.join("data/agents.json");
        let before = (
            fs::metadata(&full).unwrap().modified().unwrap(),
            fs::metadata(&agents).unwrap().modified().unwrap(),
        );
        let mut next = snapshot(401);
        next.paused = true;
        write(&guard, &next, false);
        assert_eq!(
            before,
            (
                fs::metadata(full).unwrap().modified().unwrap(),
                fs::metadata(agents).unwrap().modified().unwrap()
            )
        );
        let manifest: Value =
            serde_json::from_slice(&fs::read(guard.root.join("data/manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["paused"], true);
        assert_eq!(manifest["demo"], true);
        next.events[400]["from_self"] = json!(true);
        write(&guard, &next, true);
        assert!(
            !String::from_utf8(
                fs::read(
                    guard
                        .root
                        .join("data/chunks/000000000401-000000000600.json")
                )
                .unwrap()
            )
            .unwrap()
            .contains("from_self")
        );
    }

    #[test]
    fn sparse_publication_rewrites_only_complete_changed_chunks_and_retraction_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let guard = TreeLock::acquire(&tmp.path().join("public")).unwrap();
        let mut original = snapshot(600);
        original.events[0]["text"] = json!("must-disappear-after-retraction");
        write(&guard, &original, false);
        let untouched = guard
            .root
            .join("data/chunks/000000000201-000000000400.json");
        let before = fs::metadata(&untouched).unwrap().modified().unwrap();

        let mut complete = snapshot(601);
        complete.events[0].as_object_mut().unwrap().remove("text");
        complete.events[0]["retracted"] = json!(true);
        complete.events[600] = json!({"schema":"tolight-event/1","seq":601,"event_id":"E-601","kind":"event_retracted","created_at":"2026-11-14T00:00:00Z","retracted":false,"target":{"seq":1,"event_id":"E-1"},"reason_code":"other"});
        let mut sparse = complete.clone();
        sparse.events.retain(|event| {
            let seq = event["seq"].as_i64().unwrap();
            seq <= 200 || seq >= 600
        });
        write(&guard, &sparse, false);
        assert_eq!(
            fs::metadata(&untouched).unwrap().modified().unwrap(),
            before
        );
        assert!(tree(&guard.root).values().all(|bytes| {
            !String::from_utf8_lossy(bytes).contains("must-disappear-after-retraction")
        }));
        let fresh = tempfile::tempdir().unwrap();
        let other = TreeLock::acquire(&fresh.path().join("public")).unwrap();
        write(&other, &complete, true);
        assert_eq!(tree(&guard.root), tree(&other.root));

        // A caught-up publication needs only its anchor and current metadata.
        sparse.events.retain(|event| event["seq"] == 601);
        write(&guard, &sparse, false);
        assert_eq!(tree(&guard.root), tree(&other.root));
    }

    #[test]
    fn sparse_publication_rejects_missing_new_events_and_incomplete_retraction_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let guard = TreeLock::acquire(&tmp.path().join("public")).unwrap();
        write(&guard, &snapshot(400), false);
        let before = tree(&guard.root);
        let mut missing_new = snapshot(402);
        missing_new.events.retain(|event| event["seq"] != 401);
        assert!(write_tree(&guard, &missing_new, true, false, |_| Ok(())).is_err());
        assert_eq!(tree(&guard.root), before);

        let mut missing_target = snapshot(401);
        missing_target.events[400] = json!({"schema":"tolight-event/1","seq":401,"event_id":"E-401","kind":"event_retracted","created_at":"2026-11-14T00:00:00Z","retracted":false,"target":{"seq":1,"event_id":"E-1"},"reason_code":"other"});
        missing_target
            .events
            .retain(|event| event["seq"].as_i64().unwrap() >= 400);
        assert!(write_tree(&guard, &missing_target, true, false, |_| Ok(())).is_err());
        assert_eq!(tree(&guard.root), before);
    }

    #[test]
    fn aborted_stages_resume_from_manifest_without_partial_json_or_temps() {
        for stage in [Stage::Chunks, Stage::Agents, Stage::Manifest] {
            let tmp = tempfile::tempdir().unwrap();
            let guard = TreeLock::acquire(&tmp.path().join("public")).unwrap();
            write(&guard, &snapshot(199), false);
            let next = snapshot(401);
            let failed = write_tree(&guard, &next, true, false, |at| {
                if at == stage {
                    Err(PublishError::Invalid("injected interruption".into()))
                } else {
                    Ok(())
                }
            });
            assert!(failed.is_err());
            for bytes in tree(&guard.root).values() {
                serde_json::from_slice::<Value>(bytes).unwrap();
            }
            fs::write(guard.root.join("data/chunks/stale.json.tmp"), "incomplete").unwrap();
            write(&guard, &next, false);
            let actual = tree(&guard.root);
            let fresh = tempfile::tempdir().unwrap();
            let other = TreeLock::acquire(&fresh.path().join("public")).unwrap();
            write(&other, &next, true);
            assert_eq!(actual, tree(&other.root));
            assert!(
                actual
                    .keys()
                    .all(|path| !path.to_string_lossy().ends_with(".tmp"))
            );
        }
    }

    #[test]
    fn interrupted_retraction_rewrites_old_full_chunk_before_committing_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let guard = TreeLock::acquire(&tmp.path().join("public")).unwrap();
        let mut original = snapshot(400);
        original.events[0]["text"] = json!("unique-retraction-secret");
        write(&guard, &original, false);
        let mut next = snapshot(401);
        next.events[0].as_object_mut().unwrap().remove("text");
        next.events[0]["retracted"] = json!(true);
        next.events[400] = json!({"schema":"tolight-event/1","seq":401,"event_id":"E-401","kind":"event_retracted","created_at":"2026-11-14T00:00:00Z","retracted":false,"target":{"seq":1,"event_id":"E-1"},"reason_code":"other"});
        assert!(
            write_tree(&guard, &next, true, false, |stage| {
                if stage == Stage::Chunks {
                    Err(PublishError::Invalid("interrupted before manifest".into()))
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        let old_manifest: Value =
            serde_json::from_slice(&fs::read(guard.root.join("data/manifest.json")).unwrap())
                .unwrap();
        assert_eq!(old_manifest["newest_seq"], 400);
        assert!(
            tree(&guard.root)
                .values()
                .all(|bytes| !String::from_utf8_lossy(bytes).contains("unique-retraction-secret"))
        );
        write(&guard, &next, false);
        let fresh = tempfile::tempdir().unwrap();
        let other = TreeLock::acquire(&fresh.path().join("public")).unwrap();
        write(&other, &next, true);
        assert_eq!(tree(&guard.root), tree(&other.root));
    }

    #[test]
    fn restored_timeline_requires_rebuild_and_removes_surplus_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let guard = TreeLock::acquire(&tmp.path().join("public")).unwrap();
        write(&guard, &snapshot(401), false);
        assert!(matches!(
            check_anchor(&guard.root, &snapshot(200)),
            Err(PublishError::TimelineMismatch)
        ));
        let mut different = snapshot(401);
        different.events[400]["event_id"] = json!("E-replaced");
        assert!(matches!(
            check_anchor(&guard.root, &different),
            Err(PublishError::TimelineMismatch)
        ));
        write(&guard, &snapshot(1), true);
        assert_eq!(
            fs::read_dir(guard.root.join("data/chunks"))
                .unwrap()
                .count(),
            1
        );
        write(&guard, &snapshot(0), true);
        assert_eq!(
            fs::read_dir(guard.root.join("data/chunks"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn nested_projection_is_allowlisted_and_retracted_text_cannot_escape() {
        let mut event = snapshot(1).events.remove(0);
        event["owner_contact"] = json!("private");
        event["from_self"] = json!(true);
        event["agent"]["public_key"] = json!("private");
        event["mentions"] =
            json!([{"agent_id":"bob","display_name":"Bob","owner_contact":"private"}]);
        let clean = public_event(&event).unwrap().to_string();
        assert!(!clean.contains("private"));
        assert!(!clean.contains("from_self"));
        event["retracted"] = json!(true);
        let clean = public_event(&event).unwrap();
        assert!(clean.get("text").is_none());
        assert_eq!(clean["mentions"], json!([]));
    }

    #[test]
    fn canonical_tree_lock_is_exclusive_and_inode_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let public = tmp.path().join("public");
        let guard = TreeLock::acquire(&public).unwrap();
        assert!(matches!(
            TreeLock::acquire(&public.join(".")),
            Err(PublishError::Busy)
        ));
        let path = tmp.path().join(".tolight-publisher.lock");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let inode = fs::metadata(&path).unwrap().ino();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            drop(guard);
            let _next = TreeLock::acquire(&public).unwrap();
            assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        }
    }

    #[test]
    fn hold_lock_helper() {
        let Ok(path) = std::env::var("TOLIGHT_TEST_LOCK_CHILD") else {
            return;
        };
        let guard = TreeLock::acquire(Path::new(&path)).unwrap();
        fs::write(guard.root.join("ready"), "ready").unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn process_death_releases_os_lock_for_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let public = tmp.path().join("public");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "publisher::tests::hold_lock_helper"])
            .env("TOLIGHT_TEST_LOCK_CHILD", &public)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !public.join("ready").exists() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(public.join("ready").exists());
        assert!(matches!(
            TreeLock::acquire(&public),
            Err(PublishError::Busy)
        ));
        child.kill().unwrap();
        child.wait().unwrap();
        let guard = TreeLock::acquire(&public).unwrap();
        write(&guard, &snapshot(201), false);
    }

    #[tokio::test]
    async fn cancellation_cannot_release_a_blocking_writers_file_descriptor() {
        let tmp = tempfile::tempdir().unwrap();
        let public = tmp.path().join("public");
        let guard = TreeLock::acquire(&public).unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let _ = ready_tx.send(());
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                write(&guard, &snapshot(201), false);
            })
            .await
            .unwrap();
        });
        ready_rx.await.unwrap();
        task.abort();
        assert!(matches!(
            TreeLock::acquire(&public),
            Err(PublishError::Busy)
        ));
        release_tx.send(()).unwrap();
        let start = Instant::now();
        loop {
            if let Ok(guard) = TreeLock::acquire(&public) {
                let manifest: Value = serde_json::from_slice(
                    &fs::read(guard.root.join("data/manifest.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(manifest["newest_seq"], 201);
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(3));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn brand_jpegs_are_preflighted_and_installed_as_exact_binary_assets() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir(&source).unwrap();
        let mut config = Config::default();
        config.public.dir = tmp.path().join("public");
        fs::create_dir(&config.public.dir).unwrap();
        fs::write(
            config.public.dir.join("index.html"),
            "previous installed index",
        )
        .unwrap();
        for name in ["app.js", "style.css", "robots.txt"] {
            fs::write(source.join(name), format!("asset {name}")).unwrap();
        }
        fs::write(
            source.join("index.html"),
            "<img src=\"long-shot-coffee.jpg\"><img src=\"long-shot-seal.jpg\">",
        )
        .unwrap();
        let coffee = include_bytes!("../../../public/long-shot-coffee.jpg");
        let seal = include_bytes!("../../../public/long-shot-seal.jpg");
        assert!(valid_brand_jpeg(coffee));
        assert!(valid_brand_jpeg(seal));
        fs::write(source.join(BRAND_ASSETS[0]), coffee).unwrap();
        assert!(install_assets(&config, &source).await.is_err());
        assert_eq!(
            fs::read(config.public.dir.join("index.html")).unwrap(),
            b"previous installed index"
        );
        assert!(!config.public.dir.join(BRAND_ASSETS[0]).exists());
        fs::write(source.join(BRAND_ASSETS[1]), seal).unwrap();
        install_assets(&config, &source).await.unwrap();
        for name in BRAND_ASSETS {
            assert_eq!(
                fs::read(config.public.dir.join(name)).unwrap(),
                fs::read(source.join(name)).unwrap()
            );
        }
        let index = fs::read(config.public.dir.join("index.html")).unwrap();
        let mut broken_segment = seal.to_vec();
        broken_segment[4..6].copy_from_slice(&[0xff, 0xff]);
        for invalid in [
            b"not a JPEG".to_vec(),
            seal[..seal.len() - 2].to_vec(),
            broken_segment,
            vec![0; 4 * 1024 * 1024 + 1],
        ] {
            fs::write(source.join(BRAND_ASSETS[1]), invalid).unwrap();
            assert!(install_assets(&config, &source).await.is_err());
            assert_eq!(
                fs::read(config.public.dir.join("index.html")).unwrap(),
                index
            );
            assert_eq!(
                fs::read(config.public.dir.join(BRAND_ASSETS[1])).unwrap(),
                seal.as_slice()
            );
        }
        #[cfg(unix)]
        {
            fs::remove_file(source.join(BRAND_ASSETS[1])).unwrap();
            std::os::unix::fs::symlink(source.join(BRAND_ASSETS[0]), source.join(BRAND_ASSETS[1]))
                .unwrap();
            assert!(install_assets(&config, &source).await.is_err());
            assert_eq!(
                fs::read(config.public.dir.join("index.html")).unwrap(),
                index
            );
        }
    }

    #[tokio::test]
    async fn install_assets_is_idempotent_and_does_not_touch_data() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir(&source).unwrap();
        for name in ["index.html", "app.js", "style.css", "robots.txt"] {
            fs::write(source.join(name), format!("asset {name}")).unwrap();
        }
        let mut config = Config::default();
        config.public.dir = tmp.path().join("public");
        {
            let guard = TreeLock::acquire(&config.public.dir).unwrap();
            write(&guard, &snapshot(201), false);
        }
        let data = tree(&config.public.dir);
        install_assets(&config, &source).await.unwrap();
        install_assets(&config, &source).await.unwrap();
        assert_eq!(tree(&config.public.dir), data);
        for name in ["index.html", "app.js", "style.css", "robots.txt"] {
            assert_eq!(
                fs::read(config.public.dir.join(name)).unwrap(),
                fs::read(source.join(name)).unwrap()
            );
        }
        fs::remove_file(source.join("app.js")).unwrap();
        fs::write(source.join("index.html"), "uncommitted change").unwrap();
        assert!(install_assets(&config, &source).await.is_err());
        assert_ne!(
            fs::read(config.public.dir.join("index.html")).unwrap(),
            b"uncommitted change"
        );
    }
}
