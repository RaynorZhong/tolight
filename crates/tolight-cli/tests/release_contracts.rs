#[path = "support/delivery.rs"]
mod support;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::{fs, path::PathBuf};
use support::*;

struct Release {
    _temp: tempfile::TempDir,
    root: PathBuf,
    version: String,
    spec: String,
    vectors: Value,
}
impl Release {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        fs::create_dir(&root).unwrap();
        for (name, text) in [
            ("Cargo.lock", "version=4\n"),
            ("rust-toolchain.toml", "[toolchain]\nchannel='1.99.1'\n"),
            ("flake.nix", "{}\n"),
            ("flake.lock", "{\"version\":7}\n"),
            ("LICENSE", "MIT fixture\n"),
            ("README.md", "Local fixture\n"),
            ("SECURITY.md", "# Local fixture; contact not configured.\n"),
            (".gitignore", ".env\n.env.*\n.local/\ntarget/\n"),
            ("config.example.toml", "listen='127.0.0.1:8090'\n"),
            (
                "crates/tolight-coordinator-store/src/lib.rs",
                "pub const SCHEMA_VERSION: i64 = 1;\n",
            ),
            (
                "crates/tolight-coordinator-store/migrations/001_initial.sql",
                "SELECT 1;\n",
            ),
            (
                "docs/fixture-evidence.md",
                "Synthetic contract evidence, not a real Gate.\n",
            ),
        ] {
            write(&root, name, text);
        }
        for name in WEB_INPUTS.iter().chain(
            [
                "examples/python/minimal_client.py",
                "examples/python/check_vectors.py",
            ]
            .iter(),
        ) {
            write(&root, name, fs::read(repo().join(name)).unwrap());
        }
        let vectors =
            serde_json::from_slice(&fs::read(repo().join("docs/signing-vectors.json")).unwrap())
                .unwrap();
        let mut value = Self {
            _temp: temp,
            root,
            version: "0.1.0".into(),
            spec: "tolight-participant/0.1".into(),
            vectors,
        };
        value.configure(false, false);
        init(&value.root);
        value
    }
    fn configure(&mut self, frozen: bool, published: bool) {
        write(
            &self.root,
            "Cargo.toml",
            format!("[workspace.package]\nversion={:?}\n", self.version),
        );
        write(
            &self.root,
            "crates/tolight-core/src/lib.rs",
            format!("pub const SPEC_VERSION: &str = {:?};\n", self.spec),
        );
        self.vectors["spec_version"] = json!(self.spec);
        self.vectors["draft"] = json!(!frozen);
        json(&self.root, "docs/signing-vectors.json", &self.vectors);
        let public = self.spec.split('/').nth(1).unwrap();
        for name in ["participant-spec", "protocol", "rules", "join"] {
            write(
                &self.root,
                &format!("docs/{name}.md"),
                format!("# {name} {public}{}\n", if frozen { "" } else { " 草案" }),
            );
        }
        write(
            &self.root,
            "CHANGELOG.md",
            format!(
                "# Changelog\n\n## [{}]\n\nCode-Version: {}\nParticipant-Spec: {}\nDatabase-Schema: 1\nPublication: {}\n",
                if published {
                    &self.version
                } else {
                    "Unreleased"
                },
                self.version,
                self.spec,
                if published { "public" } else { "local-only" }
            ),
        );
        self.gates(None);
    }
    fn gates(&self, overrides: Option<(&str, &str, Vec<&str>)>) {
        let gates = GATES
            .iter()
            .map(|id| {
                let (status, evidence) = overrides
                    .as_ref()
                    .filter(|(key, _, _)| key == id)
                    .map(|(_, s, e)| (*s, e.clone()))
                    .unwrap_or(("pending", vec![]));
                json!({"id":id,"status":status,"evidence":evidence})
            })
            .collect::<Vec<_>>();
        toml(
            &self.root,
            "docs/release-status.toml",
            &json!({"schema":"tolight-release-status/1","participant_spec":self.spec,"database_schema":1,"gates":gates}),
        );
    }
    fn pass_all(&self) {
        let mut state = load_toml(self.root.join("docs/release-status.toml"));
        for gate in state["gates"].as_array_mut().unwrap() {
            gate["status"] = json!("passed");
            gate["evidence"] = json!(["docs/fixture-evidence.md"]);
        }
        toml(&self.root, "docs/release-status.toml", &state);
    }
    fn check(&self) -> Value {
        result(&cli("release-check", &self.root, &[]))
    }
    fn production(&mut self, contact: &str) {
        self.version = "2.3.4".into();
        self.spec = "tolight-participant/1.0".into();
        self.configure(true, true);
        self.pass_all();
        let rules = self.root.join("docs/rules.md");
        fs::write(
            &rules,
            format!(
                "{}\nOperator-Contact: https://tolight.com/operator-contact\n",
                fs::read_to_string(&rules).unwrap()
            ),
        )
        .unwrap();
        write(
            &self.root,
            "SECURITY.md",
            format!("# Security contract fixture\n\nContact: {contact}\n"),
        );
        let expires =
            (Utc::now() + Duration::days(180)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write(
            &self.root,
            "public/.well-known/security.txt",
            format!(
                "Contact: {contact}\nExpires: {expires}\nCanonical: https://tolight.com/.well-known/security.txt\n"
            ),
        );
    }
    fn commit_tag(&self) {
        commit(&self.root);
        git_ok(&self.root, &["tag", &format!("v{}", self.version)]);
    }
    fn formal(&self, validator: &str) -> Value {
        result(&cli(
            "release-check",
            &self.root,
            &["--production", "--tolight", validator],
        ))
    }
}

