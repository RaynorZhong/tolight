use super::{Result, SourceArgs, common::*};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::OsString,
    fmt,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::Ordering,
    time::Duration,
};

#[derive(Debug)]
struct Failure {
    code: String,
    paths: Vec<String>,
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.code.fmt(f)
    }
}
impl Error for Failure {}
fn failure(code: &str, paths: Vec<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(Failure {
        code: code.into(),
        paths,
    })
}
fn require(condition: bool, code: &str, paths: Vec<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(failure(code, paths))
    }
}

#[derive(Clone, PartialEq)]
struct Fingerprint {
    length: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}
impl Fingerprint {
    fn metadata(metadata: &fs::Metadata) -> Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }
}
struct Entry {
    path: PathBuf,
    target: Option<PathBuf>,
    mode: u32,
    fingerprint: Option<Fingerprint>,
}

fn environment() -> Result<BTreeMap<OsString, OsString>> {
    let mut env = BTreeMap::new();
    for name in [
        "HOME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TMPDIR",
        "TMP",
        "TEMP",
        "RUSTUP_HOME",
    ] {
        if let Some(value) = std::env::var_os(name) {
            env.insert(name.into(), value);
        }
    }
    let paths =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()))
            .filter(|p| p.is_absolute())
            .collect::<Vec<_>>();
    env.insert("PATH".into(), std::env::join_paths(paths)?);
    let cargo = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::repository().join(".local/cargo"));
    env.insert(
        "CARGO_HOME".into(),
        canonical_or_absolute(&cargo)?.into_os_string(),
    );
    for (key, value) in [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_ALLOW_PROTOCOL", "file"),
        ("SQLX_OFFLINE", "true"),
        ("CARGO_TERM_COLOR", "never"),
    ] {
        env.insert(key.into(), value.into());
    }
    if let Ok(value) = std::env::var("CARGO_NET_OFFLINE")
        && ["true", "false"].contains(&value.as_str())
    {
        env.insert("CARGO_NET_OFFLINE".into(), value.into());
    }
    let cache = std::env::var_os("npm_config_cache")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::repository().join(".local/npm-cache"));
    env.insert(
        "npm_config_cache".into(),
        canonical_or_absolute(&cache)?.into_os_string(),
    );
    Ok(env)
}

#[allow(
    clippy::too_many_arguments,
    reason = "argv, isolation, logging and failure policy remain explicit at every external process boundary"
)]
fn invoke(
    program: &str,
    args: &[OsString],
    cwd: &Path,
    env: &BTreeMap<OsString, OsString>,
    log: &Path,
    code: &str,
    seconds: u64,
    allow_failure: bool,
) -> Result<Output> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).env_clear().envs(env);
    let result = command_output_logged(
        command,
        Duration::from_secs(seconds),
        16 * 1024 * 1024,
        Some(log),
    )
    .map_err(|error| {
        let detail = error.to_string();
        failure(
            if CANCELLED.load(Ordering::SeqCst) {
                "interrupted"
            } else if detail.contains("timed out") {
                return failure(&format!("{code}_timeout"), vec![]);
            } else {
                code
            },
            vec![],
        )
    })?;
    require(allow_failure || result.status.success(), code, vec![])?;
    Ok(result)
}

fn git(
    args: &[&str],
    cwd: &Path,
    env: &BTreeMap<OsString, OsString>,
    log: &Path,
    allow_failure: bool,
) -> Result<Output> {
    let args = [
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "tag.gpgsign=false",
    ]
    .into_iter()
    .chain(args.iter().copied())
    .map(OsString::from)
    .collect::<Vec<_>>();
    invoke(
        "git",
        &args,
        cwd,
        env,
        log,
        "git_command_failed",
        60,
        allow_failure,
    )
}

