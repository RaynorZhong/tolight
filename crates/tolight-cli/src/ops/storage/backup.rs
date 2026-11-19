use super::{Backup, PhysicalBackup, Result, root};
use super::{common::*, wal};
use serde_json::{Value, json};
use std::{fs, io::Read, path::Path};
use time::{Duration, OffsetDateTime};

pub fn backup_name(name: &str) -> bool {
    let Some(name) = name
        .strip_prefix("tolight-")
        .and_then(|s| s.strip_suffix(".dump"))
    else {
        return false;
    };
    let Some((time, id)) = name.split_once('-') else {
        return false;
    };
    time.len() == 16
        && time::PrimitiveDateTime::parse(
            time,
            &time::format_description::parse_borrowed::<1>(
                "[year][month][day]T[hour][minute][second]Z",
            )
            .expect("fixed logical backup timestamp format"),
        )
        .is_ok()
        && id.len() == 32
        && id
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
}
fn time(value: &Value, now: f64) -> Option<f64> {
    let text = value.as_str()?;
    let value = utc(text).ok()?;
    let stamp = value.unix_timestamp() as f64 + value.nanosecond() as f64 / 1e9;
    (stamp >= 0.0 && stamp <= now + 60.0).then_some(stamp)
}
const LOGICAL_METADATA_LIMIT: u64 = 65536;
fn logical_metadata(path: &Path) -> Result<Value> {
    // Select markers before opening anything: a directory also contains large
    // binary dumps, which must never be loaded as JSON during retention scans.
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".meta.json"))
        .filter(|name| backup_name(name))
        .ok_or("not a logical backup completion marker")?;
    regular(path)?;
    let source = fs::File::open(path)?;
    if source.metadata()?.len() > LOGICAL_METADATA_LIMIT {
        return Err("logical backup completion metadata exceeds 64 KiB".into());
    }
    let mut bytes = Vec::new();
    source
        .take(LOGICAL_METADATA_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LOGICAL_METADATA_LIMIT {
        return Err("logical backup completion metadata exceeds 64 KiB".into());
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    if value["schema"] != "tolight-backup/1" || value["file"].as_str() != Some(file) {
        return Err("invalid logical backup completion metadata".into());
    }
    let hash = value["sha256"].as_str().ok_or("backup checksum")?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err("invalid logical backup checksum".into());
    }
    Ok(value)
}
pub fn successful_backup_times(directory: &Path, now: f64) -> Vec<f64> {
    let mut times = vec![];
    if symlink(directory) {
        return times;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return times;
    };
    for entry in entries.flatten() {
        if CANCELLED.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let logical = (|| -> Option<f64> {
            let value = logical_metadata(&path).ok()?;
            let file = value["file"].as_str()?;
            let dump = directory.join(file);
            regular(&dump).ok()?;
            if value.get("size").is_some()
                && value["size"].as_u64() != Some(fs::metadata(&dump).ok()?.len())
            {
                return None;
            }
            let mut magic = [0; 5];
            fs::File::open(dump).ok()?.read_exact(&mut magic).ok()?;
            if &magic != b"PGDMP" {
                return None;
            }
            time(&value["created_at"], now)
        })();
        if let Some(value) = logical {
            times.push(value)
        }
        if name.starts_with("tolight-base-") && !symlink(&path) && path.is_dir() {
            let physical = (|| -> Option<f64> {
                let marker = path.join("backup.json");
                regular(&marker).ok()?;
                if fs::metadata(&marker).ok()?.len() > 65536 {
                    return None;
                }
                let value = json_file(&marker).ok()?;
                let data = path.join("data");
                let manifest = data.join("backup_manifest");
                let version = data.join("PG_VERSION");
                regular(&manifest).ok()?;
                regular(&version).ok()?;
                let identity = value["system_identifier"].as_str()?;
                if symlink(&data)
                    || value["schema"] != "tolight-base-backup/1"
                    || value["status"] != "passed"
                    || value["data"] != "data"
                    || value["postgres_major"].as_u64() != Some(18)
                    || identity.is_empty()
                    || !identity.bytes().all(|v| v.is_ascii_digit())
                    || fs::read_to_string(version).ok()?.trim() != "18"
                    || value["manifest_sha256"].as_str() != Some(&sha256(&manifest).ok()?)
                {
                    return None;
                }
                time(&value["completed_at"], now)
            })();
            if let Some(value) = physical {
                times.push(value)
            }
        }
    }
    times
}
pub fn logical(args: Backup) -> Result<()> {
    println!("{}", logical_report(args)?);
    Ok(())
}
pub fn logical_report(args: Backup) -> Result<Value> {
    logical_inner(args, None)
}
pub fn logical_report_with_database(args: Backup, database: &str) -> Result<Value> {
    logical_inner(args, Some(database))
}
fn logical_inner(args: Backup, database: Option<&str>) -> Result<Value> {
    if !(1..=36500).contains(&args.retention_days) {
        return Err("retention-days must be between 1 and 36500".into());
    }
    let config = config(args.config.as_deref())?;
    let mut directory = private_path(&args.output_dir, &root().join("public-dist"))?;
    directory = private_path(&directory, &config.public.dir)?;
    if let Some(public) = args.public_dir {
        directory = private_path(&directory, &public)?
    }
    private_dir(&directory)?;
    let database = database
        .map(str::to_owned)
        .map(Ok)
        .unwrap_or_else(|| database_url(&config))?;
    let env = pg_environment(&database, None)?;
    let now = OffsetDateTime::now_utc();
    let name = format!(
        "tolight-{}-{}.dump",
        now.format(&time::format_description::parse_borrowed::<1>(
            "[year][month][day]T[hour][minute][second]Z",
        )?)?,
        uuid::Uuid::new_v4().simple()
    );
    let temporary = directory.join(format!(".{name}.tmp"));
    let final_path = directory.join(&name);
    new_file(&temporary)?;
    let result = (|| -> Result<Value> {
        let mut dump = command(program("pg_dump"), &env);
        dump.args(["--format=custom", "--compress=0", "--file"])
            .arg(&temporary)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if !status_poll(&mut dump)? {
            return Err(
                "backup failed; database credentials and tool diagnostics suppressed".into(),
            );
        }
        fs::File::open(&temporary)?.sync_all()?;
        fs::hard_link(&temporary, &final_path)?;
        fs::remove_file(&temporary)?;
        let hash = sha256(&final_path)?;
        if CANCELLED.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("native operation interrupted".into());
        }
        private_json(
            &directory.join(format!("{name}.meta.json")),
            &json!({"schema":"tolight-backup/1","created_at":tolight_core::datetime::utc_micros(now),"file":name,"size":fs::metadata(&final_path)?.len(),"sha256":hash,"format":"custom","compression":"none"}),
        )?;
        let mut removed = vec![];
        for entry in fs::read_dir(&directory)? {
            if CANCELLED.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("native operation interrupted".into());
            }
            let path = entry?.path();
            let prune = (|| -> Result<Option<String>> {
                let value = logical_metadata(&path)?;
                let file = value["file"].as_str().ok_or("file")?;
                let created = utc(value["created_at"].as_str().ok_or("created_at")?)?;
                // Unexpired backups need no data scan. Only an expired candidate
                // is fully hashed to prove ownership before removing its pair.
                if created >= now - Duration::days(args.retention_days) {
                    return Ok(None);
                }
                let backup = directory.join(file);
                regular(&backup)?;
                if value.get("size").is_some()
                    && value["size"].as_u64() != Some(fs::metadata(&backup)?.len())
                {
                    return Ok(None);
                }
                if value["sha256"].as_str() != Some(&sha256(&backup)?) {
                    return Ok(None);
                }
                if CANCELLED.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err("native operation interrupted".into());
                }
                fs::remove_file(backup)?;
                fs::remove_file(&path)?;
                Ok(Some(file.to_string()))
            })();
            // Cancellation is not a malformed/foreign marker to skip.
            if CANCELLED.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("native operation interrupted".into());
            }
            if let Ok(Some(name)) = prune {
                removed.push(name)
            }
        }
        removed.sort();
        sync_directory(&directory)?;
        Ok(json!({"backup":final_path,"retention_days":args.retention_days,"removed":removed}))
    })();
    let _ = fs::remove_file(temporary);
    result
}
pub fn physical(args: PhysicalBackup) -> Result<()> {
    let (report, result) = physical_outcome(args, None)?;
    println!("{report}");
    result
}
pub fn physical_report(args: PhysicalBackup) -> Result<Value> {
    let (report, result) = physical_outcome(args, None)?;
    result?;
    Ok(report)
}
pub fn physical_report_with_database(args: PhysicalBackup, database: &str) -> Result<Value> {
    let (report, result) = physical_outcome(args, Some(database))?;
    result?;
    Ok(report)
}
fn physical_outcome(args: PhysicalBackup, database: Option<&str>) -> Result<(Value, Result<()>)> {
    let config = config(Some(&args.config))?;
    let public = &config.public.dir;
    let mut out = private_path(&args.output_dir, &root().join("public-dist"))?;
    out = private_path(&out, public)?;
    if symlink(&args.output_dir) {
        return Err("backup output must not be a symlink".into());
    }
    let archive = wal::checked_archive(&args.archive_dir, Some(public))?;
    let work = temp_dir(&out, "tolight-base-")?;
    let mut report = json!({"schema":"tolight-base-backup/1","status":"failed","data":"data","archive_dir":archive,"postgres_major":18});
    let result = (|| -> Result<()> {
        let database = database
            .map(str::to_owned)
            .map(Ok)
            .unwrap_or_else(|| database_url(&config))?;
        let env = pg_environment(&database, None)?;
        let version = output(command(program("pg_basebackup"), &env).arg("--version"))?;
        if !version.split_whitespace().any(|s| s.starts_with("18.")) {
            return Err("PostgreSQL 18 tools required".into());
        }
        let settings=output(command(program("psql"),&env).args(["-w","-XAt","-F","\t","-c","SELECT system_identifier,current_setting('archive_mode'),(current_setting('archive_command')<>'' OR current_setting('archive_library')<>''),session_user FROM pg_control_system()"]))?;
        let values: Vec<_> = settings.split('\t').collect();
        if values.len() != 4 || !["on", "always"].contains(&values[1]) || values[2] != "t" {
            return Err("source continuous WAL archiving must already be configured".into());
        }
        capture(
            command(program("pg_basebackup"), &env)
                .args([
                    "--format=plain",
                    "--wal-method=stream",
                    "--checkpoint=fast",
                    "--no-password",
                    "--pgdata",
                ])
                .arg(work.join("data")),
            &work.join("backup.log"),
        )?;
        capture(
            command(program("pg_verifybackup"), &env)
                .args(["--exit-on-error"])
                .arg(work.join("data")),
            &work.join("verify.log"),
        )?;
        let manifest = json_file(&work.join("data/backup_manifest"))?;
        let identity = output(command(program("psql"), &env).args([
            "-w",
            "-XAt",
            "-c",
            "SELECT system_identifier FROM pg_control_system()",
        ]))?;
        if manifest["PostgreSQL-Backup-Manifest-Version"] != 2
            || identity.is_empty()
            || !identity.bytes().all(|c| c.is_ascii_digit())
            || manifest["System-Identifier"].to_string().trim_matches('"') != identity
        {
            return Err("base backup system identifier differs from source".into());
        }
        report = json!({"schema":"tolight-base-backup/1","status":"passed","data":"data","archive_dir":archive,"postgres_major":18,"completed_at":self::now(),"manifest_sha256":sha256(&work.join("data/backup_manifest"))?,"system_identifier":identity,"wal_ranges":manifest["WAL-Ranges"],"database":env.get(std::ffi::OsStr::new("PGDATABASE")).map(|s|s.to_string_lossy()),"format":"plain","wal_method":"stream","database_user":values[3],"public_dir":canonical(public)?,"source_archive_mode":values[1],"source_configuration_modified":false});
        sync_directory(&work)?;
        Ok(())
    })();
    private_json(&work.join("backup.json"), &report)?;
    sync_directory(&work)?;
    Ok((
        json!({"status":report["status"],"base_backup":work,"report":work.join("backup.json")}),
        result,
    ))
}
