use super::{AcceptanceArgs, Result, common::*, release};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use time::{Duration, OffsetDateTime};
use url::Url;

pub const EXTRA_GATES: &[&str] = &[
    "public_release_ci",
    "recovery_targets",
    "external_probe_delivery",
    "real_model_24h",
];
pub fn ids() -> BTreeSet<String> {
    (1..=21).map(|n| format!("A{n}")).collect()
}
pub fn required(criterion: &str) -> &'static [&'static str] {
    match criterion {
        "A2" => &["public_release_ci"],
        "A3" => &["production_origins", "draft_owner_upgrade"],
        "A4" => &["third_party_m2"],
        "A5" => &["independent_m3", "third_party_m2"],
        "A6" | "A10" | "A13" | "A18" | "A19" => {
            if criterion == "A18" {
                &["third_party_m2", "production_origins"]
            } else {
                &["production_origins"]
            }
        }
        "A14" => &["operator_channel"],
        "A15" => &["recovery_targets"],
        "A16" => &["external_probe_delivery"],
        "A17" => &[
            "security_channel",
            "public_release_ci",
            "production_origins",
        ],
        "A20" => &["real_model_24h"],
        "A21" => &[
            "operator_channel",
            "content_rules",
            "independent_m3",
            "production_origins",
        ],
        _ => &[],
    }
}

fn utc(value: &Value) -> Result<OffsetDateTime> {
    let value = value
        .as_str()
        .ok_or("external observation time must be UTC ending in Z")?;
    ensure(
        value.ends_with('Z'),
        "external observation time must be UTC ending in Z",
    )?;
    Ok(tolight_core::datetime::parse_rfc3339(value)?)
}

fn safe_path(root: &Path, value: &Value) -> Result<PathBuf> {
    let value = value
        .as_str()
        .ok_or("evidence path must be a repository-relative file")?;
    ensure(
        !value.is_empty() && !value.contains(['\\', '\0']),
        "evidence path must be a repository-relative file",
    )?;
    let path = Path::new(value);
    let parts = lexical_parts(path).ok_or("private or escaping evidence path")?;
    // Reject lexical parent traversal even when canonicalization would stay inside.
    ensure(
        !path
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir)),
        "private or escaping evidence path",
    )?;
    let private = [
        ".git",
        ".local",
        "target",
        "public-dist",
        ".aws",
        ".codex",
        ".agents",
        ".direnv",
    ];
    ensure(
        !parts.iter().any(|part| private.contains(&part.as_str())),
        "private or escaping evidence path",
    )?;
    let roots = [
        "docs",
        "tests",
        "crates",
        "examples",
        "scripts",
        "public",
        ".github",
        "frontend",
        "deploy",
        "operator-ui",
    ];
    let files = [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "flake.nix",
        "flake.lock",
        "LICENSE",
        "SECURITY.md",
        "CHANGELOG.md",
        "package.json",
        "package-lock.json",
        "tsconfig.json",
        "tsconfig.web.json",
        "tsconfig.admin.json",
    ];
    ensure(
        roots.contains(&parts[0].as_str()) || files.contains(&value),
        "evidence is outside published source paths",
    )?;
    let name = parts.last().unwrap();
    ensure(
        !["config.toml", "reference.toml", "client-state.json", ".env"].contains(&name.as_str())
            && !name.starts_with(".env.")
            && !path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| PRIVATE_SUFFIXES.contains(&s.to_ascii_lowercase().as_str())),
        "private evidence file type",
    )?;
    let full = root.join(path);
    let resolved = full
        .canonicalize()
        .map_err(|_| "evidence file is missing or escapes the repository")?;
    ensure(
        full.is_file() && resolved.starts_with(root),
        "evidence file is missing or escapes the repository",
    )?;
    ensure(
        !resolved
            .strip_prefix(root)?
            .components()
            .any(|p| private.iter().any(|v| p.as_os_str() == *v)),
        "evidence symlink resolves into private data",
    )?;
    if full.is_symlink() {
        ensure(
            !fs::read_link(&full)?.is_absolute(),
            "evidence symlink must be portable and relative",
        )?;
    }
    Ok(full)
}