fn tracked(
    source: &Path,
    env: &BTreeMap<OsString, OsString>,
    log: &Path,
) -> Result<Option<String>> {
    let top = git(&["rev-parse", "--show-toplevel"], source, env, log, true)?;
    if !top.status.success() {
        return Ok(None);
    }
    require(
        Path::new(String::from_utf8_lossy(&top.stdout).trim()).canonicalize()? == source,
        "source_must_be_repository_root",
        vec![],
    )?;
    let staged = git(&["ls-files", "--stage", "-z"], source, env, log, false)?;
    let mut links = vec![];
    let mut private = vec![];
    for row in staged
        .stdout
        .split(|b| *b == 0)
        .filter(|row| !row.is_empty())
    {
        let row = std::str::from_utf8(row)?;
        let (metadata, path) = row.split_once('\t').ok_or("invalid Git index output")?;
        if metadata.starts_with("160000 ") {
            links.push(path.into())
        }
        if private_path(Path::new(path)) {
            private.push(path.into())
        }
    }
    require(links.is_empty(), "unsupported_source_gitlink", links)?;
    require(private.is_empty(), "private_tracked_paths", private)?;
    let head = git(&["rev-parse", "--verify", "HEAD"], source, env, log, true)?;
    Ok(head
        .status
        .success()
        .then(|| String::from_utf8_lossy(&head.stdout).trim().to_owned()))
}