#[test]
fn unborn_repository_is_read_only_and_never_authorizes_publication() {
    let f = Release::new();
    let before = hashes(&f.root);
    let report = f.check();
    assert!(errors(&report).is_empty());
    assert!(has(&report, "blockers", "git_no_commit"));
    assert!(report["commit"].is_null());
    assert_eq!(report["publication_authorized"], false);
    assert_eq!(hashes(&f.root), before);
    assert!(git(&f.root, &["show-ref"]).stdout.is_empty());
}
#[test]
fn semver_tag_and_spec_are_independent_and_strict() {
    let mut f = Release::new();
    f.version = "3.4.5-rc.1+fixture.2".into();
    f.configure(false, false);
    f.commit_tag();
    let report = f.check();
    assert!(errors(&report).is_empty());
    assert_eq!(report["code_version"], f.version);
    assert_eq!(report["participant_spec"], "tolight-participant/0.1");
    assert_eq!(report["commit"], git_ok(&f.root, &["rev-parse", "HEAD"]));
    assert!(!has(&report, "blockers", "tag_not_at_commit"));
    assert!(!has(&report, "blockers", "git_dirty"));
    for tag in ["3.4.5-rc.1+fixture.2", "v3.4.6", "v03.4.5", "v3.4.5-01"] {
        assert!(
            !errors(&result(&cli("release-check", &f.root, &["--tag", tag]))).is_empty(),
            "{tag}"
        );
    }
}
#[test]
fn malformed_semver_is_rejected() {
    for version in [
        "01.2.3", "1.02.3", "1.2.03", "1.2", "1.2.3-01", "1.2.3-", "1.2.3+",
    ] {
        let mut f = Release::new();
        f.version = version.into();
        f.configure(false, false);
        assert!(!errors(&f.check()).is_empty(), "{version}");
    }
}
#[test]
fn dirty_tracked_and_untracked_files_do_not_modify_git_index_or_refs() {
    let f = Release::new();
    f.commit_tag();
    let index = fs::read(f.root.join(".git/index")).unwrap();
    let head = git_ok(&f.root, &["rev-parse", "HEAD"]);
    let readme = f.root.join("README.md");
    let original = fs::read(&readme).unwrap();
    let metadata = fs::metadata(&readme).unwrap();
    fs::File::options()
        .write(true)
        .open(&readme)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_accessed(metadata.accessed().unwrap())
                .set_modified(metadata.modified().unwrap() + std::time::Duration::from_secs(9)),
        )
        .unwrap();
    assert!(!has(&f.check(), "blockers", "git_dirty"));
    assert_eq!(fs::read(&readme).unwrap(), original);
    assert_eq!(fs::read(f.root.join(".git/index")).unwrap(), index);
    write(&f.root, "README.md", "changed tracked\n");
    assert!(has(&f.check(), "blockers", "git_dirty"));
    write(&f.root, "README.md", "Local fixture\n");
    write(&f.root, "docs/untracked.txt", "new file\n");
    assert!(has(&f.check(), "blockers", "git_dirty"));
    assert_eq!(fs::read(f.root.join(".git/index")).unwrap(), index);
    assert_eq!(git_ok(&f.root, &["rev-parse", "HEAD"]), head);
    assert_eq!(git_ok(&f.root, &["rev-parse", "refs/tags/v0.1.0"]), head);
}
#[test]
fn tag_requires_exact_commit_and_accepts_annotated_tag() {
    let f = Release::new();
    f.commit_tag();
    write(&f.root, "README.md", "next commit\n");
    commit(&f.root);
    let report = f.check();
    assert!(has(&report, "blockers", "tag_not_at_commit"));
    assert_eq!(report["commit"], git_ok(&f.root, &["rev-parse", "HEAD"]));
    git_ok(&f.root, &["tag", "--delete", "v0.1.0"]);
    git_ok(
        &f.root,
        &[
            "tag",
            "--annotate",
            "v0.1.0",
            "--message",
            "fixture annotation",
        ],
    );
    assert!(!has(&f.check(), "blockers", "tag_not_at_commit"));
}
#[test]
fn cargo_lock_is_required_and_nix_lock_is_a_pending_gate() {
    let f = Release::new();
    fs::remove_file(f.root.join("Cargo.lock")).unwrap();
    assert!(has(&f.check(), "errors", "missing:Cargo.lock"));
    write(&f.root, "Cargo.lock", "version=4\n");
    fs::remove_file(f.root.join("flake.lock")).unwrap();
    let report = f.check();
    assert!(errors(&report).is_empty());
    assert!(has(&report, "blockers", "nix_lock_unverified"));
}

