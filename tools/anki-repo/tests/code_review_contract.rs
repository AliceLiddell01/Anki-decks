//! Сквозные проверки контрактов CLI на синтетических Git-репозиториях.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::common::{TempDir, cli_binary, parse_json, run_cli, run_cli_in};
use serde_json::{Value, json};
use sha2::Digest as _;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap_or_else(|_| panic!("не удалось запустить тестовую команду Git"));
    assert!(
        output.status.success(),
        "тестовая команда Git завершилась с кодом {:?}",
        output.status.code()
    );
    String::from_utf8(output.stdout)
        .expect("вывод синтетического Git-репозитория должен быть UTF-8")
        .trim()
        .to_owned()
}

fn init_repo(temp: &TempDir) {
    let root = temp.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "contract@example.invalid"]);
    git(root, &["config", "user.name", "Contract Test"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["config", "core.autocrlf", "false"]);
}

fn commit(root: &Path, message: &str) {
    git(root, &["add", "--", ".", ":!.anki-repo"]);
    git(root, &["commit", "-qm", message]);
}

fn review_workspace(root: &Path, head: &str) -> PathBuf {
    root.join(".anki-repo/review/local").join(head)
}

fn tracked_status(root: &Path) -> String {
    git(
        root,
        &["status", "--porcelain=v1", "--", ".", ":!.anki-repo"],
    )
}

