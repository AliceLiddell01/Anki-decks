//! Контракт semantic triage на синтетическом Git-снимке без данных decks/.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::{TempDir, parse_json, run_cli_in};
use serde_json::{Value, json};

struct Fixture {
    repository: TempDir,
    artifacts: TempDir,
    pack: PathBuf,
    triage: PathBuf,
    initial: Value,
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Git: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn success(result: (i32, String, String)) -> String {
    let (code, stdout, stderr) = result;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    stdout
}

fn failure(result: (i32, String, String), expected_code: &str) {
    let (code, stdout, stderr) = result;
    assert_ne!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["error"]["code"], expected_code);
}

impl Fixture {
    fn new() -> Self {
        let repository = TempDir::new("semantic-triage-repository");
        let artifacts = TempDir::new("semantic-triage-artifacts");
        let root = repository.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "contract@example.invalid"]);
        git(root, &["config", "user.name", "Contract Test"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        git(root, &["config", "core.autocrlf", "false"]);
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"triage_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n").unwrap();
        fs::write(
            root.join("Cargo.lock"),
            "version = 3\n[[package]]\nname = \"triage_fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n").unwrap();
        git(root, &["add", "--all"]);
        git(root, &["commit", "-qm", "Базовое состояние"]);
        let base = git(root, &["rev-parse", "HEAD"]);
        fs::write(
            root.join("src/lib.rs"),
            "// Explain the changed return value to the reader.\npub fn value() -> u8 { 2 }\n",
        )
        .unwrap();
        git(root, &["add", "--all"]);
        git(root, &["commit", "-qm", "Изменение функции"]);
        let out = artifacts.path().join("evidence");
        success(run_cli_in(
            Some(root),
            &[
                "--json",
                "code-review",
                "collect",
                "--base",
                &base,
                "--head",
                "HEAD",
                "--out-dir",
                out.to_str().unwrap(),
            ],
        ));
        let pack = out.join("review.json");
        let mut evidence = read_json(&pack);
        assert!(
            !evidence["language"]["candidates"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        // Добавляем синтетические static signals, не фиксируя эвристики collector.
        evidence["candidates"] = json!(
            (0..3)
                .map(|index| json!({
                    "id": format!("synthetic-static-{index}"),
                    "detector": "synthetic_contract_signal", "path": "src/lib.rs",
                    "line": 2, "column": 1, "snippet": "pub fn value() -> u8 { 2 }",
                    "origin": "introduced_or_changed", "signals": ["contract_fixture"],
                    "source": "synthetic_test", "metadata": {}
                }))
                .collect::<Vec<_>>()
        );
        write_json(&pack, &evidence);
        let triage = artifacts.path().join("semantic-triage.json");
        success(run_cli_in(
            Some(root),
            &[
                "code-review",
                "triage",
                "init",
                "--pack",
                pack.to_str().unwrap(),
                "--out",
                triage.to_str().unwrap(),
            ],
        ));
        let initial = read_json(&triage);
        Self {
            repository,
            artifacts,
            pack,
            triage,
            initial,
        }
    }

    fn validate(&self, value: &Value) -> (i32, String, String) {
        write_json(&self.triage, value);
        self.run(&[
            "--json",
            "code-review",
            "triage",
            "validate",
            "--pack",
            self.pack.to_str().unwrap(),
            "--triage",
            self.triage.to_str().unwrap(),
        ])
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        run_cli_in(Some(self.repository.path()), args)
    }

    fn ids(&self) -> Vec<String> {
        self.initial["unreviewed_candidate_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect()
    }

    fn reviewed(&self) -> Value {
        let mut triage = self.initial.clone();
        let ids = self.ids();
        triage["unreviewed_candidate_ids"] = json!([]);
        triage["individual_decisions"] = json!([{
            "candidate_id": ids[0], "disposition": "confirmed", "reason_code": "other",
            "explanation": "Сигнал подтверждён проверкой вызывающего кода.", "finding_ids": ["finding-direct"]
        }]);
        triage["group_decisions"] = json!([{
            "id": "reviewed-group", "candidate_ids": ids[1..],
            "representative_candidate_ids": [ids[1]],
            "disposition": "acceptable", "reason_code": "test_fixture",
            "explanation": "Группа состоит из намеренно введённых тестовых свидетельств.", "finding_ids": []
        }]);
        triage["findings"] = json!([
            {"id": "finding-direct", "severity": "major", "title": "Подтверждённый дефект",
             "description": "Изменение результата нарушает контракт вызывающего кода.",
             "provenance": "direct_candidate", "candidate_ids": [ids[0]]},
            {"id": "finding-independent", "severity": "minor", "title": "Независимый дефект",
             "description": "Ручное чтение кода обнаружило отдельный дефект обработки ошибки.",
             "provenance": "independent", "candidate_ids": []}
        ]);
        triage
    }
}

#[test]
fn init_preserves_explicit_unreviewed_and_unified_candidate_space() {
    let fixture = Fixture::new();
    let evidence = read_json(&fixture.pack);
    let mut expected = evidence["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .chain(evidence["language"]["candidates"].as_array().unwrap())
        .map(|candidate| candidate["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    expected.sort();
    let mut actual = fixture.ids();
    actual.sort();
    assert_eq!(actual, expected);
    assert_eq!(
        fixture.initial["source"]["candidate_count"],
        json!(actual.len())
    );
    for field in ["individual_decisions", "group_decisions", "findings"] {
        assert!(fixture.initial[field].as_array().unwrap().is_empty());
    }
    success(fixture.validate(&fixture.initial));
    let bytes = fs::read(&fixture.triage).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(fixture.repository.path().to_str().unwrap()));
    assert!(!String::from_utf8_lossy(&bytes).contains(fixture.artifacts.path().to_str().unwrap()));
}

#[test]
fn individual_group_and_independent_findings_round_trip_deterministically() {
    let fixture = Fixture::new();
    success(fixture.validate(&fixture.reviewed()));
    let canonical = fixture.artifacts.path().join("canonical.json");
    let arguments = [
        "code-review",
        "triage",
        "validate",
        "--pack",
        fixture.pack.to_str().unwrap(),
        "--triage",
        fixture.triage.to_str().unwrap(),
        "--canonical-out",
        canonical.to_str().unwrap(),
    ];
    success(fixture.run(&arguments));
    let first = fs::read(&canonical).unwrap();
    let mut reordered = fixture.reviewed();
    reordered["group_decisions"][0]["candidate_ids"]
        .as_array_mut()
        .unwrap()
        .reverse();
    reordered["findings"].as_array_mut().unwrap().reverse();
    write_json(&fixture.triage, &reordered);
    success(fixture.run(&arguments));
    assert_eq!(fs::read(&canonical).unwrap(), first);
    let summary = parse_json(&success(fixture.run(&[
        "--json",
        "code-review",
        "triage",
        "summary",
        "--pack",
        fixture.pack.to_str().unwrap(),
        "--triage",
        canonical.to_str().unwrap(),
    ])));
    assert_eq!(
        summary["result"]["total_candidates"],
        fixture.initial["source"]["candidate_count"]
    );
    assert_eq!(
        summary["result"]["reviewed_candidates"],
        fixture.initial["source"]["candidate_count"]
    );
    assert_eq!(summary["result"]["unreviewed_candidates"], 0);
    assert_eq!(summary["result"]["individual_review"]["decision_count"], 1);
    assert_eq!(
        summary["result"]["group_review"]["decisions"]["decision_count"],
        1
    );
    assert_eq!(
        summary["result"]["group_review"]["covered_candidate_ids"],
        json!(fixture.ids().len() - 1)
    );
    assert_eq!(
        summary["result"]["group_review"]["representative_candidate_ids"],
        1
    );
    assert_eq!(
        summary["result"]["findings"]["by_provenance"]["direct_candidate"],
        1
    );
    assert_eq!(
        summary["result"]["findings"]["by_provenance"]["independent"],
        1
    );
    let report = fixture.artifacts.path().join("report.md");
    let arguments = [
        "code-review",
        "triage",
        "report",
        "--pack",
        fixture.pack.to_str().unwrap(),
        "--triage",
        canonical.to_str().unwrap(),
        "--out",
        report.to_str().unwrap(),
    ];
    success(fixture.run(&arguments));
    let first = fs::read(&report).unwrap();
    success(fixture.run(&arguments));
    assert_eq!(fs::read(&report).unwrap(), first);
    let text = String::from_utf8(first).unwrap();
    assert!(text.contains("Подтверждённый дефект"));
    assert!(text.contains("Независимый дефект"));
    assert!(!text.contains(fixture.repository.path().to_str().unwrap()));
}

#[test]
fn rejects_incompatible_source_identity_and_unaccounted_candidate() {
    let fixture = Fixture::new();
    for field in ["repository_id", "base_sha", "head_sha", "merge_base_sha"] {
        let mut triage = fixture.initial.clone();
        triage["source"]["snapshot"][field] = json!("incompatible-snapshot");
        failure(fixture.validate(&triage), "baseline_mismatch");
    }
    let mut triage = fixture.initial.clone();
    triage["source"]["review_pack_sha256"] = json!("0".repeat(64));
    failure(fixture.validate(&triage), "review_artifact_invalid");
    triage = fixture.initial.clone();
    triage["schema_version"] = json!(u32::MAX);
    failure(fixture.validate(&triage), "review_artifact_invalid");
    triage = fixture.initial.clone();
    triage["source"]["candidate_count"] = json!(0);
    failure(fixture.validate(&triage), "review_artifact_invalid");
    triage = fixture.initial.clone();
    triage["unreviewed_candidate_ids"]
        .as_array_mut()
        .unwrap()
        .pop();
    failure(fixture.validate(&triage), "review_artifact_invalid");
    let mut evidence = read_json(&fixture.pack);
    evidence["candidates"][0]["snippet"] = json!("changed immutable evidence");
    write_json(&fixture.pack, &evidence);
    failure(
        fixture.validate(&fixture.initial),
        "review_artifact_invalid",
    );
}

#[test]
fn rejects_unknown_duplicate_conflicting_and_inconsistent_links() {
    let fixture = Fixture::new();
    let valid = fixture.reviewed();
    let cases: &[fn(&mut Value)] = &[
        |v| v["individual_decisions"][0]["candidate_id"] = json!("unknown-candidate"),
        |v| v["individual_decisions"][0]["finding_ids"] = json!(["unknown-finding"]),
        |v| v["individual_decisions"][0]["finding_ids"] = json!([]),
        // Прямое подтверждение finding требует подтверждённого disposition.
        |v| v["individual_decisions"][0]["disposition"] = json!("acceptable"),
        |v| v["findings"][0]["candidate_ids"] = json!([]),
        |v| v["findings"][0]["candidate_ids"] = json!(["unknown-candidate"]),
        |v| {
            let duplicate = v["findings"][0].clone();
            v["findings"].as_array_mut().unwrap().push(duplicate);
        },
        |v| {
            let duplicate = v["individual_decisions"][0].clone();
            v["individual_decisions"]
                .as_array_mut()
                .unwrap()
                .push(duplicate);
        },
        |v| {
            let id = v["individual_decisions"][0]["candidate_id"].clone();
            v["group_decisions"][0]["candidate_ids"]
                .as_array_mut()
                .unwrap()
                .push(id);
        },
        |v| {
            let id = v["individual_decisions"][0]["candidate_id"].clone();
            v["unreviewed_candidate_ids"] = json!([id]);
        },
        |v| v["group_decisions"][0]["representative_candidate_ids"] = json!(["unknown-candidate"]),
        |v| v["group_decisions"][0]["representative_candidate_ids"] = json!([]),
        |v| v["group_decisions"][0]["candidate_ids"] = json!([]),
        |v| {
            let id = v["group_decisions"][0]["candidate_ids"][0].clone();
            v["group_decisions"][0]["candidate_ids"] = json!([id]);
        },
        |v| {
            let id = v["individual_decisions"][0]["candidate_id"].clone();
            v["group_decisions"][0]["representative_candidate_ids"] = json!([id]);
        },
        |v| {
            let duplicate = v["group_decisions"][0].clone();
            v["group_decisions"].as_array_mut().unwrap().push(duplicate);
        },
        |v| {
            let id = v["group_decisions"][0]["candidate_ids"][0].clone();
            v["group_decisions"][0]["candidate_ids"]
                .as_array_mut()
                .unwrap()
                .push(id);
        },
        |v| v["individual_decisions"][0]["explanation"] = json!("   "),
        |v| v["individual_decisions"][0]["reason_code"] = json!("unknown-reason"),
        |v| v["source"]["snapshot"]["unexpected"] = json!("must be rejected"),
    ];
    for (index, mutate) in cases.iter().enumerate() {
        let mut invalid = valid.clone();
        mutate(&mut invalid);
        let (code, stdout, stderr) = fixture.validate(&invalid);
        assert_ne!(
            code, 0,
            "некорректный artifact #{index}: {stdout}\n{stderr}"
        );
    }
}

#[test]
fn detector_assistance_remains_distinct_and_partial_review_stays_explicit() {
    let fixture = Fixture::new();
    let mut triage = fixture.reviewed();
    let unreviewed = triage["group_decisions"][0]["candidate_ids"]
        .as_array_mut()
        .unwrap()
        .pop()
        .unwrap();
    triage["unreviewed_candidate_ids"] = json!([unreviewed]);
    triage["findings"][0]["provenance"] = json!("candidate_assisted");
    triage["individual_decisions"][0]["disposition"] = json!("acceptable");
    success(fixture.validate(&triage));
    let stored = read_json(&fixture.triage);
    assert_eq!(stored["findings"][0]["provenance"], "candidate_assisted");
    assert_eq!(
        stored["unreviewed_candidate_ids"].as_array().unwrap().len(),
        1
    );
    let summary = parse_json(&success(fixture.run(&[
        "--json",
        "code-review",
        "triage",
        "summary",
        "--pack",
        fixture.pack.to_str().unwrap(),
        "--triage",
        fixture.triage.to_str().unwrap(),
    ])));
    assert_eq!(summary["result"]["unreviewed_candidates"], 1);
    assert_eq!(
        summary["result"]["findings"]["by_provenance"]["candidate_assisted"],
        1
    );
    assert_eq!(
        summary["result"]["findings"]["by_provenance"]
            .get("direct_candidate")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        0
    );
    assert_eq!(
        summary["result"]["reviewed_candidates"],
        json!(fixture.ids().len() - 1)
    );
}

#[test]
fn artifact_can_be_relocated_and_used_from_a_clone() {
    let fixture = Fixture::new();
    success(fixture.validate(&fixture.reviewed()));
    let relocated = TempDir::new("semantic-triage-relocated");
    let pack = relocated.path().join("downloaded-review.json");
    let triage = relocated.path().join("downloaded-triage.json");
    fs::copy(&fixture.pack, &pack).unwrap();
    fs::copy(&fixture.triage, &triage).unwrap();
    let clone = TempDir::new("semantic-triage-clone");
    git(
        clone.path(),
        &[
            "clone",
            "--quiet",
            fixture.repository.path().to_str().unwrap(),
            ".",
        ],
    );
    success(run_cli_in(
        Some(clone.path()),
        &[
            "code-review",
            "triage",
            "validate",
            "--pack",
            pack.to_str().unwrap(),
            "--triage",
            triage.to_str().unwrap(),
        ],
    ));
    assert_eq!(
        fs::read(&triage).unwrap(),
        fs::read(&fixture.triage).unwrap()
    );
}

#[test]
fn grouped_report_does_not_dump_every_candidate() {
    let mut fixture = Fixture::new();
    let mut pack = read_json(&fixture.pack);
    let prototype = pack["candidates"][0].clone();
    for index in 0..512 {
        let mut candidate = prototype.clone();
        candidate["id"] = json!(format!("large-group-member-{index}"));
        pack["candidates"].as_array_mut().unwrap().push(candidate);
    }
    write_json(&fixture.pack, &pack);
    let large_triage = fixture.artifacts.path().join("large-triage.json");
    success(fixture.run(&[
        "code-review",
        "triage",
        "init",
        "--pack",
        fixture.pack.to_str().unwrap(),
        "--out",
        large_triage.to_str().unwrap(),
    ]));
    fixture.triage = large_triage;
    fixture.initial = read_json(&fixture.triage);
    success(fixture.validate(&fixture.reviewed()));
    let report = fixture.artifacts.path().join("large-report.md");
    success(fixture.run(&[
        "code-review",
        "triage",
        "report",
        "--pack",
        fixture.pack.to_str().unwrap(),
        "--triage",
        fixture.triage.to_str().unwrap(),
        "--out",
        report.to_str().unwrap(),
    ]));
    let text = fs::read_to_string(report).unwrap();
    assert!(text.contains("Подтверждённый дефект"));
    assert!(
        text.len() < 15_000,
        "Markdown должен оставаться кратким при большом group review"
    );
    assert!(
        text.matches("large-group-member-").count() < 32,
        "group review не должен превращаться в dump IDs"
    );
}