fn secrets(value: &Value) -> bool {
    match value {
        Value::Object(values) => values.iter().any(|(key, value)| {
            [
                "owner_contact",
                "private_key",
                "secret_cmd",
                "database_url",
                "api_key",
                "credentials",
            ]
            .contains(&key.to_ascii_lowercase().as_str())
                || secrets(value)
        }),
        Value::Array(values) => values.iter().any(secrets),
        _ => false,
    }
}

fn nonempty_public_label(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        !text.trim().is_empty()
            && text.trim() == text
            && text.chars().count() <= 256
            && !text.chars().any(char::is_control)
    })
}

fn client_provenance(root: &Path, value: &Value) -> Result<()> {
    ensure(
        value["schema"] == "tolight-external-evidence/2",
        "independent Agent evidence needs schema 2 with actual framework/model and client provenance",
    )?;
    let agent = &value["agent"];
    ensure(
        agent.as_object().is_some_and(|fields| {
            fields.len() == 3
                && ["framework", "framework_version", "model"]
                    .iter()
                    .all(|key| nonempty_public_label(&agent[*key]))
        }),
        "independent Agent evidence requires actual framework, framework version and model identifiers",
    )?;
    let client = &value["client"];
    ensure(
        client.as_object().is_some_and(|fields| {
            fields.keys().all(|key| {
                ["kind", "revision", "repository", "source_record"].contains(&key.as_str())
            })
        }),
        "client provenance has invalid fields",
    )?;
    let kind = client["kind"]
        .as_str()
        .ok_or("client provenance kind is missing")?;
    ensure(
        ["tolink", "custom", "reference", "existing"].contains(&kind),
        "client provenance kind is not a supported implementation source",
    )?;
    let revision = client["revision"]
        .as_str()
        .ok_or("client provenance revision is missing")?;
    ensure(
        [40, 64].contains(&revision.len()) && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "client provenance requires an immutable commit or source SHA256, not a mutable branch/tag",
    )?;
    if kind == "tolink" {
        let repository = client["repository"]
            .as_str()
            .and_then(|text| Url::parse(text).ok())
            .ok_or("tolink provenance requires the official repository URL")?;
        ensure(
            repository.scheme() == "https"
                && repository.host_str() == Some("github.com")
                && repository.port().is_none()
                && repository.username().is_empty()
                && repository.password().is_none()
                && repository.query().is_none()
                && repository.fragment().is_none()
                && repository
                    .path()
                    .trim_end_matches('/')
                    .trim_end_matches(".git")
                    .eq_ignore_ascii_case("/RaynorZhong/tolink")
                && revision.len() == 40,
            "tolink provenance requires the official repository and a fixed commit",
        )?;
    }
    // Existing clients and private implementations need a public, redacted
    // provenance summary, not publication of their repository or source.
    ensure(
        kind == "tolink" || client.get("source_record").is_some(),
        "custom, reference and existing clients require a redacted source_record",
    )?;
    if let Some(record) = client.get("source_record") {
        let path = safe_path(root, record)?;
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            ensure(
                fs::metadata(&path)?.len() <= 1024 * 1024,
                "client provenance summary exceeds 1 MiB",
            )?;
            let record = read_json(path)?;
            ensure(
                !secrets(&record)
                    && !record["schema"]
                        .as_str()
                        .is_some_and(|schema| schema.starts_with("tolight-control-snapshot/")),
                "private controls or contacts cannot be client provenance",
            )?;
        }
    }
    Ok(())
}

