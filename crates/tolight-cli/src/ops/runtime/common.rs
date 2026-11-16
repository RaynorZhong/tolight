//! Shared durable state, process and HTTP boundaries.
use super::{Error, Result};
use reqwest::{Client, Url};
use serde_json::{Map, Value, json};
#[cfg(unix)]
use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) static COMMAND_CANCELLED: AtomicBool = AtomicBool::new(false);

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}
pub fn ensure(ok: bool, message: &str) -> Result {
    if ok { Ok(()) } else { Err(message.into()) }
}
pub fn now(value: Option<f64>) -> Result<f64> {
    let value = value.unwrap_or(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64());
    ensure(
        value.is_finite() && value >= 0.,
        "now must be a finite nonnegative timestamp",
    )?;
    Ok(value)
}
pub fn checked_url(
    value: &str,
    allow_http: bool,
    loopback_only: bool,
    origin: bool,
) -> Result<Url> {
    let url = Url::parse(value)?;
    let local = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    ensure(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "invalid URL credentials/fragment/host",
    )?;
    ensure(
        url.scheme() == "https" || (url.scheme() == "http" && allow_http && local),
        "HTTPS required; explicit HTTP testing is loopback only",
    )?;
    ensure(
        !loopback_only || local,
        "health and metrics endpoints must use loopback",
    )?;
    ensure(
        !origin || (url.path() == "/" && url.query().is_none()),
        "supply an Origin, not a path",
    )?;
    Ok(url)
}
pub fn client(ca: Option<&Path>, tls: Option<reqwest::tls::Version>) -> Result<Client> {
    let mut builder = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20));
    if let Some(path) = ca {
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&fs::read(path)?)?);
    }
    if let Some(version) = tls {
        builder = builder.tls_version_min(version).tls_version_max(version);
    }
    Ok(builder.build()?)
}
pub struct Response {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Vec<u8>,
}
pub async fn fetch(
    client: &Client,
    url: impl reqwest::IntoUrl,
    method: reqwest::Method,
    etag: Option<&str>,
) -> Result<Response> {
    fetch_limit(client, url, method, etag, 1024 * 1024).await
}
pub async fn fetch_limit(
    client: &Client,
    url: impl reqwest::IntoUrl,
    method: reqwest::Method,
    etag: Option<&str>,
    limit: usize,
) -> Result<Response> {
    let mut request = client.request(method, url);
    if let Some(etag) = etag {
        request = request.header("If-None-Match", etag);
    }
    let mut response = request.send().await?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure(
            body.len() + chunk.len() <= limit,
            "response exceeds configured body limit",
        )?;
        body.extend_from_slice(&chunk);
    }
    Ok(Response {
        status,
        headers,
        body,
    })
}
pub fn regular(path: &Path, required: bool) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "managed files must be regular files, never symlinks",
            )?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub fn private_open(path: &Path, append: bool) -> Result<File> {
    regular(path, false)?;
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append);
    #[cfg(unix)]
    options.mode(0o600);
    Ok(options.open(path)?)
}
pub fn save(path: &Path, value: &Value) -> Result {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .ok_or("state path has no name")?
            .to_string_lossy()
    ));
    let mut file = private_open(&temporary, false)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()?;
    Ok(())
}
pub fn load(path: &Path) -> Result<Value> {
    if !regular(path, false)? {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_reader(File::open(path)?)?;
    ensure(value.is_object(), "invalid state")?;
    Ok(value)
}
pub fn lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock = path.with_file_name(format!(
        "{}.lock",
        path.file_name()
            .ok_or("state has no filename")?
            .to_string_lossy()
    ));
    regular(&lock, false)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(lock)?;
    file.try_lock()
        .map_err(|_| "another checker is using this state file")?;
    Ok(file)
}
struct CapturedProcess {
    child: Child,
    cleaned: bool,
}
impl CapturedProcess {
    #[cfg(unix)]
    fn group_signal(&self, signal: &str) -> bool {
        // This group belongs to the Child created here, never a PID from state.
        Command::new("/bin/kill")
            .args([signal, "--", &format!("-{}", self.child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    fn has_descendants(&self) -> bool {
        #[cfg(unix)]
        {
            self.group_signal("-0")
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        // The leader may already have exited: its descendants still retain the
        // group and any inherited stdin/stdout or pending notification effects.
        #[cfg(unix)]
        self.group_signal("-KILL");
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.cleaned = true;
    }
}
impl Drop for CapturedProcess {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub fn capture(command: &mut Command, seconds: u64, input: Option<&[u8]>) -> Result<Output> {
    const OUTPUT_LIMIT: u64 = 1024 * 1024;
    ensure(
        !COMMAND_CANCELLED.load(Ordering::Acquire),
        "operational command interrupted",
    )?;
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    // A file gives finite JSON input the same EOF semantics without a blocking
    // write into a pipe that an unresponsive notification command never reads.
    let stdin = if let Some(input) = input {
        let mut file = tempfile::tempfile()?;
        file.write_all(input)?;
        file.rewind()?;
        Stdio::from(file)
    } else {
        Stdio::null()
    };
    command
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .stdin(stdin);
    #[cfg(unix)]
    command.process_group(0);
    let mut process = CapturedProcess {
        child: command.spawn()?,
        cleaned: false,
    };
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let status = loop {
        ensure(
            !COMMAND_CANCELLED.load(Ordering::Acquire),
            "operational command interrupted",
        )?;
        ensure(
            stdout.metadata()?.len() <= OUTPUT_LIMIT && stderr.metadata()?.len() <= OUTPUT_LIMIT,
            "operational command output exceeds 1 MiB",
        )?;
        if let Some(status) = process.child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            return Err("operational command timed out".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    ensure(
        !process.has_descendants(),
        "operational command left child processes running",
    )?;
    process.cleaned = true;
    ensure(
        stdout.metadata()?.len() <= OUTPUT_LIMIT && stderr.metadata()?.len() <= OUTPUT_LIMIT,
        "operational command output exceeds 1 MiB",
    )?;
    stdout.rewind()?;
    stderr.rewind()?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.take(OUTPUT_LIMIT).read_to_end(&mut out)?;
    stderr.take(OUTPUT_LIMIT).read_to_end(&mut err)?;
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
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
                "operational-command-test",
            ])
            .arg(marker);
        command
    }

    #[test]
    fn timed_out_command_stops_its_descendants_before_they_can_write() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late.txt");
        let started = Instant::now();
        let error = capture(&mut delayed_writer(&marker, true), 1, None).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(2200));
        assert!(
            !marker.exists(),
            "a timed-out descendant performed a late effect"
        );
    }

    #[test]
    fn successful_leader_does_not_leave_background_workers_or_report_success() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late.txt");
        let error = capture(&mut delayed_writer(&marker, false), 5, None).unwrap_err();
        assert!(error.to_string().contains("child processes"));
        std::thread::sleep(Duration::from_millis(2200));
        assert!(
            !marker.exists(),
            "a completed command left a live descendant"
        );
    }

    #[test]
    fn input_does_not_block_a_non_reader_and_oversize_output_is_rejected() {
        let started = Instant::now();
        let output = capture(
            Command::new("/bin/sh").args(["-c", "exit 0"]),
            1,
            Some(&vec![b'x'; 2 * 1024 * 1024]),
        )
        .unwrap();
        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(5));
        let error = capture(
            Command::new("/bin/dd").args(["if=/dev/zero", "bs=65536", "count=32"]),
            5,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("output exceeds"));
    }
}
pub fn notify(command: Option<&str>, event: &Value) -> Result {
    // Retain the old single-line notification spelling for existing shell
    // consumers, while letting serde escape every string correctly.
    struct NotificationFormat;
    impl serde_json::ser::Formatter for NotificationFormat {
        fn begin_object_value<W: Write + ?Sized>(&mut self, writer: &mut W) -> std::io::Result<()> {
            writer.write_all(b": ")
        }
    }
    let mut data = Vec::new();
    serde::Serialize::serialize(
        event,
        &mut serde_json::Serializer::with_formatter(&mut data, NotificationFormat),
    )?;
    data.push(b'\n');
    if let Some(command) = command {
        let argv: Vec<String> = serde_json::from_str(command)?;
        ensure(
            !argv.is_empty() && Path::new(&argv[0]).is_absolute(),
            "notify-command must be a JSON argv array with an absolute executable",
        )?;
        let result = capture(Command::new(&argv[0]).args(&argv[1..]), 15, Some(&data))?;
        ensure(result.status.success(), "notification command failed")
    } else {
        std::io::stdout().write_all(&data)?;
        Ok(())
    }
}
pub fn notify_conditions(
    state: &mut Value,
    conditions: &BTreeMap<String, String>,
    now: f64,
    repeat: f64,
    command: Option<&str>,
    path: &Path,
) -> Result {
    notify_conditions_with(state, conditions, now, repeat, path, |event| {
        notify(command, event)
    })
}
pub fn notify_conditions_with(
    state: &mut Value,
    conditions: &BTreeMap<String, String>,
    now: f64,
    repeat: f64,
    path: &Path,
    mut send: impl FnMut(&Value) -> Result,
) -> Result {
    let mut previous = state
        .get("alerts")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    previous.retain(|code, _| conditions.contains_key(code));
    state["alerts"] = Value::Object(previous.clone());
    for (code, detail) in conditions {
        if previous
            .get(code)
            .and_then(Value::as_f64)
            .is_none_or(|sent| now - sent >= repeat)
        {
            send(&json!({"code":code,"detail":detail,"at":now}))?;
            previous.insert(code.clone(), json!(now));
            state["alerts"] = Value::Object(previous.clone());
            save(path, state)?;
        }
    }
    Ok(())
}
pub fn object(value: Option<&Value>) -> Result<Map<String, Value>> {
    match value {
        None => Ok(Map::new()),
        Some(value) => value
            .as_object()
            .cloned()
            .ok_or_else(|| Error::from("expected object")),
    }
}
