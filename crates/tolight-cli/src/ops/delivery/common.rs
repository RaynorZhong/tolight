use super::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    net::{IpAddr, Ipv4Addr},
    path::{Component, Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use url::Url;

pub const GATES: &[&str] = &[
    "operator_channel",
    "security_channel",
    "content_rules",
    "third_party_m2",
    "independent_m3",
    "production_origins",
    "draft_owner_upgrade",
];
pub const ROOT_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "flake.nix",
    "flake.lock",
    "deny.toml",
    "LICENSE",
    "README.md",
    "CHANGELOG.md",
    "SECURITY.md",
    "tolight-plan.md",
    ".gitignore",
    ".env.example",
    "config.example.toml",
    "package.json",
    "package-lock.json",
    "tsconfig.json",
    "tsconfig.web.json",
    "tsconfig.admin.json",
];
pub const ROOT_DIRS: &[&str] = &[
    ".github",
    "crates",
    "examples",
    "scripts",
    "public",
    "tests",
    "docs",
    "deploy",
    "frontend",
    "operator-ui",
];
pub const WEB_SOURCE_FILES: &[&str] = &[
    "package.json",
    "package-lock.json",
    "tsconfig.json",
    "tsconfig.web.json",
    "tsconfig.admin.json",
    "frontend/app.ts",
    "frontend/admin.ts",
    "tests/web/app.test.ts",
    "tests/web/admin.test.ts",
    "operator-ui/index.html",
    "operator-ui/style.css",
    "public/index.html",
    "public/style.css",
    "public/robots.txt",
];
pub const PRIVATE_DIRS: &[&str] = &[
    ".git",
    ".local",
    "target",
    "public-dist",
    ".direnv",
    ".aws",
    ".codex",
    ".agents",
    "node_modules",
    "__pycache__",
];
pub const PRIVATE_SUFFIXES: &[&str] = &[
    "key", "pem", "p12", "pfx", "dump", "db", "log", "pyc", "pyo", "sqlite", "sqlite3",
];
pub static CANCELLED: AtomicBool = AtomicBool::new(false);

pub fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

pub fn read_toml(path: impl AsRef<Path>) -> Result<Value> {
    let value: toml::Value =
        toml::from_str(&fs::read_to_string(path)?).map_err(|_| "invalid TOML document")?;
    Ok(serde_json::to_value(value)?)
}

pub fn read_json(path: impl AsRef<Path>) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?).map_err(|_| "invalid JSON document")?)
}

pub fn str_field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing or invalid field: {name}").into())
}

pub fn array_field<'a>(value: &'a Value, name: &str) -> Result<&'a Vec<Value>> {
    value
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing or invalid array: {name}").into())
}

pub fn private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(path)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

pub fn private_json(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    private_file(path, &bytes)
}