fn external(root: &Path, path: &Path, criterion: &str) -> Result<String> {
    ensure(
        fs::metadata(path)?.len() <= 1024 * 1024,
        "external evidence summary exceeds 1 MiB",
    )?;
    let value = read_json(path)?;
    let fields = [
        "schema",
        "scope",
        "criterion_ids",
        "record_id",
        "observed_at",
        "record",
        "result",
        "independent_actor",
        "project_owned_client",
        "real_model",
        "process_manager",
        "window_start",
        "window_end",
        "production_url",
        "agent",
        "client",
    ];
    ensure(
        value
            .as_object()
            .is_some_and(|v| v.keys().all(|key| fields.contains(&key.as_str())))
            && !secrets(&value),
        "external evidence must be a redacted summary, not private controls",
    )?;
    ensure(
        ["tolight-external-evidence/1", "tolight-external-evidence/2"]
            .iter()
            .any(|schema| value["schema"] == *schema)
            && value["scope"] == "production"
            && value["result"] == "passed"
            && value["record_id"].as_str().is_some_and(|s| !s.is_empty()),
        "external evidence is not a passed production observation",
    )?;
    let criteria = array_field(&value, "criterion_ids")?;
    ensure(
        criteria.iter().any(|v| v == criterion)
            && criteria
                .iter()
                .all(|v| v.as_str().is_some_and(|s| ids().contains(s))),
        "external evidence does not identify this criterion",
    )?;
    let observed = utc(&value["observed_at"])?;
    ensure(
        observed <= OffsetDateTime::now_utc() + Duration::minutes(5),
        "external observation is in the future",
    )?;
    safe_path(root, &value["record"])?;
    let url = value["production_url"]
        .as_str()
        .and_then(|s| Url::parse(s).ok())
        .ok_or("external evidence requires an actual public HTTPS observation URL")?;
    ensure(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.host_str().is_some_and(|host| !placeholder_host(host)),
        "external evidence requires an actual public HTTPS observation URL",
    )?;
    if ["A4", "A5", "A18"].contains(&criterion) {
        ensure(
            value["independent_actor"] == true && value["real_model"] == true,
            "project scripts and model stubs cannot satisfy actual independent Agent acceptance",
        )?;
        // Client authorship is no longer an admission criterion. The same
        // maintained Rust client may serve a real independent Agent, while
        // framework/model provenance and actual production observations remain.
        client_provenance(root, &value)?;
    }
    if criterion == "A20" {
        ensure(
            value["real_model"] == true
                && value["process_manager"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty()),
            "A20 needs a real model and actual process manager",
        )?;
        let start = utc(&value["window_start"])?;
        let end = utc(&value["window_end"])?;
        ensure(
            end > start && end - start >= Duration::hours(24) && end <= observed,
            "A20 needs at least 24 observed production hours",
        )?;
    }
    Ok(str_field(&value, "record_id")?.into())
}

