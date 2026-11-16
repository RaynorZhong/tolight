use serde_json::Value;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tolight_core::signing::public_key_from_seed;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
fn pg(name: &str) -> PathBuf {
    let path = root().join(".local/pg18/bin").join(name);
    if path.exists() {
        path
    } else {
        PathBuf::from(name)
    }
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}
fn shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
struct Primary {
    directory: Option<TempDir>,
    env: BTreeMap<OsString, OsString>,
    config: PathBuf,
    archive: PathBuf,
    public: PathBuf,
    backup: PathBuf,
    running: bool,
}
impl Primary {
    fn root(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.env_clear().envs(&self.env).stdin(Stdio::null());
        command
    }
    fn output(&self, program: impl AsRef<std::ffi::OsStr>, args: &[&str], code: i32) -> Output {
        let output = self.command(program).args(args).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn cli(&self, args: &[&str], code: i32) -> Output {
        let mut argv = vec!["--config", self.config.to_str().unwrap()];
        argv.extend_from_slice(args);
        self.output(env!("CARGO_BIN_EXE_tolight"), &argv, code)
    }
    fn start(&mut self) {
        let options = format!(
            "-h '' -k {} -p 5432 -c timezone=UTC",
            shell(self.root().join("socket").to_str().unwrap())
        );
        self.output(
            pg("pg_ctl"),
            &[
                "-D",
                self.root().join("data").to_str().unwrap(),
                "-l",
                self.root().join("postgres.log").to_str().unwrap(),
                "-w",
                "-t",
                "15",
                "-o",
                &options,
                "start",
            ],
            0,
        );
        self.running = true;
    }
    fn stop(&mut self) {
        if self.running {
            self.output(
                pg("pg_ctl"),
                &[
                    "-D",
                    self.root().join("data").to_str().unwrap(),
                    "-m",
                    "fast",
                    "-w",
                    "stop",
                ],
                0,
            );
            self.running = false;
        }
    }
    fn add(&self, id: &str, seed: u8) {
        self.cli(
            &[
                "operator",
                "agent",
                "add",
                id,
                "--display-name",
                id,
                "--bio",
                "Inspection fixture only",
                "--owner-contact",
                "private-inspection-fixture@example.invalid",
                "--public-key",
                &public_key_from_seed(&[seed; 32]),
            ],
            0,
        );
    }
    fn new() -> Self {
        fs::create_dir_all(root().join(".local")).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("ri.")
            .tempdir_in(root().join(".local"))
            .unwrap();
        let path = directory.path();
        let socket = path.join("socket");
        fs::create_dir(&socket).unwrap();
        let archive = path.join("archive");
        let public = path.join("public");
        let config = path.join("coordinator.toml");
        let mut env = std::env::vars_os()
            .filter(|(key, _)| !key.to_string_lossy().starts_with("PG"))
            .collect::<BTreeMap<_, _>>();
        env.insert("PGHOST".into(), socket.as_os_str().into());
        env.insert("PGPORT".into(), "5432".into());
        env.insert("PGUSER".into(), "tolight".into());
        env.insert("PGDATABASE".into(), "postgres".into());
        env.insert("LC_ALL".into(), "C".into());
        let url = format!(
            "postgresql://tolight@localhost:5432/tolight_inspection?host={}",
            percent_encoding::utf8_percent_encode(
                socket.to_str().unwrap(),
                percent_encoding::NON_ALPHANUMERIC
            )
        );
        env.insert("INSPECT_SOURCE_DB".into(), url.into());
        let mut value = Self {
            directory: Some(directory),
            env,
            archive,
            public,
            config,
            backup: PathBuf::new(),
            running: false,
        };
        value.output(
            pg("initdb"),
            &[
                "-D",
                value.root().join("data").to_str().unwrap(),
                "-U",
                "tolight",
                "--encoding=UTF8",
                "--locale=C",
                "--auth-local=trust",
                "--auth-host=reject",
            ],
            0,
        );
        let archive_cmd = [
            env!("CARGO_BIN_EXE_tolight-ops").to_string(),
            "storage".into(),
            "wal-archive".into(),
            "archive".into(),
            "--archive-dir".into(),
            value.archive.to_string_lossy().into_owned(),
            "--source".into(),
            "%p".into(),
            "--name".into(),
            "%f".into(),
            "--public-dir".into(),
            value.public.to_string_lossy().into_owned(),
        ]
        .iter()
        .map(|s| shell(s))
        .collect::<Vec<_>>()
        .join(" ");
        use std::io::Write;
        let mut pgconfig = fs::OpenOptions::new()
            .append(true)
            .open(value.root().join("data/postgresql.conf"))
            .unwrap();
        write!(pgconfig,"\narchive_mode=on\narchive_timeout=0\nwal_level=replica\nmax_wal_senders=4\narchive_command={}\n",quote(&archive_cmd)).unwrap();
        value.start();
        value.output(pg("createdb"), &["tolight_inspection"], 0);
        value
            .env
            .insert("PGDATABASE".into(), "tolight_inspection".into());
        fs::write(&value.config,format!("[database]\nsecret_cmd=['/usr/bin/printenv','INSPECT_SOURCE_DB']\n[public]\ndir={}\n",serde_json::to_string(&value.public).unwrap())).unwrap();
        value.cli(&["migrate"], 0);
        value.add("before-agent", 201);
        let output = value.output(
            env!("CARGO_BIN_EXE_tolight-ops"),
            &[
                "storage",
                "physical-backup",
                "--config",
                value.config.to_str().unwrap(),
                "--output-dir",
                value.root().join("backups").to_str().unwrap(),
                "--archive-dir",
                value.archive.to_str().unwrap(),
            ],
            0,
        );
        value.backup = PathBuf::from(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["base_backup"]
                .as_str()
                .unwrap(),
        );
        value.add("after-agent", 202);
        let segment = String::from_utf8(
            value
                .output(
                    pg("psql"),
                    &[
                        "-w",
                        "-XAt",
                        "-c",
                        "SELECT pg_walfile_name(pg_current_wal_lsn()-1)",
                    ],
                    0,
                )
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        value.output(
            pg("psql"),
            &["-w", "-XAt", "-c", "SELECT pg_switch_wal()"],
            0,
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !value.archive.join(format!("{segment}.meta.json")).is_file() {
            assert!(
                Instant::now() < deadline,
                "post-backup WAL never completed archive"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        value.stop();
        value
    }
    fn inspect(&self, code: i32, extra: &[&str]) -> (Output, Value) {
        let work = root().join(".local/inspection-tests");
        self.inspect_in(&work, code, extra)
    }
    fn socket_work_dir(&self, socket_bytes: usize) -> PathBuf {
        let parent = self.root().join("w");
        let suffix = Path::new("ri-0000000000").join("socket/.s.PGSQL.65535");
        let fixed = parent.as_os_str().as_encoded_bytes().len()
            + 1
            + 1
            + suffix.as_os_str().as_encoded_bytes().len();
        assert!(
            socket_bytes > fixed,
            "fixture source path leaves no room for a padded inspection directory"
        );
        parent.join("w".repeat(socket_bytes - fixed))
    }
    fn inspect_in(&self, work: &Path, code: i32, extra: &[&str]) -> (Output, Value) {
        let mut argv = vec![
            "storage",
            "pitr-inspect",
            "--base-backup",
            self.backup.to_str().unwrap(),
            "--archive-dir",
            self.archive.to_str().unwrap(),
            "--work-dir",
            work.to_str().unwrap(),
            "--tolight",
            env!("CARGO_BIN_EXE_tolight"),
            "--startup-timeout-seconds",
            "15",
        ];
        argv.extend_from_slice(extra);
        let output = self
            .command(env!("CARGO_BIN_EXE_tolight-ops"))
            .args(&argv)
            .output()
            .unwrap();
        let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            Self::inspection_failure(&summary)
        );
        (output, summary)
    }
    fn inspection_failure(summary: &Value) -> String {
        let report_path = Path::new(summary["report"].as_str().unwrap());
        let report = json(report_path);
        let work = report_path.parent().unwrap();
        let mut diagnostic = format!("phase={} error={}", report["phase"], report["error"]);
        // Read only the fixture's startup diagnostics. A full controls export
        // or observation may contain private contacts and must not be printed.
        for name in ["pg-start.stdout", "postgres.log"] {
            if let Ok(log) = fs::read_to_string(work.join(name)) {
                diagnostic.push_str(&format!("\n{name}:\n{log}"));
            }
        }
        diagnostic
    }
}
impl Drop for Primary {
    fn drop(&mut self) {
        if self.running {
            let status = self
                .command(pg("pg_ctl"))
                .arg("-D")
                .arg(self.root().join("data"))
                .args(["-m", "fast", "-w", "stop"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if !status.is_ok_and(|s| s.success())
                && let Some(directory) = self.directory.take()
            {
                let _ = directory.keep();
            }
        }
    }
}

#[test]
fn latest_wal_is_only_an_unverified_private_observation_after_source_shutdown() {
    let mut primary = Primary::new();
    assert!(!primary.running);
    let base = primary.socket_work_dir(102);
    let old_socket = base.join("tolight-inspection-0000000000/socket/.s.PGSQL.65535");
    assert!(old_socket.as_os_str().as_encoded_bytes().len() > 107);
    let (_, summary) = primary.inspect_in(&base, 2, &[]);
    assert_eq!(summary["status"], "needs_verified_controls");
    let report = json(summary["report"].as_str().unwrap());
    for key in [
        "unknown_controls",
        "target_postgres_stopped",
        "target_data_removed",
        "temporary_archive_removed",
    ] {
        assert_eq!(report[key], true, "{key}");
    }
    for key in [
        "known_current_controls",
        "restore_eligible",
        "source_connected",
        "application_started",
        "public_tree_created",
        "importable_control_snapshot_retained",
    ] {
        assert_eq!(report[key], false, "{key}");
    }
    assert_eq!(report["target_postgres"]["tcp_authentication"], "reject");
    let socket = Path::new(report["target_postgres"]["socket"].as_str().unwrap()).join(format!(
        ".s.PGSQL.{}",
        report["target_postgres"]["port"].as_u64().unwrap()
    ));
    assert!(socket.as_os_str().as_encoded_bytes().len() < 104);
    assert!(report["coverage"]["replay_lsn"].is_string());
    assert!(!report["coverage"]["last_replay_commit_time"].is_null());
    assert!(report["coverage"]["latest_source_commit_time"].is_null());
    assert_eq!(report["coverage"]["source_upper_bound_known"], false);
    assert_eq!(
        report["coverage"]["system_identifier"],
        json(primary.backup.join("backup.json"))["system_identifier"]
    );
    let observation_path = Path::new(report["observation"].as_str().unwrap());
    let observation = json(observation_path);
    assert_eq!(
        observation["schema"],
        "tolight-unverified-recovery-observation/1"
    );
    assert_eq!(observation["unknown_controls"], true);
    assert!(observation["observed_state"].get("schema").is_none());
    let identities = observation["observed_state"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["agent_id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        identities,
        ["before-agent", "after-agent"].into_iter().collect()
    );
    let work = Path::new(report["work_dir"].as_str().unwrap());
    assert_eq!(work.parent(), Some(base.as_path()));
    assert!(
        work.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("ri-")
    );
    for name in ["public", ".inspection-raw-controls.json", "data", "archive"] {
        assert!(!work.join(name).exists(), "{name}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(observation_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(summary["report"].as_str().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(work).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    primary.start();
    primary.cli(&["operator", "agent", "list", "--pending"], 0);
    let rejected = primary.cli(
        &[
            "operator",
            "restore-controls",
            "--snapshot",
            observation_path.to_str().unwrap(),
            "--dry-run",
        ],
        1,
    );
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .to_lowercase()
            .contains("snapshot")
    );
    primary.stop();
}
#[test]
fn overlong_socket_path_fails_before_copying_data_or_starting_target_postgres() {
    let primary = Primary::new();
    let base = primary.socket_work_dir(104);
    let (_, summary) = primary.inspect_in(&base, 1, &[]);
    assert_eq!(summary["status"], "failed");
    let report = json(summary["report"].as_str().unwrap());
    assert_eq!(report["phase"], "latest_available_recovery");
    let error = report["error"].as_str().unwrap();
    assert!(error.contains("103 bytes"), "{error}");
    assert!(error.contains("shorter --work-dir"), "{error}");
    assert!(report.get("target_postgres").is_none());
    assert!(report.get("target_postgres_stopped").is_none());
    for key in [
        "unknown_controls",
        "target_data_removed",
        "temporary_archive_removed",
    ] {
        assert_eq!(report[key], true, "{key}");
    }
    for key in [
        "known_current_controls",
        "restore_eligible",
        "source_connected",
        "application_started",
        "public_tree_created",
        "importable_control_snapshot_retained",
    ] {
        assert_eq!(report[key], false, "{key}");
    }
    let work = Path::new(report["work_dir"].as_str().unwrap());
    assert_eq!(work.parent(), Some(base.as_path()));
    for name in [
        "data",
        "socket",
        "archive",
        "public",
        "pg-start.stdout",
        "postgres.log",
        ".inspection-raw-controls.json",
        "unverified-observation.json",
    ] {
        assert!(
            !work.join(name).exists(),
            "unexpected target artifact: {name}"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(summary["report"].as_str().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(work).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
#[test]
fn corrupt_wal_never_creates_public_tree_or_importable_snapshot() {
    let primary = Primary::new();
    let file = fs::read_dir(&primary.archive)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().len() == 24)
        .unwrap();
    fs::write(file, b"corrupt WAL fixture").unwrap();
    let (_, summary) = primary.inspect(1, &[]);
    let report = json(summary["report"].as_str().unwrap());
    assert_eq!(report["phase"], "wal_validation");
    assert_eq!(report["application_started"], false);
    assert_eq!(report["public_tree_created"], false);
    assert_eq!(report["unknown_controls"], true);
    assert!(
        !Path::new(report["work_dir"].as_str().unwrap())
            .join(".inspection-raw-controls.json")
            .exists()
    );
}
#[test]
fn wrong_pg_major_is_rejected_before_target_start() {
    let primary = Primary::new();
    let path = primary.backup.join("backup.json");
    let mut metadata = json(&path);
    metadata["postgres_major"] = 17.into();
    fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let (_, summary) = primary.inspect(1, &[]);
    let report = json(summary["report"].as_str().unwrap());
    assert_eq!(report["phase"], "base_backup_validation");
    assert!(report.get("target_postgres").is_none());
    assert_eq!(report["application_started"], false);
    assert_eq!(report["unknown_controls"], true);
}
#[test]
fn metadata_public_root_rejects_private_observation_output() {
    let primary = Primary::new();
    let public = primary.public.join("inspection-output");
    let output = primary.output(
        env!("CARGO_BIN_EXE_tolight-ops"),
        &[
            "storage",
            "pitr-inspect",
            "--base-backup",
            primary.backup.to_str().unwrap(),
            "--archive-dir",
            primary.archive.to_str().unwrap(),
            "--work-dir",
            public.to_str().unwrap(),
            "--tolight",
            env!("CARGO_BIN_EXE_tolight"),
        ],
        1,
    );
    assert!(!public.exists());
    assert!(output.stdout.is_empty());
}