pub fn sha256(path: impl AsRef<Path>) -> Result<String> {
    use std::io::Read;
    let mut source = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn lexical_parts(path: &Path) -> Option<Vec<String>> {
    let mut parts = vec![];
    for part in path.components() {
        match part {
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then_some(parts)
}

pub fn private_path(path: &Path) -> bool {
    let Some(parts) = lexical_parts(path) else {
        return true;
    };
    let name = parts.last().unwrap();
    parts
        .iter()
        .any(|part| PRIVATE_DIRS.contains(&part.as_str()))
        || parts.get(..2).is_some_and(|p| p == ["public", "data"])
        || path == Path::new("public/app.js")
        || ["config.toml", "reference.toml", "client-state.json", ".env"].contains(&name.as_str())
        || (name.starts_with(".env.") && name != ".env.example")
        || path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|ext| PRIVATE_SUFFIXES.contains(&ext.to_ascii_lowercase().as_str()))
}

pub fn source_path_allowed(path: &Path) -> bool {
    let Some(parts) = lexical_parts(path) else {
        return false;
    };
    !private_path(path)
        && ((parts.len() == 1 && ROOT_FILES.contains(&parts[0].as_str()))
            || ROOT_DIRS.contains(&parts[0].as_str()))
}

/// Files and symlinks needed to resolve an input, including intermediate links.
pub fn required_source_material(root: &Path, path: &Path) -> Result<BTreeSet<String>> {
    fn resolve(
        root: &Path,
        path: &Path,
        material: &mut BTreeSet<String>,
        depth: usize,
    ) -> Result<PathBuf> {
        ensure(depth < 40, "too many required source symlinks")?;
        let mut resolved = root.to_path_buf();
        for component in path.strip_prefix(root)?.components() {
            match component {
                Component::CurDir => (),
                Component::ParentDir => {
                    ensure(resolved != root, "required source escapes repository")?;
                    resolved.pop();
                }
                Component::Normal(name) => resolved.push(name),
                _ => return Err("invalid required source path".into()),
            }
            if resolved.is_symlink() {
                let relative = resolved.strip_prefix(root)?;
                ensure(
                    source_path_allowed(relative),
                    "required source points to a private path",
                )?;
                material.insert(relative.to_str().ok_or("non-UTF8 source path")?.to_owned());
                let target = fs::read_link(&resolved)?;
                ensure(!target.is_absolute(), "absolute required source symlink")?;
                resolved = resolve(
                    root,
                    &resolved
                        .parent()
                        .ok_or("invalid source parent")?
                        .join(target),
                    material,
                    depth + 1,
                )?;
            }
        }
        Ok(resolved)
    }
    let mut material = BTreeSet::new();
    let resolved = resolve(root, path, &mut material, 0)?;
    let relative = resolved.strip_prefix(root)?;
    ensure(
        source_path_allowed(relative),
        "required source points to a private path",
    )?;
    material.insert(relative.to_str().ok_or("non-UTF8 source path")?.to_owned());
    Ok(material)
}

pub fn semver(text: &str) -> Result<&str> {
    fn identifier(text: &str) -> bool {
        !text.is_empty() && text.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
    }
    let (version, metadata) = text
        .split_once('+')
        .map_or((text, None), |(a, b)| (a, Some(b)));
    ensure(
        metadata.is_none_or(|m| m.split('.').all(identifier)),
        "invalid semantic version",
    )?;
    let (base, pre) = version
        .split_once('-')
        .map_or((version, None), |(a, b)| (a, Some(b)));
    let numbers = base.split('.').collect::<Vec<_>>();
    ensure(
        numbers.len() == 3
            && numbers.iter().all(|n| {
                !n.is_empty()
                    && n.bytes().all(|c| c.is_ascii_digit())
                    && (n.len() == 1 || !n.starts_with('0'))
            }),
        "invalid semantic version",
    )?;
    ensure(
        pre.is_none_or(|p| {
            p.split('.').all(|part| {
                identifier(part)
                    && !(part.bytes().all(|c| c.is_ascii_digit())
                        && part.len() > 1
                        && part.starts_with('0'))
            })
        }),
        "invalid semantic version",
    )?;
    Ok(text)
}

pub fn set(items: impl IntoIterator<Item = impl Into<String>>) -> BTreeSet<String> {
    items.into_iter().map(Into::into).collect()
}

pub fn git(root: &Path, arguments: &[&str]) -> Result<Output> {
    let mut command = Command::new("git");
    command
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false", "-C"])
        .arg(root)
        .args(arguments);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command.stdin(Stdio::null());
    command_output(command, Duration::from_secs(60), 16 * 1024 * 1024)
}

struct OwnedProcess {
    child: Child,
    cleaned: bool,
}
impl OwnedProcess {
    #[cfg(unix)]
    fn signal_group(&self, signal: &str) -> bool {
        // This is the group created for this Child, never an ID from state.
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
            self.signal_group("-0")
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
        // A reaped leader can still leave live descendants and open log
        // handles. Failure and cancellation terminate the whole owned group.
        #[cfg(unix)]
        self.signal_group("-KILL");
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.cleaned = true;
    }
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub fn command_output(command: Command, timeout: Duration, max_bytes: u64) -> Result<Output> {
    command_output_logged(command, timeout, max_bytes, None)
}

pub fn command_output_logged(
    command: Command,
    timeout: Duration,
    max_bytes: u64,
    log: Option<&Path>,
) -> Result<Output> {
    command_output_controlled(command, timeout, max_bytes, log, &CANCELLED)
}

fn command_output_controlled(
    mut command: Command,
    timeout: Duration,
    max_bytes: u64,
    log: Option<&Path>,
    cancelled: &AtomicBool,
) -> Result<Output> {
    ensure(!cancelled.load(Ordering::SeqCst), "interrupted")?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("process timeout exceeds clock range")?;
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let outcome = (|| -> Result<Output> {
        let mut process = OwnedProcess {
            child: command.spawn()?,
            cleaned: false,
        };
        let status = loop {
            ensure(!cancelled.load(Ordering::SeqCst), "interrupted")?;
            ensure(
                stdout.metadata()?.len() <= max_bytes && stderr.metadata()?.len() <= max_bytes,
                "process output exceeds limit",
            )?;
            if let Some(status) = process.child.try_wait()? {
                break status;
            }
            ensure(Instant::now() < deadline, "process timed out")?;
            std::thread::sleep(Duration::from_millis(10));
        };
        ensure(
            !process.has_descendants(),
            "process left child processes running",
        )?;
        process.cleaned = true;
        ensure(
            stdout.metadata()?.len() <= max_bytes && stderr.metadata()?.len() <= max_bytes,
            "process output exceeds limit",
        )?;
        stdout.rewind()?;
        stderr.rewind()?;
        let mut out = vec![];
        let mut err = vec![];
        (&mut stdout)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut out)?;
        (&mut stderr)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut err)?;
        Ok(Output {
            status,
            stdout: out,
            stderr: err,
        })
    })();
    if let Some(log) = log {
        let mut options = OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut target = options.open(log)?;
        stdout.rewind()?;
        stderr.rewind()?;
        std::io::copy(&mut (&mut stdout).take(max_bytes), &mut target)?;
        std::io::copy(&mut (&mut stderr).take(max_bytes), &mut target)?;
    }
    outcome
}

pub fn public_evidence(root: &Path, value: &str) -> bool {
    if let Ok(url) = Url::parse(value) {
        return url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none();
    }
    let path = Path::new(value);
    source_path_allowed(path)
        && root.join(path).is_file()
        && root
            .join(path)
            .canonicalize()
            .is_ok_and(|p| p.starts_with(root))
}

fn ipv4_public(ip: Ipv4Addr) -> bool {
    let p = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || p[0] == 0
        || p[0] >= 224
        || (p[0] == 100 && (64..=127).contains(&p[1]))
        || (p[0] == 198 && (p[1] == 18 || p[1] == 19))
        || (p[0..3] == [192, 0, 0] && p[3] != 9 && p[3] != 10))
}