fn inventory(source: &Path) -> Result<Vec<Entry>> {
    fn visit(source: &Path, path: &Path, entries: &mut Vec<Entry>) -> Result<()> {
        let relative = path.strip_prefix(source)?;
        // A TypeScript compiler output is never source. A tracked copy is
        // rejected by tracked(), while an untracked generated copy is omitted.
        if relative == Path::new("public/app.js") {
            return Ok(());
        }
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            require(
                source_path_allowed(relative),
                "private_source_file",
                vec![relative.display().to_string()],
            )?;
            let target = fs::read_link(path)?;
            let resolved = path.canonicalize().map_err(|_| {
                failure(
                    "unsafe_source_symlink",
                    vec![relative.display().to_string()],
                )
            })?;
            require(
                !target.is_absolute()
                    && resolved.strip_prefix(source).is_ok_and(source_path_allowed),
                "unsafe_source_symlink",
                vec![relative.display().to_string()],
            )?;
            entries.push(Entry {
                path: relative.into(),
                target: Some(target),
                mode: 0,
                fingerprint: None,
            });
        } else if metadata.is_dir() {
            require(
                relative != Path::new("public/data"),
                "runtime_public_data_in_source",
                vec![relative.display().to_string()],
            )?;
            if relative
                .file_name()
                .and_then(|p| p.to_str())
                .is_some_and(|name| PRIVATE_DIRS.contains(&name))
            {
                return Ok(());
            }
            let mut children = fs::read_dir(path)?
                .map(|entry| entry.map(|v| v.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            children.sort();
            for child in children {
                visit(source, &child, entries)?;
            }
        } else if metadata.is_file() {
            require(
                source_path_allowed(relative),
                "private_source_file",
                vec![relative.display().to_string()],
            )?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o100 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            entries.push(Entry {
                path: relative.into(),
                target: None,
                mode: if executable { 0o755 } else { 0o644 },
                fingerprint: Some(Fingerprint::metadata(&metadata)?),
            });
        } else {
            return Err(failure(
                "unsupported_source_file",
                vec![relative.display().to_string()],
            ));
        }
        Ok(())
    }
    let mut entries = vec![];
    for name in set(ROOT_FILES.iter().chain(ROOT_DIRS).copied()) {
        let path = source.join(name);
        if path.exists() || path.is_symlink() {
            visit(source, &path, &mut entries)?;
        }
    }
    Ok(entries)
}

/// The same concrete input boundary used by a source snapshot, without imposing
/// the snapshot command's additional required-file contract on smaller fixtures.
pub(super) fn input_material(source: &Path) -> Result<BTreeSet<String>> {
    inventory(source)?
        .into_iter()
        .map(|entry| {
            Ok(entry
                .path
                .to_str()
                .ok_or("non-UTF8 source path")?
                .to_owned())
        })
        .collect()
}

fn entries(source: &Path) -> Result<Vec<Entry>> {
    let entries = inventory(source)?;
    let copied = set(entries.iter().map(|entry| entry.path.display().to_string()));
    let mut required = ROOT_FILES
        .iter()
        .filter(|name| {
            ![
                "flake.lock",
                ".env.example",
                "package.json",
                "package-lock.json",
                "tsconfig.json",
                "tsconfig.web.json",
                "tsconfig.admin.json",
            ]
            .contains(name)
        })
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    required.extend(
        [
            "docs/release-status.toml",
            "docs/join.md",
            "docs/participant-spec.md",
            "docs/protocol.md",
            "docs/rules.md",
            "docs/signing-vectors.json",
            "examples/python/minimal_client.py",
            "examples/python/check_vectors.py",
        ]
        .map(str::to_owned),
    );
    if source.join("package.json").exists()
        || source.join("package-lock.json").exists()
        || source.join("frontend").exists()
        || source.join("crates/tolight-cli").is_dir()
    {
        required.extend(WEB_SOURCE_FILES.iter().map(|name| (*name).to_owned()));
    }
    required.extend(
        referenced_brand_sources(source)
            .into_iter()
            .map(str::to_owned),
    );
    let mut missing = vec![];
    for name in &required {
        let path = source.join(name);
        if !path.is_file() || !required_source_material(source, &path)?.is_subset(&copied) {
            missing.push(name.clone());
        }
    }
    require(missing.is_empty(), "missing_required_source", missing)?;
    for name in required {
        require(
            source.join(&name).is_file(),
            "required_source_is_not_file",
            vec![name],
        )?;
    }
    Ok(entries)
}

fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn copy(source: &Path, destination: &Path, entries: &[Entry]) -> Result<Value> {
    private_dir(destination)?;
    let mut manifest = vec![];
    for entry in entries {
        let output = destination.join(&entry.path);
        fs::create_dir_all(output.parent().ok_or("invalid source filename")?)?;
        if let Some(target) = &entry.target {
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &output)?;
            #[cfg(not(unix))]
            return Err("source symlinks require Unix".into());
            manifest.push(json!({"path":entry.path,"kind":"symlink","target":target}));
        } else {
            let mut file = File::open(source.join(&entry.path))?;
            require(
                Some(Fingerprint::metadata(&file.metadata()?)?) == entry.fingerprint,
                "source_changed_during_copy",
                vec![entry.path.display().to_string()],
            )?;
            let mut bytes = vec![];
            file.read_to_end(&mut bytes)?;
            require(
                Some(Fingerprint::metadata(&file.metadata()?)?) == entry.fingerprint,
                "source_changed_during_copy",
                vec![entry.path.display().to_string()],
            )?;
            fs::write(&output, bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&output, fs::Permissions::from_mode(entry.mode))?;
            }
            manifest.push(json!({"path":entry.path,"kind":"file","mode":format!("{:#o}",entry.mode),"sha256":sha256(&output)?}));
        }
    }
    Ok(json!(manifest))
}

fn matches(checkout: &Path, manifest: &Value) -> Result<()> {
    for entry in manifest.as_array().ok_or("invalid source manifest")? {
        let path = checkout.join(str_field(entry, "path")?);
        let valid = if entry["kind"] == "symlink" {
            path.is_symlink() && fs::read_link(&path)? == Path::new(str_field(entry, "target")?)
        } else {
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                fs::metadata(&path)?.permissions().mode() & 0o100 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            path.is_file()
                && !path.is_symlink()
                && sha256(&path)? == entry["sha256"]
                && executable == (entry["mode"] == "0o755")
        };
        require(valid, "checkout_differs_from_snapshot", vec![])?;
    }
    Ok(())
}

