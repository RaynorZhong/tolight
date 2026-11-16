//! Shared private-file and native PostgreSQL process helpers.
use super::{Result, root};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub static CANCELLED: AtomicBool = AtomicBool::new(false);
pub type Env = BTreeMap<OsString, OsString>;
pub fn base_env() -> Env {
    std::env::vars_os()
        .filter(|(key, _)| !key.to_string_lossy().starts_with("PG"))
        .collect()
}
pub fn program(name: &str) -> PathBuf {
    let path = root().join(".local/pg18/bin").join(name);
    if path.is_file() {
        path
    } else {
        PathBuf::from(name)
    }
}
pub fn command(name: impl AsRef<OsStr>, env: &Env) -> Command {
    let mut value = Command::new(name);
    value.env_clear().envs(env).stdin(Stdio::null());
    value
}
pub fn output(command: &mut Command) -> Result<String> {
    let value = output_status(command, Duration::from_secs(30))?;
    if !value.status.success() {
        return Err("native tool failed; private diagnostics suppressed".into());
    }
    Ok(String::from_utf8(value.stdout)?.trim().into())
}
fn group_signal(child: &Child, signal: &str) -> bool {
    #[cfg(unix)]
    {
        Command::new("/bin/kill")
            .args([signal, "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(unix))]
    {
        let _ = (child, signal);
        false
    }
}
fn isolate(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    let _ = command;
}
struct CommandProcess {
    child: Child,
    cleaned: bool,
}
impl CommandProcess {
    fn spawn(command: &mut Command) -> Result<Self> {
        isolate(command);
        Ok(Self {
            child: command.spawn()?,
            cleaned: false,
        })
    }
    fn finish(&mut self, daemon_handoff: bool) -> Result<()> {
        if !daemon_handoff && group_signal(&self.child, "-0") {
            return Err("native command left child processes running".into());
        }
        self.cleaned = true;
        Ok(())
    }
}
impl Drop for CommandProcess {
    fn drop(&mut self) {
        if !self.cleaned {
            // The owned leader can already have exited while its group remains.
            group_signal(&self.child, "-KILL");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
fn wait_command(
    process: &mut CommandProcess,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
    inspect: impl Fn() -> Result<()>,
) -> Result<ExitStatus> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("invalid command deadline")?;
    loop {
        if cancelled() {
            return Err("native operation interrupted".into());
        }
        inspect()?;
        if let Some(status) = process.child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err("command interrupted or timed out; private state preserved".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
pub fn output_status(command: &mut Command, timeout: Duration) -> Result<Output> {
    const LIMIT: u64 = 1024 * 1024;
    if CANCELLED.load(Ordering::Relaxed) {
        return Err("native operation interrupted".into());
    }
    let mut out = tempfile::tempfile()?;
    let mut err = tempfile::tempfile()?;
    let inspect = || -> Result<()> {
        if out.metadata()?.len() > LIMIT || err.metadata()?.len() > LIMIT {
            return Err("native command output exceeds 1 MiB".into());
        }
        Ok(())
    };
    command
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(err.try_clone()?);
    let mut process = CommandProcess::spawn(command)?;
    let status = wait_command(
        &mut process,
        timeout,
        || CANCELLED.load(Ordering::Relaxed),
        inspect,
    )?;
    inspect()?;
    process.finish(false)?;
    out.rewind()?;
    err.rewind()?;
    let mut stdout = vec![];
    let mut stderr = vec![];
    out.take(LIMIT).read_to_end(&mut stdout)?;
    err.take(LIMIT).read_to_end(&mut stderr)?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
fn status_inner(command: &mut Command, timeout: Duration, cancellable: bool) -> Result<ExitStatus> {
    if cancellable && CANCELLED.load(Ordering::Relaxed) {
        return Err("native operation interrupted".into());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut process = CommandProcess::spawn(command)?;
    let status = wait_command(
        &mut process,
        timeout,
        || cancellable && CANCELLED.load(Ordering::Relaxed),
        || Ok(()),
    )?;
    process.finish(false)?;
    Ok(status)
}
pub fn status_poll(command: &mut Command) -> Result<bool> {
    Ok(status_inner(command, Duration::from_secs(300), true)?.success())
}
pub fn status_cleanup(command: &mut Command, timeout: Duration) -> Result<ExitStatus> {
    status_inner(command, timeout, false)
}
pub fn capture(command: &mut Command, path: &Path) -> Result<()> {
    capture_timeout(command, path, Duration::from_secs(300))
}
pub fn capture_timeout(command: &mut Command, path: &Path, timeout: Duration) -> Result<()> {
    capture_inner(command, path, timeout, true, false)
}
pub fn capture_cleanup(command: &mut Command, path: &Path) -> Result<()> {
    capture_inner(command, path, Duration::from_secs(30), false, false)
}
/// Only pg_ctl startup may successfully hand its daemon to a pre-created target guard.
pub fn capture_startup(command: &mut Command, path: &Path) -> Result<()> {
    capture_inner(command, path, Duration::from_secs(30), false, true)
}
fn capture_inner(
    command: &mut Command,
    path: &Path,
    timeout: Duration,
    cancellable: bool,
    daemon_handoff: bool,
) -> Result<()> {
    if cancellable && CANCELLED.load(Ordering::Relaxed) {
        return Err("native operation interrupted".into());
    }
    let out = new_file(path)?;
    let error = new_file(&PathBuf::from(format!("{}.stderr", path.display())))?;
    command.stdin(Stdio::null()).stdout(out).stderr(error);
    let mut process = CommandProcess::spawn(command)?;
    let status = wait_command(
        &mut process,
        timeout,
        || cancellable && CANCELLED.load(Ordering::Relaxed),
        || Ok(()),
    )?;
    if !status.success() {
        return Err("command failed; diagnostics are in the private work directory".into());
    }
    process.finish(daemon_handoff)
}
pub fn new_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
pub fn private_json(path: &Path, value: &Value) -> Result<()> {
    let mut file = new_file(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}
pub fn text(path: &Path, value: &str) -> Result<()> {
    let mut file = new_file(path)?;
    file.write_all(value.as_bytes())?;
    file.sync_all()?;
    Ok(())
}
pub fn replace_text(path: &Path, value: &str) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        regular(path)?;
        fs::remove_file(path)?;
    }
    text(path, value)
}
pub fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;

    fn delayed_writer(marker: &Path, wait: bool) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                if wait {
                    "(sleep 2; printf late > \"$1\") & wait"
                } else {
                    "(sleep 2; printf late > \"$1\") & exit 0"
                },
                "storage-command-test",
            ])
            .arg(marker);
        command
    }

    fn no_late_write(marker: &Path) {
        std::thread::sleep(Duration::from_millis(2200));
        assert!(
            !marker.exists(),
            "a stopped storage descendant performed a late write"
        );
    }

    #[test]
    fn storage_timeout_stops_descendants_before_they_can_write() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late.txt");
        let mut command = delayed_writer(&marker, true);
        let result = capture_inner(
            &mut command,
            &directory.path().join("stdout"),
            Duration::from_millis(50),
            false,
            false,
        );
        assert!(result.is_err());
        no_late_write(&marker);
    }

    #[test]
    fn successful_storage_command_rejects_and_stops_background_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late.txt");
        let result = capture_inner(
            &mut delayed_writer(&marker, false),
            &directory.path().join("stdout"),
            Duration::from_secs(2),
            false,
            false,
        );
        assert!(result.unwrap_err().to_string().contains("child processes"));
        no_late_write(&marker);
    }

    #[test]
    fn captured_output_does_not_wait_for_a_successful_leaders_background_writer() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late.txt");
        let started = Instant::now();
        let result = output_status(
            &mut delayed_writer(&marker, false),
            Duration::from_millis(150),
        );
        assert!(result.unwrap_err().to_string().contains("child processes"));
        assert!(started.elapsed() < Duration::from_secs(1));
        no_late_write(&marker);
    }

    #[test]
    fn storage_cancellation_cleans_the_whole_owned_group() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late.txt");
        let mut command = delayed_writer(&marker, true);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut process = CommandProcess::spawn(&mut command).unwrap();
        let started = Instant::now();
        let result = wait_command(
            &mut process,
            Duration::from_secs(2),
            || started.elapsed() >= Duration::from_millis(50),
            || Ok(()),
        );
        assert!(result.unwrap_err().to_string().contains("interrupted"));
        drop(process);
        no_late_write(&marker);
    }

    #[test]
    fn status_command_never_confirms_a_still_running_background_writer() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late.txt");
        let result = status_inner(
            &mut delayed_writer(&marker, false),
            Duration::from_secs(2),
            false,
        );
        assert!(result.unwrap_err().to_string().contains("child processes"));
        no_late_write(&marker);
    }

    #[test]
    fn owned_serve_child_stops_descendants_even_after_the_leader_already_exited() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late.txt");
        let mut child = OwnedChild::start(
            &mut delayed_writer(&marker, false),
            &directory.path().join("stdout"),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while child.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!child.stop().unwrap());
        no_late_write(&marker);
    }

    #[test]
    fn administrative_command_output_is_bounded_and_failure_is_sanitized() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "dd if=/dev/zero bs=1048576 count=2 2>/dev/null"]);
        let error = output_status(&mut command, Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("exceeds 1 MiB"));
    }
}
pub fn regular(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err("expected a regular file, not a symlink".into());
    }
    Ok(())
}
pub fn symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}
pub fn private_dir(path: &Path) -> Result<()> {
    if symlink(path) {
        return Err("private directory must not be a symlink".into());
    }
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub fn temp_dir(base: &Path, prefix: &str) -> Result<PathBuf> {
    private_dir(base)?;
    let path = loop {
        // Keep PostgreSQL Unix socket names below macOS's 103-byte limit.
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = base.join(format!("{prefix}{}", &id[..10]));
        match fs::create_dir(&path) {
            Ok(()) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}
pub fn canonical(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let name = path.file_name().ok_or("missing path filename")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(canonical(parent)?.join(name))
}
pub fn private_path(path: &Path, public: &Path) -> Result<PathBuf> {
    let path = canonical(path)?;
    if path.starts_with(canonical(public)?) {
        return Err("private output must be outside the public directory".into());
    }
    Ok(path)
}
pub fn json_file(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn sha256(path: &Path) -> Result<String> {
    sha256_reader(&mut File::open(path)?, || CANCELLED.load(Ordering::Relaxed))
}
fn sha256_reader(reader: &mut impl Read, mut cancelled: impl FnMut() -> bool) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        if cancelled() {
            return Err("native operation interrupted".into());
        }
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}
pub fn utc(value: &str) -> Result<DateTime<Utc>> {
    if !value.ends_with('Z') {
        return Err("UTC timestamp ending in Z required".into());
    }
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}
pub fn config(path: Option<&Path>) -> Result<tolight_core::config::Config> {
    match path {
        Some(path) => {
            Ok(tolight_core::config::Config::load(path).map_err(|_| "invalid configuration")?)
        }
        None => Ok(tolight_core::config::Config::default()),
    }
}
pub fn database_url(config: &tolight_core::config::Config) -> Result<String> {
    Ok(config
        .database
        .resolve()
        .map_err(|_| "database secret command failed")?)
}
fn decode(value: &str) -> Result<String> {
    Ok(percent_encoding::percent_decode_str(value)
        .decode_utf8()?
        .into_owned())
}
pub fn pg_environment(value: &str, database: Option<&str>) -> Result<Env> {
    let parsed = url::Url::parse(value)?;
    if !["postgres", "postgresql"].contains(&parsed.scheme()) || parsed.fragment().is_some() {
        return Err("a PostgreSQL connection URL is required".into());
    }
    let mut env = base_env();
    let name = database
        .map(str::to_owned)
        .unwrap_or(decode(parsed.path().trim_start_matches('/'))?);
    if name.is_empty() {
        return Err("database name is required".into());
    }
    env.insert("PGDATABASE".into(), name.into());
    for (name, value) in [
        ("PGHOST", parsed.host_str().map(str::to_owned)),
        ("PGPORT", parsed.port().map(|v| v.to_string())),
        (
            "PGUSER",
            (!parsed.username().is_empty()).then(|| parsed.username().to_owned()),
        ),
        ("PGPASSWORD", parsed.password().map(str::to_owned)),
    ] {
        if let Some(value) = value {
            env.insert(name.into(), decode(&value)?.into());
        }
    }
    let mappings = [
        ("host", "PGHOST"),
        ("hostaddr", "PGHOSTADDR"),
        ("port", "PGPORT"),
        ("user", "PGUSER"),
        ("password", "PGPASSWORD"),
        ("sslmode", "PGSSLMODE"),
        ("sslrootcert", "PGSSLROOTCERT"),
        ("sslcert", "PGSSLCERT"),
        ("sslkey", "PGSSLKEY"),
        ("connect_timeout", "PGCONNECT_TIMEOUT"),
        ("options", "PGOPTIONS"),
        ("application_name", "PGAPPNAME"),
        ("channel_binding", "PGCHANNELBINDING"),
        ("sslnegotiation", "PGSSLNEGOTIATION"),
    ];
    let mut seen = BTreeSet::new();
    for (key, value) in parsed.query_pairs() {
        if !seen.insert(key.to_string()) {
            return Err("repeated PostgreSQL URL parameter".into());
        }
        if key == "statement-cache-capacity" && value.parse::<usize>().is_ok() {
            continue;
        }
        let target = mappings
            .iter()
            .find(|(k, _)| *k == key)
            .ok_or("unsupported PostgreSQL URL parameter")?
            .1;
        env.insert(target.into(), value.into_owned().into());
    }
    Ok(env)
}
pub fn quote(value: impl AsRef<str>) -> String {
    format!(
        "'{}'",
        value.as_ref().replace('\\', "\\\\").replace('\'', "''")
    )
}
pub fn shell(value: impl AsRef<str>) -> String {
    format!("'{}'", value.as_ref().replace('\'', "'\\''"))
}
pub fn port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}
pub fn write_config(
    path: &Path,
    url_env: &str,
    public: &Path,
    demo: bool,
    ports: Option<(u16, u16)>,
) -> Result<()> {
    let mut value = String::new();
    if let Some((api, ops)) = ports {
        value.push_str(&format!(
            "listen=\"127.0.0.1:{api}\"\n[observability]\nlisten=\"127.0.0.1:{ops}\"\nlog_dir={}\n",
            serde_json::to_string(&path.parent().ok_or("config parent")?.join("service-logs"))?
        ));
    }
    value.push_str(&format!(
        "[database]\nsecret_cmd=[\"/usr/bin/printenv\",{}]\n[public]\ndir={}\ndemo_banner={demo}\n",
        serde_json::to_string(url_env)?,
        serde_json::to_string(public)?
    ));
    text(path, &value)
}
pub fn cli(binary: &Path, config: &Path, env: &Env) -> Command {
    let mut value = command(binary, env);
    value.arg("--config").arg(config);
    value
}
pub struct OwnedChild {
    pub child: Child,
    cleaned: bool,
}
impl OwnedChild {
    pub fn start(command: &mut Command, path: &Path) -> Result<Self> {
        if CANCELLED.load(Ordering::Relaxed) {
            return Err("native operation interrupted".into());
        }
        isolate(command);
        Ok(Self {
            child: command
                .stdout(new_file(path)?)
                .stderr(new_file(&PathBuf::from(format!(
                    "{}.stderr",
                    path.display()
                )))?)
                .spawn()?,
            cleaned: false,
        })
    }
    pub fn stop(&mut self) -> Result<bool> {
        if let Some(status) = self.child.try_wait()? {
            let descendants = !self.cleaned && group_signal(&self.child, "-0");
            if descendants {
                group_signal(&self.child, "-KILL");
            }
            self.cleaned = true;
            return Ok(status.success() && !descendants);
        }
        #[cfg(unix)]
        group_signal(&self.child, "-TERM");
        #[cfg(not(unix))]
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                let descendants = group_signal(&self.child, "-0");
                if descendants {
                    group_signal(&self.child, "-KILL");
                }
                self.cleaned = true;
                return Ok(status.success() && !descendants);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        group_signal(&self.child, "-KILL");
        self.child.kill()?;
        self.child.wait()?;
        self.cleaned = true;
        Ok(false)
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
pub fn http(port: u16, path: &str) -> Result<(u16, Value)> {
    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse()?,
        Duration::from_millis(500),
    )?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    let mut bytes = Vec::new();
    stream.take(65537).read_to_end(&mut bytes)?;
    let text = String::from_utf8(bytes)?;
    let (headers, body) = text
        .split_once("\r\n\r\n")
        .ok_or("invalid local HTTP response")?;
    let status = headers
        .split_whitespace()
        .nth(1)
        .ok_or("missing status")?
        .parse()?;
    Ok((status, serde_json::from_str(body)?))
}
pub fn healthy(child: &mut OwnedChild, api: u16, ops: u16, timeout: u64) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    while Instant::now() < deadline {
        if CANCELLED.load(Ordering::Relaxed) || child.child.try_wait()?.is_some() {
            return Err("restored serve exited before becoming healthy".into());
        }
        if let (Ok((200, health)), Ok((401, unsigned))) =
            (http(ops, "/healthz"), http(api, "/participant/v1/self"))
            && health == json!({"status":"ok"})
            && unsigned["error"]["code"] == "unauthorized"
        {
            return Ok(json!({"health_status":200,"unsigned_participant_status":401}));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("restored serve did not become healthy before startup deadline".into())
}
pub fn tree_files(directory: &Path) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    fn walk(root: &Path, path: &Path, values: &mut BTreeMap<String, String>) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err("symlink in owned/public tree".into());
            }
            if kind.is_dir() {
                walk(root, &p, values)?
            } else if kind.is_file() {
                values.insert(
                    p.strip_prefix(root)?.to_string_lossy().into_owned(),
                    sha256(&p)?,
                );
            }
        }
        Ok(())
    }
    walk(directory, directory, &mut values)?;
    Ok(values)
}
pub fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    if symlink(source) || !source.is_dir() {
        return Err("base data must be a regular directory".into());
    }
    private_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        let out = target.join(entry.file_name());
        if kind.is_symlink() || (!kind.is_file() && !kind.is_dir()) {
            return Err("base backup symlinks/tablespaces are unsupported".into());
        }
        if kind.is_dir() {
            copy_tree(&path, &out)?
        } else {
            fs::copy(&path, &out)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&out, fs::Permissions::from_mode(0o600))?;
            }
        }
    }
    Ok(())
}
pub fn validate_base(source: &Path) -> Result<()> {
    if symlink(source) || !source.is_dir() {
        return Err("base data must be a regular directory".into());
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            validate_base(&entry.path())?
        } else if !kind.is_file() {
            return Err("base backup symlinks/tablespaces are unsupported".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod hash_cancel_tests {
    use super::*;
    #[test]
    fn hashing_checks_cancellation_between_bounded_reads() {
        let bytes = vec![42; 4 * 65536];
        let mut reader = std::io::Cursor::new(&bytes);
        let mut checks = 0;
        let error = sha256_reader(&mut reader, || {
            checks += 1;
            checks > 1
        })
        .unwrap_err();
        assert!(error.to_string().contains("interrupted"));
        assert_eq!(reader.position(), 65536);
        let mut reader = std::io::Cursor::new(&bytes);
        assert_eq!(
            sha256_reader(&mut reader, || false).unwrap(),
            format!("{:x}", Sha256::digest(&bytes))
        );
    }
}
