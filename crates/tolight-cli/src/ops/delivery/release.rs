use super::{ReleaseArgs, Result, common::*};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub fn changelog_section(text: &str, version: &str) -> Result<(String, bool)> {
    let mut sections = BTreeMap::<String, String>::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## [")
            && let Some((name, _)) = rest.split_once(']')
        {
            ensure(!sections.contains_key(name), "duplicate CHANGELOG version")?;
            sections.insert(name.to_owned(), String::new());
            current = Some(name.to_owned());
        } else if let Some(name) = &current {
            sections.get_mut(name).unwrap().push_str(line);
            sections.get_mut(name).unwrap().push('\n');
        }
    }
    let exact = sections.contains_key(version);
    let section = sections
        .get(version)
        .or_else(|| sections.get("Unreleased"))
        .ok_or("CHANGELOG has no matching version or Unreleased section")?;
    Ok((format!("{}\n", section.trim()), exact))
}

fn field<'a>(section: &'a str, key: &str) -> Result<&'a str> {
    let prefix = format!("{key}:");
    let fields = section
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    ensure(
        fields.len() == 1,
        format!("CHANGELOG requires exactly one {key}"),
    )?;
    Ok(fields[0])
}

fn push(report: &mut Value, key: &str, text: impl Into<String>) {
    report[key].as_array_mut().unwrap().push(json!(text.into()));
}

fn check_commit_inputs(
    root: &Path,
    commit: &str,
    required: &[&str],
    report: &mut Value,
) -> Result<()> {
    let tree = git(root, &["ls-tree", "--full-tree", "-rz", commit])?;
    ensure(tree.status.success(), "cannot read checked commit tree")?;
    let mut modes = BTreeMap::new();
    for row in tree
        .stdout
        .split(|byte| *byte == 0)
        .filter(|row| !row.is_empty())
    {
        let row = std::str::from_utf8(row)?;
        let (metadata, name) = row.split_once('\t').ok_or("invalid Git tree entry")?;
        modes.insert(
            name.to_owned(),
            metadata
                .split_whitespace()
                .next()
                .ok_or("missing Git mode")?
                .to_owned(),
        );
    }
    let mut pending = required
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    if root.join("flake.lock").is_file() {
        pending.insert("flake.lock".into());
    }
    // git status can omit ignored inputs and files marked assume-unchanged or
    // skip-worktree. Compare the complete snapshot input boundary directly with
    // the checked commit, including Rust modules/manifests and quality scripts.
    let mut material = super::source::input_material(root)?;
    // A hidden deletion is absent from the working-tree inventory. Include the
    // commit's allowed inputs as well so skip-worktree cannot erase a module,
    // manifest, script, or directory from the comparison.
    material.extend(
        modes
            .keys()
            .filter(|name| source_path_allowed(Path::new(name)))
            .cloned(),
    );
    for name in pending {
        material.extend(required_source_material(root, &root.join(&name))?);
    }
    for name in material {
        let Some(mode) = modes.get(&name) else {
            push(
                report,
                "errors",
                format!("required_source_not_committed:{name}"),
            );
            continue;
        };
        let path = root.join(&name);
        if !fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() || metadata.file_type().is_symlink())
        {
            push(
                report,
                "blockers",
                format!("required_source_differs_from_commit:{name}"),
            );
            continue;
        }
        let bytes = if path.is_symlink() {
            fs::read_link(&path)?
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        } else {
            fs::read(&path)?
        };
        let object = format!("{commit}:{name}");
        let committed = git(root, &["show", &object])?;
        let mut mode_matches = if path.is_symlink() {
            mode == "120000"
        } else {
            mode == "100644" || mode == "100755"
        };
        #[cfg(unix)]
        if !path.is_symlink() {
            use std::os::unix::fs::PermissionsExt;
            mode_matches &=
                (fs::metadata(&path)?.permissions().mode() & 0o100 != 0) == (mode == "100755");
        }
        if !committed.status.success() || committed.stdout != bytes || !mode_matches {
            push(
                report,
                "blockers",
                format!("required_source_differs_from_commit:{name}"),
            );
        }
    }
    Ok(())
}

