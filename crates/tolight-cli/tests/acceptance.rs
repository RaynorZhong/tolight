#[path = "support/delivery.rs"]
mod support;
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::{fs, path::PathBuf};
use support::*;

fn required(id: &str) -> Vec<&'static str> {
    match id {
        "A2" => vec!["public_release_ci"],
        "A3" => vec!["production_origins", "draft_owner_upgrade"],
        "A4" => vec!["third_party_m2"],
        "A5" => vec!["independent_m3", "third_party_m2"],
        "A6" | "A10" | "A13" | "A19" => vec!["production_origins"],
        "A14" => vec!["operator_channel"],
        "A15" => vec!["recovery_targets"],
        "A16" => vec!["external_probe_delivery"],
        "A17" => vec![
            "security_channel",
            "public_release_ci",
            "production_origins",
        ],
        "A18" => vec!["third_party_m2", "production_origins"],
        "A20" => vec!["real_model_24h"],
        "A21" => vec![
            "operator_channel",
            "content_rules",
            "independent_m3",
            "production_origins",
        ],
        _ => vec![],
    }
}
struct Acceptance {
    _temp: tempfile::TempDir,
    root: PathBuf,
    data: Value,
}
impl Acceptance {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        fs::create_dir(&root).unwrap();
        let criteria=(1..=21).map(|number|json!({"id":format!("A{number}"),"title":format!("Fixture criterion {number}"),"local_status":"verified","final_status":"pending","remaining":["Synthetic unresolved fixture"],"evidence":[{"kind":"local_record","path":"docs/demos/M3.md","label":"Synthetic fixture record"}]})).collect::<Vec<_>>();
        let gates = EXTRA
            .iter()
            .map(|id| json!({"id":id,"status":"pending","evidence":[]}))
            .collect::<Vec<_>>();
        let data = json!({"schema":"tolight-acceptance-status/1","scope":"local_pre_release","gates":gates,"criteria":criteria});
        write(
            &root,
            "docs/demos/M3.md",
            "# Synthetic fixture, no real observation\n",
        );
        write(&root, "Cargo.toml", "[workspace]\n");
        write(
            &root,
            "tolight-plan.md",
            fs::read(repo().join("tolight-plan.md")).unwrap(),
        );
        toml(
            &root,
            "docs/release-status.toml",
            &json!({"schema":"tolight-release-status/1","gates":GATES.iter().map(|id|json!({"id":id,"status":"pending","evidence":[]})).collect::<Vec<_>>()}),
        );
        let value = Self {
            _temp: temp,
            root,
            data,
        };
        value.save();
        value
    }
    fn row(&mut self, id: &str) -> &mut Value {
        self.data["criteria"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|row| row["id"] == id)
            .unwrap()
    }
    fn save(&self) {
        toml(&self.root, "docs/acceptance-status.toml", &self.data);
        let rendered = cli("acceptance-check", &self.root, &["--render"]);
        write(&self.root, "docs/acceptance.md", rendered.stdout);
    }
    fn result(&self) -> Value {
        self.save();
        result(&cli("acceptance-check", &self.root, &[]))
    }
    fn invalid(&self, contains: &str) -> Value {
        let report = self.result();
        assert_eq!(report["structural_checks_passed"], false, "{report}");
        assert_eq!(report["first_release_complete"], false);
        assert!(
            errors(&report).iter().any(|error| error.contains(contains)),
            "expected {contains}: {report}"
        );
        report
    }
    fn pass_gate(&mut self, id: &str) {
        if EXTRA.contains(&id) {
            let gate = self.data["gates"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|g| g["id"] == id)
                .unwrap();
            gate["status"] = json!("passed");
            gate["evidence"] = json!(["docs/demos/M3.md"]);
        } else {
            let mut release = load_toml(self.root.join("docs/release-status.toml"));
            let gate = release["gates"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|g| g["id"] == id)
                .unwrap();
            gate["status"] = json!("passed");
            gate["evidence"] = json!(["docs/demos/M3.md"]);
            toml(&self.root, "docs/release-status.toml", &release);
        }
    }
    fn observation(&mut self, id: &str, changes: Value) -> String {
        let mut value = json!({"schema":"tolight-external-evidence/1","scope":"production","criterion_ids":[id],"record_id":format!("fixture-{id}"),"observed_at":"2020-01-03T00:00:00Z","record":"docs/demos/M3.md","result":"passed","production_url":"https://tolight.com/join.md","independent_actor":true,"project_owned_client":false,"real_model":true});
        if ["A4", "A5", "A18"].contains(&id) {
            value["schema"] = json!("tolight-external-evidence/2");
            value["agent"] = json!({
                "framework":"synthetic-fixture-framework",
                "framework_version":"1.0.0",
                "model":"synthetic-fixture-model"
            });
            // This only tests schema validation; it does not claim the cited
            // tolink revision supports online onboarding or ran a real model.
            value["client"] = json!({
                "kind":"tolink",
                "repository":"https://github.com/RaynorZhong/tolink",
                "revision":"825e5424e04465661a0e2a30390534bb2b05965c"
            });
        }
        for (key, item) in changes.as_object().unwrap() {
            value[key] = item.clone();
        }
        let path = format!("docs/demos/fixture-{id}.json");
        json(&self.root, &path, &value);
        let row = self.row(id);
        row["local_status"] = json!("verified");
        row["final_status"] = json!("complete");
        row["remaining"] = json!([]);
        row["evidence"] = json!([{"kind":"external_record","path":path,"label":"Synthetic fixture; not actual observation"}]);
        for gate in required(id) {
            self.pass_gate(gate);
        }
        path
    }
}