pub fn render(data: &Value) -> Result<String> {
    let mut lines=vec![
        "# 首版验收账本（本地范围）".to_owned(),"".into(),
        "本表逐项对应tolight-plan.md第15节A1–A21。verified只表示所列本地证据已具备；final=pending表示首版对应项尚未声明完成。本地脚本、模型桩、合成Gitfixture和质量门不能代替真实第三方、生产通知或24小时线上记录。".into(),"".into(),
        "机器事实源为[acceptance-status.toml](acceptance-status.toml)。本地核对运行scripts/acceptance-check.sh；正式核对加--final，未完成时退出2。检查器只检查覆盖、链接、记录类型和已有Gate的一致性，不重跑全部实验，也不独立证明观察者的声明。".into(),"".into(),
        "| 验收项 | 本地状态 | 最终状态 | 已有证据 | 剩余条件 |".into(),"|---|---|---|---|---|".into()];
    let mut items = array_field(data, "criteria")?.iter().collect::<Vec<_>>();
    items.sort_by_key(|item| {
        item["id"]
            .as_str()
            .and_then(|s| s.strip_prefix('A'))
            .and_then(|s| s.parse::<u32>().ok())
    });
    for item in items {
        let mut links = vec![];
        for evidence in array_field(item, "evidence")? {
            let path = str_field(evidence, "path")?;
            let relative = path
                .strip_prefix("docs/")
                .map(str::to_owned)
                .unwrap_or_else(|| format!("../{path}"));
            // Encode a relative Markdown destination with urllib.quote's
            // unreserved set plus '/', without changing its hierarchy.
            let mut encoded = String::new();
            for byte in relative.bytes() {
                if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
                    encoded.push(char::from(byte))
                } else {
                    encoded.push_str(&format!("%{byte:02X}"))
                }
            }
            links.push(format!(
                "[{}]({encoded})",
                str_field(evidence, "label")?.replace('|', "\\|")
            ));
        }
        let remaining = array_field(item, "remaining")?
            .iter()
            .map(|v| v.as_str().ok_or("invalid remaining condition"))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("；")
            .replace('|', "\\|");
        lines.push(format!(
            "| {} {} | {} | {} | {} | {remaining} |",
            str_field(item, "id")?,
            str_field(item, "title")?,
            str_field(item, "local_status")?,
            str_field(item, "final_status")?,
            links.join("、")
        ));
    }
    lines.extend(["","## 外部证据与隐私","",
        "当前7个公开接入Gate来自release-status.toml；正式源码CI、恢复目标、异机告警送达和真实模型24小时记录另在本账本登记，未取得证据时均pending。外部完成摘要必须引用实际脱敏记录、生产观察时间和来源，不接受本地输出冒充；A5的独立M3记录不得复用A4/A18的同一实走记录。没有完成这些工作时，不改为complete，不以空链接或待补文件代替证据。","",
        "A4/A5/A18的新外部摘要使用tolight-external-evidence/2，记录真实框架/模型及固定客户端来源；优先tolink，也接受自写、参考或既有实现，客户端由项目提供不再构成拒绝条件。旧v1仍适用于其他验收项，但缺少新来源字段的v1不能关闭真实Agent验收。具体格式与兼容见[external-evidence.md](external-evidence.md)。仅有tolink离线自检不代表实际接入。","",
        "这里只链接可入库的脱敏记录与源码；不链接.local中的控制快照、恢复计划、数据库备份、私钥或Owner联系方式。原始私有日志由运行者保管，公共记录只保留必要结果摘要。","",
        "M0–M3记录保留当时事实，M4本轮WAL/PITR、告警处置和最终质量门的新增证据由本轮M4记录补入。当前账本不是首版完成声明，也没有执行部署、联系他人或创建发布。",""].map(str::to_owned));
    Ok(lines.join("\n"))
}

pub fn check(source: &Path, ledger: Option<&Path>, check_render: bool) -> (Value, Option<Value>) {
    let mut report = json!({"schema":"tolight-acceptance-check/1","scope":"local_evidence_index","status":"invalid","errors":[],"pending":[],"criteria":[],"structural_checks_passed":false,"first_release_complete":false,"external_observations_independently_verified":false});
    match inspect(source, ledger, check_render, &mut report) {
        Ok(data) => (report, Some(data)),
        Err(error) => {
            report["errors"]
                .as_array_mut()
                .unwrap()
                .push(json!(error.to_string()));
            (report, None)
        }
    }
}

