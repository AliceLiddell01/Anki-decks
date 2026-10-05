//! End-to-end contracts для synthetic Git repositories.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::common::{TempDir, parse_json, run_cli, run_cli_in};
use serde_json::{Value, json};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("synthetic Git output is UTF-8")
        .trim()
        .to_owned()
}

fn init_repo(temp: &TempDir) {
    let root = temp.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "contract@example.invalid"]);
    git(root, &["config", "user.name", "Contract Test"]);
}

fn commit(root: &Path, message: &str) {
    git(root, &["add", "--all"]);
    git(root, &["commit", "-qm", message]);
}

#[test]
fn code_review_namespace_does_not_replace_card_review_commands() {
    let (code, help, stderr) = run_cli(&["code-review", "--help"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(help.contains("collect"));
    assert!(help.contains("verify"));
    assert!(help.contains("delta"));

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
    commit(temp.path(), "base");
    git(temp.path(), &["branch", "synthetic-base"]);
    let base_sha = git(temp.path(), &["rev-parse", "HEAD"]);

    fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn value() -> u8 { panic!(\"Human message\"); }\n",
    )
    .unwrap();
    commit(temp.path(), "introduce candidates");
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
            "--skip-clippy",
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

    // Повтор записи того же semantic snapshot по тому же пути идемпотентен.
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
            "--skip-clippy",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(fs::read(&baseline_path).unwrap(), baseline_bytes);

    fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n",
    )
    .unwrap();
    commit(temp.path(), "remove candidate");
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
            "--skip-clippy",
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let verified = parse_json(&stdout)["result"].clone();
    assert_eq!(verified["target"]["base_sha"], json!(base_sha));
    assert_eq!(verified["target"]["head_sha"], json!(fixed_sha));
    assert!(
        verified["delta"]["candidates"]
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
    assert_eq!(result["scan"]["candidates"], json!([]));
    assert_eq!(result["scan"]["files"][0]["path"], json!("src/message.rs"));
}
