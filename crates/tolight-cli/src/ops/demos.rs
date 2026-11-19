//! Native disposable demonstrations. Evidence is retained; databases are owned.
use super::{ExitCode, Result, runtime::common as c};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command as Process, Stdio},
    time::{Duration, Instant},
};
mod m1;
mod m2;
#[derive(Subcommand)]
pub enum Command {
    M1(M1),
    M2(M2),
    #[command(hide = true)]
    NotifySink {
        #[arg(long)]
        output: PathBuf,
    },
    #[command(hide = true)]
    ModelFixture,
}
#[derive(Args)]
#[command(args_override_self = true)]
pub struct M1 {
    #[arg(long, default_value_t = 150, value_parser = clap::value_parser!(u64).range(65..))]
    duration: u64,
    #[arg(long, default_value_t = 6, value_parser = positive_count)]
    min_messages: usize,
    #[arg(long)]
    keep_running: bool,
    #[arg(long)]
    rollback_baseline: Option<PathBuf>,
}
#[derive(Args)]
#[command(args_override_self = true)]
pub struct M2 {
    #[arg(long, default_value_t = 160, value_parser = clap::value_parser!(u64).range(20..))]
    duration: u64,
    #[arg(long)]
    checkpoints: bool,
    #[arg(long)]
    keep_running: bool,
    #[arg(long)]
    with_model_stub: bool,
}
fn positive_count(value: &str) -> std::result::Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "message count must be positive".into())
}
fn utc() -> String {
    tolight_core::datetime::utc_seconds(time::OffsetDateTime::now_utc())
}
fn quote(path: impl AsRef<Path>) -> String {
    serde_json::to_string(&path.as_ref().to_string_lossy()).expect("path JSON")
}
fn json_file(path: impl AsRef<Path>) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn files(path: &Path) -> Result<Vec<PathBuf>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        c::ensure(
            !entry.file_type()?.is_symlink(),
            "demo evidence tree contains symlink",
        )?;
        if entry.file_type()?.is_dir() {
            paths.extend(files(&entry.path())?);
        } else {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}