#[test]
fn twenty_one_local_mappings_do_not_claim_final_completion() {
    let f = Acceptance::new();
    let report = f.result();
    assert_eq!(report["criterion_count"], 21);
    assert_eq!(report["structural_checks_passed"], true);
    assert_eq!(report["pending"].as_array().unwrap().len(), 21);
    let pending = report["pending"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        pending,
        (1..=21).map(|number| format!("A{number}")).collect()
    );
    assert_eq!(report["first_release_complete"], false);
    assert_eq!(
        report["external_observations_independently_verified"],
        false
    );
    assert_eq!(report["final_complete_count"], 0);
}
#[test]
fn repository_ledger_matches_plan_and_safe_evidence_links() {
    let report = result(&cli("acceptance-check", &repo(), &[]));
    assert_eq!(report["structural_checks_passed"], true, "{report}");
    assert_eq!(report["criterion_count"], 21);
}
#[test]
fn local_final_and_render_cli_are_distinct_and_read_only() {
    let f = Acceptance::new();
    let before = hashes(&f.root);
    let local = cli("acceptance-check", &f.root, &[]);
    let final_check = cli("acceptance-check", &f.root, &["--final"]);
    let render = cli("acceptance-check", &f.root, &["--render"]);
    assert!(local.status.success());
    assert_eq!(final_check.status.code(), Some(2));
    assert_eq!(result(&final_check)["first_release_complete"], false);
    assert!(render.status.success());
    assert_eq!(
        render.stdout,
        fs::read(f.root.join("docs/acceptance.md")).unwrap()
    );
    assert_eq!(hashes(&f.root), before);
}
#[test]
fn missing_duplicate_and_unknown_ids_are_rejected() {
    for id in ["A1", "A22"] {
        let mut f = Acceptance::new();
        f.data["criteria"][20]["id"] = json!(id);
        f.invalid("each of A1-A21");
    }
    let mut f = Acceptance::new();
    f.data["criteria"].as_array_mut().unwrap().pop();
    f.invalid("each of A1-A21");
}
#[test]
fn plan_and_ledger_cannot_drift() {
    let f = Acceptance::new();
    let path = f.root.join("tolight-plan.md");
    fs::write(
        &path,
        fs::read_to_string(&path)
            .unwrap()
            .replace("**A21**", "**A22**"),
    )
    .unwrap();
    f.invalid("plan criterion IDs");
}
#[test]
fn missing_test_output_is_not_completed_evidence() {
    let mut f = Acceptance::new();
    f.row("A7")["evidence"][0]["path"] = json!("docs/demos/not-written.md");
    f.invalid("missing");
}
#[test]
fn private_or_escaping_paths_are_never_public_evidence() {
    for path in [
        ".local/controls.json",
        "docs/demo.key",
        "docs/restore.dump",
        "../outside.md",
        "https://example.org/result",
        ".",
        "docs/../private.json",
    ] {
        let mut f = Acceptance::new();
        f.row("A7")["evidence"][0]["path"] = json!(path);
        f.invalid("evidence");
    }
}
#[cfg(unix)]
#[test]
fn symbolic_links_cannot_escape_or_point_into_private_controls() {
    let mut f = Acceptance::new();
    write(
        &f.root,
        ".local/controls.json",
        "{\"owner_contact\":\"private marker\"}",
    );
    let link = f.root.join("docs/demos/linked.json");
    symlink(f.root.join(".local/controls.json"), &link).unwrap();
    f.row("A7")["evidence"][0]["path"] = json!("docs/demos/linked.json");
    f.invalid("private data");
    fs::remove_file(&link).unwrap();
    let outside = f._temp.path().join("outside.json");
    fs::write(&outside, "{}").unwrap();
    symlink(outside, &link).unwrap();
    f.invalid("escapes");
}
#[test]
fn complete_declaration_cannot_override_pending_gate() {
    let mut f = Acceptance::new();
    let row = f.row("A4");
    row["final_status"] = json!("complete");
    row["remaining"] = json!([]);
    f.invalid("required gates remain pending");
}
#[test]
fn passed_gate_needs_existing_redacted_evidence() {
    let mut f = Acceptance::new();
    f.data["gates"][0]["status"] = json!("passed");
    f.invalid("no evidence");
    f.data["gates"][0]["evidence"] = json!(["docs/missing.md"]);
    f.invalid("missing");
}
#[test]
fn duplicate_extra_gate_is_rejected() {
    let mut f = Acceptance::new();
    f.data["gates"][3]["id"] = f.data["gates"][0]["id"].clone();
    f.invalid("gates are missing or duplicated");
}
#[test]
fn source_definition_alone_is_not_test_execution() {
    let mut f = Acceptance::new();
    f.row("A7")["evidence"] = json!([{"kind":"source","path":"Cargo.toml","label":"source"}]);
    f.invalid("source code alone");
}
#[test]
fn complete_status_requires_verified_local_work_and_no_remaining_conditions() {
    let mut f = Acceptance::new();
    f.row("A7")["final_status"] = json!("complete");
    f.invalid("remaining conditions");
    let row = f.row("A7");
    row["local_status"] = json!("partial");
    row["remaining"] = json!([]);
    f.invalid("local work");
}
#[test]
fn passed_external_gate_does_not_make_project_script_an_external_observation() {
    let mut f = Acceptance::new();
    f.pass_gate("third_party_m2");
    let row = f.row("A4");
    row["final_status"] = json!("complete");
    row["remaining"] = json!([]);
    f.invalid("local records cannot replace external");
}
#[test]
fn synthetic_valid_production_summary_remains_only_a_claim() {
    let mut f = Acceptance::new();
    f.observation("A4", json!({}));
    let report = f.result();
    assert!(errors(&report).is_empty(), "{report}");
    assert_eq!(report["final_complete_count"], 1);
    assert_eq!(report["first_release_complete"], false);
    assert_eq!(
        report["external_observations_independently_verified"],
        false
    );
}
#[test]
fn local_or_non_independent_or_fake_model_never_satisfies_independent_agent() {
    for (changes, expected) in [
        (json!({"scope":"local"}), "production observation"),
        (json!({"independent_actor":false}), "project scripts"),
        (json!({"real_model":false}), "project scripts"),
    ] {
        let mut f = Acceptance::new();
        f.observation("A4", changes);
        f.invalid(expected);
    }
}
#[test]
fn project_owned_client_is_allowed_for_actual_independent_agent_claims() {
    let mut f = Acceptance::new();
    f.observation("A4", json!({"project_owned_client":true}));
    let report = f.result();
    assert!(errors(&report).is_empty(), "{report}");
    assert_eq!(
        report["external_observations_independently_verified"],
        false
    );
    assert_eq!(report["first_release_complete"], false);
}
#[test]
fn legacy_evidence_does_not_silently_close_agent_criteria() {
    for id in ["A4", "A5", "A18"] {
        let mut f = Acceptance::new();
        f.observation(id, json!({"schema":"tolight-external-evidence/1"}));
        f.invalid("needs schema 2");
    }
    let mut f = Acceptance::new();
    f.observation("A6", json!({}));
    assert!(errors(&f.result()).is_empty());
}
#[test]
fn independent_agent_evidence_requires_framework_model_and_immutable_client_source() {
    for (changes, expected) in [
        (json!({"agent":null}), "framework"),
        (
            json!({"agent":{"framework":"Hermes","framework_version":"1.0","model":""}}),
            "framework",
        ),
        (
            json!({"agent":{"framework":"Hermes","framework_version":"1.0","model":"model","unknown":true}}),
            "framework",
        ),
        (json!({"client":null}), "client provenance"),
        (
            json!({"client":{"kind":"unknown","revision":"825e5424e04465661a0e2a30390534bb2b05965c"}}),
            "kind",
        ),
        (
            json!({"client":{"kind":"tolink","repository":"https://github.com/RaynorZhong/tolink","revision":"main"}}),
            "immutable",
        ),
        (
            json!({"client":{"kind":"tolink","repository":"https://github.com/other/tolink","revision":"825e5424e04465661a0e2a30390534bb2b05965c"}}),
            "official repository",
        ),
        (
            json!({"client":{"kind":"tolink","repository":"https://github.com/RaynorZhong/tolink?token=private","revision":"825e5424e04465661a0e2a30390534bb2b05965c"}}),
            "official repository",
        ),
        (
            json!({"client":{"kind":"custom","revision":"825e5424e04465661a0e2a30390534bb2b05965c"}}),
            "source_record",
        ),
        (
            json!({"client":{"kind":"custom","source_record":".local/private.json","revision":"825e5424e04465661a0e2a30390534bb2b05965c"}}),
            "evidence",
        ),
    ] {
        let mut f = Acceptance::new();
        f.observation("A4", changes);
        f.invalid(expected);
    }
}
#[test]
fn custom_reference_and_existing_clients_can_use_redacted_source_records() {
    for kind in ["custom", "reference", "existing"] {
        let mut f = Acceptance::new();
        f.observation(
            "A4",
            json!({"client":{
                "kind":kind,
                "source_record":"docs/demos/M3.md",
                "revision":"f".repeat(64)
            }}),
        );
        assert!(errors(&f.result()).is_empty());
    }
}
#[test]
fn client_provenance_cannot_reference_private_control_json() {
    let mut f = Acceptance::new();
    write(
        &f.root,
        "docs/demos/client-source.JSON",
        "{\"schema\":\"tolight-control-snapshot/1\",\"agents\":[]}",
    );
    f.observation(
        "A4",
        json!({"client":{
            "kind":"existing",
            "source_record":"docs/demos/client-source.JSON",
            "revision":"f".repeat(64)
        }}),
    );
    f.invalid("private controls");
    write(
        &f.root,
        "docs/demos/client-source.JSON",
        "{\"api_key\":\"must not appear in output\"}",
    );
    let report = f.invalid("private controls");
    assert!(!report.to_string().contains("must not appear in output"));
}
#[test]
fn external_summaries_reject_contacts_without_echoing_them() {
    let mut f = Acceptance::new();
    let path = f.observation("A4", json!({"owner_contact":"must not appear in output"}));
    assert!(f.root.join(path).is_file());
    let report = f.invalid("private controls");
    assert!(!report.to_string().contains("must not appear in output"));
}
#[test]
fn production_urls_reject_placeholders_loopback_credentials_and_queries() {
    for url in [
        "http://tolight.com/",
        "https://example.invalid/",
        "https://[::1]/",
        "https://127.0.0.1/",
        "https://user:password@tolight.com/",
        "https://tolight.com/?token=private",
    ] {
        let mut f = Acceptance::new();
        f.observation("A4", json!({"production_url":url}));
        f.invalid("URL");
    }
}
#[test]
fn a20_requires_real_model_manager_and_at_least_twenty_four_observed_hours() {
    let mut f = Acceptance::new();
    let path=f.observation("A20",json!({"process_manager":"fixture-manager","window_start":"2020-01-01T00:00:00Z","window_end":"2020-01-01T23:59:59Z"}));
    f.invalid("24 observed");
    let mut value: Value = serde_json::from_slice(&fs::read(f.root.join(&path)).unwrap()).unwrap();
    value["window_end"] = json!("2020-01-02T00:00:00Z");
    json(&f.root, &path, &value);
    assert!(errors(&f.result()).is_empty());
    value["real_model"] = json!(false);
    json(&f.root, &path, &value);
    f.invalid("real model");
}
#[test]
fn independent_m3_record_cannot_reuse_task25_observation() {
    let mut f = Acceptance::new();
    f.observation("A4", json!({"record_id":"one-real-run"}));
    f.observation("A5", json!({"record_id":"one-real-run"}));
    f.invalid("cannot reuse");
}
#[test]
fn future_and_wrong_criterion_observations_are_rejected() {
    let mut f = Acceptance::new();
    f.observation("A4", json!({"observed_at":"9999-01-01T00:00:00Z"}));
    f.invalid("future");
    f.observation("A4", json!({"criterion_ids":["A20"]}));
    f.invalid("identify this criterion");
}
#[test]
fn public_markdown_must_match_machine_ledger() {
    let f = Acceptance::new();
    let path = f.root.join("docs/acceptance.md");
    fs::write(
        &path,
        fs::read_to_string(&path)
            .unwrap()
            .replace("| A4 ", "| A4 complete "),
    )
    .unwrap();
    let report = result(&cli("acceptance-check", &f.root, &[]));
    assert!(
        errors(&report)
            .iter()
            .any(|error| error.contains("differs"))
    );
}
#[test]
fn raw_control_snapshot_cannot_hide_under_public_record_name() {
    let mut f = Acceptance::new();
    write(
        &f.root,
        "docs/demos/ordinary-looking.json",
        "{\"schema\":\"tolight-control-snapshot/1\",\"agents\":[{\"owner_contact\":\"private\"}]}",
    );
    f.row("A7")["evidence"] = json!([{"kind":"local_record","path":"docs/demos/ordinary-looking.json","label":"not redacted"}]);
    f.invalid("private controls");
}
#[test]
fn all_complete_claims_still_require_actual_formal_release_readiness() {
    let mut f = Acceptance::new();
    for number in 1..=21 {
        let id = format!("A{number}");
        if !required(&id).is_empty() {
            f.observation(&id,json!({"process_manager":"synthetic-only","window_start":"2020-01-01T00:00:00Z","window_end":"2020-01-02T00:00:00Z"}));
        }
    }
    for row in f.data["criteria"].as_array_mut().unwrap() {
        row["local_status"] = json!("verified");
        row["final_status"] = json!("complete");
        row["remaining"] = json!([]);
    }
    toml(&f.root, "docs/acceptance-status.toml", &f.data);
    // Render disables the presentation comparison, so this reaches the
    // independent release-readiness check even for an invalid complete claim.
    let output = cli("acceptance-check", &f.root, &["--render"]);
    let report = result(&output);
    assert!(!output.status.success());
    assert_eq!(report["first_release_complete"], false);
    assert!(
        errors(&report)
            .iter()
            .any(|error| error.contains("current formal release readiness")),
        "{report}"
    );
    assert!(report["release_readiness"].is_object());
}
