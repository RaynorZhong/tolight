use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
struct Fixture {
    _dir: tempfile::TempDir,
    origin: String,
    date: Arc<Mutex<Option<String>>>,
    bin: PathBuf,
    state: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

#[tokio::test]
async fn probe_thresholds_require_positive_integral_seconds_before_writing_state() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state.json");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/probe.sh");
    for flag in ["--max-clock-skew", "--repeat-seconds"] {
        for value in ["0", "-1", "1.5", "NaN", "inf"] {
            let output = tokio::process::Command::new(&script)
                .args([
                    "--static-origin",
                    "http://127.0.0.1:1",
                    "--participant-origin",
                    "http://127.0.0.1:2",
                    "--reference-bin",
                    "/does-not-exist",
                    "--reference-config",
                    "unused",
                    "--allow-http",
                    "--state-file",
                ])
                .arg(&state)
                .arg(flag)
                .arg(value)
                .output()
                .await
                .unwrap();
            assert_eq!(output.status.code(), Some(2), "{flag} {value}");
            assert!(!state.exists());
            assert!(!directory.path().join("state.json.lock").exists());
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_during_http_wait_releases_state_lock_without_saving_a_result() {
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let state = directory.path().join("state.json");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/probe.sh");
    let mut child = tokio::process::Command::new(script)
        .args([
            "--static-origin",
            &origin,
            "--participant-origin",
            &origin,
            "--reference-bin",
            "/does-not-exist",
            "--reference-config",
            "unused",
            "--allow-http",
            "--state-file",
        ])
        .arg(&state)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (_pending_request, _) =
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
    assert!(
        tokio::process::Command::new("/bin/kill")
            .args(["-TERM", &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    let status = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
    assert!(
        !state.exists(),
        "an interrupted scrape must not save a successful probe result"
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.path().join("state.json.lock"))
        .unwrap();
    lock.try_lock()
        .expect("interrupted probe must release its state lock");
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("status.sh");
        std::fs::write(&bin, "#!/bin/sh\nexit 5\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let date = Arc::new(Mutex::new(Some("Sat, 14 Nov 2026 12:00:00 GMT".into())));
        let shared = date.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let date = shared.lock().unwrap().clone();
                tokio::spawn(async move {
                    let mut request = vec![];
                    let mut buffer = [0; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let length = stream.read(&mut buffer).await.unwrap();
                        if length == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..length]);
                    }
                    let manifest = request.starts_with(b"GET /data/manifest.json ");
                    let (status, body) = if manifest {
                        (
                            "200 OK",
                            "{\"schema\":\"tolight-manifest/1\",\"newest_seq\":0}",
                        )
                    } else {
                        ("401 Unauthorized", "{\"error\":{\"code\":\"clock_skew\"}}")
                    };
                    let header = date.map(|d| format!("Date: {d}\r\n")).unwrap_or_default();
                    let response = format!(
                        "HTTP/1.1 {status}\r\n{header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        let state = dir.path().join("state.json");
        Self {
            _dir: dir,
            origin,
            date,
            bin,
            state,
            task,
        }
    }
    async fn run(&self, now: u64, code: u8) -> std::process::Output {
        std::fs::write(&self.bin,format!("#!/bin/sh\nprintf 'misleading prose: unauthorized 421 clock_skew' >&2\nexit {code}\n")).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/probe.sh");
        tokio::process::Command::new(script)
            .args([
                "--static-origin",
                &self.origin,
                "--participant-origin",
                &self.origin,
                "--reference-bin",
                self.bin.to_str().unwrap(),
                "--reference-config",
                "unused",
                "--state-file",
                self.state.to_str().unwrap(),
                "--allow-http",
                "--now",
                &now.to_string(),
            ])
            .output()
            .await
            .unwrap()
    }
}
#[tokio::test]
async fn only_exit_five_passes_signed_host_check_and_clock_classification_uses_date() {
    let fixture = Fixture::new().await;
    let now = tolight_core::datetime::parse_rfc3339("2026-11-14T12:00:00Z")
        .unwrap()
        .unix_timestamp() as u64;
    let success = fixture.run(now, 5).await;
    assert!(
        success.status.success(),
        "{}",
        String::from_utf8_lossy(&success.stdout)
    );
    let generic = fixture.run(now, 1).await;
    assert_eq!(generic.status.code(), Some(1));
    let body = String::from_utf8(generic.stdout).unwrap();
    assert!(body.contains("signed_probe_failed"));
    assert!(!body.contains("clock_skew\""));
    let skew = fixture.run(now + 600, 1).await;
    let body = String::from_utf8(skew.stdout).unwrap();
    assert!(body.contains("clock_skew"));
    assert!(body.contains("signed_probe_failed"));
}
#[tokio::test]
async fn missing_and_malformed_dates_are_explicit_failures() {
    let fixture = Fixture::new().await;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    *fixture.date.lock().unwrap() = None;
    let output = fixture.run(now, 5).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("missing_date"));
    *fixture.date.lock().unwrap() = Some("garbage".into());
    let output = fixture.run(now, 5).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("invalid_date"));
}