fn collect_pack(root: &Path, base: &str, head: &str, output: &Path, run_clippy: bool) -> Value {
    let output_arg = output.to_string_lossy().into_owned();
    let mut args = vec![
        "--json",
        "code-review",
        "collect",
        "--base",
        base,
        "--head",
        head,
        "--out-dir",
        &output_arg,
    ];
    if run_clippy {
        args.push("--run-clippy");
    }
    // Даже при внешнем CARGO_TARGET_DIR сборка синтетического проекта остаётся временной.
    let result = Command::new(cli_binary())
        .current_dir(root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .args(args)
        .output()
        .unwrap();
    let stdout = String::from_utf8(result.stdout).unwrap();
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(
        result.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    parse_json(&stdout)["result"].clone()
}

fn write_cargo_project(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"review_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        root.join("Cargo.lock"),
        "version = 3\n\n[[package]]\nname = \"review_fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join(".gitignore"), "/target/\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n").unwrap();
}

#[test]
fn code_review_namespace_does_not_replace_card_review_commands() {
    let (code, help, stderr) = run_cli(&["code-review", "--help"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(help.contains("collect"));
    assert!(help.contains("verify"));
    assert!(help.contains("delta"));
    assert!(help.contains("queue"));

    for subcommand in ["collect", "verify"] {
        let (code, help, stderr) = run_cli(&["code-review", subcommand, "--help"]);
        assert_eq!(code, 0, "{stderr}");
        assert!(help.contains("--run-clippy"));
        assert!(help.contains("build.rs"));
        assert!(help.contains(".anki-repo/review/"), "{help}");
        assert!(help.contains("--pr-number"), "{help}");
        assert!(help.contains("local"), "{help}");
        assert!(!help.contains("--skip-clippy"));
        let (code, _, stderr) = run_cli(&["code-review", subcommand, "--skip-clippy"]);
        assert_eq!(code, 2, "{stderr}");
        assert!(stderr.contains("--skip-clippy"));
    }

    let (code, help, stderr) = run_cli(&["code-review", "execution", "prepare", "--help"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(help.contains(".anki-repo/review/"), "{help}");
    assert!(help.contains("--pr-number"), "{help}");
    assert!(help.contains("пространство"), "{help}");
    assert!(help.contains("наслед"), "{help}");

    for (args, expected) in [
        (&["delta", "--help"][..], "delta.json"),
        (
            &["triage", "init", "--help"][..],
            "semantic-triage.input.json",
        ),
        (
            &["triage", "validate", "--help"][..],
            "semantic-triage.json",
        ),
        (&["triage", "report", "--help"][..], "review-report.md"),
    ] {
        let mut command = vec!["code-review"];
        command.extend_from_slice(args);
        let (code, help, stderr) = run_cli(&command);
        assert_eq!(code, 0, "{stderr}");
        assert!(help.contains(".anki-repo/review/"), "{help}");
        assert!(help.contains(expected), "{help}");
    }

    let (code, help, stderr) = run_cli(&["review", "--help"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(help.contains("--all"));
    assert!(help.contains("--qa-code"));

    let (code, help, stderr) = run_cli(&["review-check", "--help"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(help.contains("--proposals"));
}

#[test]
fn collect_verify_and_delta_keep_sha_identity_and_visible_artifacts() {
    let temp = TempDir::new("code-review-verify");
    init_repo(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n",
    )
    .unwrap();
    commit(temp.path(), "база");
    git(temp.path(), &["branch", "synthetic-base"]);
    let base_sha = git(temp.path(), &["rev-parse", "HEAD"]);

    fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn value() -> u8 { panic!(\"Human message\"); }\n",
    )
    .unwrap();
    commit(temp.path(), "добавить кандидатов");
    git(temp.path(), &["branch", "synthetic-review"]);
    let reviewed_sha = git(temp.path(), &["rev-parse", "HEAD"]);
    let baseline_dir = review_workspace(temp.path(), &reviewed_sha);
    let baseline_dir_arg = baseline_dir.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_cli_in(
        Some(temp.path()),
        &[
            "--json",
            "code-review",
            "collect",
            "--base",
            "synthetic-base",
            "--head",
            "synthetic-review",
            "--out-dir",
            &baseline_dir_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let result = parse_json(&stdout)["result"].clone();
    assert_eq!(result["target"]["base_sha"], json!(base_sha));
    assert_eq!(result["target"]["head_sha"], json!(reviewed_sha));
    assert_eq!(result["files"], json!(1));
    assert!(result["candidates"].as_u64().unwrap() >= 2);
    assert_eq!(result["tool_runs"][0]["status"], json!("skipped"));
    let baseline_path = baseline_dir.join("review.json");
    let baseline_bytes = fs::read(&baseline_path).unwrap();
    assert!(baseline_dir.join("review-queue.json").is_file());
    assert!(baseline_dir.join("review.txt").is_file());
    assert!(
        !String::from_utf8_lossy(&baseline_bytes)
            .contains(&temp.path().to_string_lossy().to_string())
    );
    let visible = git(
        temp.path(),
        &["status", "--porcelain=v1", "--", ".anki-repo/review/local"],
    );
    assert!(visible.contains(".anki-repo/review/local"));

    // Повтор записи того же снимка по тому же пути идемпотентен.
    let (code, _, stderr) = run_cli_in(
        Some(temp.path()),
        &[
            "code-review",
            "collect",
            "--base",
            "synthetic-base",
            "--head",
            "synthetic-review",
            "--out-dir",
            &baseline_dir_arg,
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(fs::read(&baseline_path).unwrap(), baseline_bytes);

    fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n",
    )
    .unwrap();
    commit(temp.path(), "убрать кандидата");
    git(temp.path(), &["branch", "synthetic-fixed"]);
    let fixed_sha = git(temp.path(), &["rev-parse", "HEAD"]);
    let verified_dir = review_workspace(temp.path(), &fixed_sha);
    let verified_dir_arg = verified_dir.to_string_lossy().into_owned();
    let baseline_path_arg = baseline_path.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_cli_in(
        Some(temp.path()),
        &[
            "--json",
            "code-review",
            "verify",
            "--baseline",
            &baseline_path_arg,
            "--head",
            "synthetic-fixed",
            "--out-dir",
            &verified_dir_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let verified = parse_json(&stdout)["result"].clone();
    assert!(verified_dir.join("review-queue.json").is_file());
    assert!(verified_dir.join("review.txt").is_file());
    assert_eq!(verified["target"]["base_sha"], json!(base_sha));
    assert_eq!(verified["target"]["head_sha"], json!(fixed_sha));
    assert!(
        verified["delta"]["candidate_status_counts"]["gone"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(verified["delta"].get("candidates").is_none());
    let saved_verify_delta: Value =
        serde_json::from_slice(&fs::read(verified_dir.join("delta.json")).unwrap()).unwrap();
    assert!(
        saved_verify_delta["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["status"] == "gone")
    );

    let new_pack = verified_dir.join("review.json");
    let new_pack_arg = new_pack.to_string_lossy().into_owned();
    let delta_path = verified_dir.join("delta.json");
    let delta_path_arg = delta_path.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_cli_in(
        Some(temp.path()),
        &[
            "--json",
            "code-review",
            "delta",
            "--before",
            &baseline_path_arg,
            "--after",
            &new_pack_arg,
            "--out",
            &delta_path_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        parse_json(&stdout)["result"]["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["status"] == "gone")
    );
    let saved_delta: Value = serde_json::from_slice(&fs::read(delta_path).unwrap()).unwrap();
    assert_eq!(saved_delta["schema_version"], json!(1));
}

#[test]
fn default_collect_does_not_execute_reviewed_build_script() {
    let repo = TempDir::new("collect-execution-boundary");
    let external = TempDir::new("build-script-external-marker");
    init_repo(&repo);
    write_cargo_project(repo.path());
    commit(repo.path(), "база");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    let marker = external.path().join("executed.marker");
    fs::write(
        repo.path().join("build.rs"),
        format!("fn main() {{ std::fs::write({marker:?}, b\"executed\").unwrap(); }}\n"),
    )
    .unwrap();
    commit(repo.path(), "добавить синтетический build.rs");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let status_before = tracked_status(repo.path());
    let index_before = git(repo.path(), &["ls-files", "--stage"]);

    let output = review_workspace(repo.path(), &head);
    let result = collect_pack(repo.path(), &base, &head, &output, false);
    assert_eq!(result["tool_runs"][0]["status"], "skipped");
    assert!(
        result["tool_runs"][0]["message"]
            .as_str()
            .unwrap()
            .contains("--run-clippy")
    );
    assert!(
        !marker.exists(),
        "build.rs не должен исполняться по умолчанию"
    );
    assert!(!repo.path().join("target").exists());
    assert_eq!(tracked_status(repo.path()), status_before);
    assert_eq!(git(repo.path(), &["ls-files", "--stage"]), index_before);
}

#[test]
fn collect_handles_file_directory_transitions_in_both_directions() {
    let repo = TempDir::new("collect-file-directory-transition");
    init_repo(&repo);
    fs::write(repo.path().join("foo"), "old file\nsecond line\n").unwrap();
    commit(repo.path(), "добавить файл");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);

    fs::remove_file(repo.path().join("foo")).unwrap();
    fs::create_dir_all(repo.path().join("foo")).unwrap();
    fs::write(repo.path().join("foo/child.rs"), "fn child() {}\n").unwrap();
    commit(repo.path(), "заменить файл каталогом");
    let directory_head = git(repo.path(), &["rev-parse", "HEAD"]);
    let forward_dir = review_workspace(repo.path(), &directory_head);
    collect_pack(repo.path(), &base, &directory_head, &forward_dir, false);
    let forward: Value =
        serde_json::from_slice(&fs::read(forward_dir.join("review.json")).unwrap()).unwrap();
    let files = forward["scope"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["path"], "foo");
    assert_eq!(files[0]["status"], "deleted");
    assert_eq!(files[0]["post_state"], "missing");
    assert_eq!(
        files[0]["base_changed_lines"][0],
        json!({"start": 1, "end": 3})
    );
    assert_eq!(files[1]["path"], "foo/child.rs");
    assert_eq!(files[1]["status"], "added");
    assert_eq!(files[1]["base_state"], "missing");
    assert_eq!(
        files[1]["post_changed_lines"][0],
        json!({"start": 1, "end": 2})
    );

    fs::remove_file(repo.path().join("foo/child.rs")).unwrap();
    fs::remove_dir(repo.path().join("foo")).unwrap();
    fs::write(repo.path().join("foo"), "replacement file\n").unwrap();
    commit(repo.path(), "заменить каталог файлом");
    let file_head = git(repo.path(), &["rev-parse", "HEAD"]);
    let reverse_dir = review_workspace(repo.path(), &file_head);
    collect_pack(
        repo.path(),
        &directory_head,
        &file_head,
        &reverse_dir,
        false,
    );
    let reverse: Value =
        serde_json::from_slice(&fs::read(reverse_dir.join("review.json")).unwrap()).unwrap();
    let files = reverse["scope"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["path"], "foo");
    assert_eq!(files[0]["status"], "added");
    assert_eq!(files[0]["base_state"], "missing");
    assert_eq!(
        files[0]["post_changed_lines"][0],
        json!({"start": 1, "end": 2})
    );
    assert_eq!(files[1]["path"], "foo/child.rs");
    assert_eq!(files[1]["status"], "deleted");
    assert_eq!(files[1]["post_state"], "missing");
    assert!(tracked_status(repo.path()).is_empty());
}

#[test]
fn explicit_real_clippy_collect_is_byte_stable_and_idempotent() {
    let repo = TempDir::new("collect-real-clippy");
    init_repo(&repo);
    // Это доверенное рабочее пространство: без зависимостей, build.rs и proc-macro.
    write_cargo_project(repo.path());
    commit(repo.path(), "база");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn value() -> u8 { 2 }\n",
    )
    .unwrap();
    commit(repo.path(), "изменить значение");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);

    let output = review_workspace(repo.path(), &head);
    let first = collect_pack(repo.path(), &base, &head, &output, true);
    assert_eq!(
        first["tool_runs"][0]["status"],
        "success_without_diagnostics"
    );
    assert_eq!(first["tool_runs"][0]["exit_status"]["success"], true);
    assert_eq!(first["tool_runs"][0]["stderr_summary"], Value::Null);
    let json_before = fs::read(output.join("review.json")).unwrap();
    let text_before = fs::read(output.join("review.txt")).unwrap();

    let second = collect_pack(repo.path(), &base, &head, &output, true);
    assert_eq!(first, second);
    assert_eq!(fs::read(output.join("review.json")).unwrap(), json_before);
    assert_eq!(fs::read(output.join("review.txt")).unwrap(), text_before);
    assert!(repo.path().join("target").is_dir());
    assert!(tracked_status(repo.path()).is_empty());
}

#[test]
fn review_pack_is_portable_to_another_clone_for_verify_and_language_scan() {
    let repo_a = TempDir::new("review-pack-clone-a");
    let repo_b = TempDir::new("review-pack-clone-b");
    let artifacts_a = TempDir::new("review-pack-artifacts-a");
    let artifacts_b = TempDir::new("review-pack-artifacts-b");
    init_repo(&repo_a);
    fs::create_dir_all(repo_a.path().join("src")).unwrap();
    fs::write(repo_a.path().join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    commit(repo_a.path(), "база");
    let base = git(repo_a.path(), &["rev-parse", "HEAD"]);
    fs::write(
        repo_a.path().join("src/lib.rs"),
        "// Human message\npub fn value() { panic!(\"Another message\"); }\n",
    )
    .unwrap();
    commit(repo_a.path(), "добавить кандидатов");
    let head = git(repo_a.path(), &["rev-parse", "HEAD"]);
    let output = review_workspace(repo_a.path(), &head);
    let original = collect_pack(repo_a.path(), &base, &head, &output, false);
    let pack_a = output.join("review.json");
    let pack_b = artifacts_b.path().join("downloaded-review.json");
    fs::copy(&pack_a, &pack_b).unwrap();
    git(
        repo_b.path(),
        &[
            "clone",
            "--quiet",
            "--no-local",
            repo_a.path().to_str().unwrap(),
            ".",
        ],
    );
    git(repo_b.path(), &["config", "commit.gpgsign", "false"]);
    git(repo_b.path(), &["config", "core.autocrlf", "false"]);
    assert_ne!(repo_a.path(), repo_b.path());

    let verified_dir = review_workspace(repo_b.path(), &head);
    let (code, stdout, stderr) = run_cli_in(
        Some(repo_b.path()),
        &[
            "--json",
            "code-review",
            "verify",
            "--baseline",
            pack_b.to_str().unwrap(),
            "--head",
            &head,
            "--out-dir",
            verified_dir.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let verified = parse_json(&stdout)["result"].clone();
    assert_eq!(verified["target"], original["target"]);
    assert_eq!(
        fs::read(verified_dir.join("review.json")).unwrap(),
        fs::read(&pack_a).unwrap()
    );

    let scans = [
        (repo_a.path(), &pack_a, artifacts_a.path().join("scan.json")),
        (repo_b.path(), &pack_b, artifacts_b.path().join("scan.json")),
    ];
    for (root, pack, output) in &scans {
        let (code, stdout, stderr) = run_cli_in(
            Some(root),
            &[
                "--json",
                "language",
                "scan",
                "--root",
                root.to_str().unwrap(),
                "--pack",
                pack.to_str().unwrap(),
                "--out",
                output.to_str().unwrap(),
            ],
        );
        assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
        assert!(
            parse_json(&stdout)["result"]["candidates"]
                .as_u64()
                .unwrap()
                > 0
        );
    }
    assert_eq!(
        fs::read(&scans[0].2).unwrap(),
        fs::read(&scans[1].2).unwrap()
    );
    assert!(tracked_status(repo_b.path()).is_empty());
}

#[test]
fn verify_and_language_pack_scan_reject_mismatched_baseline() {
    let repo = TempDir::new("review-baseline-mismatch");
    let artifacts = TempDir::new("review-baseline-mismatch-artifacts");
    init_repo(&repo);
    fs::write(repo.path().join("message.md"), "Human message\n").unwrap();
    commit(repo.path(), "база");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let output = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &head, &head, &output, false);
    let pack_path = output.join("review.json");
    let mut pack: Value = serde_json::from_slice(&fs::read(&pack_path).unwrap()).unwrap();
    pack["target"]["repository_id"] = json!("0".repeat(64));
    fs::write(&pack_path, serde_json::to_vec_pretty(&pack).unwrap()).unwrap();
    let verify_output = review_workspace(repo.path(), &head);
    let scan_output = artifacts.path().join("scan.json");
    let commands = [
        vec![
            "--json",
            "code-review",
            "verify",
            "--baseline",
            pack_path.to_str().unwrap(),
            "--head",
            &head,
            "--out-dir",
            verify_output.to_str().unwrap(),
        ],
        vec![
            "--json",
            "language",
            "scan",
            "--root",
            repo.path().to_str().unwrap(),
            "--pack",
            pack_path.to_str().unwrap(),
            "--out",
            scan_output.to_str().unwrap(),
        ],
    ];
    let baseline_before_verify = fs::read(&pack_path).unwrap();
    let entries_before_verify: BTreeSet<_> = fs::read_dir(&verify_output)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let (code, stdout, stderr) = run_cli_in(Some(repo.path()), &commands[0]);
    assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["error"]["code"], "baseline_mismatch");
    assert_eq!(fs::read(&pack_path).unwrap(), baseline_before_verify);
    let entries_after_verify: BTreeSet<_> = fs::read_dir(&verify_output)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries_after_verify, entries_before_verify);
    assert!(verify_output.join("review.json").is_file());
    assert!(!verify_output.join("delta.json").exists());

    let (code, stdout, stderr) = run_cli_in(Some(repo.path()), &commands[1]);
    assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["error"]["code"], "baseline_mismatch");
    assert!(!scan_output.exists());
}

#[test]
fn human_review_summary_preserves_snapshot_and_evidence_meaning() {
    let repo = TempDir::new("review-human-summary");
    init_repo(&repo);
    fs::create_dir_all(repo.path().join("src")).unwrap();
    let base_source = "#[allow(dead_code)]\npub fn value() {}\n";
    fs::write(repo.path().join("src/lib.rs"), base_source).unwrap();
    commit(repo.path(), "база");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(
        repo.path().join("src/lib.rs"),
        format!("{base_source}\npub fn other() {{ panic!(\"Human message\"); }}\n"),
    )
    .unwrap();
    commit(repo.path(), "добавить кандидатов");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let output = review_workspace(repo.path(), &head);
    let result = collect_pack(repo.path(), &base, &head, &output, false);
    let text = fs::read_to_string(output.join("review.txt")).unwrap();
    let queue: Value =
        serde_json::from_slice(&fs::read(output.join("review-queue.json")).unwrap()).unwrap();
    for required in [
        format!("База: {base}"),
        format!("HEAD: {head}"),
        format!("Общий предок (merge-base): {base}"),
        "Статусы файлов: изменён: 1".into(),
        format!("Сырых кандидатов: {}", result["candidates"]),
        "review-queue.json".into(),
        "Полные свидетельства: review.json".into(),
        "Диагностик: 0".into(),
        "Анализатор clippy: skipped".into(),
    ] {
        assert!(
            text.contains(&required),
            "в review.txt отсутствует {required:?}:\n{text}"
        );
    }
    assert_eq!(
        queue["source"]["review_pack_sha256"],
        serde_json::json!(format!(
            "{:x}",
            sha2::Sha256::digest(fs::read(output.join("review.json")).unwrap())
        ))
    );
    assert_eq!(
        queue["summary"]["raw_candidates"],
        result["review_queue"]["raw_candidates"]
    );
    assert!(!text.contains(repo.path().to_str().unwrap()));
}

#[test]
fn queue_compresses_large_test_surface_and_read_only_cli_expands_it() {
    let repo = TempDir::new("review-queue-large-group");
    let artifacts = TempDir::new("review-queue-large-group-artifacts");
    init_repo(&repo);
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    commit(repo.path(), "base");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);

    let mut source = String::from(
        "// Human review message\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn repeated_setup() {\n",
    );
    for _ in 0..1_200 {
        source.push_str("        let _ = Result::<(), &str>::Err(\"fixture\").unwrap();\n");
    }
    source.push_str(
        "    }\n}\npub fn read_boundary(path: &str) { let _ = std::fs::read(path).unwrap(); }\n",
    );
    fs::write(repo.path().join("src/lib.rs"), source).unwrap();
    commit(
        repo.path(),
        "add repeated test evidence and runtime boundary",
    );
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let output = review_workspace(repo.path(), &head);
    let result = collect_pack(repo.path(), &base, &head, &output, false);
    let pack_bytes = fs::read(output.join("review.json")).unwrap();
    let queue_bytes = fs::read(output.join("review-queue.json")).unwrap();
    let pack: Value = serde_json::from_slice(&pack_bytes).unwrap();
    let queue: Value = serde_json::from_slice(&queue_bytes).unwrap();
    let text = fs::read_to_string(output.join("review.txt")).unwrap();

    assert_eq!(
        queue["source"]["review_pack_sha256"],
        json!(format!("{:x}", sha2::Sha256::digest(&pack_bytes)))
    );
    assert_eq!(queue["summary"]["raw_candidates"], result["candidates"]);
    assert!(text.contains("error_path: 1201"), "review.txt: {text}");
    assert!(text.contains("test_setup: 2401"), "review.txt: {text}");
    assert!(text.contains("runtime_boundary: 1"), "review.txt: {text}");
    let static_ids: Vec<_> = pack["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|candidate| candidate["detector"] == "error_path")
        .map(|candidate| candidate["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        static_ids.len(),
        1_201,
        "ожидались 1200 test calls и один production call"
    );
    let group = queue["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| unit["members"]["kind"] == "group")
        .expect("повторяющиеся test calls должны образовать группу");
    let group_ids = group["members"]["candidate_ids"].as_array().unwrap();
    let group_id_set: std::collections::BTreeSet<_> =
        group_ids.iter().map(|id| id.as_str().unwrap()).collect();
    assert_eq!(group_ids.len(), 1_200);
    let production = pack["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| {
            candidate["detector"] == "error_path"
                && candidate["snippet"]
                    .as_str()
                    .is_some_and(|snippet| snippet.contains("std::fs::read"))
        })
        .expect("production filesystem candidate");
    let production_id = production["id"].as_str().unwrap();
    assert!(!group_id_set.contains(production_id));
    let production_class = &queue["classifications"][production_id];
    assert_eq!(production_class["execution"], "production");
    assert_eq!(production_class["code_role"], "runtime_boundary");
    assert_eq!(group["signature"]["classification"]["execution"], "tests");
    assert_eq!(
        group["signature"]["classification"]["code_role"],
        "test_setup"
    );
    let language_ids: Vec<_> = pack["language"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|candidate| candidate["id"].as_str().unwrap())
        .collect();
    assert!(
        !language_ids.is_empty(),
        "human comment must remain in raw language evidence"
    );
    for id in &language_ids {
        assert!(queue["classifications"][*id].is_object());
    }
    assert!(
        text.lines().count() < 150,
        "review.txt должен оставаться компактным"
    );
    let non_representative = group_ids
        .iter()
        .map(|id| id.as_str().unwrap())
        .find(|id| {
            !group["members"]["representative_candidate_ids"]
                .as_array()
                .unwrap()
                .iter()
                .any(|representative| representative == *id)
        })
        .unwrap();
    assert!(!text.contains(non_representative));
    let queue_text = String::from_utf8(queue_bytes).unwrap();
    assert!(!queue_text.contains(repo.path().to_str().unwrap()));
    assert!(!queue_text.contains("findings"));
    assert!(!queue_text.contains("disposition"));

    let pack_path = output.join("review.json");
    let queue_path = output.join("review-queue.json");
    let pack_arg = pack_path.to_string_lossy().into_owned();
    let queue_arg = queue_path.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "queue",
            "validate",
            "--pack",
            &pack_arg,
            "--queue",
            &queue_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["result"]["valid"], true);

    let (code, stdout, stderr) = run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "queue",
            "summary",
            "--pack",
            &pack_arg,
            "--queue",
            &queue_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        parse_json(&stdout)["result"]["raw_candidates"],
        queue["summary"]["raw_candidates"]
    );

    let (_, page) = queue_list_page(
        repo.path(),
        &pack_path,
        &queue_path,
        &["--limit", "200", "--detector", "error_path"],
    );
    let listed_group = page["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| unit["id"] == group["id"])
        .unwrap();
    assert_eq!(listed_group["candidate_id"], Value::Null);
    assert_eq!(listed_group["candidate_count"], 1_200);
    assert_eq!(
        listed_group["representative_candidate_ids"],
        group["members"]["representative_candidate_ids"]
    );

    let group_id = group["id"].as_str().unwrap();
    let (code, stdout, stderr) = run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "queue",
            "group",
            "--pack",
            &pack_arg,
            "--queue",
            &queue_arg,
            "--id",
            group_id,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let expanded = parse_json(&stdout)["result"].clone();
    assert_eq!(
        expanded["unit"]["members"]["candidate_ids"]
            .as_array()
            .unwrap()
            .len(),
        1_200
    );
    assert_eq!(expanded["representatives"].as_array().unwrap().len(), 3);

    let candidate_id = group["members"]["representative_candidate_ids"][0]
        .as_str()
        .unwrap();
    let (code, stdout, stderr) = run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "queue",
            "candidate",
            "--pack",
            &pack_arg,
            "--queue",
            &queue_arg,
            "--id",
            candidate_id,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        parse_json(&stdout)["result"]["candidate"]["id"],
        candidate_id
    );

    for (subcommand, id, label) in [
        ("group", group_id, "Группа"),
        ("candidate", candidate_id, "Кандидат"),
    ] {
        let (code, stdout, stderr) = run_cli_in(
            Some(repo.path()),
            &[
                "code-review",
                "queue",
                subcommand,
                "--pack",
                &pack_arg,
                "--queue",
                &queue_arg,
                "--id",
                id,
            ],
        );
        assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
        assert!(stdout.contains(label) && stdout.contains(id), "{stdout}");
    }

    let individual_id = queue["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| unit["members"]["kind"] == "individual")
        .expect("очередь содержит отдельную unit")["id"]
        .as_str()
        .unwrap();
    for (subcommand, id) in [
        ("group", "missing-group"),
        ("group", individual_id),
        ("candidate", "missing-candidate"),
    ] {
        let (code, stdout, stderr) = run_cli_in(
            Some(repo.path()),
            &[
                "--json",
                "code-review",
                "queue",
                subcommand,
                "--pack",
                &pack_arg,
                "--queue",
                &queue_arg,
                "--id",
                id,
            ],
        );
        assert_eq!(code, 4, "stdout: {stdout}\nstderr: {stderr}");
        assert!(
            stderr.is_empty(),
            "JSON-режим записал ошибку в stderr: {stderr}"
        );
        let envelope = parse_json(&stdout);
        assert_eq!(envelope["error"]["code"], "not_found");
        assert!(
            envelope.get("result").is_none(),
            "unexpected result: {envelope}"
        );
    }

    for subcommand in ["group", "candidate"] {
        let (code, stdout, stderr) = run_cli_in(
            Some(repo.path()),
            &[
                "code-review",
                "queue",
                subcommand,
                "--pack",
                &pack_arg,
                "--queue",
                &queue_arg,
                "--id",
                "missing-id",
            ],
        );
        assert_eq!(code, 4, "stdout: {stdout}\nstderr: {stderr}");
        assert!(
            !stderr.is_empty(),
            "human ошибка должна быть доступна в stderr"
        );
    }

    let tampered_pack_path = artifacts.path().join("review-with-whitespace.json");
    let mut tampered = pack_bytes;
    tampered.push(b' ');
    fs::write(&tampered_pack_path, tampered).unwrap();
    let tampered_arg = tampered_pack_path.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "queue",
            "validate",
            "--pack",
            &tampered_arg,
            "--queue",
            &queue_arg,
        ],
    );
    assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        parse_json(&stdout)["error"]["code"],
        "review_artifact_invalid"
    );
}

fn queue_list_page(root: &Path, pack: &Path, queue: &Path, options: &[&str]) -> (String, Value) {
    let mut args = vec![
        "--json",
        "code-review",
        "queue",
        "list",
        "--pack",
        pack.to_str().unwrap(),
        "--queue",
        queue.to_str().unwrap(),
    ];
    args.extend_from_slice(options);
    let (code, stdout, stderr) = run_cli_in(Some(root), &args);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.is_empty(), "{stderr}");
    let result = parse_json(&stdout)["result"].clone();
    (stdout, result)
}

fn listed_unit_ids(root: &Path, pack: &Path, queue: &Path, filters: &[&str]) -> Vec<String> {
    let mut ids = Vec::new();
    let mut offset = 0;
    loop {
        let offset_arg = offset.to_string();
        let mut options = filters.to_vec();
        options.extend_from_slice(&["--limit", "17", "--offset", &offset_arg]);
        let (stdout, page) = queue_list_page(root, pack, queue, &options);
        let (repeated_stdout, _) = queue_list_page(root, pack, queue, &options);
        assert_eq!(
            stdout, repeated_stdout,
            "страница должна сериализоваться детерминированно"
        );
        let rows = page["units"].as_array().unwrap();
        assert_eq!(page["offset"], offset);
        assert_eq!(page["limit"], 17);
        assert_eq!(page["returned_units"], rows.len());
        for row in rows {
            ids.push(row["id"].as_str().unwrap().to_owned());
        }
        offset += rows.len();
        assert_eq!(
            page["has_more"],
            offset < page["matched_units"].as_u64().unwrap() as usize
        );
        if page["has_more"] == false {
            assert_eq!(offset, page["matched_units"].as_u64().unwrap() as usize);
            break;
        }
        assert!(
            !rows.is_empty(),
            "пустая страница не должна обещать продолжение"
        );
    }
    assert_eq!(
        ids.len(),
        ids.iter().collect::<BTreeSet<_>>().len(),
        "страницы повторили ID"
    );
    ids
}

#[test]
fn queue_list_pages_all_high_and_unknown_units_without_losing_raw_identity() {
    let repo = TempDir::new("review-queue-navigation");
    let artifacts = TempDir::new("review-queue-navigation-artifacts");
    init_repo(&repo);
    write_cargo_project(repo.path());
    commit(repo.path(), "база");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    let mut production = String::new();
    let mut ambiguous = String::new();
    for index in 0..64 {
        production.push_str(&format!(
            "// Human explanation for boundary {index}\npub fn boundary_{index}(path: &str) {{ let _ = std::fs::read(path).unwrap(); }}\n"
        ));
        ambiguous.push_str(&format!(
            "const RUNTIME_{index}: i32 = todo!(); #[cfg(test)] mod test_{index} {{ const TEST: i32 = todo!(); }}\n"
        ));
    }
    fs::write(repo.path().join("src/lib.rs"), production).unwrap();
    fs::write(repo.path().join("src/ambiguous.rs"), ambiguous).unwrap();
    commit(
        repo.path(),
        "добавить много отдельных доказанных и неизвестных контекстов",
    );
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let output = review_workspace(repo.path(), &head);
    let collected = collect_pack(repo.path(), &base, &head, &output, false);
    let pack_path = output.join("review.json");
    let queue_path = output.join("review-queue.json");
    let pack_bytes = fs::read(&pack_path).unwrap();
    let queue_bytes = fs::read(&queue_path).unwrap();
    let pack: Value = serde_json::from_slice(&pack_bytes).unwrap();
    let queue: Value = serde_json::from_slice(&queue_bytes).unwrap();
    let units = queue["units"].as_array().unwrap();
    assert!(!pack["candidates"].as_array().unwrap().is_empty());
    assert!(
        !pack["language"]["candidates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let raw_ids: BTreeSet<_> = pack["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .chain(pack["language"]["candidates"].as_array().unwrap())
        .map(|candidate| candidate["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        raw_ids.len(),
        collected["candidates"].as_u64().unwrap() as usize
    );
    let ordered: Vec<_> = units
        .iter()
        .map(|unit| {
            let rank = match unit["priority"].as_str().unwrap() {
                "high" => 0,
                "normal" => 1,
                "low" => 2,
                other => panic!("неизвестный приоритет {other}"),
            };
            (rank, unit["id"].as_str().unwrap())
        })
        .collect();
    assert!(
        ordered.windows(2).all(|pair| pair[0] < pair[1]),
        "порядок очереди должен зависеть от приоритета и ID"
    );
    let mut represented = Vec::new();
    for unit in units {
        let members = &unit["members"];
        if members["kind"] == "individual" {
            represented.push(members["candidate_id"].as_str().unwrap());
        } else {
            represented.extend(
                members["candidate_ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|id| id.as_str().unwrap()),
            );
        }
    }
    assert_eq!(represented.len(), raw_ids.len());
    assert_eq!(represented.into_iter().collect::<BTreeSet<_>>(), raw_ids);
    let expected_ids: Vec<_> = units
        .iter()
        .map(|unit| unit["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        listed_unit_ids(repo.path(), &pack_path, &queue_path, &[]),
        expected_ids
    );

    let (_, first_page) = queue_list_page(repo.path(), &pack_path, &queue_path, &[]);
    assert_eq!(first_page["total_units"], units.len());
    assert_eq!(first_page["matched_units"], units.len());
    assert_eq!(first_page["limit"], 50);
    assert_eq!(first_page["offset"], 0);
    assert_eq!(first_page["returned_units"], 50);
    assert_eq!(first_page["has_more"], true);
    assert_eq!(first_page["source_digest_valid"], true);
    assert_eq!(first_page["syntax_authenticity"], "verified");
    let page_fields: BTreeSet<_> = first_page
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        page_fields,
        BTreeSet::from([
            "total_units",
            "matched_units",
            "offset",
            "limit",
            "returned_units",
            "has_more",
            "units",
            "source_digest_valid",
            "syntax_authenticity",
        ])
    );
    for row in first_page["units"].as_array().unwrap() {
        let fields: BTreeSet<_> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            fields,
            BTreeSet::from([
                "id",
                "kind",
                "priority",
                "classification",
                "detector",
                "candidate_count",
                "candidate_id",
                "representative_candidate_ids"
            ])
        );
        assert!(row.get("snippet").is_none());
        assert!(row["classification"].is_object());
        let unit = units.iter().find(|unit| unit["id"] == row["id"]).unwrap();
        assert_eq!(row["classification"], unit["signature"]["classification"]);
        assert_eq!(row["detector"], unit["signature"]["detector"]);
        assert_eq!(row["kind"], unit["members"]["kind"]);
        let members = &unit["members"];
        assert_eq!(
            row["candidate_id"],
            if members["kind"] == "individual" {
                members["candidate_id"].clone()
            } else {
                Value::Null
            }
        );
        let expected_representatives = if members["kind"] == "individual" {
            json!([])
        } else {
            members["representative_candidate_ids"].clone()
        };
        let expected_count = if members["kind"] == "individual" {
            1
        } else {
            members["candidate_ids"].as_array().unwrap().len()
        };
        assert_eq!(row["candidate_count"], expected_count);
        assert_eq!(
            row["representative_candidate_ids"],
            expected_representatives
        );
        assert!(
            row["representative_candidate_ids"]
                .as_array()
                .unwrap()
                .iter()
                .all(|id| raw_ids.contains(id.as_str().unwrap()))
        );
    }

    let filter_cases: &[(&[&str], &str, &str)] = &[
        (&["--priority", "high"], "priority", "high"),
        (&["--priority", "normal"], "priority", "normal"),
        (&["--priority", "low"], "priority", "low"),
        (&["--detector", "error_path"], "detector", "error_path"),
        (&["--surface", "production"], "surfaces", "production"),
        (&["--execution", "production"], "execution", "production"),
        (&["--role", "error_path"], "role", "error_path"),
        (
            &["--text-role", "human_comment"],
            "text_role",
            "human_comment",
        ),
        (
            &["--code-role", "runtime_boundary"],
            "code_role",
            "runtime_boundary",
        ),
    ];
    for (options, dimension, expected) in filter_cases {
        let expected_filtered: Vec<_> = units
            .iter()
            .filter(|unit| match *dimension {
                "priority" => unit["priority"] == *expected,
                "detector" => unit["signature"]["detector"] == *expected,
                "surfaces" => unit["signature"]["classification"]["surfaces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|surface| surface == expected),
                _ => unit["signature"]["classification"][dimension] == *expected,
            })
            .map(|unit| unit["id"].as_str().unwrap().to_owned())
            .collect();
        if *expected == "high" {
            assert!(
                expected_filtered.len() > 50,
                "high должен занимать больше одной стандартной страницы"
            );
        }
        assert_eq!(
            listed_unit_ids(repo.path(), &pack_path, &queue_path, options),
            expected_filtered,
            "фильтр {options:?}"
        );
    }
    let unknown_ids: Vec<_> = units
        .iter()
        .filter(|unit| {
            let class: anki_repo::code_review::review_queue::StructuralClassification =
                serde_json::from_value(unit["signature"]["classification"].clone()).unwrap();
            class.is_unknown()
        })
        .map(|unit| unit["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        unknown_ids.len() > 50,
        "unknown должен занимать больше одной стандартной страницы"
    );
    assert_eq!(
        listed_unit_ids(repo.path(), &pack_path, &queue_path, &["--unknown"]),
        unknown_ids
    );
    let combined: Vec<_> = units
        .iter()
        .filter(|unit| {
            unit["priority"] == "high"
                && unit["signature"]["detector"] == "error_path"
                && unit["signature"]["classification"]["execution"] == "production"
                && unit["signature"]["classification"]["code_role"] == "runtime_boundary"
        })
        .map(|unit| unit["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        listed_unit_ids(
            repo.path(),
            &pack_path,
            &queue_path,
            &[
                "--priority",
                "high",
                "--detector",
                "error_path",
                "--execution",
                "production",
                "--code-role",
                "runtime_boundary"
            ]
        ),
        combined
    );
    let (_, empty) = queue_list_page(
        repo.path(),
        &pack_path,
        &queue_path,
        &["--detector", "missing_detector"],
    );
    assert_eq!(empty["total_units"], units.len());
    assert_eq!(empty["matched_units"], 0);
    assert_eq!(empty["returned_units"], 0);
    assert_eq!(empty["has_more"], false);
    let beyond = (units.len() + 1).to_string();
    let (_, empty) = queue_list_page(repo.path(), &pack_path, &queue_path, &["--offset", &beyond]);
    assert_eq!(empty["matched_units"], units.len());
    assert_eq!(empty["returned_units"], 0);
    assert_eq!(empty["has_more"], false);

    let text = fs::read_to_string(output.join("review.txt")).unwrap();
    assert!(
        text.lines().count() < 150,
        "сводка должна оставаться компактной"
    );
    assert!(
        text.contains("queue list"),
        "сводка должна указывать путь к остальным страницам"
    );
    assert!(
        expected_ids.iter().any(|id| !text.contains(id)),
        "fixture должен воспроизводить обрезку сводки"
    );
    for subcommand in ["validate", "summary", "list"] {
        let (code, stdout, stderr) = run_cli_in(
            Some(repo.path()),
            &[
                "code-review",
                "queue",
                subcommand,
                "--pack",
                pack_path.to_str().unwrap(),
                "--queue",
                queue_path.to_str().unwrap(),
            ],
        );
        assert_eq!(code, 0, "{stderr}");
        if subcommand == "list" {
            assert!(
                stdout.contains("Всего единиц")
                    && stdout.contains("совпало")
                    && stdout.contains("показано")
                    && stdout.contains("следующая страница"),
                "{stdout}"
            );
            let individual_id = first_page["units"]
                .as_array()
                .unwrap()
                .iter()
                .find(|unit| unit["kind"] == "individual")
                .unwrap()["candidate_id"]
                .as_str()
                .unwrap();
            assert!(stdout.contains("Исходный ID кандидата"));
            assert!(stdout.contains(individual_id), "{stdout}");
        } else {
            assert!(
                stdout.contains("очеред") || stdout.contains("Очеред"),
                "{stdout}"
            );
        }
        for untranslated in [
            "Raw candidates:",
            "Grouped candidates:",
            "Execution:",
            "Classification и priority",
            "Candidates:",
        ] {
            assert!(!stdout.contains(untranslated), "{stdout}");
        }
    }
    let candidate_id = first_page["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| unit["kind"] == "individual")
        .unwrap()["candidate_id"]
        .as_str()
        .unwrap();
    let (_, raw_detail, stderr) = run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "queue",
            "candidate",
            "--pack",
            pack_path.to_str().unwrap(),
            "--queue",
            queue_path.to_str().unwrap(),
            "--id",
            candidate_id,
        ],
    );
    assert!(stderr.is_empty());
    let detail = parse_json(&raw_detail)["result"].clone();
    assert_eq!(detail["candidate"]["id"], candidate_id);
    if let Some(raw) = pack["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["id"] == candidate_id)
    {
        assert_eq!(
            detail["candidate"], *raw,
            "queue candidate сохраняет полные исходные свидетельства"
        );
    }

    let triage_path = output.join("semantic-triage.input.json");
    for json_mode in [true, false] {
        let initialized = output.join("semantic-triage.input.json");
        if initialized.exists() {
            fs::remove_file(&initialized).unwrap();
        }
        let mut args = vec![
            "code-review",
            "triage",
            "init",
            "--pack",
            pack_path.to_str().unwrap(),
            "--out",
            initialized.to_str().unwrap(),
        ];
        if json_mode {
            args.insert(0, "--json");
        }
        let (code, stdout, stderr) = run_cli_in(Some(repo.path()), &args);
        assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
        let initial: Value = serde_json::from_slice(&fs::read(&initialized).unwrap()).unwrap();
        assert_eq!(initial["individual_decisions"], json!([]));
        assert_eq!(initial["group_decisions"], json!([]));
        assert_eq!(
            initial["unreviewed_candidate_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| id.as_str().unwrap())
                .collect::<BTreeSet<_>>(),
            raw_ids
        );
        let mut decided = initial;
        decided["unreviewed_candidate_ids"]
            .as_array_mut()
            .unwrap()
            .retain(|id| id != candidate_id);
        decided["individual_decisions"] = json!([{
            "candidate_id": candidate_id, "disposition": "uncertain", "reason_code": "insufficient_evidence",
            "explanation": "Синтетическое решение сохранено по исходному ID; требуется семантическое ревью.", "finding_ids": []
        }]);
        fs::write(&triage_path, serde_json::to_vec_pretty(&decided).unwrap()).unwrap();
        let mut args = vec![
            "code-review",
            "triage",
            "validate",
            "--pack",
            pack_path.to_str().unwrap(),
            "--triage",
            triage_path.to_str().unwrap(),
        ];
        if json_mode {
            args.insert(0, "--json");
        }
        let (code, stdout, stderr) = run_cli_in(Some(repo.path()), &args);
        assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
        if json_mode {
            let summary = parse_json(&stdout)["result"]["summary"].clone();
            assert_eq!(summary["reviewed_candidates"], 1);
            assert_eq!(summary["unreviewed_candidates"], raw_ids.len() - 1);
        }
    }
    let canonical = output.join("semantic-triage.json");
    let mut canonical_bytes = None;
    for _ in 0..2 {
        let (code, stdout, stderr) = run_cli_in(
            Some(repo.path()),
            &[
                "--json",
                "code-review",
                "triage",
                "validate",
                "--pack",
                pack_path.to_str().unwrap(),
                "--triage",
                triage_path.to_str().unwrap(),
                "--canonical-out",
                canonical.to_str().unwrap(),
            ],
        );
        assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
        let bytes = fs::read(&canonical).unwrap();
        if let Some(previous) = &canonical_bytes {
            assert_eq!(
                &bytes, previous,
                "канонический triage должен быть детерминирован"
            );
        }
        canonical_bytes = Some(bytes);
    }
    collect_pack(repo.path(), &base, &head, &output, false);
    assert_eq!(fs::read(&pack_path).unwrap(), pack_bytes);
    assert_eq!(fs::read(&queue_path).unwrap(), queue_bytes);

    let tampered_path = artifacts.path().join("changed-review.json");
    let mut tampered = pack_bytes;
    tampered.push(b' ');
    fs::write(&tampered_path, tampered).unwrap();
    for json_mode in [true, false] {
        let mut args = vec![
            "code-review",
            "queue",
            "list",
            "--pack",
            tampered_path.to_str().unwrap(),
            "--queue",
            queue_path.to_str().unwrap(),
            "--detector",
            "missing_detector",
            "--offset",
            &beyond,
        ];
        if json_mode {
            args.insert(0, "--json");
        }
        let (code, stdout, stderr) = run_cli_in(Some(repo.path()), &args);
        assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
        if json_mode {
            let envelope = parse_json(&stdout);
            assert_eq!(envelope["error"]["code"], "review_artifact_invalid");
            assert!(envelope.get("result").is_none());
            assert!(stderr.is_empty());
        } else {
            assert!(
                stdout.is_empty(),
                "human-ошибка не должна содержать страницу: {stdout}"
            );
            assert!(!stderr.is_empty());
        }
    }
}

#[test]
fn queue_list_help_explains_filters_and_rejects_invalid_pages() {
    let (code, help, stderr) = run_cli(&["code-review", "queue", "list", "--help"]);
    assert_eq!(code, 0, "{stderr}");
    for flag in [
        "--pack",
        "--queue",
        "--limit",
        "--offset",
        "--priority",
        "--unknown",
        "--detector",
        "--surface",
        "--execution",
        "--role",
        "--text-role",
        "--code-role",
    ] {
        assert!(help.contains(flag), "отсутствует {flag}: {help}");
    }
    assert!(help.contains("review.json") && help.contains("review-queue.json"));
    assert!(help.contains("Путь к исходному пакету"));
    assert!(help.contains("Путь к структурной очереди"));
    assert!(help.contains("страниц") && help.contains("Смещ"));
    for (subcommand, description) in [
        ("group", "Точный ID группы"),
        ("candidate", "Точный ID candidate"),
    ] {
        let (code, detail_help, stderr) = run_cli(&["code-review", "queue", subcommand, "--help"]);
        assert_eq!(code, 0, "{stderr}");
        assert!(detail_help.contains("Путь к исходному пакету"));
        assert!(detail_help.contains("Путь к структурной очереди"));
        assert!(detail_help.contains(description), "{detail_help}");
    }
    for limit in ["0", "201"] {
        let (code, _, stderr) = run_cli(&[
            "code-review",
            "queue",
            "list",
            "--pack",
            "review.json",
            "--queue",
            "review-queue.json",
            "--limit",
            limit,
        ]);
        assert_eq!(code, 2, "{stderr}");
        assert!(stderr.contains("--limit"));
    }
}

#[test]
fn queue_list_rejects_invalid_priority_as_cli_usage_error() {
    let (code, stdout, stderr) = run_cli(&[
        "code-review",
        "queue",
        "list",
        "--pack",
        "unused-review.json",
        "--queue",
        "unused-review-queue.json",
        "--priority",
        "urgent",
    ]);

    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.is_empty(),
        "ошибка разбора не должна выдавать результат"
    );
    assert!(stderr.contains("--priority"), "stderr: {stderr}");
}

#[test]
fn rust_text_roles_remain_orthogonal_to_execution_and_grouping() {
    let repo = TempDir::new("rust-text-role-separation");
    init_repo(&repo);
    write_cargo_project(repo.path());
    commit(repo.path(), "база");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);

    let source = r#"
fn production_log_a() { println!("Shared log phrase"); }
fn production_log_b() { eprintln!("Shared log phrase"); }
#[test] fn test_log_a() { println!("Shared log phrase"); }
#[test] fn test_log_b() { eprintln!("Shared log phrase"); }
fn production_json_a() { let _ = serde_json::json!({"message": "Shared machine phrase"}); }
fn production_json_b() { let _ = serde_json::json!({"message": "Shared machine phrase"}); }
#[test] fn test_json_a() { let _ = serde_json::json!({"message": "Shared machine phrase"}); }
#[test] fn test_json_b() { let _ = serde_json::json!({"message": "Shared machine phrase"}); }
#[test] fn fixture() { let _ = include_str!("fixtures/input.json"); assert_eq!(1, 1, "Fixture phrase"); }
#[test] fn unknown_context() { user_macro!("Unclassified phrase"); }
"#;
    fs::write(repo.path().join("src/lib.rs"), source).unwrap();
    commit(repo.path(), "добавить строки разных ролей и контекстов");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let output = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &output, false);
    let pack: Value =
        serde_json::from_slice(&fs::read(output.join("review.json")).unwrap()).unwrap();
    let queue: Value =
        serde_json::from_slice(&fs::read(output.join("review-queue.json")).unwrap()).unwrap();
    let candidates = pack["language"]["candidates"].as_array().unwrap();
    let ids_for_text = |text: &str| -> Vec<String> {
        candidates
            .iter()
            .filter(|candidate| candidate["text"] == text)
            .map(|candidate| candidate["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let log_ids = ids_for_text("Shared log phrase");
    assert_eq!(log_ids.len(), 4);
    let mut log_executions = BTreeSet::new();
    for id in &log_ids {
        let classification = &queue["classifications"][id];
        assert_eq!(classification["text_role"], "human_log");
        log_executions.insert(classification["execution"].as_str().unwrap());
        let unit = queue["units"]
            .as_array()
            .unwrap()
            .iter()
            .find(|unit| {
                unit["members"]["candidate_id"].as_str() == Some(id.as_str())
                    || unit["members"]["candidate_ids"]
                        .as_array()
                        .is_some_and(|members| members.iter().any(|member| member == id))
            })
            .unwrap();
        assert_eq!(unit["members"]["kind"], "individual");
    }
    assert_eq!(log_executions, BTreeSet::from(["production", "tests"]));

    let machine_ids = ids_for_text("Shared machine phrase");
    assert_eq!(machine_ids.len(), 4);
    let machine_units: Vec<_> = machine_ids
        .iter()
        .map(|id| {
            let classification = &queue["classifications"][id];
            assert_eq!(classification["text_role"], "machine_contract");
            queue["units"]
                .as_array()
                .unwrap()
                .iter()
                .find(|unit| {
                    unit["members"]["candidate_ids"]
                        .as_array()
                        .is_some_and(|members| members.iter().any(|member| member == id))
                })
                .unwrap()
        })
        .collect();
    let mut grouped_ids = BTreeSet::new();
    let mut execution_groups = BTreeSet::new();
    for unit in &machine_units {
        assert_eq!(unit["members"]["kind"], "group");
        let group_execution = unit["signature"]["classification"]["execution"]
            .as_str()
            .unwrap();
        let members = unit["members"]["candidate_ids"].as_array().unwrap();
        assert_eq!(members.len(), 2);
        for id in members {
            let classification = &queue["classifications"][id.as_str().unwrap()];
            assert_eq!(classification["execution"], group_execution);
        }
        grouped_ids.insert(unit["id"].as_str().unwrap());
        execution_groups.insert(group_execution);
    }
    assert_eq!(grouped_ids.len(), 2);
    assert_eq!(execution_groups, BTreeSet::from(["production", "tests"]));

    let fixture_ids = ids_for_text("Fixture phrase");
    assert_eq!(fixture_ids.len(), 1);
    assert_eq!(
        queue["classifications"][&fixture_ids[0]]["text_role"],
        "test_fixture"
    );
    assert_eq!(
        queue["classifications"][&fixture_ids[0]]["execution"],
        "tests"
    );
    let unknown_ids = ids_for_text("Unclassified phrase");
    assert_eq!(unknown_ids.len(), 1);
    assert_eq!(
        queue["classifications"][&unknown_ids[0]]["text_role"],
        "unknown"
    );
    let unknown_class: anki_repo::code_review::review_queue::StructuralClassification =
        serde_json::from_value(queue["classifications"][&unknown_ids[0]].clone()).unwrap();
    assert!(unknown_class.is_unknown());
}

#[test]
fn collect_keeps_mixed_rust_items_unknown_when_a_line_has_no_single_column() {
    let repo = TempDir::new("collect-mixed-rust-items");
    init_repo(&repo);
    write_cargo_project(repo.path());
    commit(repo.path(), "база");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);

    fs::write(
        repo.path().join("src/lib.rs"),
        "const RUNTIME: i32 = todo!(); #[cfg(test)] mod tests { const TEST: i32 = todo!(); }\n",
    )
    .unwrap();
    commit(repo.path(), "смешать test и runtime items на одной строке");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let output = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &output, false);

    let pack: Value =
        serde_json::from_slice(&fs::read(output.join("review.json")).unwrap()).unwrap();
    let queue: Value =
        serde_json::from_slice(&fs::read(output.join("review-queue.json")).unwrap()).unwrap();
    let candidate = pack["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| {
            candidate["detector"] == "error_path"
                && candidate["snippet"]
                    .as_str()
                    .is_some_and(|snippet| snippet.matches("todo!").count() == 2)
        })
        .expect("строка с двумя todo! должна дать один error_path candidate");
    assert_eq!(candidate["line"], 1);
    assert_eq!(candidate["column"], Value::Null);

    let classification = &queue["classifications"][candidate["id"].as_str().unwrap()];
    assert_eq!(classification["execution"], Value::Null);
    assert_eq!(classification["code_role"], "unknown");
    assert_eq!(classification["code_basis"], "unknown");
    assert!(queue["summary"]["unknown_candidates"].as_u64().unwrap() > 0);
}

#[test]
fn collect_uses_rust_test_surface_and_canonical_file_surface_labels() {
    let repo = TempDir::new("collect-rust-test-surface");
    init_repo(&repo);
    write_cargo_project(repo.path());
    commit(repo.path(), "база");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);

    fs::create_dir_all(repo.path().join("tests")).unwrap();
    fs::create_dir_all(repo.path().join(".agents")).unwrap();
    fs::write(
        repo.path().join("tests/helper.rs"),
        "pub fn read_fixture(path: &str) { let _ = std::fs::read(path).unwrap(); }\n",
    )
    .unwrap();
    fs::write(
        repo.path().join(".agents/context.rs"),
        "pub fn context() {}\n",
    )
    .unwrap();
    commit(repo.path(), "добавить тестовый helper и агентский context");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let output = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &output, false);

    let pack: Value =
        serde_json::from_slice(&fs::read(output.join("review.json")).unwrap()).unwrap();
    let queue: Value =
        serde_json::from_slice(&fs::read(output.join("review-queue.json")).unwrap()).unwrap();
    let summary = fs::read_to_string(output.join("review.txt")).unwrap();
    let candidate = pack["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| {
            candidate["path"] == "tests/helper.rs"
                && candidate["detector"] == "error_path"
                && candidate["snippet"]
                    .as_str()
                    .is_some_and(|snippet| snippet.contains("std::fs::read"))
        })
        .expect("test helper должен давать error_path candidate");
    let classification = &queue["classifications"][candidate["id"].as_str().unwrap()];
    assert_eq!(classification["execution"], "tests");
    assert_eq!(classification["code_role"], "test_helper");
    assert!(summary.contains("agent_context: 1"));
    assert!(!summary.contains("agentcontext"));
}

#[test]
fn collect_distinguishes_function_tests_and_resolved_io_boundaries() {
    let repo = TempDir::new("collect-function-test-boundaries");
    init_repo(&repo);
    write_cargo_project(repo.path());
    commit(repo.path(), "база");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    let mut source =
        String::from("struct File; impl File { fn open(_: &str) -> Result<(), ()> { Ok(()) } }\n");
    for (attribute, name) in [
        ("#[test]", "ordinary_test"),
        ("#[tokio::test]", "async_test"),
        ("", "production"),
    ] {
        source.push_str(&format!("{attribute}\nfn {name}() {{\n"));
        for _ in 0..8 {
            source.push_str("    let _ = None::<u8>.unwrap();\n");
        }
        source.push_str("}\n");
    }
    source.push_str("#[unknown::test]\nfn unresolved_test() { let _ = None::<u8>.unwrap(); }\n");
    source.push_str("fn local_type() { let _ = File::open(\"data\").unwrap(); }\n");
    source.push_str("fn proven_io() { let _ = std::fs::File::open(\"data\").unwrap(); }\n");
    fs::write(repo.path().join("src/lib.rs"), source).unwrap();
    commit(repo.path(), "добавить атрибуты функций и одноимённые типы");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let output = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &output, false);
    let pack: Value =
        serde_json::from_slice(&fs::read(output.join("review.json")).unwrap()).unwrap();
    let queue: Value =
        serde_json::from_slice(&fs::read(output.join("review-queue.json")).unwrap()).unwrap();
    let candidates = pack["candidates"].as_array().unwrap();
    let units = queue["units"].as_array().unwrap();
    let detail = |fragment: &str| {
        let candidate = candidates
            .iter()
            .find(|candidate| {
                candidate["detector"] == "error_path"
                    && candidate["snippet"]
                        .as_str()
                        .is_some_and(|snippet| snippet.contains(fragment))
            })
            .unwrap();
        &queue["classifications"][candidate["id"].as_str().unwrap()]
    };
    assert_eq!(detail("fn local_type")["execution"], "production");
    assert_eq!(detail("fn local_type")["code_role"], "runtime");
    assert_eq!(detail("fn proven_io")["code_role"], "runtime_boundary");
    assert_eq!(detail("fn unresolved_test")["execution"], Value::Null);
    assert_eq!(detail("fn unresolved_test")["code_role"], "unknown");
    let setup_group = units
        .iter()
        .find(|unit| {
            unit["members"]["kind"] == "group"
                && unit["signature"]["classification"]["code_role"] == "test_setup"
        })
        .expect("доказанные функции test должны образовать отдельную группу");
    assert_eq!(
        setup_group["signature"]["classification"]["execution"],
        "tests"
    );
    assert_eq!(setup_group["priority"], "low");
    let test_candidates: Vec<_> = candidates
        .iter()
        .filter(|candidate| {
            candidate["detector"] == "error_path"
                && queue["classifications"][candidate["id"].as_str().unwrap()]["execution"]
                    == "tests"
        })
        .collect();
    assert_eq!(test_candidates.len(), 16);
    for id in setup_group["members"]["candidate_ids"].as_array().unwrap() {
        assert!(
            test_candidates
                .iter()
                .any(|candidate| candidate["id"] == *id)
        );
    }
    let ordinary = candidates
        .iter()
        .find(|candidate| {
            candidate["detector"] == "error_path"
                && queue["classifications"][candidate["id"].as_str().unwrap()]["code_role"]
                    == "runtime"
                && candidate["snippet"]
                    .as_str()
                    .is_some_and(|snippet| snippet.contains("None::<u8>"))
        })
        .unwrap();
    let ordinary_id = ordinary["id"].as_str().unwrap();
    assert!(
        !setup_group["members"]["candidate_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == ordinary_id)
    );
    let production_unit = units
        .iter()
        .find(|unit| unit["members"]["candidate_id"] == ordinary_id)
        .unwrap();
    assert_eq!(production_unit["priority"], "high");
}

#[test]
fn language_scan_dry_run_apply_and_check_are_end_to_end() {
    let temp = TempDir::new("language-cli-flow");
    init_repo(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let source_path = temp.path().join("src/message.rs");
    fs::write(&source_path, "// Human message\npub fn value() {}\n").unwrap();

    let scan_path = temp.path().join("language-scan.json");
    let scan_path_arg = scan_path.to_string_lossy().into_owned();
    let root_arg = temp.path().to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_cli_in(
        Some(temp.path()),
        &[
            "--json",
            "language",
            "scan",
            "--root",
            &root_arg,
            "--path",
            "src/message.rs",
            "--out",
            &scan_path_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["result"]["candidates"], json!(1));
    let scan: Value = serde_json::from_slice(&fs::read(&scan_path).unwrap()).unwrap();
    let candidate = scan["candidates"][0].clone();
    let decisions_path = temp.path().join("language-decisions.json");
    fs::write(
        &decisions_path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "decisions": [{
                "candidate": candidate,
                "action": "replace",
                "replacement": "Русское сообщение",
                "reason": "Подтверждён перевод человеческого комментария"
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let decisions_arg = decisions_path.to_string_lossy().into_owned();

    let (code, stdout, stderr) = run_cli_in(
        Some(temp.path()),
        &[
            "language",
            "apply",
            "--root",
            &root_arg,
            "--decisions",
            &decisions_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        fs::read_to_string(&source_path)
            .unwrap()
            .contains("Human message")
    );

    let (code, stdout, stderr) = run_cli_in(
        Some(temp.path()),
        &[
            "language",
            "apply",
            "--root",
            &root_arg,
            "--decisions",
            &decisions_arg,
            "--apply",
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        fs::read_to_string(&source_path)
            .unwrap()
            .contains("Русское сообщение")
    );

    let (code, stdout, stderr) = run_cli_in(
        Some(temp.path()),
        &[
            "--json",
            "language",
            "check",
            "--root",
            &root_arg,
            "--scan",
            &scan_path_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let result = parse_json(&stdout)["result"].clone();
    assert_eq!(result["candidates"], json!([]));
    assert_eq!(result["candidates_total"], json!(0));
    assert_eq!(result["files_total"], json!(1));
    assert!(result.get("scan").is_none());
}

#[test]
fn language_check_bounds_stdout_and_keeps_full_scan_in_requested_artifact() {
    let temp = TempDir::new("language-check-bounded-output");
    init_repo(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let root_arg = temp.path().to_string_lossy().into_owned();
    let source_path = temp.path().join("src/messages.rs");
    let source = (0..25)
        .map(|index| {
            if index == 0 {
                format!(
                    "// Human message number {index:02} {}\n",
                    "additional ".repeat(80)
                )
            } else {
                format!("// Human message number {index:02}\n")
            }
        })
        .collect::<String>();
    fs::write(&source_path, source).unwrap();

    let scan_path = temp.path().join("language-scan.json");
    let scan_path_arg = scan_path.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_cli_in(
        Some(temp.path()),
        &[
            "--json",
            "language",
            "scan",
            "--root",
            &root_arg,
            "--path",
            "src/messages.rs",
            "--out",
            &scan_path_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let scan: Value = serde_json::from_slice(&fs::read(&scan_path).unwrap()).unwrap();
    assert_eq!(scan["candidates"].as_array().unwrap().len(), 25);

    let (code, stdout, stderr) = run_cli_in(
        Some(temp.path()),
        &[
            "--json",
            "language",
            "check",
            "--root",
            &root_arg,
            "--scan",
            &scan_path_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let result = parse_json(&stdout)["result"].clone();
    assert_eq!(result["files_total"], json!(1));
    assert_eq!(result["candidates_total"], json!(25));
    assert_eq!(result["candidates_truncated"], json!(true));
    assert_eq!(result["candidates"].as_array().unwrap().len(), 20);
    assert!(result.get("scan").is_none());
    assert!(result.get("files").is_none());
    assert!(result.get("skipped").is_none());
    assert!(
        result["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|candidate| candidate["text"].as_str().unwrap().chars().count() <= 121)
    );
    assert_eq!(result["candidates"][0]["text_truncated"], json!(true));

    let human = run_cli_in(
        Some(temp.path()),
        &[
            "language",
            "check",
            "--root",
            &root_arg,
            "--scan",
            &scan_path_arg,
        ],
    );
    assert_eq!(human.0, 0, "stdout: {}\nstderr: {}", human.1, human.2);
    assert!(human.1.contains("Показано кандидатов: 20 из 25"));
    assert!(human.1.contains("Выборка усечена: да"));
    assert!(!human.1.contains("Human message number 24"));

    let updated_path = temp.path().join("language-check.json");
    let updated_path_arg = updated_path.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_cli_in(
        Some(temp.path()),
        &[
            "--json",
            "language",
            "check",
            "--root",
            &root_arg,
            "--scan",
            &scan_path_arg,
            "--out",
            &updated_path_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let bounded_stdout = parse_json(&stdout)["result"].clone();
    assert_eq!(bounded_stdout["candidates"].as_array().unwrap().len(), 20);
    let full_scan: Value = serde_json::from_slice(&fs::read(updated_path).unwrap()).unwrap();
    assert_eq!(full_scan["candidates"].as_array().unwrap().len(), 25);
}

fn boundary_fixture(name: &str) -> (TempDir, String, String) {
    let repo = TempDir::new(name);
    init_repo(&repo);
    write_cargo_project(repo.path());
    commit(repo.path(), "база");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn first(path: &str) { let _ = std::fs::read(path).unwrap(); }\npub fn second(path: &str) { let _ = std::fs::read(path).unwrap(); }\n",
    )
    .unwrap();
    commit(repo.path(), "добавить границы ввода");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    (repo, base, head)
}

fn assert_review_failure(result: (i32, String, String), expected: &str) {
    let (code, stdout, stderr) = result;
    let expected_exit = if expected == "review_artifact_conflict" {
        7
    } else {
        3
    };
    assert_eq!(code, expected_exit, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["error"]["code"], expected);
}

fn review_success(result: (i32, String, String)) -> Value {
    let (code, stdout, stderr) = result;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    parse_json(&stdout)["result"].clone()
}

fn ignored_review_fixture(name: &str) -> (TempDir, String, String) {
    let repo = TempDir::new(name);
    init_repo(&repo);
    write_cargo_project(repo.path());
    fs::write(
        repo.path().join(".gitignore"),
        "/target/\n/.anki-repo/review/\n",
    )
    .unwrap();
    commit(repo.path(), "база с локальными артефактами ревью");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn value(path: &str) { let _ = std::fs::read(path).unwrap(); }\n",
    )
    .unwrap();
    commit(repo.path(), "изменение для ревью");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    (repo, base, head)
}

fn prepare_execution_job(root: &Path, pack: &Path, scope: &str) -> PathBuf {
    let prepared = review_success(run_cli_in(
        Some(root),
        &[
            "--json",
            "code-review",
            "execution",
            "prepare",
            "--pack",
            pack.to_str().unwrap(),
            "--mode",
            "isolated_checks",
            "--scope",
            scope,
        ],
    ));
    root.join(prepared["job_directory"].as_str().unwrap())
}

fn run_execution_job(
    root: &Path,
    job: &Path,
    timeout: &str,
    argv: &[&str],
) -> (i32, String, String) {
    let mut arguments = vec![
        "--json",
        "code-review",
        "execution",
        "run",
        job.to_str().unwrap(),
        "--timeout-seconds",
        timeout,
        "--",
    ];
    arguments.extend_from_slice(argv);
    run_cli_in(Some(root), &arguments)
}

fn collect_pr_workspace(root: &Path, base: &str, head: &str, pr: &str) -> PathBuf {
    let result = review_success(run_cli_in(
        Some(root),
        &[
            "--json",
            "code-review",
            "collect",
            "--base",
            base,
            "--head",
            head,
            "--pr-number",
            pr,
        ],
    ));
    let relative = Path::new(result["artifact_dir"].as_str().unwrap());
    assert_eq!(relative, Path::new(".anki-repo/review").join(pr).join(head));
    root.join(relative)
}

#[cfg(unix)]
#[test]
fn pr_review_pipeline_keeps_all_artifacts_and_job_in_returned_workspace() {
    let (repo, base, head) = ignored_review_fixture("pr-review-pipeline");
    let artifacts = collect_pr_workspace(repo.path(), &base, &head, "23");
    let pack = artifacts.join("review.json");
    let queue = artifacts.join("review-queue.json");
    let evidence = fs::read(&pack).unwrap();
    let queue_bytes = fs::read(&queue).unwrap();
    let triage = artifacts.join("semantic-triage.input.json");
    let canonical = artifacts.join("semantic-triage.json");
    let report = artifacts.join("review-report.md");
    review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "queue",
            "validate",
            "--pack",
            pack.to_str().unwrap(),
            "--queue",
            queue.to_str().unwrap(),
        ],
    ));
    review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "triage",
            "init",
            "--pack",
            pack.to_str().unwrap(),
            "--out",
            triage.to_str().unwrap(),
        ],
    ));
    review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "triage",
            "validate",
            "--pack",
            pack.to_str().unwrap(),
            "--triage",
            triage.to_str().unwrap(),
            "--canonical-out",
            canonical.to_str().unwrap(),
        ],
    ));
    let report_arguments = [
        "--json",
        "code-review",
        "triage",
        "report",
        "--pack",
        pack.to_str().unwrap(),
        "--triage",
        canonical.to_str().unwrap(),
        "--out",
        report.to_str().unwrap(),
    ];
    review_success(run_cli_in(Some(repo.path()), &report_arguments));
    let report_bytes = fs::read(&report).unwrap();
    review_success(run_cli_in(Some(repo.path()), &report_arguments));
    assert_eq!(fs::read(&report).unwrap(), report_bytes);

    let prepared = review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "execution",
            "prepare",
            "--pack",
            pack.to_str().unwrap(),
            "--mode",
            "isolated_checks",
            "--scope",
            "contracts",
        ],
    ));
    let job = repo
        .path()
        .join(prepared["job_directory"].as_str().unwrap());
    assert_eq!(job.parent().unwrap(), artifacts.join("runs"));
    assert_eq!(
        job.file_name().unwrap(),
        prepared["job_id"].as_str().unwrap()
    );
    let manifest: Value = serde_json::from_slice(&fs::read(job.join("job.json")).unwrap()).unwrap();
    assert_eq!(manifest["namespace"], "23");
    assert_eq!(manifest["source"]["snapshot"]["head_sha"], head);
    assert_eq!(
        manifest["source"]["review_pack_sha256"],
        format!("{:x}", sha2::Sha256::digest(&evidence))
    );
    assert_eq!(
        repo.path().join(prepared["worktree"].as_str().unwrap()),
        job.join("worktree")
    );
    assert_eq!(git(&job.join("worktree"), &["rev-parse", "HEAD"]), head);
    let run = review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "execution",
            "run",
            job.to_str().unwrap(),
            "--timeout-seconds",
            "10",
            "--",
            "/bin/sh",
            "-c",
            "printf 'synthetic report\\n' > \"$TMPDIR/subagent-report.md\"; printf 'isolated output\\n'",
        ],
    ));
    assert_eq!(run["status"], "passed");
    assert_eq!(run["namespace"], "23");
    assert_eq!(run["source"]["snapshot"]["head_sha"], head);
    let subagent_report = job.join("tmp/subagent-report.md");
    assert_eq!(
        fs::read_to_string(&subagent_report).unwrap(),
        "synthetic report\n"
    );
    assert!(subagent_report.starts_with(&artifacts));
    assert_eq!(
        fs::read_to_string(job.join(run["stdout"]["log"].as_str().unwrap())).unwrap(),
        "isolated output\n"
    );
    assert!(job.join("result.json").is_file());
    assert!(!review_workspace(repo.path(), &head).exists());
    assert_eq!(fs::read(&pack).unwrap(), evidence);
    assert_eq!(fs::read(&queue).unwrap(), queue_bytes);
    assert!(
        git(repo.path(), &["check-ignore", artifacts.to_str().unwrap()])
            .contains(".anki-repo/review/23/")
    );
    assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
    assert!(git(repo.path(), &["ls-files", "--", ".anki-repo/review"]).is_empty());
    review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "execution",
            "cleanup",
            job.to_str().unwrap(),
        ],
    ));
}

#[cfg(unix)]
#[test]
fn execution_run_exit_codes_distinguish_failed_incomplete_timeout_and_unavailable() {
    let (repo, base, head) = ignored_review_fixture("execution-exit-codes");
    let artifacts = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &artifacts, false);
    let pack = artifacts.join("review.json");

    let failed_job = prepare_execution_job(repo.path(), &pack, "failed");
    let (code, stdout, stderr) =
        run_execution_job(repo.path(), &failed_job, "10", &["/bin/sh", "-c", "exit 1"]);
    assert_eq!(code, 9, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["result"]["status"], "failed");

    let incomplete_job = prepare_execution_job(repo.path(), &pack, "incomplete");
    let (code, stdout, stderr) = run_execution_job(
        repo.path(),
        &incomplete_job,
        "10",
        &["/bin/sh", "-c", "printf '\\n' >> src/lib.rs"],
    );
    assert_eq!(code, 10, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["result"]["status"], "incomplete");

    let timeout_job = prepare_execution_job(repo.path(), &pack, "timeout");
    let (code, stdout, stderr) =
        run_execution_job(repo.path(), &timeout_job, "1", &["/bin/sleep", "5"]);
    assert_eq!(code, 11, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["result"]["status"], "timed_out");

    let unavailable_job = prepare_execution_job(repo.path(), &pack, "unavailable");
    let (code, stdout, stderr) = run_execution_job(
        repo.path(),
        &unavailable_job,
        "10",
        &["/missing/anki-repo-execution-command"],
    );
    assert_eq!(code, 127, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["result"]["status"], "unavailable");
}

#[cfg(unix)]
#[test]
fn execution_run_sigterm_cancels_and_stops_the_child_process_group() {
    use rustix::process::{Pid, Signal, kill_process, test_kill_process_group};

    let (repo, base, head) = ignored_review_fixture("execution-sigterm-cancellation");
    let artifacts = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &artifacts, false);
    let pack = artifacts.join("review.json");
    let job = prepare_execution_job(repo.path(), &pack, "sigterm");
    let mut runner = Command::new(cli_binary())
        .current_dir(repo.path())
        .args([
            "--json",
            "code-review",
            "execution",
            "run",
            job.to_str().unwrap(),
            "--timeout-seconds",
            "30",
            "--",
            "/bin/sh",
            "-c",
            "printf '%s' \"$$\" > \"$TMPDIR/child.pid\"; exec /bin/sleep 30",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let child_pid_path = job.join("tmp/child.pid");
    let startup_deadline = Instant::now() + Duration::from_secs(10);
    while !child_pid_path.is_file() {
        if let Some(status) = runner.try_wait().unwrap() {
            let output = runner.wait_with_output().unwrap();
            panic!(
                "execution run закончился до запуска команды: {status:?}; stdout: {}; stderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            Instant::now() < startup_deadline,
            "execution run не запустил дочернюю команду"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let process_group = Pid::from_raw(
        fs::read_to_string(&child_pid_path)
            .unwrap()
            .parse::<i32>()
            .unwrap(),
    )
    .unwrap();
    assert!(test_kill_process_group(process_group).is_ok());

    let runner_pid = Pid::from_raw(runner.id() as i32).unwrap();
    kill_process(runner_pid, Signal::TERM).unwrap();
    let completion_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if runner.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= completion_deadline {
            let _ = rustix::process::kill_process_group(process_group, Signal::KILL);
            let _ = runner.kill();
            let _ = runner.wait();
            panic!("execution run не обработал SIGTERM своевременно");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = runner.wait_with_output().unwrap();
    if output.status.code() != Some(12) {
        let _ = rustix::process::kill_process_group(process_group, Signal::KILL);
    }
    assert_eq!(
        output.status.code(),
        Some(12),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = parse_json(&String::from_utf8(output.stdout).unwrap())["result"].clone();
    assert_eq!(result["status"], "cancelled");
    assert!(test_kill_process_group(process_group).is_err());
}

#[test]
fn verify_new_head_uses_separate_pr_workspace_and_keeps_previous_documents() {
    let (repo, base, head) = ignored_review_fixture("pr-review-next-head");
    let previous = collect_pr_workspace(repo.path(), &base, &head, "31");
    let baseline = previous.join("review.json");
    let triage = previous.join("semantic-triage.input.json");
    review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "triage",
            "init",
            "--pack",
            baseline.to_str().unwrap(),
            "--out",
            triage.to_str().unwrap(),
        ],
    ));
    let canonical = previous.join("semantic-triage.json");
    let report = previous.join("review-report.md");
    review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "triage",
            "validate",
            "--pack",
            baseline.to_str().unwrap(),
            "--triage",
            triage.to_str().unwrap(),
            "--canonical-out",
            canonical.to_str().unwrap(),
        ],
    ));
    review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "triage",
            "report",
            "--pack",
            baseline.to_str().unwrap(),
            "--triage",
            canonical.to_str().unwrap(),
            "--out",
            report.to_str().unwrap(),
        ],
    ));
    let saved = [
        "review.json",
        "review-queue.json",
        "review.txt",
        "semantic-triage.input.json",
        "semantic-triage.json",
        "review-report.md",
    ]
    .map(|name| (name, fs::read(previous.join(name)).unwrap()));
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn value() -> u8 { 7 }\n",
    )
    .unwrap();
    commit(repo.path(), "следующая версия того же PR");
    let next_head = git(repo.path(), &["rev-parse", "HEAD"]);
    assert_ne!(head, next_head);
    assert_review_failure(
        run_cli_in(
            Some(repo.path()),
            &[
                "--json",
                "code-review",
                "verify",
                "--baseline",
                baseline.to_str().unwrap(),
                "--head",
                &next_head,
                "--pr-number",
                "32",
            ],
        ),
        "invalid_request",
    );
    assert!(!repo.path().join(".anki-repo/review/32").exists());
    assert!(
        !repo
            .path()
            .join(".anki-repo/review/31")
            .join(&next_head)
            .exists()
    );
    let result = review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "verify",
            "--baseline",
            baseline.to_str().unwrap(),
            "--head",
            &next_head,
        ],
    ));
    let next = repo.path().join(result["artifact_dir"].as_str().unwrap());
    assert_eq!(
        next,
        repo.path().join(".anki-repo/review/31").join(&next_head)
    );
    for name in [
        "review.json",
        "review-queue.json",
        "review.txt",
        "delta.json",
    ] {
        assert!(next.join(name).is_file(), "missing {name}");
    }
    assert!(!next.join("semantic-triage.input.json").exists());
    assert!(!next.join("semantic-triage.json").exists());
    assert!(!next.join("review-report.md").exists());
    assert_review_failure(
        run_cli_in(
            Some(repo.path()),
            &[
                "--json",
                "code-review",
                "triage",
                "validate",
                "--pack",
                next.join("review.json").to_str().unwrap(),
                "--triage",
                triage.to_str().unwrap(),
                "--canonical-out",
                next.join("semantic-triage.json").to_str().unwrap(),
            ],
        ),
        "review_artifact_invalid",
    );
    assert!(!next.join("semantic-triage.json").exists());
    for (name, bytes) in saved {
        assert_eq!(fs::read(previous.join(name)).unwrap(), bytes);
    }
    assert!(!review_workspace(repo.path(), &next_head).exists());
    assert_eq!(
        fs::read_dir(repo.path().join(".anki-repo/review/31"))
            .unwrap()
            .count(),
        2
    );
    assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
}

#[cfg(unix)]
#[test]
fn execution_prepare_inherits_local_namespace_when_source_has_no_pr() {
    let (repo, base, head) = ignored_review_fixture("execution-inherit-local");
    let artifacts = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &artifacts, false);
    let pack = artifacts.join("review.json");
    let prepared = review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "execution",
            "prepare",
            "--pack",
            pack.to_str().unwrap(),
            "--mode",
            "isolated_checks",
            "--scope",
            "local-test",
        ],
    ));
    let job = repo
        .path()
        .join(prepared["job_directory"].as_str().unwrap());
    assert_eq!(job.parent().unwrap(), artifacts.join("runs"));
    let manifest: Value = serde_json::from_slice(&fs::read(job.join("job.json")).unwrap()).unwrap();
    assert_eq!(manifest["namespace"], "local");
    review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "execution",
            "cleanup",
            job.to_str().unwrap(),
        ],
    ));
}

