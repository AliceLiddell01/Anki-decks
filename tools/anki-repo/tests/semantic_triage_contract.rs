//! Контракт semantic triage на синтетическом Git-снимке без данных decks/.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::{TempDir, parse_json, run_cli_in};
use serde_json::{Value, json};

struct Fixture {
    repository: TempDir,
    artifacts: PathBuf,
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
    assert_eq!(
        parse_json(&stdout)["error"]["code"],
        expected_code,
        "stdout: {stdout}\nstderr: {stderr}"
    );
}

impl Fixture {
    fn new() -> Self {
        let repository = TempDir::new("semantic-triage-repository");
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
        let head = git(root, &["rev-parse", "HEAD"]);
        let out = root.join(".anki-repo/review/local").join(head);
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
        let triage = out.join("semantic-triage.input.json");
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
            artifacts: out,
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

    fn canonicalize(&self, output: &Path) -> (i32, String, String) {
        self.run(&[
            "--json",
            "code-review",
            "triage",
            "validate",
            "--pack",
            self.pack.to_str().unwrap(),
            "--triage",
            self.triage.to_str().unwrap(),
            "--canonical-out",
            output.to_str().unwrap(),
        ])
    }

    fn report(&self, triage: &Path, output: &Path) -> (i32, String, String) {
        self.run(&[
            "--json",
            "code-review",
            "triage",
            "report",
            "--pack",
            self.pack.to_str().unwrap(),
            "--triage",
            triage.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
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
    assert!(
        !String::from_utf8_lossy(&bytes).contains(fixture.artifacts.as_path().to_str().unwrap())
    );
}

#[test]
fn init_is_idempotent_and_preserves_edited_decisions() {
    let fixture = Fixture::new();
    let arguments = [
        "--json",
        "code-review",
        "triage",
        "init",
        "--pack",
        fixture.pack.to_str().unwrap(),
        "--out",
        fixture.triage.to_str().unwrap(),
    ];
    let initial_bytes = fs::read(&fixture.triage).unwrap();
    success(fixture.run(&arguments));
    assert_eq!(fs::read(&fixture.triage).unwrap(), initial_bytes);
    success(fixture.validate(&fixture.reviewed()));
    let edited_bytes = fs::read(&fixture.triage).unwrap();
    failure(fixture.run(&arguments), "review_artifact_conflict");
    assert_eq!(fs::read(&fixture.triage).unwrap(), edited_bytes);
}

#[test]
fn derived_outputs_refresh_after_decision_changes_and_remain_idempotent() {
    let fixture = Fixture::new();
    let canonical = fixture.artifacts.as_path().join("semantic-triage.json");
    let report = fixture.artifacts.as_path().join("review-report.md");
    let evidence_bytes = fs::read(&fixture.pack).unwrap();
    let first_triage = fixture.reviewed();
    write_json(&fixture.triage, &first_triage);
    success(fixture.canonicalize(&canonical));
    assert_eq!(read_json(&canonical), first_triage);
    success(fixture.report(&canonical, &report));
    let first_canonical_bytes = fs::read(&canonical).unwrap();
    let first_report_bytes = fs::read(&report).unwrap();

    let mut updated = first_triage;
    updated["individual_decisions"][0]["disposition"] = json!("acceptable");
    updated["individual_decisions"][0]["explanation"] =
        json!("Проверка вызовов уточнила связь сигнала с дефектом.");
    updated["findings"][0]["provenance"] = json!("candidate_assisted");
    updated["findings"][0]["severity"] = json!("minor");
    updated["findings"][0]["title"] = json!("Уточнённый дефект");
    write_json(&fixture.triage, &updated);
    let editable_bytes = fs::read(&fixture.triage).unwrap();
    success(fixture.canonicalize(&canonical));
    assert_eq!(read_json(&canonical), updated);
    let updated_canonical_bytes = fs::read(&canonical).unwrap();
    assert_ne!(updated_canonical_bytes, first_canonical_bytes);
    success(fixture.report(&canonical, &report));
    let updated_report_bytes = fs::read(&report).unwrap();
    assert_ne!(updated_report_bytes, first_report_bytes);
    let report_text = String::from_utf8(updated_report_bytes.clone()).unwrap();
    assert!(report_text.contains("Уточнённый дефект"));
    assert!(report_text.contains("candidate_assisted"));
    assert!(!report_text.contains("Подтверждённый дефект"));

    success(fixture.canonicalize(&canonical));
    success(fixture.report(&canonical, &report));
    assert_eq!(fs::read(&canonical).unwrap(), updated_canonical_bytes);
    assert_eq!(fs::read(&report).unwrap(), updated_report_bytes);
    assert_eq!(fs::read(&fixture.triage).unwrap(), editable_bytes);
    assert_eq!(fs::read(&fixture.pack).unwrap(), evidence_bytes);

    // Провал текущей проверки не должен менять уже существующие производные файлы.
    updated["individual_decisions"][0]["finding_ids"] = json!(["unknown-finding"]);
    write_json(&fixture.triage, &updated);
    failure(fixture.canonicalize(&canonical), "review_artifact_invalid");
    failure(
        fixture.report(&fixture.triage, &report),
        "review_artifact_invalid",
    );
    assert_eq!(fs::read(&canonical).unwrap(), updated_canonical_bytes);
    assert_eq!(fs::read(&report).unwrap(), updated_report_bytes);
}

#[test]
fn derived_overwrite_rejects_source_collisions_and_unsafe_existing_paths() {
    let fixture = Fixture::new();
    success(fixture.validate(&fixture.reviewed()));
    for source in [&fixture.pack, &fixture.triage] {
        let before = fs::read(source).unwrap();
        failure(fixture.canonicalize(source), "invalid_request");
        failure(fixture.report(&fixture.triage, source), "invalid_request");
        assert_eq!(fs::read(source).unwrap(), before);
    }

    let directory = fixture.artifacts.as_path().join("existing-directory");
    fs::create_dir(&directory).unwrap();
    failure(fixture.canonicalize(&directory), "invalid_request");
    failure(
        fixture.report(&fixture.triage, &directory),
        "invalid_request",
    );
    assert!(directory.is_dir());
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);

    let decks = fixture.repository.path().join("decks");
    fs::create_dir(&decks).unwrap();
    fs::create_dir(decks.join("nested")).unwrap();
    let protected_paths = [
        decks.join("existing-output"),
        fixture.repository.path().join(".git/existing-output"),
        decks.join("nested/../existing-output"),
    ];
    for output in protected_paths {
        fs::write(&output, b"protected bytes").unwrap();
        failure(fixture.canonicalize(&output), "invalid_request");
        failure(fixture.report(&fixture.triage, &output), "invalid_request");
        assert_eq!(fs::read(&output).unwrap(), b"protected bytes");
    }
}

#[cfg(unix)]
#[test]
fn derived_overwrite_rejects_live_dangling_and_protected_parent_symlinks() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    success(fixture.validate(&fixture.reviewed()));
    let target = fixture.artifacts.as_path().join("protected-target");
    fs::write(&target, b"protected bytes").unwrap();
    let missing = fixture.artifacts.as_path().join("missing-target");
    for (name, target) in [("live-link", &target), ("dangling-link", &missing)] {
        let output = fixture.artifacts.as_path().join(name);
        symlink(target, &output).unwrap();
        failure(fixture.canonicalize(&output), "invalid_request");
        failure(fixture.report(&fixture.triage, &output), "invalid_request");
        assert_eq!(fs::read_link(&output).unwrap(), *target);
    }
    assert_eq!(fs::read(&target).unwrap(), b"protected bytes");
    assert!(!missing.exists());

    let decks = fixture.repository.path().join("decks");
    fs::create_dir(&decks).unwrap();
    for (name, parent) in [
        ("decks-alias", decks),
        ("metadata-alias", fixture.repository.path().join(".git")),
    ] {
        let target = parent.join("existing-output");
        fs::write(&target, b"protected bytes").unwrap();
        let alias = fixture.artifacts.as_path().join(name);
        symlink(parent, &alias).unwrap();
        let output = alias.join("existing-output");
        failure(fixture.canonicalize(&output), "invalid_request");
        failure(fixture.report(&fixture.triage, &output), "invalid_request");
        assert_eq!(fs::read(target).unwrap(), b"protected bytes");
    }

    let safe_parent = fixture.artifacts.as_path().join("safe-parent");
    fs::create_dir(&safe_parent).unwrap();
    let safe_alias = fixture.artifacts.as_path().join("safe-alias");
    symlink(&safe_parent, &safe_alias).unwrap();
    let output = safe_alias.join("nested/canonical.json");
    failure(fixture.canonicalize(&output), "invalid_request");
    assert!(!safe_parent.join("nested/canonical.json").exists());
}

#[test]
fn individual_group_and_independent_findings_round_trip_deterministically() {
    let fixture = Fixture::new();
    success(fixture.validate(&fixture.reviewed()));
    let canonical = fixture.artifacts.as_path().join("semantic-triage.json");
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
    let report = fixture.artifacts.as_path().join("review-report.md");
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
    // Нарушения структуры, покрытия и связей принадлежат review_artifact_invalid.
    // Несовместимость снимка проверяется отдельно как baseline_mismatch.
    type InvalidCase = (fn(&mut Value), &'static str);
    let cases: &[InvalidCase] = &[
        (
            |v| v["individual_decisions"][0]["candidate_id"] = json!("unknown-candidate"),
            "review_artifact_invalid",
        ),
        (
            |v| v["individual_decisions"][0]["finding_ids"] = json!(["unknown-finding"]),
            "review_artifact_invalid",
        ),
        (
            |v| v["individual_decisions"][0]["finding_ids"] = json!([]),
            "review_artifact_invalid",
        ),
        // Прямое подтверждение finding требует подтверждённого disposition.
        (
            |v| v["individual_decisions"][0]["disposition"] = json!("acceptable"),
            "review_artifact_invalid",
        ),
        (
            |v| v["findings"][0]["candidate_ids"] = json!([]),
            "review_artifact_invalid",
        ),
        (
            |v| v["findings"][0]["candidate_ids"] = json!(["unknown-candidate"]),
            "review_artifact_invalid",
        ),
        (
            |v| {
                let duplicate = v["findings"][0].clone();
                v["findings"].as_array_mut().unwrap().push(duplicate);
            },
            "review_artifact_invalid",
        ),
        (
            |v| {
                let duplicate = v["individual_decisions"][0].clone();
                v["individual_decisions"]
                    .as_array_mut()
                    .unwrap()
                    .push(duplicate);
            },
            "review_artifact_invalid",
        ),
        (
            |v| {
                let id = v["individual_decisions"][0]["candidate_id"].clone();
                v["group_decisions"][0]["candidate_ids"]
                    .as_array_mut()
                    .unwrap()
                    .push(id);
            },
            "review_artifact_invalid",
        ),
        (
            |v| {
                let id = v["individual_decisions"][0]["candidate_id"].clone();
                v["unreviewed_candidate_ids"] = json!([id]);
            },
            "review_artifact_invalid",
        ),
        (
            |v| {
                v["group_decisions"][0]["representative_candidate_ids"] =
                    json!(["unknown-candidate"])
            },
            "review_artifact_invalid",
        ),
        (
            |v| v["group_decisions"][0]["representative_candidate_ids"] = json!([]),
            "review_artifact_invalid",
        ),
        (
            |v| v["group_decisions"][0]["candidate_ids"] = json!([]),
            "review_artifact_invalid",
        ),
        (
            |v| {
                let id = v["group_decisions"][0]["candidate_ids"][0].clone();
                v["group_decisions"][0]["candidate_ids"] = json!([id]);
            },
            "review_artifact_invalid",
        ),
        (
            |v| {
                let id = v["individual_decisions"][0]["candidate_id"].clone();
                v["group_decisions"][0]["representative_candidate_ids"] = json!([id]);
            },
            "review_artifact_invalid",
        ),
        (
            |v| {
                let duplicate = v["group_decisions"][0].clone();
                v["group_decisions"].as_array_mut().unwrap().push(duplicate);
            },
            "review_artifact_invalid",
        ),
        (
            |v| {
                let id = v["group_decisions"][0]["candidate_ids"][0].clone();
                v["group_decisions"][0]["candidate_ids"]
                    .as_array_mut()
                    .unwrap()
                    .push(id);
            },
            "review_artifact_invalid",
        ),
        (
            |v| v["individual_decisions"][0]["explanation"] = json!("   "),
            "review_artifact_invalid",
        ),
        (
            |v| v["individual_decisions"][0]["reason_code"] = json!("unknown-reason"),
            "review_artifact_invalid",
        ),
        (
            |v| v["source"]["snapshot"]["unexpected"] = json!("must be rejected"),
            "review_artifact_invalid",
        ),
    ];
    for (mutate, expected_code) in cases {
        let mut invalid = valid.clone();
        mutate(&mut invalid);
        failure(fixture.validate(&invalid), expected_code);
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
fn validation_without_output_is_portable_and_keeps_typed_mismatches() {
    let fixture = Fixture::new();
    let original = parse_json(&success(fixture.validate(&fixture.reviewed())));
    let outside = TempDir::new("semantic-triage-outside-git");
    let pack = outside.path().join("review.json");
    let triage = outside.path().join("semantic-triage.json");
    fs::copy(&fixture.pack, &pack).unwrap();
    fs::copy(&fixture.triage, &triage).unwrap();
    assert!(
        !Command::new("git")
            .current_dir(outside.path())
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let arguments = [
        "--json",
        "code-review",
        "triage",
        "validate",
        "--pack",
        pack.to_str().unwrap(),
        "--triage",
        triage.to_str().unwrap(),
    ];
    let result = parse_json(&success(run_cli_in(Some(outside.path()), &arguments)));
    assert_eq!(result["result"], original["result"]);
    let original_triage_bytes = fs::read(&triage).unwrap();
    let mut incompatible_snapshot = read_json(&triage);
    incompatible_snapshot["source"]["snapshot"]["head_sha"] = json!("incompatible-snapshot");
    write_json(&triage, &incompatible_snapshot);
    failure(
        run_cli_in(Some(outside.path()), &arguments),
        "baseline_mismatch",
    );
    fs::write(&triage, &original_triage_bytes).unwrap();

    // Семантически тот же JSON с другими байтами всё равно имеет другой SHA-256.
    let mut changed_pack_bytes = fs::read(&pack).unwrap();
    changed_pack_bytes.push(b'\n');
    fs::write(&pack, changed_pack_bytes).unwrap();
    failure(
        run_cli_in(Some(outside.path()), &arguments),
        "review_artifact_invalid",
    );
    assert_eq!(fs::read(triage).unwrap(), original_triage_bytes);
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
    let large_triage = fixture
        .artifacts
        .as_path()
        .join("semantic-triage.input.json");
    fs::remove_file(&large_triage).unwrap();
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
    let report = fixture.artifacts.as_path().join("review-report.md");
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

#[test]
fn triage_writes_reject_arbitrary_names_external_targets_and_tracked_files() {
    let fixture = Fixture::new();
    success(fixture.validate(&fixture.reviewed()));
    let outside = TempDir::new("triage-write-boundary-outside");
    let cargo = fixture.repository.path().join("Cargo.toml");
    let cargo_bytes = fs::read(&cargo).unwrap();
    let arbitrary = fixture.artifacts.join("unexpected.json");
    let external = outside.path().join("semantic-triage.json");
    let pack_bytes = fs::read(&fixture.pack).unwrap();
    let input_bytes = fs::read(&fixture.triage).unwrap();
    for output in [&cargo, &arbitrary, &external] {
        failure(fixture.canonicalize(output), "invalid_request");
        failure(fixture.report(&fixture.triage, output), "invalid_request");
        failure(
            fixture.run(&[
                "--json",
                "code-review",
                "triage",
                "init",
                "--pack",
                fixture.pack.to_str().unwrap(),
                "--out",
                output.to_str().unwrap(),
            ]),
            "invalid_request",
        );
        assert_eq!(fs::read(&cargo).unwrap(), cargo_bytes);
        assert_eq!(fs::read(&fixture.pack).unwrap(), pack_bytes);
        assert_eq!(fs::read(&fixture.triage).unwrap(), input_bytes);
        assert!(!arbitrary.exists());
        assert!(!external.exists());
    }
}

#[test]
fn derived_outputs_reject_unproven_existing_artifacts_and_tracked_overwrite() {
    let fixture = Fixture::new();
    success(fixture.validate(&fixture.reviewed()));
    let canonical = fixture.artifacts.join("semantic-triage.json");
    let report = fixture.artifacts.join("review-report.md");
    for output in [&canonical, &report] {
        fs::write(output, b"unrelated existing bytes").unwrap();
    }
    failure(fixture.canonicalize(&canonical), "review_artifact_conflict");
    failure(
        fixture.report(&fixture.triage, &report),
        "review_artifact_conflict",
    );
    for output in [&canonical, &report] {
        assert_eq!(fs::read(output).unwrap(), b"unrelated existing bytes");
        fs::remove_file(output).unwrap();
    }
    success(fixture.canonicalize(&canonical));
    success(fixture.report(&canonical, &report));
    // Даже собственный валидный artifact нельзя обновлять после помещения в Git index.
    git(
        fixture.repository.path(),
        &[
            "add",
            "--",
            canonical.to_str().unwrap(),
            report.to_str().unwrap(),
        ],
    );
    let canonical_bytes = fs::read(&canonical).unwrap();
    let report_bytes = fs::read(&report).unwrap();
    let mut updated = fixture.reviewed();
    updated["findings"][0]["title"] = json!("Изменённый дефект");
    write_json(&fixture.triage, &updated);
    failure(fixture.canonicalize(&canonical), "invalid_request");
    failure(fixture.report(&fixture.triage, &report), "invalid_request");
    assert_eq!(fs::read(canonical).unwrap(), canonical_bytes);
    assert_eq!(fs::read(report).unwrap(), report_bytes);
}

#[test]
fn triage_external_pack_is_readable_but_cannot_authorize_artifact_writes() {
    let fixture = Fixture::new();
    success(fixture.validate(&fixture.reviewed()));
    let outside = TempDir::new("triage-external-pack-write");
    let pack = outside.path().join("review.json");
    fs::copy(&fixture.pack, &pack).unwrap();
    let output = fixture.artifacts.join("semantic-triage.json");
    failure(
        fixture.run(&[
            "--json",
            "code-review",
            "triage",
            "validate",
            "--pack",
            pack.to_str().unwrap(),
            "--triage",
            fixture.triage.to_str().unwrap(),
            "--canonical-out",
            output.to_str().unwrap(),
        ]),
        "invalid_request",
    );
    assert!(!output.exists());
    assert_eq!(fs::read(pack).unwrap(), fs::read(&fixture.pack).unwrap());
}

#[cfg(unix)]
#[test]
fn triage_rejects_symlink_at_canonical_artifact_without_changing_target() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    success(fixture.validate(&fixture.reviewed()));
    let outside = TempDir::new("triage-canonical-symlink-target");
    let target = outside.path().join("protected");
    fs::write(&target, b"protected bytes").unwrap();
    let canonical = fixture.artifacts.join("semantic-triage.json");
    let report = fixture.artifacts.join("review-report.md");
    symlink(&target, &canonical).unwrap();
    symlink(&target, &report).unwrap();
    failure(fixture.canonicalize(&canonical), "review_artifact_conflict");
    failure(
        fixture.report(&fixture.triage, &report),
        "review_artifact_conflict",
    );
    assert_eq!(fs::read(&target).unwrap(), b"protected bytes");
    assert_eq!(fs::read_link(canonical).unwrap(), target);
    assert_eq!(fs::read_link(report).unwrap(), target);
}
