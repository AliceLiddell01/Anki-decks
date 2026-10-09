//! Регрессии границ записи CLI learning на независимых Git-фикстурах.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::common::{TempDir, run_cli_in};

fn git(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn repository(label: &str) -> TempDir {
    let temp = TempDir::new(label);
    git(temp.path(), &["init", "-q"]);
    git(
        temp.path(),
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(temp.path(), &["config", "user.name", "Learning Fixture"]);
    git(temp.path(), &["config", "commit.gpgsign", "false"]);
    temp
}

fn response(root: &Path, arguments: &[&str]) -> (i32, Value) {
    let mut argv = vec!["--json", "code-review", "learning"];
    argv.extend_from_slice(arguments);
    let (code, stdout, stderr) = run_cli_in(Some(root), &argv);
    let value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("{error}: {stdout}; {stderr}"));
    (code, value)
}

#[test]
fn missing_import_inputs_and_feedback_never_create_database_directory() {
    let temp = repository("learning-cli-no-input-writes");
    let root = temp.path();
    let (code, _) = response(
        root,
        &[
            "import",
            "--pack",
            "missing.json",
            "--queue",
            "missing-queue.json",
        ],
    );
    assert_ne!(code, 0);
    assert!(!root.join(".anki-repo/learning").exists());
    let (code, value) = response(
        root,
        &[
            "feedback",
            "record",
            "--review-id",
            "absent",
            "--unit-id",
            "absent",
            "--kind",
            "usefulness",
            "--usefulness",
            "useful",
            "--explanation",
            "Проверено",
        ],
    );
    assert_eq!(code, 4);
    assert_eq!(value["error"]["code"], "not_found");
    assert!(!root.join(".anki-repo/learning").exists());
}

#[test]
fn invalid_queue_triage_and_execution_are_rejected_before_database_creation() {
    let temp = repository("learning-cli-provenance-before-create");
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn parse(v: &str) -> Option<u32> { v.parse().ok() }\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "База"]);
    let base = git(root, &["rev-parse", "HEAD"]);
    fs::write(
        root.join("src/lib.rs"),
        "pub fn parse(v: &str) -> u32 { v.parse().unwrap() }\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "Правка"]);
    let head = git(root, &["rev-parse", "HEAD"]);
    let (code, _, stderr) = run_cli_in(
        Some(root),
        &["code-review", "collect", "--base", &base, "--head", &head],
    );
    assert_eq!(code, 0, "{stderr}");
    let workspace = format!(".anki-repo/review/local/{head}");
    let pack = format!("{workspace}/review.json");
    let queue = format!("{workspace}/review-queue.json");
    fs::write(root.join("invalid.json"), "{}").unwrap();
    for extra in [
        vec!["--triage", "invalid.json"],
        vec!["--execution", "missing/result.json"],
    ] {
        let mut args = vec!["import", "--pack", &pack, "--queue", &queue];
        args.extend(extra);
        let (code, _) = response(root, &args);
        assert_ne!(code, 0);
        assert!(!root.join(".anki-repo/learning").exists());
    }
    let mut document: Value =
        serde_json::from_slice(&fs::read(root.join(&queue)).unwrap()).unwrap();
    document["source"]["review_pack_sha256"] = serde_json::json!("0".repeat(64));
    fs::write(root.join(&queue), serde_json::to_vec(&document).unwrap()).unwrap();
    let (code, _) = response(root, &["import", "--pack", &pack, "--queue", &queue]);
    assert_ne!(code, 0);
    assert!(!root.join(".anki-repo/learning").exists());
    // Неверный вход отвергается до попытки открыть даже существующий файл базы.
    let preserved = "существующий файл, не SQLite".as_bytes();
    fs::write(root.join("state.sqlite"), preserved).unwrap();
    let (code, value) = response(
        root,
        &[
            "import",
            "--pack",
            &pack,
            "--queue",
            &queue,
            "--db",
            "state.sqlite",
        ],
    );
    assert_eq!(code, 7);
    assert_eq!(value["error"]["code"], "source_changed");
    assert_eq!(fs::read(root.join("state.sqlite")).unwrap(), preserved);
}

#[test]
fn oversized_archive_is_a_controlled_error_without_database_writes() {
    let temp = repository("learning-cli-bounded-archive");
    let root = temp.path();
    let archive = fs::File::create(root.join("archive.json")).unwrap();
    archive
        .set_len(anki_repo::run::MAX_LEARNING_ARCHIVE_BYTES as u64 + 1)
        .unwrap();
    let (code, value) = response(root, &["restore", "--archive", "archive.json"]);
    assert_eq!(code, 3);
    assert_eq!(value["error"]["code"], "learning_export_invalid");
    assert_eq!(value["error"]["details"]["reason"], "archive_too_large");
    assert_eq!(
        value["error"]["details"]["max_bytes"],
        anki_repo::run::MAX_LEARNING_ARCHIVE_BYTES
    );
    assert!(!root.join(".anki-repo/learning").exists());
    let preserved = "существующие данные".as_bytes();
    fs::write(root.join("state.sqlite"), preserved).unwrap();
    let (code, value) = response(
        root,
        &[
            "restore",
            "--archive",
            "archive.json",
            "--db",
            "state.sqlite",
        ],
    );
    assert_eq!(code, 3);
    assert_eq!(value["error"]["details"]["reason"], "archive_too_large");
    assert_eq!(fs::read(root.join("state.sqlite")).unwrap(), preserved);
    assert!(!root.join("state.sqlite-wal").exists());
    assert!(!root.join("state.sqlite-shm").exists());
}