#[cfg(unix)]
#[test]
fn execution_rejects_namespace_and_pack_path_mismatch_before_creating_any_job() {
    let (repo, base, head) = ignored_review_fixture("execution-source-namespace");
    let artifacts = collect_pr_workspace(repo.path(), &base, &head, "37");
    let pack = artifacts.join("review.json");
    let local = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &local, false);
    let local_pack = local.join("review.json");
    let outside = TempDir::new("execution-outside-pack");
    let copied = outside.path().join("review.json");
    fs::copy(&pack, &copied).unwrap();
    let sibling = artifacts.join("copied-review.json");
    fs::copy(&pack, &sibling).unwrap();
    let dotdot = artifacts.join("../").join(&head).join("review.json");
    let worktrees = git(repo.path(), &["worktree", "list", "--porcelain"]);
    let status = git(repo.path(), &["status", "--porcelain=v1"]);
    for (source, requested) in [
        (pack.as_path(), Some("38")),
        (pack.as_path(), Some("local")),
        (local_pack.as_path(), Some("37")),
        (copied.as_path(), None),
        (sibling.as_path(), None),
        (dotdot.as_path(), None),
    ] {
        let mut args = vec![
            "--json",
            "code-review",
            "execution",
            "prepare",
            "--pack",
            source.to_str().unwrap(),
            "--mode",
            "isolated_checks",
            "--scope",
            "source-test",
        ];
        if let Some(pr) = requested {
            args.extend(["--pr-number", pr]);
        }
        assert_review_failure(run_cli_in(Some(repo.path()), &args), "invalid_request");
        assert!(!artifacts.join("runs").exists());
        assert!(!local.join("runs").exists());
        assert!(!repo.path().join(".anki-repo/review/38").exists());
        assert_eq!(
            git(repo.path(), &["worktree", "list", "--porcelain"]),
            worktrees
        );
        assert_eq!(git(repo.path(), &["status", "--porcelain=v1"]), status);
    }
}