fn reject_ambient(checkout: &Path, cargo_home: &Path) -> Result<()> {
    let mut found = vec![];
    for parent in checkout.ancestors().skip(1) {
        for name in [".env", ".cargo/config", ".cargo/config.toml"] {
            let path = parent.join(name);
            if path.exists() || path.is_symlink() {
                found.push(path.display().to_string());
            }
        }
    }
    for name in ["config", "config.toml"] {
        let path = cargo_home.join(name);
        if path.exists() || path.is_symlink() {
            found.push(path.display().to_string());
        }
    }
    found.sort();
    found.dedup();
    require(found.is_empty(), "ambient_build_configuration", found)
}

fn rehearse(args: &SourceArgs, source: &Path, work: &Path, report: &mut Value) -> Result<()> {
    let env = environment()?;
    let log = work.join("git.log");
    private_file(&log, b"")?;
    report["source_head"] = json!(tracked(source, &env, &log)?);
    let entries = entries(source)?;
    let fixture = work.join("fixture");
    let manifest = copy(source, &fixture, &entries)?;
    let package = read_toml(fixture.join("Cargo.toml"))?;
    let version = package["workspace"]["package"]["version"]
        .as_str()
        .ok_or("invalid workspace version")?;
    require(semver(version).is_ok(), "invalid_workspace_version", vec![])?;
    let tag = format!("local-rehearsal-v{version}");
    report["code_version"] = json!(version);
    report["fixture_tag"] = json!(tag);
    report["fixture_tag_is_formal_release"] = json!(false);
    report["source_file_count"] = json!(entries.len());
    private_json(&work.join("source-manifest.json"), &manifest)?;
    git(
        &["init", "--initial-branch=local-rehearsal"],
        &fixture,
        &env,
        &log,
        false,
    )?;
    git(
        &["add", "--all", "--force", "--", "."],
        &fixture,
        &env,
        &log,
        false,
    )?;
    let mut fixture_env = env.clone();
    for (key, value) in [
        ("GIT_AUTHOR_NAME", "tolight local rehearsal"),
        ("GIT_AUTHOR_EMAIL", "local-rehearsal@example.invalid"),
        ("GIT_COMMITTER_NAME", "tolight local rehearsal"),
        ("GIT_COMMITTER_EMAIL", "local-rehearsal@example.invalid"),
    ] {
        fixture_env.insert(key.into(), value.into());
    }
    git(
        &[
            "commit",
            "--message",
            "Unpublished local source rehearsal snapshot",
        ],
        &fixture,
        &fixture_env,
        &log,
        false,
    )?;
    let commit =
        String::from_utf8(git(&["rev-parse", "HEAD"], &fixture, &env, &log, false)?.stdout)?
            .trim()
            .to_owned();
    git(
        &[
            "tag",
            "--annotate",
            &tag,
            "--message",
            "Local unpublished rehearsal fixture; not a release",
        ],
        &fixture,
        &fixture_env,
        &log,
        false,
    )?;
    let tagged = git(
        &["rev-parse", &format!("refs/tags/{tag}^{{}}")],
        &fixture,
        &env,
        &log,
        false,
    )?;
    require(
        String::from_utf8_lossy(&tagged.stdout).trim() == commit,
        "fixture_tag_does_not_match_commit",
        vec![],
    )?;
    let checkout = work.join("checkout");
    git(
        &[
            "clone",
            "--no-local",
            "--no-hardlinks",
            "--no-checkout",
            fixture.to_str().ok_or("non-UTF8 fixture path")?,
            checkout.to_str().ok_or("non-UTF8 checkout path")?,
        ],
        work,
        &env,
        &log,
        false,
    )?;
    git(
        &["checkout", "--detach", &commit],
        &checkout,
        &env,
        &log,
        false,
    )?;
    let actual = git(&["rev-parse", "HEAD"], &checkout, &env, &log, false)?;
    let dirty = git(
        &["status", "--porcelain", "--untracked-files=all"],
        &checkout,
        &env,
        &log,
        false,
    )?;
    let tracked = git(&["ls-files", "-z"], &checkout, &env, &log, false)?;
    let names = set(tracked
        .stdout
        .split(|b| *b == 0)
        .filter(|v| !v.is_empty())
        .map(|v| String::from_utf8_lossy(v).into_owned()));
    require(
        String::from_utf8_lossy(&actual.stdout).trim() == commit
            && dirty.stdout.is_empty()
            && names == set(entries.iter().map(|e| e.path.display().to_string())),
        "checkout_not_exact_and_clean",
        vec![],
    )?;
    matches(&checkout, &manifest)?;
    report["commit"] = json!(commit);
    report["checkout_head"] = json!(String::from_utf8_lossy(&actual.stdout).trim());
    report["checkout_clean_before_build"] = json!(true);
    report["checkout"] = json!(checkout);
    report["fixture"] = json!(fixture);
    if args.prepare_only {
        report["status"] = json!("prepared");
        return Ok(());
    }
    reject_ambient(
        &checkout,
        Path::new(
            env.get(std::ffi::OsStr::new("CARGO_HOME"))
                .ok_or("missing Cargo home")?,
        ),
    )?;
    let target = work.join("target");
    let offline = checkout.join("crates/tolight-coordinator-store/.sqlx");
    let mut build_env = env.clone();
    build_env.insert("CARGO_TARGET_DIR".into(), target.as_os_str().to_owned());
    build_env.insert("SQLX_OFFLINE_DIR".into(), offline.as_os_str().to_owned());
    // npm rejects loading the same path as both its user and global config.
    // Distinct private empty files also prevent borrowing the operator's setup.
    let npm_user = work.join("npm-user.conf");
    let npm_global = work.join("npm-global.conf");
    private_file(&npm_user, b"")?;
    private_file(&npm_global, b"")?;
    build_env.insert("npm_config_userconfig".into(), npm_user.into_os_string());
    build_env.insert(
        "npm_config_globalconfig".into(),
        npm_global.into_os_string(),
    );
    if checkout.join("package-lock.json").is_file() {
        invoke(
            "npm",
            &["ci".into(), "--ignore-scripts".into()],
            &checkout,
            &build_env,
            &work.join("npm-install.log"),
            "npm_install_failed",
            900,
            false,
        )?;
        invoke(
            "npm",
            &["run".into(), "build:web".into()],
            &checkout,
            &build_env,
            &work.join("frontend-build.log"),
            "frontend_build_failed",
            300,
            false,
        )?;
        report["frontend_built"] = json!(true);
    }
    report["build_command"] = json!(["cargo", "build", "--locked", "--workspace"]);
    report["build_environment"] = json!({"SQLX_OFFLINE":"true","SQLX_OFFLINE_DIR":offline,"CARGO_TARGET_DIR":target,"CARGO_HOME":env.get(std::ffi::OsStr::new("CARGO_HOME"))});
    invoke(
        "cargo",
        &["build".into(), "--locked".into(), "--workspace".into()],
        &checkout,
        &build_env,
        &work.join("build.log"),
        "cargo_build_failed",
        1800,
        false,
    )?;
    let metadata = invoke(
        "cargo",
        &[
            "metadata".into(),
            "--locked".into(),
            "--no-deps".into(),
            "--format-version".into(),
            "1".into(),
        ],
        &checkout,
        &build_env,
        &work.join("metadata.log"),
        "cargo_metadata_failed",
        30,
        false,
    )?;
    let metadata: Value = serde_json::from_slice(&metadata.stdout)?;
    let members = set(array_field(&metadata, "workspace_members")?
        .iter()
        .filter_map(Value::as_str));
    let mut binaries = BTreeSet::new();
    for package in array_field(&metadata, "packages")? {
        if members.contains(str_field(package, "id")?) {
            for target in array_field(package, "targets")? {
                if target["kind"]
                    .as_array()
                    .is_some_and(|k| k.iter().any(|v| v == "bin"))
                {
                    binaries.insert(str_field(target, "name")?.to_owned());
                }
            }
        }
    }
    require(
        binaries.contains("tolight") && binaries.contains("reference-client"),
        "expected_binary_missing",
        vec![],
    )?;
    for binary in &binaries {
        let path = target
            .join("debug")
            .join(format!("{binary}{}", std::env::consts::EXE_SUFFIX));
        require(path.is_file(), "expected_binary_missing", vec![])?;
        invoke(
            path.to_str().ok_or("non-UTF8 binary path")?,
            &["--help".into()],
            &checkout,
            &build_env,
            &work.join("binary-help.log"),
            "binary_help_failed",
            15,
            false,
        )?;
    }
    let dirty = git(
        &["status", "--porcelain", "--untracked-files=all"],
        &checkout,
        &env,
        &log,
        false,
    )?;
    matches(&checkout, &manifest)?;
    require(
        dirty.stdout.is_empty(),
        "build_changed_checked_out_source",
        vec![],
    )?;
    report["built"] = json!(true);
    report["status"] = json!("passed");
    report["binary_help_passed"] = json!(binaries);
    report["checkout_clean_after_build"] = json!(true);
    report["target_dir"] = json!(target);
    Ok(())
}