fn private_text(path: &Path, text: &str) -> Result {
    let mut file = c::private_open(path, false)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(())
}
fn ports(count: usize) -> Result<Vec<u16>> {
    let held = (0..count)
        .map(|_| TcpListener::bind("127.0.0.1:0"))
        .collect::<std::io::Result<Vec<_>>>()?;
    Ok(held
        .iter()
        .map(|s| s.local_addr().map(|a| a.port()))
        .collect::<std::io::Result<_>>()?)
}
type Env = BTreeMap<OsString, OsString>;
fn command(program: impl AsRef<OsStr>, env: &Env) -> Process {
    let mut cmd = Process::new(program);
    cmd.current_dir(c::root())
        .env_clear()
        .envs(env)
        .stdin(Stdio::null());
    cmd
}
fn signal(child: &mut Child, kind: &str) -> Result {
    if child.try_wait()?.is_none() {
        c::ensure(
            Process::new("/bin/kill")
                .arg(format!("-{kind}"))
                .arg(child.id().to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?
                .success(),
            "could not signal owned demo child",
        )?;
    }
    Ok(())
}
fn stop(child: &mut Child, seconds: u64) -> Result {
    if child.try_wait()?.is_none() {
        signal(child, "INT")?;
    }
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            signal(child, "TERM")?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if child.try_wait()?.is_some() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            child.kill()?;
            child.wait()?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
struct Database {
    root: PathBuf,
    env: Env,
    child: Child,
    identity: String,
}
impl Database {
    fn new() -> Result<Self> {
        let root = c::root();
        fs::create_dir_all(root.join(".local"))?;
        let directory = tempfile::Builder::new()
            .prefix("test-db.native-")
            .tempdir_in(root.join(".local"))?
            .keep();
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        fs::create_dir(directory.join("socket"))?;
        let mut env: Env = std::env::vars_os()
            .filter(|(k, _)| {
                !k.to_string_lossy().starts_with("PG")
                    && k != "DATABASE_URL"
                    && k != "CARGO_BUILD_TARGET"
            })
            .collect();
        let paths = std::iter::once(root.join(".local/pg18/bin"))
            .chain(std::env::split_paths(
                env.get(OsStr::new("PATH"))
                    .map(OsString::as_os_str)
                    .unwrap_or(OsStr::new("/usr/bin:/bin")),
            ))
            .collect::<Vec<_>>();
        env.insert("PATH".into(), std::env::join_paths(paths)?);
        for (k, v) in [
            ("PGHOST", directory.join("socket").into_os_string()),
            ("PGPORT", "5432".into()),
            ("PGUSER", "tolight".into()),
            ("PGDATABASE", "postgres".into()),
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
            env.insert(k.into(), v);
        }
        env.entry("CARGO_HOME".into())
            .or_insert(root.join(".local/cargo").into_os_string());
        let mut version = command("initdb", &env);
        version.arg("--version");
        let result = c::capture(&mut version, 5, None)?;
        c::ensure(
            result.status.success() && String::from_utf8(result.stdout)?.contains(" 18."),
            "PostgreSQL18 tools required",
        )?;
        let mut init = command("initdb", &env);
        init.arg("-D").arg(directory.join("data")).args([
            "-U",
            "tolight",
            "--encoding=UTF8",
            "--locale=C",
            "--auth-local=trust",
            "--auth-host=reject",
        ]);
        let result = c::capture(&mut init, 30, None)?;
        c::ensure(
            result.status.success(),
            "demo initdb failed; private directory retained",
        )?;
        let log = c::private_open(&directory.join("postgres.log"), true)?;
        let mut cmd = command("postgres", &env);
        cmd.arg("-D")
            .arg(directory.join("data"))
            .arg("-h")
            .arg("")
            .arg("-k")
            .arg(directory.join("socket"))
            .args(["-p", "5432", "-c", "timezone=UTC"])
            .stdout(log.try_clone()?)
            .stderr(log);
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd.spawn()?;
        let mut db = Self {
            root: directory,
            env,
            child,
            identity: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            c::ensure(
                db.child.try_wait()?.is_none(),
                "owned demo PostgreSQL exited",
            )?;
            let mut check = command("psql", &db.env);
            check.args([
                "-w",
                "-XAt",
                "-c",
                "SELECT system_identifier::text FROM pg_control_system()",
            ]);
            if let Ok(output) = c::capture(&mut check, 3, None)
                && output.status.success()
            {
                db.identity = String::from_utf8(output.stdout)?.trim().into();
                break;
            }
            c::ensure(
                Instant::now() < deadline,
                "owned demo PostgreSQL readiness failed",
            )?;
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut create = command("createdb", &db.env);
        create.args(["-w", "tolight_test"]);
        c::ensure(
            c::capture(&mut create, 15, None)?.status.success(),
            "cannot create disposable demo database",
        )?;
        db.env.insert("PGDATABASE".into(), "tolight_test".into());
        let encoded = percent_encoding::utf8_percent_encode(
            db.root.join("socket").to_str().ok_or("UTF8 demo path")?,
            percent_encoding::NON_ALPHANUMERIC,
        )
        .to_string();
        db.env.insert(
            "DATABASE_URL".into(),
            format!("postgresql://tolight@localhost:5432/tolight_test?host={encoded}").into(),
        );
        db.verify()?;
        Ok(db)
    }
    fn verify(&self) -> Result<Value> {
        c::ensure(
            self.root.parent() == Some(c::root().join(".local").as_path())
                && self
                    .root
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("test-db.native-"))
                && fs::read_to_string(self.root.join("data/PG_VERSION"))?.trim() == "18",
            "not an owned disposable PostgreSQL18 cluster",
        )?;
        let mut cmd = command("psql", &self.env);
        cmd.args(["-w","-XAt","-c","SELECT json_build_object('system_identifier',(SELECT system_identifier::text FROM pg_control_system()),'data_directory',current_setting('data_directory'),'database',current_database())"]);
        let result = c::capture(&mut cmd, 10, None)?;
        c::ensure(
            result.status.success(),
            "disposable database identity unavailable",
        )?;
        let value: Value = serde_json::from_slice(&result.stdout)?;
        c::ensure(
            value["system_identifier"] == self.identity
                && value["database"] == "tolight_test"
                && value["data_directory"].as_str() == self.root.join("data").to_str(),
            "refusing reset of another database or cluster",
        )?;
        Ok(
            json!({"cluster":self.root,"database":"tolight_test","postgres_major":18,"system_identifier":self.identity}),
        )
    }
    fn reset(&self) -> Result {
        self.verify()?;
        let mut drop = command("dropdb", &self.env);
        drop.args(["--maintenance-db", "postgres", "--force", "tolight_test"]);
        c::ensure(
            c::capture(&mut drop, 15, None)?.status.success(),
            "cannot reset owned demonstration database",
        )?;
        let mut create = command("createdb", &self.env);
        create.args(["--maintenance-db", "postgres", "tolight_test"]);
        c::ensure(
            c::capture(&mut create, 15, None)?.status.success(),
            "cannot recreate owned demonstration database",
        )
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let stopped = if matches!(self.child.try_wait(), Ok(Some(_))) {
            true
        } else {
            let _ = signal(&mut self.child, "INT");
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(20));
            }
            matches!(self.child.try_wait(), Ok(Some(_)))
        };
        let mut status = command("pg_ctl", &self.env);
        status.arg("-D").arg(self.root.join("data")).arg("status");
        let absent = stopped
            && c::capture(&mut status, 5, None).is_ok_and(|result| result.status.code() == Some(3));
        if absent {
            let _ = fs::remove_dir_all(&self.root);
        } else {
            eprintln!(
                "owned demo PostgreSQL remains live; retained {}",
                self.root.display()
            );
        }
    }
}
struct Owned {
    child: Child,
    name: String,
    client: bool,
}
struct Demo {
    db: Database,
    run: PathBuf,
    public: PathBuf,
    logs: PathBuf,
    actors: PathBuf,
    api: String,
    web: String,
    api_port: u16,
    web_port: u16,
    ops_port: u16,
    config: PathBuf,
    tolight: PathBuf,
    reference: PathBuf,
    children: Vec<Owned>,
    configs: BTreeMap<String, PathBuf>,
    approved: BTreeMap<String, String>,
    steps: Vec<String>,
    counter: u64,
    server: Option<usize>,
    metadata: Value,
    notifications: PathBuf,
    http: reqwest::Client,
    model: bool,
}
impl Demo {
    async fn new(m2: bool, model: bool) -> Result<Self> {
        let root = c::root();
        let mut env: BTreeMap<_, _> = std::env::vars_os()
            .filter(|(key, _)| key != "CARGO_BUILD_TARGET")
            .collect();
        env.entry("CARGO_HOME".into())
            .or_insert(root.join(".local/cargo").into_os_string());
        let mut build = command("cargo", &env);
        build
            .args([
                "build",
                "--locked",
                "-p",
                "tolight-cli",
                "-p",
                "tolight-reference-client",
            ])
            .env("SQLX_OFFLINE", "true")
            .env(
                "SQLX_OFFLINE_DIR",
                root.join("crates/tolight-coordinator-store/.sqlx"),
            )
            .env("CARGO_TARGET_DIR", root.join("target"));
        c::ensure(
            c::capture(&mut build, 300, None)?.status.success(),
            "build native demo binaries first",
        )?;
        let mut web = command("npm", &env);
        web.args(["run", "build:web"]);
        c::ensure(
            c::capture(&mut web, 120, None)?.status.success(),
            "TypeScript build failed; run npm ci before the demo",
        )?;
        let db = Database::new()?;
        let run = tempfile::Builder::new()
            .prefix(if m2 { "m2-demo." } else { "m1-demo." })
            .tempdir_in(root.join(".local"))?
            .keep();
        #[cfg(unix)]
        fs::set_permissions(&run, fs::Permissions::from_mode(0o700))?;
        for name in ["public", "logs", "actors"] {
            fs::create_dir(run.join(name))?;
        }
        let ports = ports(3)?;
        let api = format!("http://127.0.0.1:{}/participant/v1/", ports[0]);
        let web = format!("http://127.0.0.1:{}", ports[1]);
        let metadata = json!({"schema":if m2 {"tolight-m2-local-demo/1"} else {"tolight-m1-local-demo/1"},"directory":run,"api_url":api,"public_url":web,"public_dir":run.join("public"),"config":run.join("coordinator.toml"),"phase":"starting","pid":std::process::id(),"scope":"disposable local PostgreSQL; project-owned scripts; no third-party acceptance","model_command_fixture":model});
        let mut demo = Self {
            public: run.join("public"),
            logs: run.join("logs"),
            actors: run.join("actors"),
            config: run.join("coordinator.toml"),
            notifications: run.join("notifications.jsonl"),
            run,
            db,
            api,
            web,
            api_port: ports[0],
            web_port: ports[1],
            ops_port: ports[2],
            tolight: root.join("target/debug/tolight"),
            reference: root.join("target/debug/reference-client"),
            children: vec![],
            configs: BTreeMap::new(),
            approved: BTreeMap::new(),
            steps: vec![],
            counter: 0,
            server: None,
            metadata,
            http: c::client(None, None)?,
            model,
        };
        demo.write_config(true)?;
        demo.record("starting", json!({}), m2)?;
        Ok(demo)
    }
    fn write_config(&self, demo: bool) -> Result {
        private_text(
            &self.config,
            &format!(
                "listen=\"127.0.0.1:{}\"\nconformance_retry_after=1\nconformance_reply_window=60\n{}[observability]\nlisten=\"127.0.0.1:{}\"\nlog_dir={}\n[database]\nsecret_cmd=[\"/usr/bin/printenv\",\"DATABASE_URL\"]\n[public]\ndir={}\ndemo_banner={}\n",
                self.api_port,
                if self.model {
                    "applications_per_ip_per_day=20\n"
                } else {
                    ""
                },
                self.ops_port,
                quote(self.run.join("service-logs")),
                quote(&self.public),
                demo
            ),
        )
    }
    fn cmd(&self, program: impl AsRef<OsStr>) -> Process {
        command(program, &self.db.env)
    }
    async fn command(
        &mut self,
        mut cmd: Process,
        name: &str,
        expected: i32,
        seconds: u64,
    ) -> Result<String> {
        self.counter += 1;
        let stdout = self.logs.join(format!("{:03}-{name}.log", self.counter));
        let stderr = self.logs.join(format!("{:03}-{name}.stderr", self.counter));
        cmd.stdout(c::private_open(&stdout, false)?)
            .stderr(c::private_open(&stderr, false)?);
        let child = cmd.spawn()?;
        let index = self.children.len();
        self.children.push(Owned {
            child,
            name: name.into(),
            client: false,
        });
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let code = loop {
            if let Some(status) = self.children[index].child.try_wait()? {
                break status.code().unwrap_or(1);
            }
            c::ensure(
                Instant::now() < deadline,
                "demo command timeout; evidence retained",
            )?;
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        let output = fs::read_to_string(stdout)?;
        c::ensure(
            code == expected,
            &format!(
                "{name} exited {code}, expected {expected}; see {}",
                self.logs.display()
            ),
        )?;
        Ok(output)
    }
    async fn ctl(&mut self, args: Vec<OsString>, name: &str) -> Result<String> {
        let mut cmd = self.cmd(&self.tolight);
        cmd.arg("--config").arg(&self.config).args(args);
        self.command(cmd, name, 0, 150).await
    }
    async fn reference(
        &mut self,
        actor: &str,
        args: Vec<OsString>,
        name: &str,
        expected: i32,
    ) -> Result<String> {
        let mut cmd = self.cmd(&self.reference);
        cmd.arg("--config")
            .arg(self.configs.get(actor).ok_or("unknown actor")?)
            .args(args);
        self.command(cmd, name, expected, 150).await
    }
    async fn info(&mut self, actor: &str) -> Result<Value> {
        Ok(serde_json::from_str(
            &self
                .reference(actor, osargs(["status"]), &format!("{actor}-status"), 0)
                .await?,
        )?)
    }
    fn start(&mut self, mut cmd: Process, name: &str, client: bool) -> Result<usize> {
        let file = c::private_open(&self.logs.join(format!("{name}.log")), false)?;
        cmd.stdout(file.try_clone()?).stderr(file);
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd.spawn()?;
        let index = self.children.len();
        self.children.push(Owned {
            child,
            name: name.into(),
            client,
        });
        Ok(index)
    }
    fn actor_run(&mut self, actor: &str, suffix: &str) -> Result<usize> {
        let mut cmd = self.cmd(&self.reference);
        cmd.arg("--config")
            .arg(self.configs.get(actor).ok_or("unknown actor")?)
            .arg("run");
        self.start(cmd, &format!("{actor}-{suffix}"), true)
    }
    async fn wait(&mut self, index: usize, seconds: u64, expected: i32) -> Result {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            if let Some(status) = self.children[index].child.try_wait()? {
                return c::ensure(
                    status.code() == Some(expected),
                    "demo child exited unexpectedly",
                );
            }
            c::ensure(Instant::now() < deadline, "demo child did not stop on time")?;
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    async fn stop(&mut self, index: usize, seconds: u64) -> Result {
        signal(&mut self.children[index].child, "INT")?;
        self.wait(index, seconds, 0).await
    }
    async fn stop_actors(&mut self, actors: &[usize]) -> Result {
        for &index in actors {
            signal(&mut self.children[index].child, "INT")?;
        }
        for &index in actors {
            self.wait(index, 75, 0).await?;
        }
        Ok(())
    }
    async fn server(&mut self, name: &str) -> Result {
        let mut cmd = self.cmd(&self.tolight);
        cmd.arg("--config").arg(&self.config).arg("serve");
        let index = self.start(cmd, name, false)?;
        self.server = Some(index);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            c::ensure(
                self.children[index].child.try_wait()?.is_none(),
                "Coordinator exited before readiness",
            )?;
            if c::fetch(
                &self.http,
                format!("{}self", self.api),
                reqwest::Method::GET,
                None,
            )
            .await
            .is_ok_and(|r| r.status == 401)
            {
                return Ok(());
            }
            c::ensure(Instant::now() < deadline, "Coordinator readiness timeout")?;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    fn start_web(&mut self) -> Result {
        let mut cmd = self.cmd(&self.tolight);
        cmd.arg("serve-public")
            .arg("--dir")
            .arg(&self.public)
            .arg("--listen")
            .arg(format!("127.0.0.1:{}", self.web_port));
        self.start(cmd, "public-web", false)?;
        Ok(())
    }
    async fn actor(
        &mut self,
        name: &str,
        other: &str,
        unique: Option<&str>,
        model: bool,
        proactive: bool,
    ) -> Result {
        let directory = self.actors.join(name);
        fs::create_dir(&directory)?;
        let lines=unique.map(|s|format!("{s}\n")).unwrap_or_else(||format!("@{other} 我会从一个具体例子开始观察。\n@{other} 收到，继续交换这次本地测试的观察。\n"));
        private_text(&directory.join("lines.txt"), &lines)?;
        let extra = if model {
            private_text(
                &directory.join("mandate.md"),
                "项目方本地模型命令桩：只输出固定测试句，不接真实模型、不使用工具或私人记忆。\n",
            )?;
            format!(
                "[model]\ncommand={}\nmandate_path={}\nmax_output_tokens=128\ntimeout_seconds=10\n",
                serde_json::to_string(&vec![
                    std::env::current_exe()?.to_string_lossy().into_owned(),
                    "demo".into(),
                    "model-fixture".into()
                ])?,
                quote(directory.join("mandate.md"))
            )
        } else {
            String::new()
        };
        let title = name
            .split('-')
            .map(|part| {
                let mut chars = part.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("-");
        let path = directory.join("client.toml");
        private_text(
            &path,
            &format!(
                "base_url={}\nkey_path={}\nstate_path={}\nlines_path={}\nmode=\"{}\"\nmin_speaking_interval_seconds=0\nlong_poll_seconds=2\n{}[application]\nagent_id={}\ndisplay_name={}\nbio={}\nowner_contact={}\nframework_label=\"project local fixture\"\n{}",
                serde_json::to_string(&self.api)?,
                quote(directory.join("identity.key")),
                quote(directory.join("state.json")),
                quote(directory.join("lines.txt")),
                if model { "model" } else { "script" },
                if proactive {
                    "proactive_interval_seconds=3\n"
                } else {
                    ""
                },
                serde_json::to_string(name)?,
                serde_json::to_string(&format!("{title} Local Agent"))?,
                serde_json::to_string(&format!("项目方 M2 本地测试角色 {name}"))?,
                serde_json::to_string(&format!("{name}@example.invalid"))?,
                extra
            ),
        )?;
        self.configs.insert(name.into(), path);
        self.reference(name, osargs(["keygen"]), &format!("{name}-keygen"), 0)
            .await?;
        self.reference(name, osargs(["apply"]), &format!("{name}-apply"), 0)
            .await?;
        Ok(())
    }
    async fn sandbox(&mut self, actor: &str) -> Result {
        let index = self.actor_run(actor, "sandbox")?;
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            c::ensure(
                self.children[index].child.try_wait()?.is_none(),
                "sandbox client exited early",
            )?;
            let info = self.info(actor).await?;
            if !info["conformance"]["passed_at"].is_null() {
                c::ensure(
                    info["state"] == "PENDING"
                        && info["conformance"]["last_run"]["passed"] == true
                        && info["conformance"]["runs_today"] == 1,
                    "sandbox must pass its first real session",
                )?;
                self.stop(index, 75).await?;
                c::save(&self.run.join(format!("{actor}-conformance.json")), &info)?;
                return Ok(());
            }
            c::ensure(
                Instant::now() < deadline,
                "real three-check sandbox timeout",
            )?;
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
    }
    async fn application_id(&mut self, actor: &str) -> Result<String> {
        let listing = self
            .ctl(
                osargs(["operator", "agent", "list"]),
                &format!("{actor}-review"),
            )
            .await?;
        let line = listing
            .lines()
            .find(|line| line.starts_with(&format!("{actor} | ")))
            .ok_or("actor absent in operator list")?;
        let id = line
            .split("application_id=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .ok_or("missing application identity")?;
        uuid::Uuid::parse_str(id)?;
        Ok(id.into())
    }
    async fn approve(&mut self, actor: &str) -> Result {
        let id = self.application_id(actor).await?;
        let output = self
            .ctl(
                osargs(["operator", "agent", "approve", actor, "--yes"]),
                &format!("{actor}-approval"),
            )
            .await?;
        c::ensure(
            output.contains("已通过")
                && output.contains(&id)
                && self.info(actor).await?["state"] == "ACTIVE",
            "approval did not preserve reviewed identity",
        )?;
        self.approved.insert(actor.into(), id);
        c::save(
            &self.run.join("approved-test-fixtures.json"),
            &json!(self.approved),
        )
    }
    fn events(&self) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        for path in files(&self.public.join("data/chunks"))?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|s| s == "json"))
        {
            events.extend(
                json_file(path)?["events"]
                    .as_array()
                    .ok_or("chunk events")?
                    .clone(),
            );
        }
        Ok(events)
    }
    async fn publish(&mut self, name: &str) -> Result {
        self.ctl(osargs(["publish"]), name).await?;
        Ok(())
    }
    fn evidence(&mut self, name: &str, value: Value) -> Result {
        c::save(&self.run.join(format!("{name}.json")), &value)?;
        self.steps.push(name.into());
        println!("通过：{name}");
        Ok(())
    }
    fn record(&mut self, phase: &str, extra: Value, m2: bool) -> Result {
        self.metadata["phase"] = json!(phase);
        self.metadata["updated_at"] = json!(utc());
        if let Some(extra) = extra.as_object() {
            for (k, v) in extra {
                self.metadata[k] = v.clone();
            }
        }
        c::save(&self.run.join("run.json"), &self.metadata)?;
        c::save(
            &c::root().join(if m2 {
                ".local/m2-demo-latest.json"
            } else {
                ".local/m1-demo-latest.json"
            }),
            &self.metadata,
        )
    }
    async fn checkpoint(&mut self, phase: &str, extra: Value, enabled: bool) -> Result {
        self.record(phase, extra, true)?;
        println!(
            "{}",
            json!({"phase":phase,"public_url":self.web,"api_url":self.api,"directory":self.run})
        );
        if enabled {
            let marker = self.run.join(format!("continue-{phase}"));
            println!("页面检查点 {phase}：等待 {}（300秒）", marker.display());
            let deadline = Instant::now() + Duration::from_secs(300);
            while !marker.is_file() {
                c::ensure(Instant::now() < deadline, "page checkpoint timed out")?;
                if let Some(server) = self.server {
                    c::ensure(
                        self.children[server].child.try_wait()?.is_none(),
                        "Coordinator exited during checkpoint",
                    )?;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        Ok(())
    }
    async fn public_cursor(&self, actor: &str) -> Result {
        let directory = self.actors.join(actor);
        let seed = tolight_core::signing::b64_decode::<32>(
            fs::read_to_string(directory.join("identity.key"))?.trim(),
        )?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(state) = json_file(directory.join("state.json"))
                && let Some(cursor) = state["cursor"].as_str()
            {
                let mut url = url::Url::parse(&format!("{}events", self.api))?;
                url.query_pairs_mut()
                    .append_pair("after", cursor)
                    .append_pair("limit", "1")
                    .append_pair("wait", "0");
                let timestamp = time::OffsetDateTime::now_utc().unix_timestamp().to_string();
                let authority = format!("127.0.0.1:{}", self.api_port);
                let signed = tolight_core::signing::sign_request(
                    &seed,
                    "GET",
                    &authority,
                    "http",
                    url.path(),
                    url.query().unwrap_or(""),
                    &timestamp,
                    b"",
                )?;
                let response = self
                    .http
                    .get(url)
                    .header("Tolight-Key", signed.key)
                    .header("Tolight-Timestamp", signed.timestamp)
                    .header("Tolight-Signature", signed.signature)
                    .send()
                    .await?;
                if response.status().is_success() {
                    let page: Value = response.json().await?;
                    if page["timeline"] == "public" && page["cursor_reset"] == false {
                        return Ok(());
                    }
                }
            }
            c::ensure(
                Instant::now() < deadline,
                "client has not saved a current public cursor",
            )?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    async fn verify(&mut self, demo: bool, docs: bool, name: &str) -> Result {
        let mut cmd = self.cmd(std::env::current_exe()?);
        cmd.args(["runtime", "verify-public"])
            .arg(&self.web)
            .arg("--allow-http");
        if demo {
            cmd.arg("--expect-demo");
        }
        if docs {
            cmd.arg("--expect-docs");
        }
        self.command(cmd, name, 0, 120).await?;
        Ok(())
    }
    fn cleanup(&mut self) -> bool {
        let mut stopped = true;
        let mut order = (0..self.children.len()).collect::<Vec<_>>();
        order.sort_by_key(|&index| (!self.children[index].client, std::cmp::Reverse(index)));
        for index in order {
            let seconds = if self.children[index].client { 75 } else { 15 };
            if let Err(error) = stop(&mut self.children[index].child, seconds) {
                stopped = false;
                eprintln!(
                    "demo child {} cleanup failed: {error}",
                    self.children[index].name
                );
            }
        }
        stopped
    }
}
impl Drop for Demo {
    fn drop(&mut self) {
        let stopped = self.cleanup();
        self.metadata["processes_stopped"] = json!(stopped);
        self.metadata["updated_at"] = json!(utc());
        let _ = c::save(&self.run.join("run.json"), &self.metadata);
        let latest = c::root().join(if self.metadata["schema"] == "tolight-m2-local-demo/1" {
            ".local/m2-demo-latest.json"
        } else {
            ".local/m1-demo-latest.json"
        });
        if json_file(&latest).is_ok_and(|value| value["directory"] == self.metadata["directory"]) {
            let _ = c::save(&latest, &self.metadata);
        }
        eprintln!(
            "demo processes stopped; private evidence retained: {}",
            self.run.display()
        );
    }
}
fn osargs<const N: usize>(args: [&str; N]) -> Vec<OsString> {
    args.into_iter().map(OsString::from).collect()
}
pub async fn run(command: Command) -> Result {
    match command {
        Command::NotifySink { output } => {
            let value: Value = serde_json::from_reader(std::io::stdin().lock())?;
            let mut file = c::private_open(&output, true)?;
            writeln!(file, "{value}")?;
            file.sync_all()?;
            Ok(())
        }
        Command::ModelFixture => {
            let value: Value = serde_json::from_reader(std::io::stdin().lock())?;
            c::ensure(
                value["schema"] == "tolight-model-request/1",
                "invalid model fixture schema",
            )?;
            println!("@alice 项目方本地模型命令桩已通过无工具边界。");
            Ok(())
        }
        Command::M1(args) => m1::run(args).await,
        Command::M2(args) => m2::run(args).await,
    }
}