#[test]
fn pr_derived_outputs_reject_foreign_namespace_traversal_and_conflicting_evidence() {
    let (repo, base, head) = ignored_review_fixture("pr-derived-output-boundary");
    let artifacts = collect_pr_workspace(repo.path(), &base, &head, "43");
    let pack = artifacts.join("review.json");
    let evidence = fs::read(&pack).unwrap();
    let triage = artifacts.join("semantic-triage.input.json");
    review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "triage",
            "init",
            "--pack",
            pack.to_str().unwrap(),
            "--out",
            triage.to_str().unwrap(),
        ],
    ));
    let outside = TempDir::new("pr-derived-output-external");
    let external = outside.path().join("review-report.md");
    let foreign = repo
        .path()
        .join(".anki-repo/review/44")
        .join(&head)
        .join("review-report.md");
    let local = review_workspace(repo.path(), &head).join("review-report.md");
    let tracked = repo.path().join("Cargo.toml");
    let tracked_bytes = fs::read(&tracked).unwrap();
    let traversal = artifacts.join("../").join(&head).join("review-report.md");
    for output in [&external, &foreign, &local, &tracked, &traversal, &pack] {
        assert_review_failure(
            run_cli_in(
                Some(repo.path()),
                &[
                    "--json",
                    "code-review",
                    "triage",
                    "report",
                    "--pack",
                    pack.to_str().unwrap(),
                    "--triage",
                    triage.to_str().unwrap(),
                    "--out",
                    output.to_str().unwrap(),
                ],
            ),
            "invalid_request",
        );
        assert_eq!(fs::read(&pack).unwrap(), evidence);
        assert_eq!(fs::read(&tracked).unwrap(), tracked_bytes);
        assert!(!artifacts.join("review-report.md").exists());
        assert!(!external.exists());
        assert!(!repo.path().join(".anki-repo/review/44").exists());
        assert!(!review_workspace(repo.path(), &head).exists());
    }
    let report = artifacts.join("review-report.md");
    fs::write(&report, "Чужой документ без ownership marker.\n").unwrap();
    let foreign_bytes = fs::read(&report).unwrap();
    assert_review_failure(
        run_cli_in(
            Some(repo.path()),
            &[
                "--json",
                "code-review",
                "triage",
                "report",
                "--pack",
                pack.to_str().unwrap(),
                "--triage",
                triage.to_str().unwrap(),
                "--out",
                report.to_str().unwrap(),
            ],
        ),
        "review_artifact_conflict",
    );
    assert_eq!(fs::read(&report).unwrap(), foreign_bytes);
    assert_eq!(fs::read(&pack).unwrap(), evidence);
    assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
}