pub fn globally_routable(host: &str) -> bool {
    match host.trim_matches(['[', ']']).parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ipv4_public(ip),
        Ok(IpAddr::V6(ip)) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return ipv4_public(mapped);
            }
            let p = ip.segments();
            !(ip.is_unspecified()
                || ip.is_loopback()
                || (p[0] & 0xfe00) == 0xfc00
                || (p[0] & 0xffc0) == 0xfe80
                || (p[0] & 0xff00) == 0xff00
                || (p[0] == 0x2001 && p[1] == 0xdb8)
                || p[0] == 0)
        }
        Err(_) => host.contains('.') && !host.chars().any(char::is_whitespace),
    }
}

pub fn placeholder_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    ["localhost", "example.com", "example.org", "example.net"].contains(&host.as_str())
        || [".invalid", ".example", ".test", ".localhost"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
        || !globally_routable(&host)
}

pub fn placeholder_contact(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return true;
    };
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return true;
    }
    match url.scheme() {
        "https" => url.host_str().is_none_or(placeholder_host) || url.port() == Some(0),
        "mailto" => {
            let Ok(address) = percent_encoding::percent_decode_str(url.path()).decode_utf8() else {
                return true;
            };
            let parts = address.split('@').collect::<Vec<_>>();
            parts.len() != 2
                || parts[0].is_empty()
                || address.chars().any(char::is_whitespace)
                || placeholder_host(parts[1])
        }
        "tel" => {
            let Some(number) = url.path().strip_prefix('+') else {
                return true;
            };
            let (number, extension) = number
                .split_once(";ext=")
                .map_or((number, None), |(a, b)| (a, Some(b)));
            !(7..=15).contains(&number.len())
                || number.starts_with('0')
                || !number.bytes().all(|c| c.is_ascii_digit())
                || extension.is_some_and(|e| e.is_empty() || !e.bytes().all(|c| c.is_ascii_digit()))
        }
        _ => true,
    }
}

pub fn contains_version(text: &str, version: &str) -> bool {
    text.match_indices(version).any(|(index, _)| {
        let before = text[..index].chars().next_back();
        let after = text[index + version.len()..].chars().next();
        let word = |c: char| c.is_alphanumeric() || c == '_';
        before.is_none_or(|c| !word(c)) && after.is_none_or(|c| !word(c))
    })
}

pub fn constant(source: &str, prefix: &str) -> Result<String> {
    let found = source
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix(prefix)?
                .split_once('=')
                .map(|(_, value)| value.trim().trim_end_matches(';').trim().to_owned())
        })
        .collect::<Vec<_>>();
    ensure(
        found.len() == 1,
        format!("invalid runtime constant: {prefix}"),
    )?;
    Ok(found[0].clone())
}

