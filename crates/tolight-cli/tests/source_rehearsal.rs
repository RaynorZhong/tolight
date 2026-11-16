#[path = "support/delivery.rs"]
mod support;
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::{fs, path::PathBuf};
use support::*;

struct Source {
    _temp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
}
impl Source {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        let work = temp.path().join("private-runs");
        fs::create_dir(&root).unwrap();
        for (name, text) in [
            (
                ".gitignore",
                ".local/\n__pycache__/\nnode_modules/\npublic/app.js\n",
            ),
            (
                "Cargo.toml",
                "[workspace]\nmembers=['crates/fixture']\nresolver='2'\n[workspace.package]\nversion='0.1.0'\n",
            ),
            (
                "Cargo.lock",
                "version=4\n\n[[package]]\nname='fixture'\nversion='0.1.0'\n",
            ),
            ("flake.nix", "{}\n"),
            ("deny.toml", "[licenses]\nallow=['MIT']\n"),
            ("LICENSE", "MIT\n"),
            ("README.md", "# source contract fixture\n"),
            ("CHANGELOG.md", "# Unreleased fixture\n"),
            ("SECURITY.md", "# No contact in fixture\n"),
            ("tolight-plan.md", "# local plan fixture\n"),
            ("config.example.toml", "listen='127.0.0.1:8090'\n"),
            ("docs/release-status.toml", "published=false\n"),
            ("docs/join.md", "# join fixture\n"),
            ("docs/participant-spec.md", "# spec fixture\n"),
            ("docs/protocol.md", "# protocol fixture\n"),
            ("docs/rules.md", "# rules fixture\n"),
            ("docs/signing-vectors.json", "{\"draft\":true}\n"),
            (
                "examples/python/minimal_client.py",
                "# independent interoperability example fixture\n",
            ),
            (
                "examples/python/check_vectors.py",
                "# independent example check fixture\n",
            ),
            ("scripts/fixture.sh", "#!/bin/sh\nexit 0\n"),
            (
                "crates/fixture/Cargo.toml",
                "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n[[bin]]\nname='tolight'\npath='src/tolight.rs'\n[[bin]]\nname='reference-client'\npath='src/reference.rs'\n",
            ),
            (
                "crates/fixture/src/tolight.rs",
                "fn main(){println!(\"tolight source fixture --help\");}\n",
            ),
            (
                "crates/fixture/src/reference.rs",
                "fn main(){println!(\"reference source fixture --help\");}\n",
            ),
            (".github/workflows/fixture.yml", "name: local fixture\n"),
            (
                "public/index.html",
                "<!doctype html><title>fixture</title>\n",
            ),
            ("public/style.css", "body { color: black; }\n"),
            ("public/robots.txt", "User-agent: *\nAllow: /\n"),
            ("tests/fixture.txt", "local fixture\n"),
            ("deploy/Caddyfile.example", "# public deployment template\n"),
            ("deploy/launchd.example.plist", "<plist/>\n"),
        ] {
            write(&root, name, text);
        }
        write(
            &root,
            "rust-toolchain.toml",
            fs::read(repo().join("rust-toolchain.toml")).unwrap(),
        );
        #[cfg(unix)]
        {
            fs::set_permissions(
                root.join("scripts/fixture.sh"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            symlink("../tolight-plan.md", root.join("docs/plan.md")).unwrap();
        }
        Self {
            _temp: temp,
            root,
            work,
        }
    }
    fn invoke(&self, prepare: bool) -> (std::process::Output, Value) {
        let mut args = vec!["--work-dir", self.work.to_str().unwrap()];
        if prepare {
            args.push("--prepare-only");
        }
        let output = cli("source-rehearsal", &self.root, &args);
        let summary = result(&output);
        let report = summary["report"]
            .as_str()
            .map(|path| serde_json::from_slice(&fs::read(path).unwrap()).unwrap())
            .unwrap_or(summary);
        (output, report)
    }
}

#[cfg(unix)]
#[test]
fn prepared_checkout_is_exact_private_and_preserves_links_exec_and_deployment_templates() {
    let f = Source::new();
    write(&f.root, ".local/secret.key", "private-value-must-not-print");
    write(
        &f.root,
        "scripts/__pycache__/fixture.pyc",
        "generated cache",
    );
    write(&f.root, ".env", "PRIVATE=private-value-must-not-print\n");
    write(&f.root, ".env.example", "EXAMPLE_ONLY=\n");
    write(&f.root, "public/app.js", "generated output, never source\n");
    let (output, report) = f.invoke(true);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["status"], "prepared");
    assert_eq!(report["scope"], "local_unpublished_source_rehearsal");
    assert_eq!(report["schema"], "tolight-source-rehearsal/1");
    assert_eq!(report["built"], false);
    assert_eq!(report["published"], false);
    assert_eq!(report["fixture_tag_is_formal_release"], false);
    assert!(!f.root.join(".git").exists());
    let checkout = PathBuf::from(report["checkout"].as_str().unwrap());
    let fixture = PathBuf::from(report["fixture"].as_str().unwrap());
    assert_eq!(git_ok(&checkout, &["rev-parse", "HEAD"]), report["commit"]);
    assert!(
        git_ok(
            &checkout,
            &["status", "--porcelain", "--untracked-files=all"]
        )
        .is_empty()
    );
    assert_eq!(
        git_ok(
            &fixture,
            &[
                "rev-parse",
                &format!("refs/tags/{}^{{}}", report["fixture_tag"].as_str().unwrap())
            ]
        ),
        report["commit"]
    );
    assert_eq!(
        git_ok(
            &fixture,
            &[
                "cat-file",
                "-t",
                &format!("refs/tags/{}", report["fixture_tag"].as_str().unwrap())
            ]
        ),
        "tag"
    );
    assert_ne!(
        fs::metadata(checkout.join("scripts/fixture.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100,
        0
    );
    assert_eq!(
        fs::read_link(checkout.join("docs/plan.md")).unwrap(),
        PathBuf::from("../tolight-plan.md")
    );
    assert_eq!(
        fs::read(checkout.join("docs/plan.md")).unwrap(),
        fs::read(f.root.join("tolight-plan.md")).unwrap()
    );
    for name in [
        ".env.example",
        "deploy/Caddyfile.example",
        "deploy/launchd.example.plist",
    ] {
        assert_eq!(
            fs::read(checkout.join(name)).unwrap(),
            fs::read(f.root.join(name)).unwrap()
        );
    }
    for name in [
        ".env",
        ".local",
        "scripts/__pycache__",
        "target",
        "public-dist",
        "public/app.js",
    ] {
        assert!(!checkout.join(name).exists(), "{name}");
    }
    for (path, mode) in [
        (PathBuf::from(report["work_dir"].as_str().unwrap()), 0o700),
        (PathBuf::from(report["report"].as_str().unwrap()), 0o600),
        (
            PathBuf::from(report["work_dir"].as_str().unwrap()).join("git.log"),
            0o600,
        ),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-value-must-not-print"));
    assert!(
        !PathBuf::from(report["work_dir"].as_str().unwrap())
            .join("build.log")
            .exists()
    );
}
#[test]
fn original_git_commit_tags_config_index_and_files_are_unchanged() {
    let f = Source::new();
    init(&f.root);
    commit(&f.root);
    git_ok(&f.root, &["tag", "original-marker"]);
    git_ok(&f.root, &["config", "user.name", "Must not change"]);
    let head = git_ok(&f.root, &["rev-parse", "HEAD"]);
    let before = hashes(&f.root);
    let (output, report) = f.invoke(true);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["source_head"], head);
    assert_eq!(git_ok(&f.root, &["rev-parse", "HEAD"]), head);
    assert_eq!(git_ok(&f.root, &["tag", "--list"]), "original-marker");
    assert!(git_ok(&f.root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
    assert_eq!(hashes(&f.root), before);
}
#[test]
fn ignored_private_tracked_files_are_rejected() {
    for name in [
        ".env",
        ".local/token.txt",
        "config.toml",
        "crates/private.key",
        "docs/old.dump",
    ] {
        let f = Source::new();
        init(&f.root);
        write(&f.root, name, "private-value-must-not-print");
        git_ok(&f.root, &["add", "--force", "--", name]);
        let (output, report) = f.invoke(true);
        assert!(!output.status.success());
        assert_eq!(report["error_code"], "private_tracked_paths");
        assert!(
            report["rejected_paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == name)
        );
        assert_eq!(report["built"], false);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("private-value-must-not-print"));
    }
}
#[test]
fn untracked_private_file_inside_public_source_is_not_packaged() {
    let f = Source::new();
    write(
        &f.root,
        "deploy/private.key",
        "private-value-must-not-print",
    );
    let (output, report) = f.invoke(true);
    assert!(!output.status.success());
    assert_eq!(report["error_code"], "private_source_file");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-value-must-not-print"));
}
#[test]
fn uninitialized_gitlink_is_rejected_before_snapshot() {
    let f = Source::new();
    init(&f.root);
    commit(&f.root);
    let head = git_ok(&f.root, &["rev-parse", "HEAD"]);
    git_ok(
        &f.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},crates/submodule"),
        ],
    );
    assert!(!f.root.join("crates/submodule").exists());
    let (output, report) = f.invoke(true);
    assert!(!output.status.success());
    assert_eq!(report["error_code"], "unsupported_source_gitlink");
    assert_eq!(
        report["rejected_paths"],
        serde_json::json!(["crates/submodule"])
    );
    assert_eq!(report["built"], false);
}
#[test]
fn runtime_public_data_is_not_a_source_snapshot() {
    let f = Source::new();
    write(&f.root, "public/data/manifest.json", "{\"newest_seq\":100}");
    let (output, report) = f.invoke(true);
    assert!(!output.status.success());
    assert_eq!(report["error_code"], "runtime_public_data_in_source");
    assert_eq!(report["built"], false);
}
#[cfg(unix)]
#[test]
fn escaping_absolute_and_private_target_symlinks_fail_without_echoing_data() {
    let f = Source::new();
    let outside = f._temp.path().join("outside.txt");
    fs::write(&outside, "external-private-value").unwrap();
    write(&f.root, ".local/inside-private.txt", "inside-private-value");
    for target in [
        PathBuf::from("../../outside.txt"),
        f.root.join("README.md"),
        PathBuf::from("../.local/inside-private.txt"),
    ] {
        let link = f.root.join("docs/bad-link");
        symlink(target, &link).unwrap();
        let (output, report) = f.invoke(true);
        assert!(!output.status.success());
        assert_eq!(report["error_code"], "unsafe_source_symlink");
        assert_eq!(report["built"], false);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("external-private-value"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("inside-private-value"));
        fs::remove_file(link).unwrap();
    }
}
#[test]
fn missing_lock_and_output_inside_public_source_are_refused() {
    let f = Source::new();
    fs::remove_file(f.root.join("Cargo.lock")).unwrap();
    let (output, report) = f.invoke(true);
    assert!(!output.status.success());
    assert_eq!(report["error_code"], "missing_required_source");
    assert!(
        report["rejected_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "Cargo.lock")
    );
    let public = f.root.join("public/output");
    let output = cli(
        "source-rehearsal",
        &f.root,
        &["--work-dir", public.to_str().unwrap(), "--prepare-only"],
    );
    assert!(!output.status.success());
    assert_eq!(result(&output)["status"], "failed");
    assert!(!public.exists());
}

#[test]
fn product_snapshot_requires_typescript_even_when_the_entire_frontend_is_missing() {
    let f = Source::new();
    write(
        &f.root,
        "crates/tolight-cli/Cargo.toml",
        "[package]\nname='tolight-cli'\nversion='0.1.0'\n",
    );
    write(
        &f.root,
        "public/app.js",
        "stale generated output cannot replace TypeScript source\n",
    );
    let (output, report) = f.invoke(true);
    assert!(!output.status.success());
    assert_eq!(report["error_code"], "missing_required_source");
    for name in WEB_INPUTS.iter().take(6) {
        assert!(
            report["rejected_paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path == name),
            "{name}: {report}"
        );
    }
    assert_eq!(report["built"], false);
    assert_eq!(report["published"], false);
}

#[test]
fn typescript_snapshot_also_requires_the_static_page_inputs() {
    for missing in ["public/index.html", "public/style.css", "public/robots.txt"] {
        let f = Source::new();
        for name in WEB_INPUTS {
            write(&f.root, name, fs::read(repo().join(name)).unwrap());
        }
        fs::remove_file(f.root.join(missing)).unwrap();
        let (output, report) = f.invoke(true);
        assert!(!output.status.success());
        assert_eq!(report["error_code"], "missing_required_source");
        assert!(
            report["rejected_paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path == missing),
            "{report}"
        );
    }
}

#[cfg(unix)]
#[test]
fn typescript_snapshot_keeps_complete_internal_directory_symlink_dependencies() {
    let f = Source::new();
    for name in WEB_INPUTS {
        write(&f.root, name, fs::read(repo().join(name)).unwrap());
    }
    fs::rename(
        f.root.join("frontend"),
        f.root.join("crates/frontend-source"),
    )
    .unwrap();
    symlink("crates/frontend-source", f.root.join("frontend")).unwrap();
    let (output, report) = f.invoke(false);
    assert!(output.status.success(), "{report}");
    let checkout = PathBuf::from(report["checkout"].as_str().unwrap());
    assert_eq!(
        fs::read_link(checkout.join("frontend")).unwrap(),
        PathBuf::from("crates/frontend-source")
    );
    assert_eq!(
        fs::read(checkout.join("frontend/app.ts")).unwrap(),
        fs::read(f.root.join("frontend/app.ts")).unwrap()
    );
    assert_eq!(report["frontend_built"], true);
    assert_eq!(report["checkout_clean_after_build"], true);
    assert!(checkout.join("public/app.js").is_file());
}
#[test]
fn ambient_ancestor_env_and_cargo_configuration_block_actual_build() {
    for name in [".env", ".cargo/config", ".cargo/config.toml"] {
        let f = Source::new();
        let path = f._temp.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "malformed-private-configuration-not-for-build").unwrap();
        let (output, report) = f.invoke(false);
        assert!(!output.status.success());
        assert_eq!(report["error_code"], "ambient_build_configuration");
        assert!(
            report["rejected_paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == path.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(report["built"], false);
        assert!(
            !PathBuf::from(report["work_dir"].as_str().unwrap())
                .join("build.log")
                .exists()
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("malformed-private-configuration")
        );
    }
}
#[test]
fn real_cargo_failure_never_becomes_successful_rehearsal() {
    let f = Source::new();
    write(
        &f.root,
        "crates/fixture/src/tolight.rs",
        "this is intentionally invalid Rust\n",
    );
    let (output, report) = f.invoke(false);
    assert!(!output.status.success());
    assert_eq!(report["status"], "failed");
    assert_eq!(report["error_code"], "cargo_build_failed");
    assert_eq!(
        report["build_command"],
        serde_json::json!(["cargo", "build", "--locked", "--workspace"])
    );
    assert_eq!(report["build_environment"]["SQLX_OFFLINE"], "true");
    assert_eq!(report["built"], false);
    assert_eq!(report["published"], false);
    assert!(report.get("binary_help_passed").is_none());
    assert!(
        PathBuf::from(report["work_dir"].as_str().unwrap())
            .join("build.log")
            .is_file()
    );
}
#[test]
fn exact_private_checkout_really_builds_and_runs_both_native_binaries() {
    let f = Source::new();
    let before = hashes(&f.root);
    let (output, report) = f.invoke(false);
    assert!(
        output.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["status"], "passed");
    assert_eq!(report["built"], true);
    assert_eq!(report["published"], false);
    assert_eq!(report["commit"], report["checkout_head"]);
    assert_eq!(report["checkout_clean_before_build"], true);
    assert_eq!(report["checkout_clean_after_build"], true);
    let names = report["binary_help_passed"].as_array().unwrap();
    assert!(names.iter().any(|v| v == "tolight"));
    assert!(names.iter().any(|v| v == "reference-client"));
    assert_eq!(hashes(&f.root), before);
}
#[test]
fn typescript_snapshot_cold_installs_locked_dev_dependencies_and_compiles_without_borrowed_node_modules()
 {
    let f = Source::new();
    for name in [
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
    ] {
        write(&f.root, name, fs::read(repo().join(name)).unwrap());
    }
    write(
        &f.root,
        "node_modules/DO_NOT_COPY",
        "never borrow original modules\n",
    );
    write(&f.root, "public/app.js", "stale original generated bytes\n");
    write(
        &f.root,
        ".local/build/operator-ui/admin.js",
        "stale private admin generated bytes\n",
    );
    let (output, report) = f.invoke(false);
    assert!(
        output.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["frontend_built"], true);
    let checkout = PathBuf::from(report["checkout"].as_str().unwrap());
    assert!(!checkout.join("node_modules/DO_NOT_COPY").exists());
    assert!(checkout.join("node_modules/typescript/bin/tsc").is_file());
    let javascript = fs::read_to_string(checkout.join("public/app.js")).unwrap();
    assert!(!javascript.contains("stale original generated bytes"));
    let admin = fs::read_to_string(checkout.join(".local/build/operator-ui/admin.js")).unwrap();
    assert!(!admin.contains("stale private admin generated bytes"));
    assert!(!checkout.join("public/admin.js").exists());
    assert_eq!(report["checkout_clean_after_build"], true);
    let manifest: Value = serde_json::from_slice(
        &fs::read(PathBuf::from(report["work_dir"].as_str().unwrap()).join("source-manifest.json"))
            .unwrap(),
    )
    .unwrap();
    assert!(!manifest.as_array().unwrap().iter().any(|v| {
        v["path"] == "public/app.js"
            || v["path"]
                .as_str()
                .is_some_and(|v| v.starts_with("node_modules/"))
    }));
}