#[cfg(unix)]
#[test]
fn pr_report_symlink_does_not_write_to_external_file() {
    use std::os::unix::fs::symlink;

    let (repo, base, head) = ignored_review_fixture("pr-report-symlink");
    let artifacts = collect_pr_workspace(repo.path(), &base, &head, "47");
    let pack = artifacts.join("review.json");
    let triage = artifacts.join("semantic-triage.input.json");
    review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "triage",
            "init",
            "--pack",
            pack.to_str().unwrap(),
            "--out",
            triage.to_str().unwrap(),
        ],
    ));
    let outside = TempDir::new("pr-report-symlink-outside");
    let target = outside.path().join("document.md");
    fs::write(&target, "Внешний документ.\n").unwrap();
    let bytes = fs::read(&target).unwrap();
    let report = artifacts.join("review-report.md");
    symlink(&target, &report).unwrap();
    assert_review_failure(
        run_cli_in(
            Some(repo.path()),
            &[
                "--json",
                "code-review",
                "triage",
                "report",
                "--pack",
                pack.to_str().unwrap(),
                "--triage",
                triage.to_str().unwrap(),
                "--out",
                report.to_str().unwrap(),
            ],
        ),
        "review_artifact_conflict",
    );
    assert_eq!(fs::read(&target).unwrap(), bytes);
    assert!(
        fs::symlink_metadata(&report)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn collect_rejects_arbitrary_output_before_writing_or_running_clippy() {
    let (repo, base, head) = boundary_fixture("collect-output-boundary");
    let outside = TempDir::new("collect-output-outside");
    let tracked = repo.path().join("Cargo.toml");
    let arbitrary = repo.path().join("arbitrary/nested");
    let external = outside.path().join("output");
    let dotdot = review_workspace(repo.path(), &head).join("../").join(&head);
    let cargo_bytes = fs::read(&tracked).unwrap();
    let status = git(repo.path(), &["status", "--porcelain=v1"]);
    let index = git(repo.path(), &["ls-files", "--stage"]);
    for output in [&arbitrary, &tracked, &external, &dotdot] {
        assert_review_failure(
            run_cli_in(
                Some(repo.path()),
                &[
                    "--json",
                    "code-review",
                    "collect",
                    "--base",
                    &base,
                    "--head",
                    &head,
                    "--run-clippy",
                    "--out-dir",
                    output.to_str().unwrap(),
                ],
            ),
            "invalid_request",
        );
        assert_eq!(fs::read(&tracked).unwrap(), cargo_bytes);
        assert!(!arbitrary.exists());
        assert!(!external.exists());
        assert!(!repo.path().join(".anki-repo").exists());
        assert!(!repo.path().join("target").exists());
        assert_eq!(git(repo.path(), &["status", "--porcelain=v1"]), status);
        assert_eq!(git(repo.path(), &["ls-files", "--stage"]), index);
    }
}

#[cfg(unix)]
#[test]
fn collect_rejects_symlink_namespace_before_any_artifact_write() {
    use std::os::unix::fs::symlink;

    let (repo, base, head) = boundary_fixture("collect-symlink-namespace");
    let outside = TempDir::new("collect-symlink-outside");
    fs::create_dir_all(repo.path().join(".anki-repo/review")).unwrap();
    symlink(outside.path(), repo.path().join(".anki-repo/review/local")).unwrap();
    let output = review_workspace(repo.path(), &head);
    assert_review_failure(
        run_cli_in(
            Some(repo.path()),
            &[
                "--json",
                "code-review",
                "collect",
                "--base",
                &base,
                "--head",
                &head,
                "--out-dir",
                output.to_str().unwrap(),
            ],
        ),
        "invalid_request",
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    assert!(!outside.path().join(&head).exists());
}

#[test]
fn collect_rejects_noncanonical_pr_number_before_creating_workspace() {
    let (repo, base, head) = boundary_fixture("collect-pr-leading-zero");
    let (code, stdout, stderr) = run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "collect",
            "--base",
            &base,
            "--head",
            &head,
            "--pr-number",
            "017",
        ],
    );
    assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["error"]["code"], "invalid_request");
    assert!(!repo.path().join(".anki-repo/review").exists());
}