fn inspect(
    source: &Path,
    ledger: Option<&Path>,
    check_render: bool,
    report: &mut Value,
) -> Result<Value> {
    let root = source.canonicalize()?;
    let ledger = ledger
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("docs/acceptance-status.toml"));
    let ledger = if ledger.is_absolute() {
        ledger
    } else {
        root.join(ledger)
    };
    ensure(
        ledger.canonicalize()?.starts_with(&root),
        "ledger must remain inside the source repository",
    )?;
    let data = read_toml(ledger)?;
    ensure(
        data["schema"] == "tolight-acceptance-status/1" && data["scope"] == "local_pre_release",
        "unsupported acceptance ledger schema/scope",
    )?;
    let items = array_field(&data, "criteria")?;
    ensure(
        items.len() == 21
            && items.iter().all(Value::is_object)
            && set(items.iter().filter_map(|v| v["id"].as_str())) == ids(),
        "criteria must contain each of A1-A21 exactly once",
    )?;
    let plan = fs::read_to_string(root.join("tolight-plan.md"))?;
    let plan_ids = set(plan
        .lines()
        .filter_map(|line| {
            line.strip_prefix("- [ ] **")
                .or_else(|| line.strip_prefix("- [x] **"))
        })
        .filter_map(|line| line.split_once("**").map(|(id, _)| id)));
    ensure(
        plan_ids == ids(),
        "acceptance ledger and plan criterion IDs differ",
    )?;
    let release = read_toml(root.join("docs/release-status.toml"))?;
    ensure(
        release["schema"] == "tolight-release-status/1",
        "unsupported release gates schema",
    )?;
    let shared = array_field(&release, "gates")?;
    let extra = array_field(&data, "gates")?;
    ensure(
        shared.len() == GATES.len()
            && shared.iter().all(Value::is_object)
            && set(shared.iter().filter_map(|v| v["id"].as_str())) == set(GATES.iter().copied()),
        "release gates are missing or duplicated",
    )?;
    ensure(
        extra.len() == EXTRA_GATES.len()
            && extra.iter().all(Value::is_object)
            && set(extra.iter().filter_map(|v| v["id"].as_str()))
                == set(EXTRA_GATES.iter().copied()),
        "additional acceptance gates are missing or duplicated",
    )?;
    let mut gates = BTreeMap::new();
    for gate in shared.iter().chain(extra) {
        let status = str_field(gate, "status")?;
        let evidence = array_field(gate, "evidence")?;
        ensure(
            ["pending", "passed"].contains(&status),
            "invalid gate status/evidence",
        )?;
        gates.insert(str_field(gate, "id")?.to_owned(), status.to_owned());
        if status == "passed" {
            ensure(
                !evidence.is_empty(),
                format!("passed gate has no evidence: {}", str_field(gate, "id")?),
            )?;
            for path in evidence {
                safe_path(&root, path)?;
            }
        }
    }
    let mut observations = BTreeMap::new();
    for item in items {
        let criterion = str_field(item, "id")?;
        let local = str_field(item, "local_status")?;
        let final_status = str_field(item, "final_status")?;
        ensure(
            ["verified", "partial", "pending"].contains(&local)
                && ["pending", "complete"].contains(&final_status),
            format!("invalid criterion status: {criterion}"),
        )?;
        ensure(
            item["title"]
                .as_str()
                .is_some_and(|v| !v.is_empty() && !v.contains(['\n', '|'])),
            "invalid criterion title",
        )?;
        let remaining = array_field(item, "remaining")?;
        ensure(
            remaining.iter().all(|v| {
                v.as_str()
                    .is_some_and(|s| !s.is_empty() && !s.contains('\n'))
            }),
            "invalid remaining conditions",
        )?;
        let evidence = array_field(item, "evidence")?;
        ensure(
            !evidence.is_empty(),
            "each criterion needs linked evidence, including pending-state records",
        )?;
        let mut types = BTreeSet::new();
        let mut records = BTreeSet::new();
        for entry in evidence {
            ensure(
                entry
                    .as_object()
                    .is_some_and(|v| set(v.keys().cloned()) == set(["kind", "path", "label"])),
                "invalid evidence shape",
            )?;
            let kind = str_field(entry, "kind")?;
            ensure(
                ["local_record", "test_output", "source", "external_record"].contains(&kind),
                "invalid evidence kind",
            )?;
            ensure(
                entry["label"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty() && !v.contains(['\n', '[', ']'])),
                "invalid evidence label",
            )?;
            let path = safe_path(&root, &entry["path"])?;
            if path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("json"))
            {
                ensure(
                    fs::metadata(&path)?.len() <= 1024 * 1024,
                    "evidence summary exceeds 1 MiB",
                )?;
                let content = read_json(&path)?;
                ensure(
                    !secrets(&content)
                        && !content["schema"]
                            .as_str()
                            .is_some_and(|v| v.starts_with("tolight-control-snapshot/")),
                    "private controls or contacts cannot be public evidence",
                )?;
            }
            if kind == "external_record" {
                records.insert(external(&root, &path, criterion)?);
            }
            types.insert(kind.to_owned());
        }
        ensure(
            local != "verified"
                || ["local_record", "test_output", "external_record"]
                    .iter()
                    .any(|v| types.contains(*v)),
            "source code alone is not a completed local test record",
        )?;
        let unmet = required(criterion)
            .iter()
            .filter(|g| gates.get(**g).is_none_or(|s| s != "passed"))
            .copied()
            .collect::<Vec<_>>();
        if final_status == "complete" {
            ensure(
                local == "verified" && remaining.is_empty(),
                format!(
                    "complete criterion still has local work or remaining conditions: {criterion}"
                ),
            )?;
            ensure(
                unmet.is_empty(),
                format!(
                    "criterion marked complete while required gates remain pending: {criterion}"
                ),
            )?;
            ensure(
                required(criterion).is_empty() || !records.is_empty(),
                format!("local records cannot replace external completion evidence: {criterion}"),
            )?;
        } else {
            ensure(
                !remaining.is_empty(),
                "pending criterion must explain what remains",
            )?;
            report["pending"]
                .as_array_mut()
                .unwrap()
                .push(json!(criterion));
        }
        observations.insert(criterion.to_owned(), records);
        report["criteria"].as_array_mut().unwrap().push(json!({"id":criterion,"local_status":local,"final_status":final_status,"required_gates_pending":unmet,"evidence_count":evidence.len()}));
    }
    ensure(
        observations["A5"].is_disjoint(&observations["A4"])
            && observations["A5"].is_disjoint(&observations["A18"]),
        "M3 independent onboarding cannot reuse the Task25 observation",
    )?;
    if check_render {
        ensure(
            fs::read_to_string(root.join("docs/acceptance.md"))? == render(&data)?,
            "acceptance.md differs from the machine ledger; regenerate with --render",
        )?;
    }
    if report["pending"].as_array().unwrap().is_empty() {
        let readiness = release::check(&root, None, true, None);
        let mut limited = serde_json::Map::new();
        for key in ["status", "errors", "blockers", "ready_for_public_release"] {
            limited.insert(key.into(), readiness[key].clone());
        }
        report["release_readiness"] = json!(limited);
        ensure(
            readiness["ready_for_public_release"] == true,
            "complete acceptance declarations conflict with current formal release readiness",
        )?;
    }
    report["structural_checks_passed"] = json!(true);
    let complete = report["pending"].as_array().unwrap().is_empty();
    report["status"] = json!(if complete { "complete" } else { "pending" });
    report["first_release_complete"] = json!(complete);
    report["local_counts"] = json!({"verified":items.iter().filter(|i|i["local_status"]=="verified").count(),"partial":items.iter().filter(|i|i["local_status"]=="partial").count(),"pending":items.iter().filter(|i|i["local_status"]=="pending").count()});
    report["final_complete_count"] = json!(
        items
            .iter()
            .filter(|i| i["final_status"] == "complete")
            .count()
    );
    report["criterion_count"] = json!(items.len());
    Ok(data)
}

pub fn run(args: AcceptanceArgs) -> Result<()> {
    let (report, data) = check(&args.source, args.ledger.as_deref(), !args.render);
    if let (true, Some(data)) = (args.render, data) {
        print!("{}", render(&data)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    let code = if !report["errors"].as_array().unwrap().is_empty() {
        1
    } else if args.final_check && report["first_release_complete"] != true {
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