#[test]
fn typescript_build_and_static_page_sources_are_required_without_generated_javascript() {
    for name in WEB_INPUTS {
        let f = Release::new();
        fs::remove_file(f.root.join(name)).unwrap();
        assert!(
            has(&f.check(), "errors", &format!("missing:{name}")),
            "{name}"
        );
    }
    let f = Release::new();
    assert!(!f.root.join("public/app.js").exists());
    assert!(errors(&f.check()).is_empty());
}

#[test]
fn ignored_required_inputs_must_exist_in_the_checked_commit() {
    for name in [
        "Cargo.lock",
        "package-lock.json",
        "frontend/app.ts",
        "flake.lock",
    ] {
        let mut f = Release::new();
        f.production("https://tolight.com/security-report");
        write(
            &f.root,
            ".gitignore",
            format!(".local/\ntarget/\n/{name}\n"),
        );
        f.commit_tag();
        assert!(git_ok(&f.root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
        let before = hashes(&f.root);
        let output = cli(
            "release-check",
            &f.root,
            &["--production", "--tolight", TOLIGHT],
        );
        assert_eq!(output.status.code(), Some(1), "{name}");
        assert!(
            has(
                &result(&output),
                "errors",
                &format!("required_source_not_committed:{name}")
            ),
            "{name}"
        );
        assert_eq!(hashes(&f.root), before);
        // Staging a missing input still does not put it in the tagged commit.
        git_ok(&f.root, &["add", "--force", "--", name]);
        assert!(
            has(
                &f.check(),
                "errors",
                &format!("required_source_not_committed:{name}")
            ),
            "{name}"
        );
    }
}

#[test]
fn assume_unchanged_cannot_hide_modified_required_inputs_from_the_tag() {
    let mut f = Release::new();
    f.production("https://tolight.com/security-report");
    f.commit_tag();
    git_ok(
        &f.root,
        &[
            "update-index",
            "--assume-unchanged",
            "--",
            "frontend/app.ts",
        ],
    );
    write(
        &f.root,
        "frontend/app.ts",
        "export const changedAfterTag = true;\n",
    );
    assert!(git_ok(&f.root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
    let before = hashes(&f.root);
    let report = f.formal(TOLIGHT);
    assert_eq!(report["ready_for_public_release"], false);
    assert!(has(
        &report,
        "blockers",
        "required_source_differs_from_commit:frontend/app.ts"
    ));
    assert_eq!(hashes(&f.root), before);
}

#[test]
fn hidden_rust_manifests_and_quality_scripts_are_compared_with_the_tag() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        for name in [
            "crates/tolight-coordinator/src/room.rs",
            "crates/tolight-coordinator/Cargo.toml",
            "scripts/quality-gate.sh",
            ".github/workflows/ci.yml",
        ] {
            let mut f = Release::new();
            f.production("https://tolight.com/security-report");
            write(&f.root, name, "original public build input\n");
            f.commit_tag();
            assert_eq!(f.formal(TOLIGHT)["ready_for_public_release"], true);
            git_ok(&f.root, &["update-index", flag, "--", name]);
            write(&f.root, name, "changed after the checked tag\n");
            assert!(
                git_ok(&f.root, &["status", "--porcelain", "--untracked-files=all"]).is_empty()
            );
            let before = hashes(&f.root);
            let output = cli(
                "release-check",
                &f.root,
                &["--production", "--tolight", TOLIGHT],
            );
            assert_eq!(output.status.code(), Some(2), "{flag}: {name}");
            assert!(has(
                &result(&output),
                "blockers",
                &format!("required_source_differs_from_commit:{name}")
            ));
            assert_eq!(hashes(&f.root), before);
        }
    }
}

#[test]
fn hidden_deletions_and_type_changes_cannot_remove_committed_source_inputs() {
    for directory in [false, true] {
        let mut f = Release::new();
        f.production("https://tolight.com/security-report");
        let name = "crates/chat/src/room.rs";
        write(&f.root, name, "// committed source module\n");
        f.commit_tag();
        assert_eq!(f.formal(TOLIGHT)["ready_for_public_release"], true);
        git_ok(&f.root, &["update-index", "--skip-worktree", "--", name]);
        if directory {
            fs::remove_dir_all(f.root.join("crates/chat")).unwrap();
        } else {
            fs::remove_file(f.root.join(name)).unwrap();
        }
        assert!(git_ok(&f.root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
        let before = hashes(&f.root);
        let output = cli(
            "release-check",
            &f.root,
            &["--production", "--tolight", TOLIGHT],
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(has(
            &result(&output),
            "blockers",
            &format!("required_source_differs_from_commit:{name}")
        ));
        assert_eq!(hashes(&f.root), before);
    }

    let mut f = Release::new();
    f.production("https://tolight.com/security-report");
    let name = "crates/chat/src/room.rs";
    write(&f.root, name, "// committed source module\n");
    f.commit_tag();
    git_ok(&f.root, &["update-index", "--skip-worktree", "--", name]);
    fs::remove_file(f.root.join(name)).unwrap();
    fs::create_dir(f.root.join(name)).unwrap();
    let report = f.formal(TOLIGHT);
    assert!(has(
        &report,
        "blockers",
        &format!("required_source_differs_from_commit:{name}")
    ));
    assert_eq!(report["ready_for_public_release"], false);
}

#[test]
fn ignored_actual_rust_and_quality_inputs_cannot_be_omitted_from_the_commit() {
    for name in [
        "crates/tolight-coordinator/src/room.rs",
        "crates/tolight-coordinator/Cargo.toml",
        "scripts/quality-gate.sh",
    ] {
        let mut f = Release::new();
        f.production("https://tolight.com/security-report");
        write(&f.root, name, "existing ignored public build input\n");
        write(
            &f.root,
            ".gitignore",
            format!(".local/\ntarget/\n/{name}\n"),
        );
        f.commit_tag();
        assert!(git_ok(&f.root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
        let before = hashes(&f.root);
        let output = cli(
            "release-check",
            &f.root,
            &["--production", "--tolight", TOLIGHT],
        );
        assert_eq!(output.status.code(), Some(1), "{name}");
        assert!(has(
            &result(&output),
            "errors",
            &format!("required_source_not_committed:{name}")
        ));
        assert_eq!(hashes(&f.root), before);
    }
}

#[test]
fn ignored_generated_javascript_and_local_private_trees_stay_outside_source_inputs() {
    let mut f = Release::new();
    f.production("https://tolight.com/security-report");
    write(
        &f.root,
        ".gitignore",
        ".local/\ntarget/\n/public/app.js\nnode_modules/\n",
    );
    f.commit_tag();
    for name in [
        "public/app.js",
        ".local/private.pem",
        "target/generated.rs",
        "frontend/node_modules/private-config.json",
    ] {
        write(
            &f.root,
            name,
            "private/generated fixture must not be inspected\n",
        );
    }
    assert!(git_ok(&f.root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
    let before = hashes(&f.root);
    let report = f.formal(TOLIGHT);
    assert_eq!(report["ready_for_public_release"], true, "{report}");
    assert!(!report.to_string().contains("private/generated fixture"));
    assert_eq!(hashes(&f.root), before);
}

#[cfg(unix)]
#[test]
fn hidden_rust_input_directory_links_and_script_execute_modes_are_checked() {
    for directory in [false, true] {
        let mut f = Release::new();
        f.production("https://tolight.com/security-report");
        let (link, first, second) = if directory {
            write(&f.root, "crates/chat/versions/a/room.rs", "// version a\n");
            write(&f.root, "crates/chat/versions/b/room.rs", "// version b\n");
            fs::create_dir_all(f.root.join("crates/chat/src")).unwrap();
            symlink("current/room.rs", f.root.join("crates/chat/src/room.rs")).unwrap();
            ("crates/chat/src/current", "../versions/a", "../versions/b")
        } else {
            write(&f.root, "crates/chat/src/a.rs", "// version a\n");
            write(&f.root, "crates/chat/src/b.rs", "// version b\n");
            symlink("current.rs", f.root.join("crates/chat/src/room.rs")).unwrap();
            ("crates/chat/src/current.rs", "a.rs", "b.rs")
        };
        symlink(first, f.root.join(link)).unwrap();
        f.commit_tag();
        assert_eq!(f.formal(TOLIGHT)["ready_for_public_release"], true);
        git_ok(&f.root, &["update-index", "--skip-worktree", "--", link]);
        fs::remove_file(f.root.join(link)).unwrap();
        symlink(second, f.root.join(link)).unwrap();
        assert!(git_ok(&f.root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
        let before = hashes(&f.root);
        let report = f.formal(TOLIGHT);
        assert!(has(
            &report,
            "blockers",
            &format!("required_source_differs_from_commit:{link}")
        ));
        assert_eq!(hashes(&f.root), before);
    }
    let mut f = Release::new();
    f.production("https://tolight.com/security-report");
    let name = "scripts/quality-gate.sh";
    write(&f.root, name, "#!/bin/sh\nexit 0\n");
    fs::set_permissions(f.root.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    f.commit_tag();
    assert_eq!(f.formal(TOLIGHT)["ready_for_public_release"], true);
    git_ok(&f.root, &["update-index", "--assume-unchanged", "--", name]);
    fs::set_permissions(f.root.join(name), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(git_ok(&f.root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
    let report = f.formal(TOLIGHT);
    assert!(has(
        &report,
        "blockers",
        &format!("required_source_differs_from_commit:{name}")
    ));
}

#[cfg(unix)]
#[test]
fn required_internal_symlink_dependencies_must_also_be_committed() {
    let mut f = Release::new();
    f.production("https://tolight.com/security-report");
    fs::rename(
        f.root.join("frontend/app.ts"),
        f.root.join("frontend/source.ts"),
    )
    .unwrap();
    symlink("source.ts", f.root.join("frontend/app.ts")).unwrap();
    f.commit_tag();
    assert_eq!(f.formal(TOLIGHT)["ready_for_public_release"], true);
    git_ok(&f.root, &["rm", "--cached", "--", "frontend/source.ts"]);
    let ignored =
        fs::read_to_string(f.root.join(".gitignore")).unwrap() + "\n/frontend/source.ts\n";
    write(&f.root, ".gitignore", ignored);
    commit(&f.root);
    let report = f.formal(TOLIGHT);
    assert!(has(
        &report,
        "errors",
        "required_source_not_committed:frontend/source.ts"
    ));
    assert_eq!(report["ready_for_public_release"], false);
}

#[cfg(unix)]
#[test]
fn intermediate_file_and_directory_symlinks_cannot_hide_changes_from_the_commit() {
    for directory in [false, true] {
        let mut f = Release::new();
        f.production("https://tolight.com/security-report");
        fs::remove_file(f.root.join("frontend/app.ts")).unwrap();
        let (link, first, second) = if directory {
            write(
                &f.root,
                "frontend/versions/a/app.ts",
                "export const version = 'a';\n",
            );
            write(
                &f.root,
                "frontend/versions/b/app.ts",
                "export const version = 'b';\n",
            );
            symlink("current/app.ts", f.root.join("frontend/app.ts")).unwrap();
            ("frontend/current", "versions/a", "versions/b")
        } else {
            write(&f.root, "frontend/a.ts", "export const version = 'a';\n");
            write(&f.root, "frontend/b.ts", "export const version = 'b';\n");
            symlink("source.ts", f.root.join("frontend/app.ts")).unwrap();
            ("frontend/source.ts", "a.ts", "b.ts")
        };
        symlink(first, f.root.join(link)).unwrap();
        f.commit_tag();
        assert_eq!(f.formal(TOLIGHT)["ready_for_public_release"], true);
        git_ok(&f.root, &["update-index", "--skip-worktree", "--", link]);
        fs::remove_file(f.root.join(link)).unwrap();
        symlink(second, f.root.join(link)).unwrap();
        assert!(git_ok(&f.root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
        let before = hashes(&f.root);
        let report = f.formal(TOLIGHT);
        assert!(
            has(
                &report,
                "blockers",
                &format!("required_source_differs_from_commit:{link}")
            ),
            "{report}"
        );
        assert_eq!(report["ready_for_public_release"], false);
        assert_eq!(hashes(&f.root), before);
    }
}

#[cfg(unix)]
#[test]
fn executable_mode_matches_git_owner_execute_semantics_even_with_hidden_changes() {
    for (committed, current, ready) in [(0o644, 0o641, true), (0o755, 0o654, false)] {
        let mut f = Release::new();
        f.production("https://tolight.com/security-report");
        let file = f.root.join("public/style.css");
        fs::set_permissions(&file, fs::Permissions::from_mode(committed)).unwrap();
        f.commit_tag();
        git_ok(
            &f.root,
            &[
                "update-index",
                "--assume-unchanged",
                "--",
                "public/style.css",
            ],
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(current)).unwrap();
        assert!(git_ok(&f.root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
        let report = f.formal(TOLIGHT);
        assert_eq!(report["ready_for_public_release"], ready, "{report}");
        assert_eq!(
            has(
                &report,
                "blockers",
                "required_source_differs_from_commit:public/style.css"
            ),
            !ready
        );
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            current
        );
    }
}
#[test]
fn schema_constant_migrations_and_status_must_match() {
    let f = Release::new();
    write(
        &f.root,
        "crates/tolight-coordinator-store/migrations/002_more.sql",
        "SELECT 2;\n",
    );
    assert!(!errors(&f.check()).is_empty());
    fs::remove_file(
        f.root
            .join("crates/tolight-coordinator-store/migrations/002_more.sql"),
    )
    .unwrap();
    let mut state = load_toml(f.root.join("docs/release-status.toml"));
    state["database_schema"] = json!(2);
    toml(&f.root, "docs/release-status.toml", &state);
    assert!(!errors(&f.check()).is_empty());
}
#[test]
fn vector_versions_boolean_draft_and_document_versions_are_checked() {
    for (key, value) in [
        ("spec_version", json!("tolight-participant/9.9")),
        ("draft", json!("false")),
    ] {
        let f = Release::new();
        let mut vectors = f.vectors.clone();
        vectors[key] = value;
        json(&f.root, "docs/signing-vectors.json", &vectors);
        assert!(!errors(&f.check()).is_empty());
    }
    let f = Release::new();
    write(&f.root, "docs/protocol.md", "# Protocol 2.0\n");
    assert!(!errors(&f.check()).is_empty());
}
#[test]
fn duplicate_changelog_versions_fields_and_wrong_mapping_are_invalid() {
    let mut f = Release::new();
    f.configure(false, true);
    let path = f.root.join("CHANGELOG.md");
    let original = fs::read_to_string(&path).unwrap();
    for value in [
        format!("{original}\n## [0.1.0]\nother\n"),
        format!("{original}Code-Version: 0.1.0\n"),
        original.replace("Database-Schema: 1", "Database-Schema: 2"),
        original.replace(
            "Participant-Spec: tolight-participant/0.1",
            "Participant-Spec: tolight-participant/8.8",
        ),
        original.replace("Publication: public\n", ""),
    ] {
        fs::write(&path, value).unwrap();
        assert!(!errors(&f.check()).is_empty());
    }
}
#[test]
fn every_gate_needs_passed_status_and_existing_evidence() {
    let mut f = Release::new();
    f.spec = "tolight-participant/1.0".into();
    f.configure(true, true);
    for id in GATES {
        for evidence in [vec![], vec!["docs/fixture-evidence.md"]] {
            f.pass_all();
            let mut state = load_toml(f.root.join("docs/release-status.toml"));
            let gate = state["gates"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|g| g["id"] == *id)
                .unwrap();
            gate["status"] = json!(if evidence.is_empty() {
                "passed"
            } else {
                "pending"
            });
            gate["evidence"] = json!(evidence);
            toml(&f.root, "docs/release-status.toml", &state);
            let report = f.formal(TOLIGHT);
            assert!(has(&report, "blockers", &format!("gate:{id}")));
            assert_eq!(report["ready_for_public_release"], false);
        }
    }
    for evidence in [
        "docs/missing.md",
        ".local/private.json",
        "https://user:password@tolight.com/report",
    ] {
        f.gates(Some(("operator_channel", "passed", vec![evidence])));
        assert!(!errors(&f.check()).is_empty());
    }
}
#[test]
fn malformed_gate_shapes_are_reported_not_panicked() {
    for mutation in ["status", "evidence", "duplicate", "schema"] {
        let f = Release::new();
        let mut state = load_toml(f.root.join("docs/release-status.toml"));
        match mutation {
            "status" => state["gates"][0]["status"] = json!("approved"),
            "evidence" => state["gates"][0]["evidence"] = json!("not-an-array"),
            "duplicate" => state["gates"][1]["id"] = state["gates"][0]["id"].clone(),
            _ => state["database_schema"] = json!(true),
        }
        toml(&f.root, "docs/release-status.toml", &state);
        assert!(!errors(&f.check()).is_empty());
    }
    let f = Release::new();
    write(
        &f.root,
        "docs/release-status.toml",
        "schema='tolight-release-status/1'\nparticipant_spec='tolight-participant/0.1'\ndatabase_schema=1\ngates=['a','b','c','d','e','f','g']\n",
    );
    assert!(!errors(&f.check()).is_empty());
}
#[test]
fn ignored_but_tracked_private_paths_are_rejected() {
    for name in [
        ".env",
        ".local/cache.json",
        "docs/.env.backup",
        "examples/identity.key",
        "public/data/manifest.json",
        "docs/identity.PEM",
        "examples/identity.P12",
        "docs/backup.DB",
        ".aws/local.json",
        "public/app.js",
    ] {
        let f = Release::new();
        f.commit_tag();
        write(&f.root, name, "test-only private marker\n");
        git_ok(&f.root, &["add", "--force", "--", name]);
        git_ok(&f.root, &["commit", "--quiet", "-m", "private fixture"]);
        assert!(
            has(
                &f.check(),
                "errors",
                &format!("private_or_escaping_tracked_path:{name}")
            ),
            "{name}"
        );
    }
}
#[test]
fn public_signing_vectors_and_example_config_are_allowed() {
    let f = Release::new();
    write(&f.root, ".env.example", "PUBLIC_FIXTURE=1\n");
    git_ok(&f.root, &["add", "--force", "--", ".env.example"]);
    f.commit_tag();
    assert!(errors(&f.check()).is_empty());
    assert!(
        fs::read_to_string(f.root.join("docs/signing-vectors.json"))
            .unwrap()
            .contains("\"seed\"")
    );
    write(&f.root, "config.toml", "private runtime configuration\n");
    git_ok(&f.root, &["add", "--force", "--", "config.toml"]);
    assert!(has(
        &f.check(),
        "errors",
        "private_or_escaping_tracked_path:config.toml"
    ));
}
#[test]
fn deployment_and_typescript_sources_are_included_but_runtime_keys_are_not() {
    let f = Release::new();
    for name in [
        "deploy/Caddyfile.example",
        "deploy/launchd.example.plist",
        "frontend/app.ts",
        "package.json",
        "package-lock.json",
        "tsconfig.json",
        "tsconfig.web.json",
    ] {
        write(&f.root, name, "public source fixture\n");
    }
    f.commit_tag();
    assert!(errors(&f.check()).is_empty());
    write(&f.root, "deploy/server.key", "private fixture\n");
    git_ok(&f.root, &["add", "--force", "deploy/server.key"]);
    commit(&f.root);
    assert!(has(
        &f.check(),
        "errors",
        "private_or_escaping_tracked_path:deploy/server.key"
    ));
    write(&f.root, "deploy/.env", "PRIVATE_FIXTURE=1\n");
    git_ok(&f.root, &["add", "--force", "--", "deploy/.env"]);
    assert!(has(
        &f.check(),
        "errors",
        "private_or_escaping_tracked_path:deploy/.env"
    ));
}
#[cfg(unix)]
#[test]
fn evidence_and_tracked_symlinks_cannot_escape_or_point_into_private_data() {
    let f = Release::new();
    let outside = f._temp.path().join("outside.md");
    fs::write(&outside, "outside\n").unwrap();
    symlink(&outside, f.root.join("docs/escaped.md")).unwrap();
    f.gates(Some((
        "operator_channel",
        "passed",
        vec!["docs/escaped.md"],
    )));
    assert!(!errors(&f.check()).is_empty());
    f.gates(None);
    f.commit_tag();
    assert!(has(
        &f.check(),
        "errors",
        "private_or_escaping_tracked_path:docs/escaped.md"
    ));
}
#[cfg(unix)]
#[test]
fn absolute_and_private_target_links_are_rejected_but_relative_public_link_is_allowed() {
    let f = Release::new();
    write(&f.root, ".local/private.json", "private\n");
    symlink(f.root.join("README.md"), f.root.join("docs/absolute.md")).unwrap();
    symlink(
        "../.local/private.json",
        f.root.join("docs/private-link.md"),
    )
    .unwrap();
    symlink("../README.md", f.root.join("docs/relative.md")).unwrap();
    f.commit_tag();
    let report = f.check();
    for name in ["docs/absolute.md", "docs/private-link.md"] {
        assert!(has(
            &report,
            "errors",
            &format!("private_or_escaping_tracked_path:{name}")
        ));
    }
    assert!(!has(
        &report,
        "errors",
        "private_or_escaping_tracked_path:docs/relative.md"
    ));
}
#[test]
fn tracked_gitlinks_are_rejected_without_initialization() {
    let f = Release::new();
    f.commit_tag();
    let head = git_ok(&f.root, &["rev-parse", "HEAD"]);
    git_ok(
        &f.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},crates/vendor"),
        ],
    );
    git_ok(&f.root, &["commit", "--quiet", "-m", "gitlink"]);
    assert!(has(
        &f.check(),
        "errors",
        "submodules_not_in_source_contract"
    ));
    assert!(!f.root.join(".git/modules").exists());
    assert!(git_ok(&f.root, &["ls-files", "--stage", "crates/vendor"]).starts_with("160000 "));
}
#[test]
fn cli_local_final_exit_codes_never_claim_permission() {
    let f = Release::new();
    let local = cli("release-check", &f.root, &[]);
    assert!(local.status.success());
    assert_eq!(result(&local)["status"], "blocked");
    assert_eq!(result(&local)["publication_authorized"], false);
    let formal = cli("release-check", &f.root, &["--production"]);
    assert_eq!(formal.status.code(), Some(2));
    assert_eq!(result(&formal)["scope"], "production_preflight");
    fs::remove_file(f.root.join("Cargo.lock")).unwrap();
    assert_eq!(cli("release-check", &f.root, &[]).status.code(), Some(1));
}
#[cfg(unix)]
#[test]
fn report_and_notes_are_private_and_exclusive() {
    let f = Release::new();
    let report = f._temp.path().join("report.json");
    let notes = f._temp.path().join("notes.md");
    let output = cli(
        "release-check",
        &f.root,
        &[
            "--report",
            report.to_str().unwrap(),
            "--notes",
            notes.to_str().unwrap(),
        ],
    );
    assert!(output.status.success());
    assert_eq!(
        fs::metadata(&report).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&notes).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        fs::read_to_string(&notes)
            .unwrap()
            .contains("Code-Version: 0.1.0")
    );
    let saved = fs::read(&report).unwrap();
    assert!(
        !cli(
            "release-check",
            &f.root,
            &["--report", report.to_str().unwrap()]
        )
        .status
        .success()
    );
    assert_eq!(fs::read(report).unwrap(), saved);
}
#[test]
fn production_positive_uses_actual_validator_and_does_not_authorize_publication() {
    let mut f = Release::new();
    f.production("https://tolight.com/security-report");
    f.commit_tag();
    let head = git_ok(&f.root, &["rev-parse", "HEAD"]);
    let report = f.formal(TOLIGHT);
    assert!(errors(&report).is_empty(), "{report}");
    assert_eq!(report["blockers"], json!([]));
    assert_eq!(report["status"], "ready");
    assert_eq!(report["ready_for_public_release"], true, "{report}");
    assert_eq!(report["publication_authorized"], false);
    assert_eq!(
        report["security"]["contacts"],
        json!(["https://tolight.com/security-report"])
    );
    assert_eq!(git_ok(&f.root, &["rev-parse", "HEAD"]), head);
    assert!(git_ok(&f.root, &["status", "--porcelain"]).is_empty());
    let output = cli(
        "release-check",
        &f.root,
        &["--production", "--tolight", TOLIGHT],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(result(&output)["status"], "ready");
}
#[test]
fn draft_specs_and_security_policy_block_even_with_gate_claims() {
    let mut f = Release::new();
    f.production("https://tolight.com/security-report");
    write(
        &f.root,
        "docs/participant-spec.md",
        "# Participant spec 1.0 草案\n",
    );
    write(
        &f.root,
        "SECURITY.md",
        "# Security 草案\n\nContact: https://tolight.com/security-report\n",
    );
    f.commit_tag();
    let report = f.formal(TOLIGHT);
    assert!(errors(&report).is_empty(), "{report}");
    assert!(has(&report, "blockers", "security_policy_draft"));
    assert!(has(&report, "blockers", "spec_not_frozen"));
    assert_eq!(report["ready_for_public_release"], false);
    f.configure(false, true);
    f.pass_all();
    assert!(has(&f.check(), "blockers", "spec_not_frozen"));
}
#[test]
fn operator_contacts_reject_malformed_private_and_placeholder_uris() {
    let mut f = Release::new();
    f.production("https://tolight.com/security-report");
    f.commit_tag();
    for contact in [
        "https://",
        "mailto:",
        "tel:",
        "https://[::1]/report",
        "https://127.0.0.1/report",
        "https://contact.invalid/report",
        "https://user:pass@tolight.com/report",
        "https://tolight.com/report?token=private",
        "https://tolight.com/report#fragment",
    ] {
        write(
            &f.root,
            "docs/rules.md",
            format!("# Rules 1.0\nOperator-Contact: {contact}\n"),
        );
        let report = f.formal(TOLIGHT);
        assert!(errors(&report).is_empty(), "{report}");
        assert!(
            has(&report, "blockers", "operator_contact_unpublished"),
            "{contact}"
        );
    }
}
#[test]
fn security_placeholders_expiry_missing_validator_and_canonical_are_distinguished() {
    let mut f = Release::new();
    f.production("https://fixture.invalid/report");
    f.commit_tag();
    let report = f.formal(TOLIGHT);
    assert!(errors(&report).is_empty(), "{report}");
    assert_eq!(
        report["security"]["contacts"],
        json!(["https://fixture.invalid/report"])
    );
    assert!(has(&report, "blockers", "security_contact_placeholder"));
    let path = f.root.join("public/.well-known/security.txt");
    let current = fs::read_to_string(&path).unwrap();
    let expired = current
        .lines()
        .map(|line| {
            if line.starts_with("Expires:") {
                "Expires: 2000-01-01T00:00:00Z"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, expired).unwrap();
    assert!(has(&f.formal(TOLIGHT), "errors", "security_assets_invalid"));
    assert!(has(
        &f.formal(f.root.join("missing-validator").to_str().unwrap()),
        "blockers",
        "security_validator_unavailable"
    ));
    f.production("https://tolight.com/security-report");
    write(
        &f.root,
        "public/.well-known/security.txt",
        fs::read_to_string(&path).unwrap().replace(
            "https://tolight.com/.well-known/security.txt",
            "https://fixture.invalid/.well-known/security.txt",
        ),
    );
    let report = f.formal(TOLIGHT);
    assert!(errors(&report).is_empty(), "{report}");
    assert!(has(&report, "blockers", "security_canonical_placeholder"));
}