#[test]
fn collect_pr_artifacts_all_belong_to_namespace_and_full_head() {
    let (repo, base, head) = boundary_fixture("collect-pr-namespace");
    let (code, stdout, stderr) = run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "collect",
            "--base",
            &base,
            "--head",
            &head,
            "--pr-number",
            "17",
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["result"]["target"]["head_sha"], head);
    let namespace = repo.path().join(".anki-repo/review/17");
    assert_eq!(fs::read_dir(&namespace).unwrap().count(), 1);
    let output = namespace.join(&head);
    let files = fs::read_dir(&output)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != ".writer.lock")
        .collect::<BTreeSet<_>>();
    assert_eq!(
        files,
        BTreeSet::from([
            "review.json".into(),
            "review-queue.json".into(),
            "review.txt".into()
        ])
    );
    assert!(!review_workspace(repo.path(), &head).exists());
}

#[test]
fn queue_cli_rejects_consistent_forgery_but_marks_structure_only_explicitly() {
    use anki_repo::code_review::model::ReviewPack;
    use anki_repo::code_review::review_queue::{
        self, ClassificationBasis, CodeRole, SyntaxContext,
    };
    use anki_repo::code_review::scope::FileSurface;

    let (repo, base, _) = boundary_fixture("queue-authenticity-boundary");
    let mut source = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    source.push_str("#[cfg(test)] mod tests { #[test] fn setup() {\n");
    for _ in 0..3 {
        source.push_str("let _ = Result::<(), &str>::Err(\"fixture\").unwrap();\n");
    }
    source.push_str("} }\n");
    fs::write(repo.path().join("src/lib.rs"), source).unwrap();
    commit(
        repo.path(),
        "Добавить синтетическую группу тестовых сигналов",
    );
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let output = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &output, false);
    let pack_path = output.join("review.json");
    let queue_path = output.join("review-queue.json");
    let arguments = [
        "--json",
        "code-review",
        "queue",
        "validate",
        "--pack",
        pack_path.to_str().unwrap(),
        "--queue",
        queue_path.to_str().unwrap(),
    ];
    let (code, stdout, stderr) = run_cli_in(Some(repo.path()), &arguments);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        parse_json(&stdout)["result"]["syntax_authenticity"],
        "verified"
    );
    let authentic: review_queue::ReviewQueue =
        serde_json::from_slice(&fs::read(&queue_path).unwrap()).unwrap();
    let authentic_group = authentic.units.iter().find(|unit| unit.is_group()).unwrap();
    let authentic_candidate = authentic_group.candidate_ids()[0].clone();
    for command in ["list", "summary", "group", "candidate"] {
        let mut read_arguments = arguments.to_vec();
        read_arguments[3] = command;
        if command == "group" || command == "candidate" {
            read_arguments.extend([
                "--id",
                if command == "group" {
                    &authentic_group.id
                } else {
                    &authentic_candidate
                },
            ]);
        }
        let result = review_success(run_cli_in(Some(repo.path()), &read_arguments));
        assert_eq!(result["source_digest_valid"], true, "{command}: {result}");
        assert_eq!(
            result["syntax_authenticity"], "verified",
            "{command}: {result}"
        );
    }
    let bytes = fs::read(&pack_path).unwrap();
    let pack: ReviewPack = serde_json::from_slice(&bytes).unwrap();
    let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
    let contexts = pack
        .all_candidates()
        .iter()
        .map(|candidate| {
            (
                candidate.id.clone(),
                SyntaxContext {
                    execution: Some(FileSurface::Tests),
                    code_role: CodeRole::TestSetup,
                    text_role: None,
                    signature: Some("method:unwrap".into()),
                    basis: ClassificationBasis::SyntaxContext,
                },
            )
        })
        .collect();
    let forged = review_queue::build(&pack, &digest, &contexts).unwrap();
    assert!(review_queue::validate(&forged, &pack, &digest).is_ok());
    assert_eq!(forged.summary.group_units, 1);
    fs::write(&queue_path, serde_json::to_vec_pretty(&forged).unwrap()).unwrap();
    let forged_bytes = fs::read(&queue_path).unwrap();
    assert_review_failure(
        run_cli_in(Some(repo.path()), &arguments),
        "review_artifact_invalid",
    );
    let group_id = forged
        .units
        .iter()
        .find(|unit| unit.is_group())
        .unwrap()
        .id
        .clone();
    let candidate_id = pack.all_candidates()[0].id.clone();
    for command in ["validate", "list", "summary", "group", "candidate"] {
        let mut read_arguments = arguments.to_vec();
        read_arguments[3] = command;
        if command == "group" || command == "candidate" {
            read_arguments.extend([
                "--id",
                if command == "group" {
                    &group_id
                } else {
                    &candidate_id
                },
            ]);
        }
        assert_review_failure(
            run_cli_in(Some(repo.path()), &read_arguments),
            "review_artifact_invalid",
        );
        read_arguments.push("--structure-only");
        let (code, stdout, stderr) = run_cli_in(Some(repo.path()), &read_arguments);
        assert_eq!(code, 0, "{command}: stdout: {stdout}\nstderr: {stderr}");
        let result = parse_json(&stdout)["result"].clone();
        assert_eq!(result["source_digest_valid"], true, "{command}: {result}");
        assert_eq!(
            result["syntax_authenticity"], "structure_only",
            "{command}: {result}"
        );
        assert_eq!(fs::read(&queue_path).unwrap(), forged_bytes);
        assert_eq!(fs::read(&pack_path).unwrap(), bytes);
    }
    assert_eq!(fs::read(&queue_path).unwrap(), forged_bytes);
    assert_eq!(fs::read(&pack_path).unwrap(), bytes);
}

