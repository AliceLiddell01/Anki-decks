use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("kanji-assets-cli-{}-{counter}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("repository/decks")).expect("synthetic repository root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn cli(temp: &TempDir, args: &[&str]) -> Output {
    let store = temp.path().join("owned-store");
    let repository = temp.path().join("repository");
    Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--store")
        .arg(store)
        .arg("--repository-root")
        .arg(repository)
        .arg("--output")
        .arg("json")
        .args(args)
        .output()
        .expect("kanji-assets process запускается")
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("stdout содержит JSON contract")
}

#[test]
fn cli_reports_store_unicode_selection_idempotency_and_conflicts_as_json() {
    let temp = TempDir::new();
    let init = cli(&temp, &["init"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stdout)
    );
    let init_json = json(&init);
    assert_eq!(init_json["operation"], "init");
    assert_eq!(init_json["outcome"], "initialized");
    assert_eq!(init_json["changed"], true);
    assert!(init_json["store"]["store_id"].as_str().unwrap().len() >= 32);

    let init_repeat = cli(&temp, &["init"]);
    assert!(init_repeat.status.success());
    assert_eq!(json(&init_repeat)["outcome"], "already_initialized");
    assert_eq!(json(&init_repeat)["changed"], false);

    let source = temp.path().join("claim.png");
    fs::write(&source, b"not an image").expect("synthetic bytes");
    let source_text = source.to_str().unwrap();
    let first = cli(
        &temp,
        &["ingest", "--character", "𠮷", "--file", source_text],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let first_json = json(&first);
    assert_eq!(first_json["outcome"], "candidate_created");
    assert_eq!(first_json["assets"][0]["identity"]["key"], "𠮷");
    assert_eq!(
        first_json["assets"][0]["domain_metadata"]["unicode_codepoints"][0],
        "U+20BB7"
    );
    assert_eq!(first_json["assets"][0]["to_state"], "pending");
    assert_eq!(
        first_json["assets"][0]["validation_status"],
        serde_json::Value::Null
    );

    let repeat = cli(
        &temp,
        &["ingest", "--character", "𠮷", "--file", source_text],
    );
    assert!(repeat.status.success());
    assert_eq!(json(&repeat)["outcome"], "already_present");
    assert_eq!(json(&repeat)["changed"], false);

    let plan = cli(
        &temp,
        &[
            "plan",
            "--mode",
            "new",
            "--validator-id",
            "kanji-cv",
            "--validator-version",
            "future-1",
        ],
    );
    assert!(plan.status.success());
    let plan_json = json(&plan);
    assert_eq!(plan_json["mode"], "new");
    assert_eq!(plan_json["assets"].as_array().unwrap().len(), 1);
    assert_eq!(plan_json["validator_available"], true);
    assert_eq!(plan_json["blockers"].as_array().unwrap().len(), 0);

    let full = cli(
        &temp,
        &[
            "plan",
            "--mode",
            "full",
            "--validator-id",
            "kanji-cv",
            "--validator-version",
            "future-1",
        ],
    );
    assert!(full.status.success());
    assert_eq!(json(&full)["mode"], "full");
    assert_eq!(json(&full)["assets"].as_array().unwrap().len(), 1);

    let other = temp.path().join("different.bin");
    fs::write(&other, b"different").unwrap();
    let other_text = other.to_str().unwrap();
    let conflict = cli(
        &temp,
        &["ingest", "--character", "𠮷", "--file", other_text],
    );
    assert_eq!(conflict.status.code(), Some(3));
    let conflict_json = json(&conflict);
    assert_eq!(conflict_json["outcome"], "conflict");
    assert_eq!(conflict_json["conflicts"][0]["code"], "identity_conflict");
    assert!(
        conflict_json["conflicts"][0]["existing_sha256"]
            .as_str()
            .is_some()
    );
    assert!(
        conflict_json["conflicts"][0]["candidate_sha256"]
            .as_str()
            .is_some()
    );
}

#[test]
fn list_and_plan_reject_a_missing_store_without_creating_it() {
    let temp = TempDir::new();
    let missing_store = temp.path().join("owned-store");
    let list = cli(&temp, &["list"]);
    assert_eq!(list.status.code(), Some(4));
    assert_eq!(json(&list)["error"]["code"], "store_missing");
    assert!(!missing_store.exists());

    let plan = cli(
        &temp,
        &[
            "plan",
            "--mode",
            "new",
            "--validator-id",
            "kanji-cv",
            "--validator-version",
            "future-1",
        ],
    );
    assert_eq!(plan.status.code(), Some(4));
    assert_eq!(json(&plan)["error"]["code"], "store_missing");
    assert!(!missing_store.exists());
}

#[test]
fn list_does_not_initialize_an_existing_empty_directory() {
    let temp = TempDir::new();
    let store = temp.path().join("owned-store");
    fs::create_dir(&store).expect("empty store path exists");

    let output = cli(&temp, &["list"]);

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json(&output)["error"]["code"], "store_not_owned");
    assert_eq!(fs::read_dir(&store).unwrap().count(), 0);
}

#[test]
fn cli_rejects_store_under_decks_before_creating_it() {
    let temp = TempDir::new();
    let store = temp.path().join("repository/decks/kanji-assets");
    let output = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--store")
        .arg(&store)
        .arg("--repository-root")
        .arg(temp.path().join("repository"))
        .arg("--output")
        .arg("json")
        .arg("init")
        .output()
        .expect("CLI запускается");
    assert_eq!(output.status.code(), Some(4));
    let value = json(&output);
    assert_eq!(value["error"]["code"], "boundary_violation");
    assert!(!store.exists());
}

#[test]
fn cli_rejects_explicit_source_under_decks_before_creating_the_store() {
    let temp = TempDir::new();
    let source = temp
        .path()
        .join("repository/decks/japanese/media/candidate.png");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, b"synthetic user media").unwrap();
    let source_text = source.to_str().unwrap();

    let output = cli(
        &temp,
        &["ingest", "--character", "漢", "--file", source_text],
    );

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json(&output)["error"]["code"], "boundary_violation");
    assert!(
        !temp.path().join("owned-store").exists(),
        "защищённый source отклоняется до создания store"
    );
}