pub fn canonical_or_absolute(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let absolute = std::path::absolute(path)?;
    let Some(parent) = absolute.parent() else {
        return Err("invalid path".into());
    };
    let parent = canonical_or_absolute(parent)?;
    Ok(parent.join(absolute.file_name().ok_or("invalid filename")?))
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;

    fn delayed_writer(marker: &Path, close_handles: bool, wait: bool) -> Command {
        let mut command = Command::new("/bin/sh");
        let body = format!(
            "(sleep 1; printf late > \"$1\") {} & {}",
            if close_handles { ">/dev/null 2>&1" } else { "" },
            if wait { "wait" } else { "exit 0" },
        );
        command
            .args(["-c", &body, "delivery-process-test"])
            .arg(marker);
        command
    }

    #[test]
    fn exited_leader_cannot_report_success_or_leave_a_late_writer() {
        for close_handles in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let marker = directory.path().join("late.txt");
            let result = command_output(
                delayed_writer(&marker, close_handles, false),
                Duration::from_secs(3),
                1024,
            );
            // Let the original child finish even when this regression runs
            // against the broken implementation, so a red run leaves no jobs.
            std::thread::sleep(Duration::from_millis(1200));
            assert!(!marker.exists(), "an exited command left a live descendant");
            assert!(result.unwrap_err().to_string().contains("child processes"));
        }
    }

    #[test]
    fn timeout_stops_term_resistant_leader_and_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late.txt");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "trap '' TERM; (sleep 1; printf late > \"$1\") & wait",
                "delivery-timeout-test",
            ])
            .arg(&marker);
        let started = Instant::now();
        let error = command_output(command, Duration::from_millis(100), 1024).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(1200));
        assert!(!marker.exists(), "timed-out work ran after cleanup");
    }

    #[test]
    fn cancellation_stops_the_group_and_does_not_start_new_work() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late.txt");
        let ready = directory.path().join("ready.txt");
        let cancelled = AtomicBool::new(false);
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "(sleep 1; printf late > \"$1\") & printf ready > \"$2\"; wait",
                "delivery-cancel-test",
            ])
            .arg(&marker)
            .arg(&ready);
        std::thread::scope(|scope| {
            let setter = scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(3);
                while !ready.is_file() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                cancelled.store(true, Ordering::SeqCst);
                ready.is_file()
            });
            let error =
                command_output_controlled(command, Duration::from_secs(5), 1024, None, &cancelled)
                    .unwrap_err();
            assert!(setter.join().unwrap(), "command never became ready");
            assert!(error.to_string().contains("interrupted"));
        });
        std::thread::sleep(Duration::from_millis(1200));
        assert!(!marker.exists(), "cancelled work performed a late write");
        let mut next = Command::new("/bin/sh");
        next.args([
            "-c",
            "printf unexpected > \"$1\"",
            "delivery-pre-cancelled-test",
        ])
        .arg(&marker);
        let error = command_output_controlled(next, Duration::from_secs(1), 1024, None, &cancelled)
            .unwrap_err();
        assert!(error.to_string().contains("interrupted"));
        assert!(!marker.exists());
    }

    #[test]
    fn oversized_output_stops_descendants_and_keeps_private_logs_bounded() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late.txt");
        let log = directory.path().join("build.log");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "(sleep 1; printf late > \"$1\") >/dev/null 2>&1 & head -c 4096 /dev/zero; wait",
                "delivery-output-test",
            ])
            .arg(&marker);
        let error =
            command_output_logged(command, Duration::from_secs(3), 1024, Some(&log)).unwrap_err();
        assert!(error.to_string().contains("output exceeds limit"));
        assert_eq!(fs::metadata(&log).unwrap().len(), 1024);
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::thread::sleep(Duration::from_millis(1200));
        assert!(
            !marker.exists(),
            "output-limited work performed a late write"
        );
    }

    #[test]
    fn completed_commands_keep_status_output_and_private_logging() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("build.log");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf stdout; printf stderr >&2; exit 7"]);
        let output =
            command_output_logged(command, Duration::from_secs(3), 1024, Some(&log)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"stdout");
        assert_eq!(output.stderr, b"stderr");
        assert_eq!(fs::read(&log).unwrap(), b"stdoutstderr");
        assert_eq!(
            fs::metadata(log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
