//! Сквозные проверки контрактов CLI на синтетических Git-репозиториях.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::common::{TempDir, cli_binary, parse_json, run_cli, run_cli_in};
use serde_json::{Value, json};

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
    git(root, &["add", "--all"]);
    git(root, &["commit", "-qm", message]);
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

    for subcommand in ["collect", "verify"] {
        let (code, help, stderr) = run_cli(&["code-review", subcommand, "--help"]);
        assert_eq!(code, 0, "{stderr}");
        assert!(help.contains("--run-clippy"));
        assert!(help.contains("build.rs"));
        assert!(!help.contains("--skip-clippy"));
        let (code, _, stderr) = run_cli(&["code-review", subcommand, "--skip-clippy"]);
        assert_eq!(code, 2, "{stderr}");
        assert!(stderr.contains("--skip-clippy"));
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
    let baseline_dir = temp.path().join(".anki-repo/review/baseline");
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
    assert!(
        !String::from_utf8_lossy(&baseline_bytes)
            .contains(&temp.path().to_string_lossy().to_string())
    );
    let visible = git(
        temp.path(),
        &[
            "status",
            "--porcelain=v1",
            "--",
            ".anki-repo/review/baseline",
        ],
    );
    assert!(visible.contains(".anki-repo/review/baseline"));

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
    let verified_dir = temp.path().join(".anki-repo/review/verified");
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
    let delta_path = temp.path().join("shared-delta.json");
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
    let artifacts = TempDir::new("collect-execution-artifacts");
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
    let status_before = git(repo.path(), &["status", "--porcelain=v1"]);
    let index_before = git(repo.path(), &["ls-files", "--stage"]);

    let output = artifacts.path().join("collected");
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
    assert_eq!(
        git(repo.path(), &["status", "--porcelain=v1"]),
        status_before
    );
    assert_eq!(git(repo.path(), &["ls-files", "--stage"]), index_before);
}

#[test]
fn collect_handles_file_directory_transitions_in_both_directions() {
    let repo = TempDir::new("collect-file-directory-transition");
    let artifacts = TempDir::new("collect-file-directory-artifacts");
    init_repo(&repo);
    fs::write(repo.path().join("foo"), "old file\nsecond line\n").unwrap();
    commit(repo.path(), "добавить файл");
    let base = git(repo.path(), &["rev-parse", "HEAD"]);

    fs::remove_file(repo.path().join("foo")).unwrap();
    fs::create_dir_all(repo.path().join("foo")).unwrap();
    fs::write(repo.path().join("foo/child.rs"), "fn child() {}\n").unwrap();
    commit(repo.path(), "заменить файл каталогом");
    let directory_head = git(repo.path(), &["rev-parse", "HEAD"]);
    let forward_dir = artifacts.path().join("file-to-directory");
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
    let reverse_dir = artifacts.path().join("directory-to-file");
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
    assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
}

#[test]
fn explicit_real_clippy_collect_is_byte_stable_and_idempotent() {
    let repo = TempDir::new("collect-real-clippy");
    let artifacts = TempDir::new("collect-real-clippy-artifacts");
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

    let output = artifacts.path().join("collected");
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
    assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
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
    let output = artifacts_a.path().join("collected");
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

    let verified_dir = artifacts_b.path().join("verified");
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
    assert!(git(repo_b.path(), &["status", "--porcelain=v1"]).is_empty());
}

#[test]
fn verify_and_language_pack_scan_reject_mismatched_baseline() {
    let repo = TempDir::new("review-baseline-mismatch");
    let artifacts = TempDir::new("review-baseline-mismatch-artifacts");
    init_repo(&repo);
    fs::write(repo.path().join("message.md"), "Human message\n").unwrap();
    commit(repo.path(), "база");
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let output = artifacts.path().join("collected");
    collect_pack(repo.path(), &head, &head, &output, false);
    let pack_path = output.join("review.json");
    let mut pack: Value = serde_json::from_slice(&fs::read(&pack_path).unwrap()).unwrap();
    pack["target"]["repository_id"] = json!("0".repeat(64));
    fs::write(&pack_path, serde_json::to_vec_pretty(&pack).unwrap()).unwrap();
    let verify_output = artifacts.path().join("verified");
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
    for args in commands {
        let (code, stdout, stderr) = run_cli_in(Some(repo.path()), &args);
        assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
        assert_eq!(parse_json(&stdout)["error"]["code"], "baseline_mismatch");
    }
    assert!(!verify_output.exists());
    assert!(!scan_output.exists());
}

#[test]
fn human_review_summary_preserves_snapshot_and_evidence_meaning() {
    let repo = TempDir::new("review-human-summary");
    let artifacts = TempDir::new("review-human-summary-artifacts");
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
    let output = artifacts.path().join("collected");
    let result = collect_pack(repo.path(), &base, &head, &output, false);
    let text = fs::read_to_string(output.join("review.txt")).unwrap();
    for required in [
        format!("База: {base}"),
        format!("HEAD: {head}"),
        format!("Общий предок (merge-base): {base}"),
        "src/lib.rs изменён".into(),
        format!(
            "Кандидатов: {} (требуют семантической проверки)",
            result["candidates"]
        ),
        "rust_suppression src/lib.rs:1 существовал в исходной версии".into(),
        "error_path src/lib.rs:4 внесён или изменён диапазоном".into(),
        "Диагностик: 0 (сами по себе не являются подтверждёнными замечаниями)".into(),
        "Анализатор clippy: skipped".into(),
    ] {
        assert!(
            text.contains(&required),
            "в review.txt отсутствует {required:?}:\n{text}"
        );
    }
    assert!(!text.contains(repo.path().to_str().unwrap()));
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