#[cfg(unix)]
#[test]
fn cli_rejects_decks_source_reached_through_a_symlink_alias() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let repository = temp.path().join("repository");
    let decks = repository.join("decks");
    fs::remove_dir(&decks).unwrap();
    let actual_decks = temp.path().join("private-decks");
    let source = actual_decks.join("media/candidate.png");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, b"synthetic user media").unwrap();
    symlink(&actual_decks, &decks).unwrap();
    let aliased_source = decks.join("media/candidate.png");
    let source_text = aliased_source.to_str().unwrap();

    let output = cli(
        &temp,
        &["ingest", "--character", "漢", "--file", source_text],
    );

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json(&output)["error"]["code"], "boundary_violation");
    assert!(
        !temp.path().join("owned-store").exists(),
        "symlink alias не даёт прочитать source или создать store"
    );
}

#[test]
fn cli_detects_decks_boundary_even_when_repository_root_argument_is_wrong() {
    let temp = TempDir::new();
    let repository = temp.path().join("repository");
    let store = repository.join("decks/local-store");
    let unrelated = temp.path().join("unrelated");
    fs::create_dir_all(&unrelated).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--store")
        .arg(&store)
        .arg("--repository-root")
        .arg(&unrelated)
        .arg("--output")
        .arg("json")
        .arg("init")
        .current_dir(temp.path())
        .output()
        .expect("CLI запускается");
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json(&output)["error"]["code"], "boundary_violation");
    assert!(!store.exists());
}

#[test]
fn cli_returns_machine_readable_invalid_input_without_creating_the_store() {
    let temp = TempDir::new();
    let invalid_character = cli(
        &temp,
        &["ingest", "--character", "", "--file", "missing.bin"],
    );
    assert_eq!(invalid_character.status.code(), Some(3));
    assert_eq!(
        json(&invalid_character)["error"]["code"],
        "invalid_identity"
    );

    let missing_source = cli(
        &temp,
        &[
            "ingest",
            "--character",
            "漢",
            "--file",
            "missing-source.bin",
        ],
    );
    assert_eq!(missing_source.status.code(), Some(3));
    assert_eq!(
        json(&missing_source)["error"]["code"],
        "source_file_missing"
    );

    let invalid_validator = cli(
        &temp,
        &[
            "plan",
            "--mode",
            "new",
            "--validator-id",
            "",
            "--validator-version",
            "1",
        ],
    );
    assert_eq!(invalid_validator.status.code(), Some(3));
    assert_eq!(
        json(&invalid_validator)["error"]["code"],
        "invalid_validator_identity"
    );
    assert!(!temp.path().join("owned-store").exists());
}

#[test]
fn cli_reports_unsupported_manifest_schema_with_a_stable_json_code() {
    let temp = TempDir::new();
    let init = cli(&temp, &["init"]);
    assert!(init.status.success());
    let manifest_path = temp.path().join("owned-store/manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["schema_version"] = serde_json::json!(500);
    manifest["future_field"] = serde_json::json!({"format": "future"});
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let output = cli(&temp, &["list"]);
    assert_eq!(output.status.code(), Some(4));
    let value = json(&output);
    assert_eq!(value["outcome"], "blocked");
    assert_eq!(value["error"]["code"], "unsupported_schema_version");
    assert_eq!(value["store"]["store_id"], serde_json::Value::Null);
}

#[test]
fn cli_argument_usage_error_writes_stderr_without_json_stdout() {
    let temp = TempDir::new();
    let output = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--output")
        .arg("json")
        .arg("invalid-command")
        .current_dir(temp.path())
        .output()
        .expect("CLI запускается");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    assert!(
        !temp.path().join(".asset-store/kanji").exists(),
        "parse error does not execute a store operation"
    );
}

#[test]
fn default_store_resolves_to_workspace_root_when_run_from_a_subdirectory() {
    let temp = TempDir::new();
    let repository = temp.path().join("repository");
    let nested = repository.join("tools/asset-store");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(repository.join("decks")).unwrap();
    fs::write(repository.join("Cargo.toml"), "[workspace]\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--repository-root")
        .arg(&repository)
        .arg("--output")
        .arg("json")
        .arg("init")
        .current_dir(&nested)
        .output()
        .expect("CLI запускается");
    assert!(output.status.success());
    let expected = repository.join(".asset-store/kanji");
    assert!(expected.exists());
    assert!(!nested.join(".asset-store").exists());
    assert_eq!(
        json(&output)["store"]["path"],
        expected.display().to_string()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn default_store_resolves_symlinked_repository_root_with_parent_components() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let repository = temp.path().join("repository");
    let nested = repository.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(repository.join("decks")).unwrap();
    fs::write(repository.join("Cargo.toml"), "[workspace]\n").unwrap();
    let alias = temp.path().join("repository-alias");
    symlink(&repository, &alias).unwrap();
    let repository_argument = alias.join("nested").join("..");

    let output = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--repository-root")
        .arg(&repository_argument)
        .arg("--output")
        .arg("json")
        .arg("init")
        .current_dir(temp.path())
        .output()
        .expect("CLI запускается");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = repository.join(".asset-store/kanji");
    assert!(expected.exists());
    assert_eq!(
        json(&output)["store"]["path"],
        expected.display().to_string()
    );
}