fn run_inner(args: SourceArgs) -> Result<()> {
    let source = args.source.canonicalize()?;
    let base = canonical_or_absolute(&args.work_dir)?;
    if let Ok(relative) = base.strip_prefix(&source) {
        require(
            relative
                .components()
                .next()
                .is_some_and(|c| c.as_os_str() == ".local"),
            "work_directory_must_be_private_and_outside_source_allowlist",
            vec![],
        )?;
    }
    private_dir(&base)?;
    let work = tempfile::Builder::new()
        .prefix("source-rehearsal.")
        .tempdir_in(&base)?
        .keep();
    private_dir(&work)?;
    let mut report = json!({"schema":"tolight-source-rehearsal/1","scope":"local_unpublished_source_rehearsal","status":"failed","built":false,"published":false,"user_repository_modified":false,
        "started_at":Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros,true),"source":source,"work_dir":work,"report":work.join("report.json")});
    if let Err(error) = rehearse(&args, &source, &work, &mut report) {
        if let Some(error) = error.downcast_ref::<Failure>() {
            report["error_code"] = json!(error.code);
            report["rejected_paths"] = json!(error.paths)
        } else {
            report["error_code"] = json!(if CANCELLED.load(Ordering::SeqCst) {
                "interrupted"
            } else {
                "source_rehearsal_failed"
            })
        }
    }
    report["finished_at"] = json!(Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true));
    private_json(&work.join("report.json"), &report)?;
    let mut summary = serde_json::Map::new();
    for key in [
        "scope",
        "status",
        "built",
        "published",
        "work_dir",
        "report",
        "error_code",
        "commit",
        "fixture_tag",
    ] {
        if let Some(value) = report.get(key) {
            summary.insert(key.into(), value.clone());
        }
    }
    println!("{}", serde_json::to_string(&summary)?);
    if ["prepared", "passed"].contains(&report["status"].as_str().unwrap_or("")) {
        Ok(())
    } else {
        Err(Box::new(crate::ops::ExitCode(1)))
    }
}

pub fn run(args: SourceArgs) -> Result<()> {
    match run_inner(args) {
        Ok(()) => Ok(()),
        Err(error) if error.downcast_ref::<crate::ops::ExitCode>().is_some() => Err(error),
        Err(_) => {
            println!(
                "{}",
                json!({"scope":"local_unpublished_source_rehearsal","status":"failed","built":false,"published":false,"error_code":"source_or_work_directory_invalid"})
            );
            Err(Box::new(crate::ops::ExitCode(1)))
        }
    }
}
