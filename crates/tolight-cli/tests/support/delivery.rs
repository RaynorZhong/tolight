#![allow(dead_code)]
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

pub const OPS: &str = env!("CARGO_BIN_EXE_tolight-ops");
pub const TOLIGHT: &str = env!("CARGO_BIN_EXE_tolight");
pub const WEB_INPUTS: &[&str] = &[
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
pub const GATES: &[&str] = &[
    "operator_channel",
    "security_channel",
    "content_rules",
    "third_party_m2",
    "independent_m3",
    "production_origins",
    "draft_owner_upgrade",
];
pub const EXTRA: &[&str] = &[
    "public_release_ci",
    "recovery_targets",
    "external_probe_delivery",
    "real_model_24h",
];
pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
pub fn write(root: &Path, name: &str, content: impl AsRef<[u8]>) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}
pub fn json(root: &Path, name: &str, value: &Value) {
    write(root, name, serde_json::to_vec_pretty(value).unwrap());
}
pub fn toml(root: &Path, name: &str, value: &Value) {
    write(root, name, toml::to_string(value).unwrap());
}
pub fn load_toml(path: impl AsRef<Path>) -> Value {
    let value: toml::Value = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    serde_json::to_value(value).unwrap()
}
pub fn cli(command: &str, root: &Path, arguments: &[&str]) -> Output {
    Command::new(OPS)
        .args(["delivery", command, "--source"])
        .arg(root)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}
pub fn result(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid report: {error}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}
pub fn git(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new("git");
    command
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-C",
        ])
        .arg(root)
        .args(args);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Local contract fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Local contract fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}
pub fn git_ok(root: &Path, args: &[&str]) -> String {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
pub fn init(root: &Path) {
    git_ok(root, &["init", "--quiet", "--initial-branch=main"]);
}
pub fn commit(root: &Path) {
    git_ok(root, &["add", "--all"]);
    git_ok(root, &["commit", "--quiet", "-m", "local fixture"]);
}
pub fn hashes(root: &Path) -> BTreeMap<PathBuf, String> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, String>) {
        let mut paths = fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        paths.sort();
        for p in paths {
            if p.is_symlink() {
                out.insert(
                    p.strip_prefix(root).unwrap().into(),
                    format!("link:{}", fs::read_link(&p).unwrap().display()),
                );
            } else if p.is_dir() {
                walk(root, &p, out)
            } else {
                out.insert(
                    p.strip_prefix(root).unwrap().into(),
                    format!("{:x}", Sha256::digest(fs::read(p).unwrap())),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}
pub fn errors(report: &Value) -> Vec<&str> {
    report["errors"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect()
}
pub fn has(report: &Value, field: &str, value: &str) -> bool {
    report[field].as_array().unwrap().iter().any(|v| v == value)
}
