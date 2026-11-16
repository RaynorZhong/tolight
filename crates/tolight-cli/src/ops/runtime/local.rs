//! Persistent PG18 foreground supervisor. State compatibility is intentional.
use super::{Result, common as c, lan};
#[path = "local_automation.rs"]
mod automation;
use clap::{Args as ClapArgs, Subcommand};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::{
    fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    net::{UnixListener, UnixStream},
    process::CommandExt,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
const MARKER: &str = "tolight-local-dev/1";
const RUNTIME: &str = "tolight-local-runtime/1";
const CONTROL_PROTOCOL: &str = "tolight-local-control/2";
const CONTROL_REQUEST_MAX_BYTES: u64 = 4096;
const CONTROL_RESPONSE_MAX_BYTES: u64 = 64 * 1024;
type Env = BTreeMap<OsString, OsString>;
#[derive(ClapArgs)]
pub struct Args {
    #[arg(long,global=true,default_value_os_t=c::root().join(".local/dev"))]
    directory: PathBuf,
    #[arg(long, global = true)]
    tolight: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Up {
        #[arg(long)]
        maintenance: bool,
        #[arg(long)]
        api_port: Option<u16>,
        #[arg(long)]
        web_port: Option<u16>,
        #[arg(long)]
        ops_port: Option<u16>,
        #[arg(long)]
        pg_port: Option<u16>,
        #[command(flatten)]
        lan: LanOptions,
        #[command(flatten)]
        automation: automation::Options,
    },
    Status,
    /// Read-only checks of owned services and public resources.
    Check,
    Down,
    Migrate,
    Publish {
        #[arg(long)]
        rebuild: bool,
    },
    Operator {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
    Backup {
        #[arg(long,default_value="both",value_parser=["both","logical","physical"])]
        kind: String,
    },
}
#[derive(ClapArgs, Default)]
struct LanOptions {
    /// Explicit private IPv4 for LAN web and TLS participant access.
    #[arg(long, conflicts_with = "no_lan")]
    lan_ip: Option<Ipv4Addr>,
    #[arg(long, conflicts_with = "no_lan")]
    lan_web_port: Option<u16>,
    #[arg(long, conflicts_with = "no_lan")]
    lan_api_port: Option<u16>,
    /// Disable persisted LAN access; retain the local database and CA.
    #[arg(long)]
    no_lan: bool,
}
struct UpOptions {
    lan: LanOptions,
    automation: automation::Options,
}
impl LanOptions {
    fn resolve(self, previous: Option<lan::Lan>) -> Result<Option<lan::Lan>> {
        if self.no_lan {
            return Ok(None);
        }
        let mut selected = previous;
        if let Some(ip) = self.lan_ip {
            selected = Some(selected.unwrap_or(lan::Lan {
                ip,
                web_port: 8080,
                api_port: 8443,
            }));
            selected.as_mut().expect("selected LAN").ip = ip;
        }
        if let Some(settings) = selected.as_mut() {
            if let Some(port) = self.lan_web_port {
                settings.web_port = port;
            }
            if let Some(port) = self.lan_api_port {
                settings.api_port = port;
            }
            settings.validate()?;
        } else {
            c::ensure(
                self.lan_web_port.is_none() && self.lan_api_port.is_none(),
                "LAN ports require --lan-ip or an existing LAN configuration",
            )?;
        }
        Ok(selected)
    }
}
fn lan_settings(value: &Value) -> Result<Option<lan::Lan>> {
    let Some(value) = value.get("lan").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let settings: lan::Lan = serde_json::from_value(value.clone())?;
    settings.validate()?;
    Ok(Some(settings))
}
fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| "managed paths must be UTF-8".into())
}
fn default_binary() -> PathBuf {
    c::root().join("target/debug/tolight")
}
fn binary(value: &Path) -> Result<PathBuf> {
    c::ensure(
        value.is_absolute() && !value.is_symlink(),
        "--tolight must be an absolute regular executable, not a symlink",
    )?;
    c::regular(value, true)?;
    #[cfg(unix)]
    c::ensure(
        fs::metadata(value)?.permissions().mode() & 0o111 != 0,
        "--tolight must identify an executable",
    )?;
    Ok(value.to_path_buf())
}
fn marker_binary(marker: &Value) -> Result<PathBuf> {
    let path = marker
        .get("tolight_binary")
        .map(|v| v.as_str().ok_or("invalid binary selection"))
        .transpose()?
        .map(PathBuf::from)
        .unwrap_or_else(default_binary);
    c::ensure(path.is_absolute(), "invalid binary selection")?;
    Ok(path)
}
fn checked_directory(value: &Path) -> Result<PathBuf> {
    let raw = if value.is_absolute() {
        value.to_path_buf()
    } else {
        std::env::current_dir()?.join(value)
    };
    let mut path = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::ParentDir => {
                path.pop();
            }
            Component::CurDir => {}
            other => path.push(other.as_os_str()),
        }
    }
    let local = c::root().join(".local");
    c::ensure(
        path.starts_with(&local)
            && path != local
            && path
                .join("socket/.s.PGSQL.65535")
                .as_os_str()
                .as_encoded_bytes()
                .len()
                < 104,
        "directory must be a short dedicated child of this repository's .local",
    )?;
    for ancestor in path.ancestors() {
        c::ensure(
            !ancestor.is_symlink(),
            "symlink directories are not supported",
        )?;
        if ancestor != path && ancestor != local && ancestor.starts_with(&local) {
            c::ensure(
                !ancestor.join("environment.json").exists()
                    && !ancestor.join("PG_VERSION").exists()
                    && !ancestor.join("data/manifest.json").exists(),
                "environment cannot be nested in another environment, database or public tree",
            )?;
        }
    }
    Ok(path)
}
fn no_symlinks(path: &Path) -> Result {
    if !path.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        c::ensure(
            !kind.is_symlink(),
            "managed trees must not contain symlinks",
        )?;
        if kind.is_dir() {
            no_symlinks(&entry.path())?;
        }
    }
    Ok(())
}
fn safe_paths(directory: &Path) -> Result {
    for name in [
        "postgres",
        "socket",
        "public",
        "logs",
        "logs/service",
        "backups",
        "backups/logical",
        "backups/physical",
        "archive",
        "lan",
        "operator-ui",
        "automation",
    ] {
        let path = directory.join(name);
        c::ensure(
            !path.is_symlink() && (!path.exists() || path.is_dir()),
            "managed subdirectories must be real directories",
        )?;
    }
    for name in ["public", "logs", "lan", "operator-ui", "automation"] {
        no_symlinks(&directory.join(name))?;
    }
    for name in [
        "environment.json",
        "runtime.json",
        "config.toml",
        "postgres.conf",
        "pg_hba.conf",
        "pg_ident.conf",
        "lifecycle.lock",
        "backup-operation.lock",
    ] {
        c::regular(&directory.join(name), false)?;
    }
    Ok(())
}
fn private_directory(path: &Path) -> Result {
    fs::create_dir_all(path)?;
    c::ensure(!path.is_symlink(), "managed directories cannot be symlinks")?;
    #[cfg(unix)]
    {
        let current = c::capture(Command::new("/usr/bin/id").arg("-u"), 3, None)?;
        let uid = String::from_utf8(current.stdout)?.trim().parse::<u32>()?;
        c::ensure(
            fs::metadata(path)?.uid() == uid,
            "managed directory must belong to current user",
        )?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn marker(directory: &Path) -> Result<Value> {
    let value = c::load(&directory.join("environment.json"))?;
    c::ensure(
        value["schema"] == MARKER
            && value["directory"].as_str() == Some(path_text(directory)?)
            && ["prepared", "initializing", "ready"]
                .contains(&value["phase"].as_str().unwrap_or(""))
            && uuid::Uuid::parse_str(
                value["environment_id"]
                    .as_str()
                    .ok_or("invalid environment identity")?,
            )
            .is_ok()
            && value["postgres"]["major"] == 18
            && value["postgres"]["database"] == "tolight_dev"
            && value["postgres"]["user"] == "tolight",
        "invalid local environment ownership marker",
    )?;
    ports(&value)?;
    lan_settings(&value)?;
    c::ensure(
        value.get("admin_ui").is_none_or(Value::is_boolean),
        "invalid local admin UI marker",
    )?;
    safe_paths(directory)?;
    marker_binary(&value)?;
    Ok(value)
}
fn ports(marker: &Value) -> Result<BTreeMap<String, u16>> {
    let object = marker["ports"].as_object().ok_or("invalid ports")?;
    c::ensure(object.len() == 4, "invalid ports")?;
    let mut result = BTreeMap::new();
    for name in ["api", "web", "ops", "pg"] {
        let value = u16::try_from(
            object
                .get(name)
                .and_then(Value::as_u64)
                .ok_or("invalid port")?,
        )?;
        c::ensure(value > 0, "ports must be in 1..65535")?;
        result.insert(name.into(), value);
    }
    c::ensure(
        [result["api"], result["web"], result["ops"]]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .len()
            == 3,
        "API, web and ops ports must differ",
    )?;
    Ok(result)
}
fn environment(directory: &Path, ports: &BTreeMap<String, u16>) -> Result<Env> {
    let root = c::root();
    let mut env: Env = std::env::vars_os()
        .filter(|(key, _)| {
            let k = key.to_string_lossy();
            !k.starts_with("PG")
                && ![
                    "DATABASE_URL",
                    "TOLIGHT_LOCAL_DATABASE_URL",
                    "CARGO_BUILD_TARGET",
                ]
                .contains(&k.as_ref())
        })
        .collect();
    for (key, value) in [
        ("PGHOST", directory.join("socket").into_os_string()),
        ("PGPORT", ports["pg"].to_string().into()),
        ("PGUSER", "tolight".into()),
        ("PGDATABASE", "tolight_dev".into()),
        ("PGCONNECT_TIMEOUT", "3".into()),
        ("LC_ALL", "C".into()),
        ("SQLX_OFFLINE", "true".into()),
        ("CARGO_TARGET_DIR", root.join("target").into_os_string()),
        (
            "SQLX_OFFLINE_DIR",
            root.join("crates/tolight-coordinator-store/.sqlx")
                .into_os_string(),
        ),
        ("TOLIGHT_OPS_BIN", std::env::current_exe()?.into_os_string()),
    ] {
        env.insert(key.into(), value);
    }
    let socket = percent_encoding::utf8_percent_encode(
        path_text(&directory.join("socket"))?,
        percent_encoding::NON_ALPHANUMERIC,
    )
    .to_string();
    env.insert(
        "TOLIGHT_LOCAL_DATABASE_URL".into(),
        format!(
            "postgresql://tolight@localhost:{}/tolight_dev?host={socket}",
            ports["pg"]
        )
        .into(),
    );
    let paths = std::iter::once(root.join(".local/pg18/bin"))
        .chain(std::env::split_paths(
            env.get(OsStr::new("PATH"))
                .map(OsString::as_os_str)
                .unwrap_or(OsStr::new("/usr/bin:/bin")),
        ))
        .collect::<Vec<_>>();
    env.insert("PATH".into(), std::env::join_paths(paths)?);
    env.entry("CARGO_HOME".into())
        .or_insert(root.join(".local/cargo").into_os_string());
    Ok(env)
}
fn command(program: impl AsRef<OsStr>, env: &Env) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .envs(env)
        .current_dir(c::root())
        .stdin(Stdio::null());
    cmd
}
fn output(mut cmd: Command, seconds: u64) -> Result<String> {
    let output = c::capture(&mut cmd, seconds, None)?;
    c::ensure(
        output.status.success(),
        "local tool failed; see private logs",
    )?;
    Ok(String::from_utf8(output.stdout)?.trim().into())
}
fn system_identifier(directory: &Path, env: &Env) -> Result<String> {
    let data = directory.join("postgres");
    c::regular(&data.join("global/pg_control"), true)?;
    c::ensure(
        fs::read_to_string(data.join("PG_VERSION"))?.trim() == "18",
        "local environment requires PostgreSQL18",
    )?;
    let mut cmd = command("pg_controldata", env);
    cmd.arg("-D").arg(data);
    let text = output(cmd, 15)?;
    let identity = text
        .lines()
        .find_map(|line| line.strip_prefix("Database system identifier:"))
        .map(str::trim)
        .ok_or("cannot verify PostgreSQL system identity")?;
    c::ensure(
        !identity.is_empty() && identity.chars().all(|c| c.is_ascii_digit()),
        "invalid system identity",
    )?;
    Ok(identity.into())
}
fn verify_cluster(directory: &Path, marker: &Value, env: &Env) -> Result {
    c::ensure(
        marker["postgres"]["system_identifier"].as_str()
            == Some(system_identifier(directory, env)?.as_str()),
        "PostgreSQL system identity differs from owned environment",
    )
}
fn verify_database(directory: &Path, marker: &Value, env: &Env, database: &str) -> Result {
    let mut cmd = command("psql", env);
    cmd.env("PGDATABASE",database).args(["-w","-XAt","-c","SELECT json_build_object('system_identifier',(SELECT system_identifier::text FROM pg_control_system()),'data_directory',current_setting('data_directory'),'database',current_database(),'major',current_setting('server_version_num')::int/10000)"]);
    let value: Value = serde_json::from_str(&output(cmd, 15)?)?;
    c::ensure(
        value["system_identifier"] == marker["postgres"]["system_identifier"]
            && value["data_directory"].as_str() == Some(path_text(&directory.join("postgres"))?)
            && value["database"] == database
            && value["major"] == 18,
        "connection does not identify this owned local PostgreSQL database",
    )
}
fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}
fn configs(
    directory: &Path,
    ports: &BTreeMap<String, u16>,
    lan: Option<&lan::Lan>,
    admin_ui: bool,
) -> Result<BTreeMap<String, String>> {
    let root = c::root();
    let archive = [
        root.join("scripts/wal-archive.sh")
            .to_string_lossy()
            .into_owned(),
        "archive".into(),
        "--archive-dir".into(),
        directory.join("archive").to_string_lossy().into_owned(),
        "--source".into(),
        "%p".into(),
        "--name".into(),
        "%f".into(),
        "--public-dir".into(),
        directory.join("public").to_string_lossy().into_owned(),
    ]
    .iter()
    .map(|s| shell_quote(s))
    .collect::<Vec<_>>()
    .join(" ");
    let quote = |s: &str| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"));
    let jsonpath = |p: PathBuf| serde_json::to_string(&p.to_string_lossy()).expect("path JSON");
    let scheme = if lan.is_some() { "https" } else { "http" };
    let mut hosts = vec![format!("127.0.0.1:{}", ports["api"])];
    if let Some(lan) = lan {
        hosts.push(format!("{}:{}", lan.ip, lan.api_port));
    }
    let proxy = if lan.is_some() {
        "[network]\ntrusted_proxies=[\"127.0.0.1\"]\nallow_direct_loopback=true\n"
    } else {
        ""
    };
    let admin = if admin_ui {
        format!(
            "[admin]\nenabled=true\nassets_dir={}\n",
            jsonpath(directory.join("operator-ui"))
        )
    } else {
        String::new()
    };
    Ok(BTreeMap::from([("config.toml".into(),format!("listen=\"127.0.0.1:{}\"\nparticipant_scheme=\"{scheme}\"\nparticipant_hosts={}\n[database]\nsecret_cmd=[\"/usr/bin/printenv\",\"TOLIGHT_LOCAL_DATABASE_URL\"]\n[public]\ndir={}\ndemo_banner=false\n[observability]\nlisten=\"127.0.0.1:{}\"\nlog_dir={}\n{proxy}{admin}",ports["api"],serde_json::to_string(&hosts)?,jsonpath(directory.join("public")),ports["ops"],jsonpath(directory.join("logs/service")))),
    ("postgres.conf".into(),format!("data_directory={}\nlisten_addresses=''\nport={}\nunix_socket_directories={}\nunix_socket_permissions=0700\nhba_file={}\nident_file={}\narchive_mode=on\narchive_timeout=0\narchive_library=''\nwal_level=replica\nmax_wal_senders=4\narchive_command={}\nmax_connections=40\nshared_buffers='32MB'\nfsync=on\ntimezone='UTC'\nlc_messages='C'\n",quote(path_text(&directory.join("postgres"))?),ports["pg"],quote(path_text(&directory.join("socket"))?),quote(path_text(&directory.join("pg_hba.conf"))?),quote(path_text(&directory.join("pg_ident.conf"))?),quote(&archive))),
    ("pg_hba.conf".into(),"local all tolight trust\nlocal replication tolight trust\nhost all all 127.0.0.1/32 reject\nhost all all ::1/128 reject\n".into()),("pg_ident.conf".into(),String::new())]))
}
fn check_configs(directory: &Path, marker: &Value) -> Result {
    // Missing admin_ui identifies the exact pre-admin generated configuration.
    // Only a normal up under the lifecycle lock upgrades the marker and bytes.
    for (name, expected) in configs(
        directory,
        &ports(marker)?,
        lan_settings(marker)?.as_ref(),
        marker["admin_ui"] == true,
    )? {
        c::regular(&directory.join(&name), true)?;
        c::ensure(
            fs::read_to_string(directory.join(name))? == expected,
            "managed configuration changed; refusing another database/listener",
        )?;
    }
    Ok(())
}
fn process_identity(pid: u32) -> Result<Option<String>> {
    if pid <= 1 {
        return Ok(None);
    }
    let mut cmd = Command::new("ps");
    cmd.args(["-p", &pid.to_string(), "-o", "lstart=", "-o", "command="])
        .env("LC_ALL", "C")
        .env("TZ", "UTC");
    let result = c::capture(&mut cmd, 3, None)?;
    Ok(
        (result.status.success() && !result.stdout.trim_ascii().is_empty())
            .then(|| format!("{:x}", Sha256::digest(result.stdout.trim_ascii()))),
    )
}
fn process_record(child: &Child) -> Result<Value> {
    Ok(json!({"pid":child.id(),"start_identity":process_identity(child.id())?}))
}
fn runtime(directory: &Path, marker: &Value) -> Result<Option<Value>> {
    if !c::regular(&directory.join("runtime.json"), false)? {
        return Ok(None);
    }
    let value = c::load(&directory.join("runtime.json"))?;
    c::ensure(
        value.get("control_protocol").is_none() || value["control_protocol"] == CONTROL_PROTOCOL,
        "unsupported local control protocol",
    )?;
    c::ensure(
        value["schema"] == RUNTIME
            && value["environment_id"] == marker["environment_id"]
            && value
                .get("tolight_binary")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(default_binary)
                == marker_binary(marker)?,
        "invalid local runtime ownership record",
    )?;
    for record in std::iter::once(&value["supervisor"]).chain(
        value["children"]
            .as_object()
            .ok_or("runtime children missing")?
            .values(),
    ) {
        let pid = u32::try_from(record["pid"].as_u64().ok_or("invalid process ID")?)?;
        let identity = record["start_identity"]
            .as_str()
            .ok_or("missing process identity")?;
        c::ensure(
            process_identity(pid)?.as_deref() == Some(identity),
            "stale or mismatched process identity",
        )?;
    }
    let expected = if value["mode"] == "maintenance" {
        vec!["postgres"]
    } else if lan_settings(marker)?.is_some() {
        vec!["postgres", "serve", "web", "lan"]
    } else {
        vec!["postgres", "serve", "web"]
    };
    c::ensure(
        lan_settings(&value)? == lan_settings(marker)?,
        "runtime LAN configuration differs from owned environment",
    )?;
    let children = value["children"].as_object().ok_or("invalid children")?;
    c::ensure(
        children.len() == expected.len()
            && expected.iter().all(|name| children.contains_key(*name)),
        "runtime child ownership record incomplete",
    )?;
    Ok(Some(value))
}
fn control_ownership(runtime: &Value) -> Result<String> {
    let binary = runtime
        .get("tolight_binary")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(default_binary);
    let ownership = json!({"children":runtime["children"],"mode":runtime["mode"],"lan":runtime["lan"],"tolight_binary":binary});
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&ownership)?)
    ))
}
#[cfg(unix)]
fn read_control_response(stream: impl Read) -> Result<Value> {
    let mut line = String::new();
    BufReader::new(stream)
        .take(CONTROL_RESPONSE_MAX_BYTES + 1)
        .read_line(&mut line)?;
    c::ensure(
        line.len() as u64 <= CONTROL_RESPONSE_MAX_BYTES,
        "oversized control response",
    )?;
    Ok(serde_json::from_str(&line)?)
}
#[cfg(unix)]
fn control(directory: &Path, marker: &Value, runtime: &Value, operation: &str) -> Result<Value> {
    let path = directory.join("control.sock");
    c::ensure(
        fs::symlink_metadata(&path)?.file_type().is_socket(),
        "supervisor control socket unavailable",
    )?;
    let request = json!({"operation":operation,"control_protocol":CONTROL_PROTOCOL,"ownership_sha256":control_ownership(runtime)?,"environment_id":marker["environment_id"],"run_id":runtime["run_id"],"supervisor_pid":runtime["supervisor"]["pid"]});
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    let response = read_control_response(stream)?;
    c::ensure(
        response["ok"] == true
            && response.get("control_protocol") == runtime.get("control_protocol")
            && ["environment_id", "run_id", "supervisor_pid"]
                .iter()
                .all(|k| response[*k] == request[*k])
            && ["children", "mode"]
                .iter()
                .all(|k| response[*k] == runtime[*k])
            && response["lan"] == runtime["lan"]
            && response
                .get("tolight_binary")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(default_binary)
                == marker_binary(marker)?,
        "supervisor handshake differs from environment/run/process",
    )?;
    Ok(response)
}
fn status(directory: &Path) -> Result<Value> {
    let mut result = json!({"schema":"tolight-local-status/1","directory":directory,"environment_id":null,"initialized":false,"phase":"uninitialized","running":false,"runtime_valid":false,"mode":"stopped","supervisor":null,"config":directory.join("config.toml")});
    if !directory.exists() {
        return Ok(result);
    }
    let marker = marker(directory)?;
    result["automation"] = automation::stored_status(directory);
    result["automation"]["running"] = json!(false);
    let ports = ports(&marker)?;
    result["environment_id"] = marker["environment_id"].clone();
    result["initialized"] = json!(marker["phase"] == "ready");
    result["phase"] = marker["phase"].clone();
    result["tolight_binary"] = json!(marker_binary(&marker)?);
    result["binary_mode"] = marker
        .get("binary_mode")
        .cloned()
        .unwrap_or(json!("source"));
    result["origins"] = json!(
        ports
            .iter()
            .filter(|(k, _)| k.as_str() != "pg")
            .map(|(k, v)| (k.clone(), format!("http://127.0.0.1:{v}")))
            .collect::<BTreeMap<_, _>>()
    );
    if marker["admin_ui"] == true {
        result["admin_url"] = json!(format!("http://127.0.0.1:{}/admin/", ports["ops"]));
    }
    if let Some(lan) = lan_settings(&marker)? {
        result["lan"] = lan.describe(directory)?;
    }
    result["postgres"] = json!({"major":18,"system_identifier":marker["postgres"]["system_identifier"],"running":false,"data_directory":directory.join("postgres")});
    if marker["phase"] == "ready" {
        let env = environment(directory, &ports)?;
        verify_cluster(directory, &marker, &env)?;
        result["postgres"]["running"] =
            json!(verify_database(directory, &marker, &env, "tolight_dev").is_ok());
        match runtime(directory, &marker).and_then(|runtime| {
            runtime
                .map(|r| control(directory, &marker, &r, "status").map(|response| (r, response)))
                .transpose()
        }) {
            Ok(Some((runtime, response))) => {
                result["runtime_valid"] = json!(true);
                result["running"] =
                    json!(response["ready"] == true && result["postgres"]["running"] == true);
                result["supervisor"] = runtime["supervisor"].clone();
                result["mode"] = runtime["mode"].clone();
                result["automation"] = response.get("automation").cloned().unwrap_or_else(
                    || json!({"enabled":false,"running":false,"restart_required":true}),
                );
            }
            Ok(None) => {}
            Err(_) => {
                result["runtime_error"] =
                    json!("runtime/supervisor/child identity could not be verified")
            }
        };
        let orphan = result["postgres"]["running"] == true && result["runtime_valid"] != true;
        result["orphaned"] = json!(orphan);
        if orphan {
            result["mode"] = json!("orphaned");
            result["recovery_required"] = json!(
                "Owned PostgreSQL remains live without a verified supervisor; automatic PID takeover/kill is refused."
            );
        }
    }
    Ok(result)
}
fn signal_child(child: &mut Child, signal: &str) -> Result {
    if child.try_wait()?.is_none() {
        let status = Command::new("/bin/kill")
            .arg(format!("-{signal}"))
            .arg(child.id().to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        c::ensure(status.success(), "could not signal owned child")?;
    }
    Ok(())
}
fn wait_child(child: &mut Child, seconds: u64) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn group_signal(child: &Child, signal: &str) -> bool {
    // Every caller spawned this Child with process_group(0); no saved PID is used.
    Command::new("/bin/kill")
        .args([signal, "--", &format!("-{}", child.id())])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
fn wait_group(child: &mut Child, seconds: u64) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if child.try_wait()?.is_some() && !group_signal(child, "-0") {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn stop_child(child: &mut Child, postgres: bool) -> Result<bool> {
    if !postgres {
        if child.try_wait()?.is_some() && !group_signal(child, "-0") {
            return Ok(true);
        }
        group_signal(child, "-TERM");
        if wait_group(child, 10)? {
            return Ok(true);
        }
        group_signal(child, "-KILL");
        let _ = child.kill();
        return wait_group(child, 5);
    }
    if child.try_wait()?.is_some() {
        return Ok(true);
    }
    // Postmaster controls its own workers and WAL shutdown. Never group-KILL it.
    signal_child(child, "INT")?;
    wait_child(child, 20)
}
struct Supervisor {
    directory: PathBuf,
    children: BTreeMap<String, Child>,
    listener: Option<UnixListener>,
    runtime_written: bool,
    cleaned: bool,
    automation: Option<automation::Worker>,
    _lock: File,
}
impl Supervisor {
    fn cleanup(&mut self) -> bool {
        if self.cleaned {
            return true;
        }
        let mut clean = true;
        if let Some(worker) = &mut self.automation {
            clean = worker.shutdown() && clean;
        }
        // Wake participant long polls and finish their HTTP responses while the
        // TLS proxy is still alive. Draining the proxy first otherwise cuts
        // valid reads at its shorter graceful-shutdown deadline.
        for name in ["serve", "lan", "web", "postgres"] {
            if let Some(child) = self.children.get_mut(name) {
                clean = stop_child(child, name == "postgres").unwrap_or(false) && clean;
            }
        }
        self.listener.take();
        if self.runtime_written && clean {
            for name in ["runtime.json", "control.sock"] {
                if let Err(e) = fs::remove_file(self.directory.join(name))
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    clean = false;
                }
            }
        }
        self.cleaned = true;
        clean
    }
    fn start(&mut self, name: &str, mut cmd: Command) -> Result {
        let log = c::private_open(
            &self.directory.join("logs").join(format!("{name}.log")),
            true,
        )?;
        cmd.stdout(log.try_clone()?)
            .stderr(log)
            .stdin(Stdio::null());
        #[cfg(unix)]
        cmd.process_group(0);
        self.children.insert(name.into(), cmd.spawn()?);
        Ok(())
    }
    fn alive(&mut self) -> Result<bool> {
        for child in self.children.values_mut() {
            if child.try_wait()?.is_some() {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        if !self.cleanup() {
            eprintln!(
                "owned PostgreSQL did not stop normally; data retained without arbitrary PID signal: {}",
                self.directory.display()
            );
        }
    }
}
struct StartupCommand(Child);
impl Drop for StartupCommand {
    fn drop(&mut self) {
        let _ = stop_child(&mut self.0, false);
    }
}
fn logged(mut command: Command, directory: &Path, name: &str, stop: &AtomicBool) -> Result {
    let file = c::private_open(&directory.join("logs").join(name), true)?;
    command.stdout(file.try_clone()?).stderr(file);
    #[cfg(unix)]
    command.process_group(0);
    let mut owned = StartupCommand(command.spawn()?);
    loop {
        if let Some(status) = owned.0.try_wait()? {
            c::ensure(
                !group_signal(&owned.0, "-0"),
                "local command left background processes; startup refused",
            )?;
            return c::ensure(
                status.success(),
                &format!("local command failed; see private logs/{name}"),
            );
        }
        if stop.load(Ordering::Acquire) {
            return Err("startup interrupted; partial initialization retained".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn http_status(port: u16, path: &str) -> Result<u16> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(500))?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    let mut line = String::new();
    BufReader::new(stream).take(1024).read_line(&mut line)?;
    Ok(line
        .split_whitespace()
        .nth(1)
        .ok_or("invalid HTTP status")?
        .parse()?)
}
fn lock_lifecycle(directory: &Path, create: bool) -> Result<File> {
    let path = directory.join("lifecycle.lock");
    c::regular(&path, !create)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path)?;
    file.try_lock().map_err(
        |_| "this environment already has a foreground supervisor or startup in progress",
    )?;
    Ok(file)
}
fn up(
    directory: &Path,
    selected: Option<PathBuf>,
    maintenance: bool,
    overrides: [Option<u16>; 4],
    options: UpOptions,
    stop: Arc<AtomicBool>,
) -> Result {
    let UpOptions {
        lan: lan_options,
        automation: automation_options,
    } = options;
    // Validate a requested exposure before creating files or starting children.
    let previous_lan = if directory.join("environment.json").exists() {
        lan_settings(&marker(directory)?)?
    } else {
        None
    };
    let selected_lan = lan_options.resolve(previous_lan)?;
    if directory.exists() {
        safe_paths(directory)?;
        if directory.join("environment.json").exists() {
            marker(directory)?;
        } else {
            c::ensure(
                fs::read_dir(directory)?.next().is_none(),
                "refusing an existing non-owned nonempty directory without modifying it",
            )?;
        }
    }
    private_directory(directory)?;
    safe_paths(directory)?;
    let lock = lock_lifecycle(directory, true)?;
    let mut supervisor = Supervisor {
        directory: directory.into(),
        children: BTreeMap::new(),
        listener: None,
        runtime_written: false,
        cleaned: false,
        automation: None,
        _lock: lock,
    };
    let marker_path = directory.join("environment.json");
    let mut metadata = if marker_path.exists() {
        let marker = marker(directory)?;
        c::ensure(
            marker["phase"] != "initializing",
            "initialization was interrupted; files retained, refusing reinitialization",
        )?;
        marker
    } else {
        c::ensure(
            fs::read_dir(directory)?
                .all(|entry| entry.is_ok_and(|entry| entry.file_name() == "lifecycle.lock")),
            "refusing existing non-owned directory",
        )?;
        let marker = json!({"schema":MARKER,"directory":directory,"environment_id":uuid::Uuid::new_v4(),"phase":"prepared","ports":{"api":8090,"web":8080,"ops":8091,"pg":5432},"postgres":{"major":18,"database":"tolight_dev","user":"tolight","system_identifier":null}});
        c::save(&marker_path, &marker)?;
        marker
    };
    let mut selected_ports = ports(&metadata)?;
    for (name, value) in ["api", "web", "ops", "pg"].into_iter().zip(overrides) {
        if let Some(value) = value {
            selected_ports.insert(name.into(), value);
        }
    }
    let mut check = metadata.clone();
    check["ports"] = json!(selected_ports);
    ports(&check)?;
    c::ensure(
        selected_lan.is_none() || ![80, 443].contains(&selected_ports["api"]),
        "LAN mode requires a non-default loopback API port (use 8090); HTTP/HTTPS default-port signature normalization differs",
    )?;
    if !maintenance {
        let mut held = Vec::new();
        for name in ["api", "web", "ops"] {
            held.push(
                TcpListener::bind(("127.0.0.1", selected_ports[name]))
                    .map_err(|_| "a requested loopback port is already in use")?,
            );
        }
        if let Some(lan) = &selected_lan {
            for port in [lan.web_port, lan.api_port] {
                held.push(
                    TcpListener::bind((lan.ip, port)).map_err(
                        |_| "requested LAN address is unavailable or port already in use",
                    )?,
                );
            }
            lan::caddy()?;
        }
        drop(held);
    }
    let env = environment(directory, &selected_ports)?;
    let mut version = command("initdb", &env);
    version.arg("--version");
    c::ensure(
        output(version, 15)?
            .split_whitespace()
            .any(|part| part.starts_with("18.")),
        "PostgreSQL18 tools required",
    )?;
    if metadata["phase"] == "ready" {
        verify_cluster(directory, &metadata, &env)?;
        check_configs(directory, &metadata)?;
        if directory.join("postgres/postmaster.pid").exists() {
            let mut cmd = command("pg_ctl", &env);
            cmd.arg("-D").arg(directory.join("postgres")).arg("status");
            c::ensure(
                !c::capture(&mut cmd, 5, None)?.status.success(),
                "owned PostgreSQL already running without this supervisor; refusing takeover",
            )?;
        }
    }
    for name in [
        "logs",
        "socket",
        "public",
        "archive",
        "backups",
        "backups/logical",
        "backups/physical",
        "operator-ui",
    ] {
        private_directory(&directory.join(name))?;
    }
    let source = selected.is_none();
    let selected = if let Some(selected) = selected {
        binary(&selected)?
    } else {
        let mut cmd = command("cargo", &env);
        cmd.args(["build", "--locked", "-p", "tolight-cli"]);
        logged(cmd, directory, "build.log", &stop)?;
        binary(&default_binary())?
    };
    if source && !maintenance {
        let mut cmd = command("npm", &env);
        cmd.args(["run", "build:web"]);
        logged(cmd,directory,"web-build.log",&stop).map_err(|_|"TypeScript build failed; install locked development dependencies with npm ci before local up")?;
    }
    if !maintenance {
        let asset = c::root().join(".local/build/operator-ui/admin.js");
        c::regular(&asset, true)?;
        let bytes = fs::read(&asset)?;
        c::ensure(
            !bytes.is_empty() && bytes.len() <= 1024 * 1024,
            "admin JavaScript asset missing or too large; run npm run build:web first",
        )?;
        let mut target = c::private_open(&directory.join("operator-ui/admin.js"), false)?;
        target.write_all(&bytes)?;
        target.sync_all()?;
    }
    metadata["tolight_binary"] = json!(selected);
    metadata["binary_mode"] = json!(if source { "source" } else { "prebuilt" });
    metadata["admin_ui"] = json!(true);
    metadata["ports"] = json!(selected_ports);
    if let Some(lan) = &selected_lan {
        metadata["lan"] = serde_json::to_value(lan)?;
    } else {
        metadata
            .as_object_mut()
            .expect("owned marker")
            .remove("lan");
    }
    for (name, text) in configs(directory, &selected_ports, selected_lan.as_ref(), true)? {
        let mut file = c::private_open(&directory.join(name), false)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    let fresh = metadata["phase"] == "prepared";
    if fresh {
        c::ensure(
            !directory.join("postgres").exists(),
            "prepared environment contains PostgreSQL data; refusing blind init",
        )?;
        metadata["phase"] = json!("initializing");
        c::save(&marker_path, &metadata)?;
        let mut cmd = command("initdb", &env);
        cmd.arg("-D").arg(directory.join("postgres")).args([
            "-U",
            "tolight",
            "--encoding=UTF8",
            "--locale=C",
            "--auth-local=trust",
            "--auth-host=reject",
        ]);
        logged(cmd, directory, "initdb.log", &stop)?;
        metadata["postgres"]["system_identifier"] = json!(system_identifier(directory, &env)?);
    } else {
        no_symlinks(&directory.join("postgres"))?;
        for name in ["recovery.signal", "standby.signal"] {
            c::ensure(
                !directory.join("postgres").join(name).exists(),
                "local up does not start recovery/standby data",
            )?;
        }
        let auto = directory.join("postgres/postgresql.auto.conf");
        c::regular(&auto, true)?;
        c::ensure(
            fs::read_to_string(auto)?
                .lines()
                .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#')),
            "postgresql.auto.conf overrides are unsupported",
        )?;
    }
    c::save(&marker_path, &metadata)?;
    let mut cmd = command("postgres", &env);
    cmd.arg("-D")
        .arg(directory.join("postgres"))
        .arg("-c")
        .arg(format!(
            "config_file={}",
            directory.join("postgres.conf").display()
        ));
    supervisor.start("postgres", cmd)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        c::ensure(
            !stop.load(Ordering::Acquire) && supervisor.alive()?,
            "PostgreSQL startup stopped; data retained",
        )?;
        if verify_database(directory, &metadata, &env, "postgres").is_ok() {
            break;
        }
        c::ensure(
            Instant::now() < deadline,
            "owned PostgreSQL failed readiness/identity verification",
        )?;
        std::thread::sleep(Duration::from_millis(100));
    }
    let cli = |args: &[&str]| {
        let mut cmd = command(&selected, &env);
        cmd.arg("--config")
            .arg(directory.join("config.toml"))
            .args(args);
        cmd
    };
    let install = || {
        let mut cmd = cli(&["publish", "--install-assets"]);
        cmd.arg(c::root().join("public"))
            .arg("--docs")
            .arg(c::root().join("docs"))
            .arg("--examples")
            .arg(c::root().join("examples/python"));
        cmd
    };
    if fresh {
        let mut cmd = command("createdb", &env);
        cmd.env("PGDATABASE", "postgres")
            .args(["-w", "tolight_dev"]);
        logged(cmd, directory, "createdb.log", &stop)?;
        verify_database(directory, &metadata, &env, "tolight_dev")?;
        logged(cli(&["migrate"]), directory, "migrate.log", &stop)?;
        logged(install(), directory, "assets.log", &stop)?;
        logged(
            cli(&["publish", "--rebuild"]),
            directory,
            "rebuild.log",
            &stop,
        )?;
        metadata["phase"] = json!("ready");
        c::save(&marker_path, &metadata)?;
    }
    verify_database(directory, &metadata, &env, "tolight_dev")?;
    if !fresh && !maintenance {
        logged(install(), directory, "assets.log", &stop)?;
    }
    if !maintenance {
        supervisor.start("serve", cli(&["serve"]))?;
        let mut cmd = command(&selected, &env);
        cmd.arg("serve-public")
            .arg("--dir")
            .arg(directory.join("public"))
            .arg("--listen")
            .arg(format!("127.0.0.1:{}", selected_ports["web"]));
        supervisor.start("web", cmd)?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            c::ensure(
                !stop.load(Ordering::Acquire) && supervisor.alive()?,
                "owned local child exited during startup",
            )?;
            if http_status(selected_ports["ops"], "/healthz").ok() == Some(200)
                && http_status(selected_ports["api"], "/participant/v1/self").ok() == Some(401)
                && http_status(selected_ports["web"], "/data/manifest.json").ok() == Some(200)
            {
                break;
            }
            c::ensure(
                Instant::now() < deadline,
                "local services did not become ready",
            )?;
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(lan) = &selected_lan {
            let command =
                lan::prepare(directory, lan, selected_ports["api"], selected_ports["web"])?;
            supervisor.start("lan", command)?;
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                c::ensure(
                    !stop.load(Ordering::Acquire) && supervisor.alive()?,
                    "LAN gateway stopped during startup; see private logs/lan.log",
                )?;
                if lan::ready(directory, lan) {
                    break;
                }
                c::ensure(
                    Instant::now() < deadline,
                    "LAN TLS gateway did not become ready",
                )?;
                std::thread::sleep(Duration::from_millis(100));
            }
            lan::publish_bootstrap(directory, lan)?;
        } else {
            lan::remove_bootstrap(directory)?;
        }
    }
    let control_path = directory.join("control.sock");
    if let Ok(metadata) = fs::symlink_metadata(&control_path) {
        c::ensure(
            metadata.file_type().is_socket(),
            "refusing non-socket control path",
        )?;
        fs::remove_file(&control_path)?;
    }
    let listener = UnixListener::bind(&control_path)?;
    fs::set_permissions(&control_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    supervisor.listener = Some(listener);
    let children: serde_json::Map<String, Value> = supervisor
        .children
        .iter()
        .map(|(name, child)| process_record(child).map(|record| (name.clone(), record)))
        .collect::<Result<_>>()?;
    let runtime = json!({"schema":RUNTIME,"control_protocol":CONTROL_PROTOCOL,"environment_id":metadata["environment_id"],"run_id":uuid::Uuid::new_v4(),"tolight_binary":selected,"mode":if maintenance{"maintenance"}else{"normal"},"supervisor":{"pid":std::process::id(),"start_identity":process_identity(std::process::id())?},"children":children,"ports":selected_ports,"lan":selected_lan,"automation":automation_options.describe(maintenance)});
    c::save(&directory.join("runtime.json"), &runtime)?;
    supervisor.runtime_written = true;
    let ownership = control_ownership(&runtime)?;
    if !maintenance && !automation_options.no_automation {
        crate::ops::storage::reset_cancel();
        c::COMMAND_CANCELLED.store(false, Ordering::Release);
        let database = env
            .get(OsStr::new("TOLIGHT_LOCAL_DATABASE_URL"))
            .and_then(|value| value.to_str())
            .ok_or("managed database URL unavailable")?;
        supervisor.automation = Some(automation::Worker::start(
            directory,
            &runtime,
            database,
            &automation_options,
        )?);
    }
    println!(
        "{}",
        json!({"event":"ready","environment_id":metadata["environment_id"],"directory":directory,"origins":selected_ports.iter().filter(|(k,_)|k.as_str()!="pg").map(|(k,v)|(k,format!("http://127.0.0.1:{v}"))).collect::<BTreeMap<_,_>>(),"admin_url":format!("http://127.0.0.1:{}/admin/",selected_ports["ops"]),"lan":selected_lan.as_ref().map(|lan|lan.describe(directory)).transpose()?})
    );
    std::io::stdout().flush()?;
    while !stop.load(Ordering::Acquire) {
        c::ensure(
            supervisor.alive()?,
            "an owned local child exited; stopping remaining owned children",
        )?;
        match supervisor
            .listener
            .as_ref()
            .expect("control listener")
            .accept()
        {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                let mut line = String::new();
                let read = BufReader::new(&mut stream)
                    .take(CONTROL_REQUEST_MAX_BYTES + 1)
                    .read_line(&mut line);
                if read.is_ok()
                    && line.len() as u64 <= CONTROL_REQUEST_MAX_BYTES
                    && let Ok(request) = serde_json::from_str::<Value>(&line)
                {
                    let stop_requested = request["operation"] == "stop_verified";
                    let valid = ["environment_id", "run_id"]
                        .iter()
                        .all(|k| request[*k] == runtime[*k])
                        && request["supervisor_pid"] == runtime["supervisor"]["pid"]
                        && (request["operation"] == "status"
                            || (stop_requested
                                && request["control_protocol"] == CONTROL_PROTOCOL
                                && request["ownership_sha256"].as_str() == Some(&ownership)));
                    let mut response = json!({"ok":valid,"ready":valid,"control_protocol":CONTROL_PROTOCOL,"environment_id":runtime["environment_id"],"run_id":runtime["run_id"],"supervisor_pid":runtime["supervisor"]["pid"],"children":runtime["children"],"mode":runtime["mode"],"tolight_binary":runtime["tolight_binary"],"lan":runtime["lan"]});
                    if request["operation"] == "status" {
                        response["automation"] = supervisor
                            .automation
                            .as_ref()
                            .map(automation::Worker::snapshot)
                            .unwrap_or_else(
                                || json!({"configuration":runtime["automation"],"running":false}),
                            );
                    }
                    let encoded = serde_json::to_vec(&response)?;
                    c::ensure(
                        (encoded.len() as u64) < CONTROL_RESPONSE_MAX_BYTES,
                        "oversized control response",
                    )?;
                    let _ = stream.write_all(&encoded);
                    let _ = stream.write_all(b"\n");
                    if valid && stop_requested {
                        stop.store(true, Ordering::Release);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(error) => return Err(error.into()),
        }
    }
    c::ensure(
        supervisor.cleanup(),
        "owned PostgreSQL did not stop normally; data retained",
    )
}
fn down(directory: &Path) -> Result {
    let marker = marker(directory)?;
    let env = environment(directory, &ports(&marker)?)?;
    if marker["phase"] == "ready" {
        verify_cluster(directory, &marker, &env)?;
    }
    let Some(runtime) = runtime(directory, &marker)? else {
        let _lock = lock_lifecycle(directory, false)?;
        if marker["phase"] == "ready" {
            c::ensure(
                verify_database(directory, &marker, &env, "tolight_dev").is_err(),
                "owned PostgreSQL live without verified supervisor; refusing takeover",
            )?;
        }
        println!(
            "{}",
            json!({"stopped":true,"environment_id":marker["environment_id"],"already_stopped":true})
        );
        return Ok(());
    };
    let operation = if runtime.get("control_protocol").is_some() {
        // An older supervisor cannot mistake this name for its weaker legacy
        // stop operation, even if its runtime capability is falsely upgraded.
        "stop_verified"
    } else {
        // Only a genuinely legacy response (also without a capability) can
        // authorize the original read-only preflight followed by legacy stop.
        control(directory, &marker, &runtime, "status")?;
        "stop"
    };
    control(directory, &marker, &runtime, operation)?;
    // Three application groups can each need 10s TERM + 5s KILL, then PG 20s.
    let deadline = Instant::now() + Duration::from_secs(70);
    while directory.join("runtime.json").exists() {
        c::ensure(
            Instant::now() < deadline,
            "supervisor shutdown did not complete; no fallback PID kill",
        )?;
        std::thread::sleep(Duration::from_millis(100));
    }
    println!(
        "{}",
        json!({"stopped":true,"environment_id":marker["environment_id"]})
    );
    Ok(())
}
fn operational(directory: &Path) -> Result<(Value, Env, Value)> {
    let marker = marker(directory)?;
    c::ensure(marker["phase"] == "ready", "environment is not initialized")?;
    let env = environment(directory, &ports(&marker)?)?;
    verify_cluster(directory, &marker, &env)?;
    check_configs(directory, &marker)?;
    let runtime = runtime(directory, &marker)?.ok_or("start owned environment with up first")?;
    control(directory, &marker, &runtime, "status")?;
    verify_database(directory, &marker, &env, "tolight_dev")?;
    Ok((marker, env, runtime))
}
fn checked_arguments(args: &Args) -> Result<(PathBuf, Option<PathBuf>)> {
    let directory = checked_directory(&args.directory)?;
    let selected = args.tolight.as_deref().map(binary).transpose()?;
    if !matches!(&args.command, Action::Up { .. })
        && let Some(selected) = &selected
        && directory.join("environment.json").exists()
    {
        c::ensure(
            *selected == marker_binary(&marker(&directory)?)?,
            "--tolight differs from environment binary",
        )?;
    }
    Ok((directory, selected))
}
fn execute(args: Args, stop: Arc<AtomicBool>) -> Result {
    let (directory, selected) = checked_arguments(&args)?;
    match args.command {
        Action::Status => {
            println!("{}", status(&directory)?);
            Ok(())
        }
        Action::Down => down(&directory),
        Action::Up {
            maintenance,
            api_port,
            web_port,
            ops_port,
            pg_port,
            lan,
            automation,
        } => up(
            &directory,
            selected,
            maintenance,
            [api_port, web_port, ops_port, pg_port],
            UpOptions { lan, automation },
            stop,
        ),
        action => {
            let (marker, env, runtime) = operational(&directory)?;
            let selected = binary(&marker_binary(&marker)?)?;
            match action {
                Action::Migrate | Action::Publish { .. } => {
                    c::ensure(
                        runtime["mode"] == "maintenance",
                        "explicit migrate/publish requires up --maintenance",
                    )?;
                    let mut cmd = command(selected, &env);
                    cmd.arg("--config").arg(directory.join("config.toml")).arg(
                        if matches!(&action, Action::Migrate) {
                            "migrate"
                        } else {
                            "publish"
                        },
                    );
                    if matches!(&action, Action::Publish { rebuild: true }) {
                        cmd.arg("--rebuild");
                    }
                    let status = cmd
                        .stdin(Stdio::inherit())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .status()?;
                    if status.success() {
                        Ok(())
                    } else {
                        Err(Box::new(crate::ops::ExitCode(status.code().unwrap_or(1))))
                    }
                }
                Action::Operator { arguments } => {
                    c::ensure(
                        !arguments.is_empty()
                            && arguments.iter().all(|v| {
                                v != "--config" && !v.to_string_lossy().starts_with("--config=")
                            }),
                        "operator requires a subcommand and cannot override managed config",
                    )?;
                    let status = command(selected, &env)
                        .arg("--config")
                        .arg(directory.join("config.toml"))
                        .arg("operator")
                        .args(arguments)
                        .stdin(Stdio::inherit())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .status()?;
                    if status.success() {
                        Ok(())
                    } else {
                        Err(Box::new(crate::ops::ExitCode(status.code().unwrap_or(1))))
                    }
                }
                Action::Backup { kind } => {
                    let _lock = c::lock(&directory.join("backup-operation"))?;
                    let mut outputs = serde_json::Map::new();
                    let database = env
                        .get(OsStr::new("TOLIGHT_LOCAL_DATABASE_URL"))
                        .and_then(|value| value.to_str())
                        .ok_or("managed database URL unavailable")?;
                    for name in if kind == "both" {
                        vec!["logical", "physical"]
                    } else {
                        vec![kind.as_str()]
                    } {
                        c::ensure(!stop.load(Ordering::Acquire), "backup interrupted")?;
                        let report = if name == "logical" {
                            crate::ops::storage::backup::logical_report_with_database(
                                crate::ops::storage::Backup {
                                    config: Some(directory.join("config.toml")),
                                    output_dir: directory.join("backups/logical"),
                                    public_dir: None,
                                    retention_days: 14,
                                },
                                database,
                            )?
                        } else {
                            crate::ops::storage::backup::physical_report_with_database(
                                crate::ops::storage::PhysicalBackup {
                                    config: directory.join("config.toml"),
                                    output_dir: directory.join("backups/physical"),
                                    archive_dir: directory.join("archive"),
                                },
                                database,
                            )?
                        };
                        outputs.insert(name.into(), report);
                    }
                    println!(
                        "{}",
                        json!({"environment_id":marker["environment_id"],"backups":outputs})
                    );
                    Ok(())
                }
                _ => unreachable!(),
            }
        }
    }
}
pub async fn run(args: Args) -> Result {
    if matches!(&args.command, Action::Check) {
        c::COMMAND_CANCELLED.store(false, Ordering::Release);
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let check = async move {
            let snapshot = tokio::task::spawn_blocking(move || -> Result<Value> {
                let (directory, _) = checked_arguments(&args)?;
                let snapshot = status(&directory)?;
                if snapshot["initialized"] == true {
                    let owned = marker(&directory)?;
                    check_configs(&directory, &owned)?;
                    binary(&marker_binary(&owned)?)?;
                }
                Ok(snapshot)
            })
            .await??;
            super::check::run(snapshot).await
        };
        return tokio::select! {
            result=check=>result,
            _=terminate.recv()=>{c::COMMAND_CANCELLED.store(true,Ordering::Release);Err("local check interrupted".into())},
            _=interrupt.recv()=>{c::COMMAND_CANCELLED.store(true,Ordering::Release);Err("local check interrupted".into())},
        };
    }
    let is_up = matches!(&args.command, Action::Up { .. });
    let is_backup = matches!(&args.command, Action::Backup { .. });
    if is_backup {
        crate::ops::storage::reset_cancel();
        c::COMMAND_CANCELLED.store(false, Ordering::Release);
    }
    // Register both receivers before startup can create subprocesses.
    let signals = if is_up || is_backup {
        Some((
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
        ))
    } else {
        None
    };
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let mut worker = tokio::task::spawn_blocking(move || execute(args, worker_stop));
    if let Some((mut terminate, mut interrupt)) = signals {
        tokio::select! {result=&mut worker=>return result?,_=interrupt.recv()=>{},_=terminate.recv()=>{}}
        stop.store(true, Ordering::Release);
        if is_backup {
            crate::ops::storage::request_cancel();
            c::COMMAND_CANCELLED.store(true, Ordering::Release);
        }
    }
    worker.await?
}

#[cfg(all(test, unix))]
mod startup_process_tests {
    use super::*;

    #[test]
    fn control_socket_accepts_larger_bounded_frames_and_refuses_oversized_ones() {
        for (bytes, accepted) in [(8192, true), (CONTROL_RESPONSE_MAX_BYTES as usize, false)] {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            reader
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            writer
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let sender = std::thread::spawn(move || {
                let payload = json!({"payload":"x".repeat(bytes)});
                let _ = serde_json::to_writer(&mut writer, &payload);
                let _ = writer.write_all(b"\n");
            });
            let result = read_control_response(reader);
            if accepted {
                assert_eq!(result.unwrap()["payload"].as_str().unwrap().len(), bytes);
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("oversized control response")
                );
            }
            sender.join().unwrap();
        }
    }

    #[test]
    fn legacy_config_verification_stays_exact_until_the_owned_marker_is_upgraded() {
        let directory = tempfile::tempdir().unwrap();
        let ports = BTreeMap::from([
            ("api".into(), 8090),
            ("web".into(), 8080),
            ("ops".into(), 8091),
            ("pg".into(), 5432),
        ]);
        let mut marker = json!({"ports":ports});
        let legacy = configs(directory.path(), &ports, None, false).unwrap();
        for (name, text) in &legacy {
            fs::write(directory.path().join(name), text).unwrap();
        }
        check_configs(directory.path(), &marker).unwrap();
        marker["admin_ui"] = json!(true);
        assert!(check_configs(directory.path(), &marker).is_err());
        let upgraded = configs(directory.path(), &ports, None, true).unwrap();
        assert!(upgraded["config.toml"].contains("[admin]\nenabled=true"));
        for (name, text) in &upgraded {
            fs::write(directory.path().join(name), text).unwrap();
        }
        check_configs(directory.path(), &marker).unwrap();
        fs::write(directory.path().join("config.toml"), &legacy["config.toml"]).unwrap();
        assert!(check_configs(directory.path(), &marker).is_err());
    }

    fn delayed_startup(marker: &Path, wait: bool) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                if wait {
                    "(sleep 2; printf late > \"$1\") & printf ready > \"$2\"; wait"
                } else {
                    "(sleep 2; printf late > \"$1\") & exit 0"
                },
                "local-startup-test",
            ])
            .arg(marker)
            .arg(marker.with_extension("ready"));
        command
    }

    #[test]
    fn interrupted_startup_does_not_leave_a_background_writer() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("logs")).unwrap();
        let marker = directory.path().join("late.txt");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let path = directory.path().to_path_buf();
        let command = delayed_startup(&marker, true);
        let worker = std::thread::spawn(move || logged(command, &path, "build.log", &worker_stop));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !marker.with_extension("ready").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let ready = marker.with_extension("ready").exists();
        stop.store(true, Ordering::Release);
        let result = worker.join().unwrap();
        std::thread::sleep(Duration::from_millis(2200));
        assert!(ready, "startup fixture failed before creating its child");
        assert!(result.is_err());
        assert!(!marker.exists(), "interrupted build left a live descendant");
    }

    #[test]
    fn startup_leader_success_does_not_accept_or_leave_a_background_writer() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("logs")).unwrap();
        let marker = directory.path().join("late.txt");
        let result = logged(
            delayed_startup(&marker, false),
            directory.path(),
            "build.log",
            &AtomicBool::new(false),
        );
        std::thread::sleep(Duration::from_millis(2200));
        assert!(
            result.is_err(),
            "startup acknowledged an unfinished background build"
        );
        assert!(!marker.exists());
    }
}