#[test]
fn queue_validation_requires_git_images_unless_structure_only_is_requested() {
    let (repo, base, head) = boundary_fixture("queue-missing-git-images");
    let output = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &output, false);
    let outside = TempDir::new("queue-without-repository");
    let pack = outside.path().join("review.json");
    let queue = outside.path().join("review-queue.json");
    fs::copy(output.join("review.json"), &pack).unwrap();
    fs::copy(output.join("review-queue.json"), &queue).unwrap();
    let arguments = [
        "--json",
        "code-review",
        "queue",
        "validate",
        "--pack",
        pack.to_str().unwrap(),
        "--queue",
        queue.to_str().unwrap(),
    ];
    let (code, stdout, stderr) = run_cli_in(Some(outside.path()), &arguments);
    assert_review_failure((code, stdout, stderr), "syntax_authenticity_unavailable");
    let mut explicit = arguments.to_vec();
    explicit.push("--structure-only");
    let (code, stdout, stderr) = run_cli_in(Some(outside.path()), &explicit);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        parse_json(&stdout)["result"]["syntax_authenticity"],
        "structure_only"
    );
    explicit.remove(0);
    let (code, stdout, stderr) = run_cli_in(Some(outside.path()), &explicit);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("structure_only"), "{stdout}");
}

#[test]
fn queue_parse_failure_requires_explicit_structure_only() {
    let (repo, base, _) = boundary_fixture("queue-unparseable-git-image");
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn broken() { let _ = std::fs::read(\"x\").unwrap();\n",
    )
    .unwrap();
    commit(repo.path(), "сохранить незавершённый исходник");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let output = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &output, false);
    let pack = output.join("review.json");
    let queue = output.join("review-queue.json");
    let arguments = [
        "--json",
        "code-review",
        "queue",
        "validate",
        "--pack",
        pack.to_str().unwrap(),
        "--queue",
        queue.to_str().unwrap(),
    ];
    assert_review_failure(
        run_cli_in(Some(repo.path()), &arguments),
        "syntax_authenticity_unavailable",
    );
    let mut explicit = arguments.to_vec();
    explicit.push("--structure-only");
    let (code, stdout, stderr) = run_cli_in(Some(repo.path()), &explicit);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        parse_json(&stdout)["result"]["syntax_authenticity"],
        "structure_only"
    );
}