pub fn check(root: &Path, tag: Option<&str>, production: bool, validator: Option<&Path>) -> Value {
    let mut report = json!({"schema":"tolight-release-check/1","scope":if production {"production_preflight"}else{"local_readiness"},
        "status":"invalid","publication_authorized":false,"checks_passed":false,"ready_for_public_release":false,"errors":[],"blockers":[]});
    if let Err(error) = inspect(root, tag, validator, &mut report) {
        push(&mut report, "errors", error.to_string());
    }
    if report["errors"].as_array().unwrap().is_empty() {
        let ready = report["blockers"].as_array().unwrap().is_empty();
        report["status"] = json!(if ready { "ready" } else { "blocked" });
        report["checks_passed"] = json!(true);
        report["ready_for_public_release"] = json!(ready);
    }
    report
}

fn inspect(
    source: &Path,
    tag: Option<&str>,
    validator: Option<&Path>,
    report: &mut Value,
) -> Result<()> {
    let root = source.canonicalize()?;
    let mut required = vec![
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "flake.nix",
        "LICENSE",
        "CHANGELOG.md",
        "SECURITY.md",
        "docs/release-status.toml",
        "docs/participant-spec.md",
        "docs/protocol.md",
        "docs/rules.md",
        "docs/join.md",
        "docs/signing-vectors.json",
        "crates/tolight-core/src/lib.rs",
        "crates/tolight-coordinator-store/src/lib.rs",
        "examples/python/minimal_client.py",
        "examples/python/check_vectors.py",
    ];
    required.extend_from_slice(WEB_SOURCE_FILES);
    required.extend(referenced_brand_sources(&root));
    for name in &required {
        let path = root.join(name);
        if !path.is_file() {
            push(report, "errors", format!("missing:{name}"));
        } else if !path.canonicalize()?.starts_with(&root) {
            push(report, "errors", format!("escaping_source:{name}"));
        }
    }
    if !report["errors"].as_array().unwrap().is_empty() {
        return Ok(());
    }
    if !root.join("flake.lock").is_file() {
        push(report, "blockers", "nix_lock_unverified");
    }
    let cargo = read_toml(root.join("Cargo.toml"))?;
    let version = cargo["workspace"]["package"]["version"]
        .as_str()
        .ok_or("invalid workspace package version")?;
    semver(version)?;
    let expected_tag = format!("v{version}");
    let tag = tag.unwrap_or(&expected_tag);
    ensure(
        tag.strip_prefix('v')
            .is_some_and(|value| semver(value).is_ok() && value == version),
        "tag must match Cargo workspace version",
    )?;
    let core = fs::read_to_string(root.join("crates/tolight-core/src/lib.rs"))?;
    let spec: String = serde_json::from_str(&constant(&core, "pub const SPEC_VERSION:")?)?;
    let public_version = spec
        .strip_prefix("tolight-participant/")
        .ok_or("invalid participant SPEC_VERSION")?;
    let pieces = public_version.split('.').collect::<Vec<_>>();
    ensure(
        pieces.len() == 2
            && pieces
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit())),
        "invalid participant SPEC_VERSION",
    )?;
    let vectors = read_json(root.join("docs/signing-vectors.json"))?;
    ensure(
        vectors["spec_version"] == spec && vectors["draft"].is_boolean(),
        "signing vectors version/draft mismatch",
    )?;
    for name in ["participant-spec", "protocol", "rules"] {
        let document = fs::read_to_string(root.join(format!("docs/{name}.md")))?;
        ensure(
            contains_version(document.lines().next().unwrap_or(""), public_version),
            format!("document version mismatch: {name}"),
        )?;
    }
    ensure(
        contains_version(
            &fs::read_to_string(root.join("docs/join.md"))?,
            public_version,
        ),
        "join version mismatch",
    )?;
    let store = fs::read_to_string(root.join("crates/tolight-coordinator-store/src/lib.rs"))?;
    let schema = constant(&store, "pub const SCHEMA_VERSION:")?.parse::<i64>()?;
    let migrations = fs::read_dir(root.join("crates/tolight-coordinator-store/migrations"))?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            let (number, _) = name.split_once('_')?;
            if !name.ends_with(".sql") {
                return None;
            }
            number.parse::<i64>().ok()
        })
        .collect::<Vec<_>>();
    ensure(
        migrations.iter().max() == Some(&schema),
        "database schema/migrations mismatch",
    )?;
    let state = read_toml(root.join("docs/release-status.toml"))?;
    ensure(
        state["schema"] == "tolight-release-status/1"
            && state["participant_spec"] == spec
            && state["database_schema"].as_i64() == Some(schema),
        "release status version/schema mismatch",
    )?;
    let entries = array_field(&state, "gates")?;
    ensure(
        entries.len() == GATES.len()
            && entries.iter().all(Value::is_object)
            && set(entries.iter().filter_map(|g| g["id"].as_str())) == set(GATES.iter().copied()),
        "release status gate IDs are missing or duplicated",
    )?;
    for item in entries {
        let id = str_field(item, "id")?;
        let status = str_field(item, "status")?;
        let evidence = array_field(item, "evidence")?;
        ensure(
            ["pending", "passed"].contains(&status),
            "invalid gate status/evidence",
        )?;
        if status != "passed" || evidence.is_empty() {
            push(report, "blockers", format!("gate:{id}"));
        } else {
            ensure(
                evidence
                    .iter()
                    .all(|value| value.as_str().is_some_and(|v| public_evidence(&root, v))),
                format!("gate evidence must reference existing public source or HTTPS: {id}"),
            )?;
        }
    }
    let (notes, exact) =
        changelog_section(&fs::read_to_string(root.join("CHANGELOG.md"))?, version)?;
    ensure(
        field(&notes, "Code-Version")? == version
            && field(&notes, "Participant-Spec")? == spec
            && field(&notes, "Database-Schema")? == schema.to_string(),
        "CHANGELOG code/spec/schema mapping mismatch",
    )?;
    if !exact || field(&notes, "Publication")? != "public" {
        push(report, "blockers", "unreleased_changelog");
    }
    let draft_documents = ["participant-spec", "protocol", "rules", "join"]
        .iter()
        .try_fold(false, |draft, name| -> Result<bool> {
            Ok(draft
                || fs::read_to_string(root.join(format!("docs/{name}.md")))?
                    .lines()
                    .take(9)
                    .any(|line| line.contains("草案")))
        })?;
    if vectors["draft"] == true || pieces[0].bytes().all(|c| c == b'0') || draft_documents {
        push(report, "blockers", "spec_not_frozen");
    }
    let rules = fs::read_to_string(root.join("docs/rules.md"))?;
    let contacts = rules
        .lines()
        .filter_map(|line| line.strip_prefix("Operator-Contact:"))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>();
    report["operator_contacts"] = json!(contacts);
    if contacts.is_empty() || contacts.iter().any(|v| placeholder_contact(v)) {
        push(report, "blockers", "operator_contact_unpublished");
    }
    report["code_version"] = json!(version);
    report["tag"] = json!(tag);
    report["participant_spec"] = json!(spec);
    report["database_schema"] = json!(schema);
    report["notes"] = json!(notes);

    let head = git(&root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if !head.status.success() {
        report["commit"] = Value::Null;
        push(report, "blockers", "git_no_commit");
    } else {
        let commit = String::from_utf8(head.stdout)?.trim().to_owned();
        report["commit"] = json!(commit);
        let top = git(&root, &["rev-parse", "--show-toplevel"])?;
        if !top.status.success()
            || Path::new(String::from_utf8_lossy(&top.stdout).trim()).canonicalize()? != root
        {
            push(report, "errors", "git_root_mismatch");
        }
        let dirty = git(
            &root,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        if !dirty.status.success() {
            push(report, "errors", "git_status_failed");
        } else if !dirty.stdout.is_empty() {
            push(report, "blockers", "git_dirty");
        }
        let tag_ref = format!("refs/tags/{tag}^{{commit}}");
        let tagged = git(&root, &["rev-parse", "--verify", &tag_ref])?;
        if !tagged.status.success() || String::from_utf8_lossy(&tagged.stdout).trim() != commit {
            push(report, "blockers", "tag_not_at_commit");
        }
        let tracked = git(&root, &["ls-files", "-z"])?;
        if !tracked.status.success() {
            push(report, "errors", "git_files_failed");
        } else {
            for name in tracked.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
                let name = std::str::from_utf8(name)?;
                let path = root.join(name);
                let bad_link = if path.is_symlink() {
                    let target = fs::read_link(&path)?;
                    let resolved = path.canonicalize()?;
                    target.is_absolute()
                        || !resolved.starts_with(&root)
                        || !resolved.strip_prefix(&root).is_ok_and(source_path_allowed)
                } else {
                    false
                };
                if !source_path_allowed(Path::new(name)) || bad_link {
                    push(
                        report,
                        "errors",
                        format!("private_or_escaping_tracked_path:{name}"),
                    );
                }
            }
        }
        let staged = git(&root, &["ls-files", "--stage", "-z"])?;
        if !staged.status.success() {
            push(report, "errors", "git_modes_failed");
        } else if staged
            .stdout
            .split(|b| *b == 0)
            .any(|row| row.starts_with(b"160000 "))
        {
            push(report, "errors", "submodules_not_in_source_contract");
        }
        // Validate tracked path privacy and repository identity before the
        // inventory can read any file bytes. Preserve those specific failures
        // rather than obscuring them with a snapshot-boundary error.
        if report["errors"].as_array().unwrap().is_empty() {
            check_commit_inputs(&root, &commit, &required, report)?;
        }
    }
    report["security"] = Value::Null;
    let security = root.join("public/.well-known/security.txt");
    if !security.exists() && !security.is_symlink() {
        push(report, "blockers", "security_contact_unpublished");
        return Ok(());
    }
    let validator = validator
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/debug/tolight"));
    if !validator.is_file() {
        push(report, "blockers", "security_validator_unavailable");
        return Ok(());
    }
    let mut command = Command::new(validator);
    command
        .args(["check-security", "--source"])
        .arg(root.join("public"))
        .current_dir(&root);
    let validated = (|| -> Result<Value> {
        let output = command_output(command, Duration::from_secs(15), 2 * 1024 * 1024)?;
        let metadata: Value = serde_json::from_slice(&output.stdout)?;
        ensure(
            output.status.success()
                && metadata["contacts"]
                    .as_array()
                    .is_some_and(|v| !v.is_empty()),
            "invalid security assets",
        )?;
        Ok(metadata)
    })();
    match validated {
        Err(_) => push(report, "errors", "security_assets_invalid"),
        Ok(metadata) => {
            if metadata["contacts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str().is_none_or(placeholder_contact))
            {
                push(report, "blockers", "security_contact_placeholder");
            }
            if metadata["canonical"].as_array().is_some_and(|values| {
                values
                    .iter()
                    .any(|v| v.as_str().is_none_or(placeholder_contact))
            }) {
                push(report, "blockers", "security_canonical_placeholder");
            }
            let intro = fs::read_to_string(root.join("SECURITY.md"))?
                .lines()
                .take(9)
                .collect::<Vec<_>>()
                .join("\n");
            if intro.contains("草案") || intro.contains("尚未") {
                push(report, "blockers", "security_policy_draft");
            }
            report["security"] = metadata;
        }
    }
    Ok(())
}

pub fn run(args: ReleaseArgs) -> Result<()> {
    let mut report = check(
        &args.source,
        args.tag.as_deref(),
        args.production,
        args.tolight.as_deref(),
    );
    let notes = report
        .as_object_mut()
        .unwrap()
        .remove("notes")
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    if let Some(path) = args.report {
        private_json(&path, &report)?;
    }
    let invalid = !report["errors"].as_array().unwrap().is_empty();
    if let Some(path) = args.notes {
        ensure(!invalid, "cannot extract notes from invalid source")?;
        private_file(&path, notes.as_bytes())?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    let code = if invalid {
        1
    } else if args.production && !report["blockers"].as_array().unwrap().is_empty() {
        2
    } else {
        0
    };
    if code == 0 {
        Ok(())
    } else {
        Err(Box::new(crate::ops::ExitCode(code)))
    }
}