#[test]
fn verify_and_delta_allow_external_input_but_reject_writes_outside_review_workspace() {
    let (repo, base, head) = boundary_fixture("verify-delta-output-boundary");
    let output = review_workspace(repo.path(), &head);
    collect_pack(repo.path(), &base, &head, &output, false);
    let outside = TempDir::new("verify-delta-external-input");
    let baseline = outside.path().join("downloaded-review.json");
    fs::copy(output.join("review.json"), &baseline).unwrap();
    let pack = output.join("review.json");
    let original = fs::read(&pack).unwrap();
    let cargo = repo.path().join("Cargo.toml");
    let cargo_bytes = fs::read(&cargo).unwrap();
    let arbitrary = repo.path().join("unexpected/nested");
    let external = outside.path().join("output");
    for target in [&cargo, &arbitrary, &external] {
        assert_review_failure(
            run_cli_in(
                Some(repo.path()),
                &[
                    "--json",
                    "code-review",
                    "verify",
                    "--baseline",
                    baseline.to_str().unwrap(),
                    "--head",
                    &head,
                    "--run-clippy",
                    "--out-dir",
                    target.to_str().unwrap(),
                ],
            ),
            "invalid_request",
        );
        assert_review_failure(
            run_cli_in(
                Some(repo.path()),
                &[
                    "--json",
                    "code-review",
                    "delta",
                    "--before",
                    baseline.to_str().unwrap(),
                    "--after",
                    pack.to_str().unwrap(),
                    "--out",
                    target.to_str().unwrap(),
                ],
            ),
            "invalid_request",
        );
        assert_eq!(fs::read(&cargo).unwrap(), cargo_bytes);
        assert_eq!(fs::read(&pack).unwrap(), original);
        assert!(!arbitrary.exists());
        assert!(!external.exists());
        assert!(!output.join("delta.json").exists());
        assert!(!repo.path().join("target").exists());
    }
    let (code, stdout, stderr) = run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "delta",
            "--before",
            baseline.to_str().unwrap(),
            "--after",
            pack.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(parse_json(&stdout)["result"]["after"]["head_sha"], head);
}

#[test]
fn returned_artifact_path_is_reusable_from_repository_root() {
    let (repo, base, head) = boundary_fixture("review-returned-root-path");
    let worktrees_before = git(repo.path(), &["worktree", "list", "--porcelain"]);
    let result = review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "collect",
            "--base",
            &base,
            "--head",
            &head,
        ],
    ));
    let artifact = result["artifact_dir"].as_str().unwrap();
    assert!(!Path::new(artifact).is_absolute());
    let pack = format!("{artifact}/review.json");
    let queue = format!("{artifact}/review-queue.json");
    let result = review_success(run_cli_in(
        Some(repo.path()),
        &[
            "--json",
            "code-review",
            "queue",
            "list",
            "--pack",
            &pack,
            "--queue",
            &queue,
        ],
    ));
    assert_eq!(result["source_digest_valid"], true);
    assert_eq!(result["syntax_authenticity"], "verified");
    assert_eq!(
        git(repo.path(), &["worktree", "list", "--porcelain"]),
        worktrees_before
    );
    assert!(!repo.path().join(artifact).join("runs").exists());
}
