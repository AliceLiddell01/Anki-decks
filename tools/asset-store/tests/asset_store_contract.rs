use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use asset_store::{
    AssetIdentity, AssetStore, DetectedFormat, ErrorCode, IngestRequest, LifecycleState,
    SelectionMode, SemanticDecision, SemanticStatus, SemanticValidator, StoreOptions,
    ValidationEvidence, ValidatorFailure, ValidatorIdentity,
};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "asset-store-contract-{}-{label}-{counter}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("fixture root создаётся");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path().join(name);
        fs::write(&path, bytes).expect("synthetic fixture записывается");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open_store(path: &Path) -> AssetStore {
    AssetStore::open(StoreOptions::new(path)).expect("store открывается")
}

fn identity(key: &str) -> AssetIdentity {
    AssetIdentity::new("generic", key).expect("synthetic identity корректна")
}

fn ingest(
    store: &AssetStore,
    id: AssetIdentity,
    source_path: PathBuf,
) -> asset_store::IngestOutcome {
    store
        .ingest(IngestRequest {
            identity: id,
            source_path,
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .expect("explicit ingest успешен")
}

struct FixedValidator {
    identity: ValidatorIdentity,
    status: SemanticStatus,
    fail: bool,
}

struct StatusByKey {
    identity: ValidatorIdentity,
}

struct NoEvidenceValidator;

impl SemanticValidator for NoEvidenceValidator {
    fn identity(&self) -> ValidatorIdentity {
        ValidatorIdentity::new("empty-evidence", "1").unwrap()
    }

    fn validate(
        &self,
        _asset: &asset_store::AssetRecord,
        _bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        Ok(SemanticDecision::new(SemanticStatus::Verified, Vec::new()))
    }
}

impl SemanticValidator for StatusByKey {
    fn identity(&self) -> ValidatorIdentity {
        self.identity.clone()
    }

    fn validate(
        &self,
        asset: &asset_store::AssetRecord,
        _bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let status = match asset.identity.key.as_str() {
            "verified" => SemanticStatus::Verified,
            "rejected" => SemanticStatus::Rejected,
            "uncertain" => SemanticStatus::Uncertain,
            "corrupt" => SemanticStatus::Corrupt,
            _ => unreachable!("fixture содержит только четыре заданные identity"),
        };
        Ok(SemanticDecision::new(
            status,
            vec![ValidationEvidence {
                kind: "synthetic".to_owned(),
                summary: status.as_str().to_owned(),
                details: None,
            }],
        ))
    }
}

impl FixedValidator {
    fn new(status: SemanticStatus) -> Self {
        Self {
            identity: ValidatorIdentity::new("synthetic", "1").expect("validator valid"),
            status,
            fail: false,
        }
    }
}

impl SemanticValidator for FixedValidator {
    fn identity(&self) -> ValidatorIdentity {
        self.identity.clone()
    }

    fn validate(
        &self,
        _asset: &asset_store::AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        if self.fail {
            return Err(ValidatorFailure::new(
                "synthetic_failure",
                "fixture failure",
            ));
        }
        let mut contents = Vec::new();
        bytes
            .read_to_end(&mut contents)
            .map_err(|error| ValidatorFailure::new("read_failed", error.to_string()))?;
        Ok(SemanticDecision::new(
            self.status,
            vec![ValidationEvidence {
                kind: "synthetic".to_owned(),
                summary: format!("checked {} bytes", contents.len()),
                details: None,
            }],
        ))
    }
}

#[test]
fn empty_store_reopens_and_explicit_ingest_is_idempotent_with_hash_conflicts() {
    let temp = TempDir::new("reopen-ingest");
    let root = temp.path().join("owned");
    let store = open_store(&root);
    let store_id = store.store_id().to_owned();
    assert!(
        store
            .verify_integrity()
            .expect("empty store valid")
            .is_empty()
    );

    let source = temp.write("claim.png", b"not actually a png");
    let created = ingest(&store, identity("item-a"), source.clone());
    assert!(created.changed);
    assert_eq!(created.asset.lifecycle, LifecycleState::Pending);
    assert_eq!(created.asset.format, DetectedFormat::Unknown);
    assert_eq!(created.asset.provenance.source_kind, "local_import");
    assert_eq!(created.asset.provenance.source_name, "claim.png");
    assert_eq!(
        created.asset.sha256,
        "fb228003c83decc652be4afdde0282a21fc3aacda92c9e0c912fe0967a9c2f88"
    );

    let repeated = ingest(&store, identity("item-a"), source.clone());
    assert!(!repeated.changed);
    assert_eq!(repeated.asset.sha256, created.asset.sha256);
    assert_eq!(
        store.verify_integrity().expect("state remains valid").len(),
        1
    );

    let different = temp.write("replacement.bin", b"different bytes");
    let error = store
        .ingest(IngestRequest {
            identity: identity("item-a"),
            source_path: different.clone(),
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .expect_err("silent overwrite запрещён");
    assert_eq!(error.code, ErrorCode::IdentityConflict);
    assert_eq!(
        error.details["existing_sha256"],
        serde_json::json!(created.asset.sha256)
    );
    assert_eq!(
        store.verify_integrity().expect("conflict не изменил state")[0].sha256,
        created.asset.sha256
    );

    let checked = FixedValidator::new(SemanticStatus::Verified);
    store
        .validate(SelectionMode::New, &checked)
        .expect("synthetic validator фиксирует VERIFIED");
    let current = store.verify_integrity().expect("verified state valid")[0].clone();
    assert_eq!(current.lifecycle, LifecycleState::Verified);

    let replaced = store
        .ingest(IngestRequest {
            identity: identity("item-a"),
            source_path: different,
            domain_metadata: None,
            replace_expected_sha256: Some(current.sha256.clone()),
        })
        .expect("явная compare-and-swap замена разрешена");
    assert!(replaced.changed);
    assert_eq!(replaced.previous.as_ref().unwrap().sha256, current.sha256);
    assert_eq!(replaced.asset.lifecycle, LifecycleState::Pending);
    assert!(replaced.asset.validation.is_none());

    drop(store);
    let reopened = open_store(&root);
    assert_eq!(reopened.store_id(), store_id);
    let assets = reopened.verify_integrity().expect("reopened state valid");
    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0].sha256, replaced.asset.sha256);
    assert_eq!(assets[0].identity.key, "item-a");
    assert_eq!(
        reopened
            .select(SelectionMode::New, &checked.identity)
            .expect("replacement selection valid")
            .len(),
        1,
        "новые bytes не наследуют старый semantic decision"
    );
}

#[test]
fn lifecycle_maps_only_verified_decision_to_trusted_and_selection_is_hash_versioned() {
    let temp = TempDir::new("semantic-lifecycle");
    let store = open_store(&temp.path().join("store"));
    for (name, bytes) in [
        ("verified", b"v".as_slice()),
        ("rejected", b"r".as_slice()),
        ("uncertain", b"u".as_slice()),
        ("corrupt", b"c".as_slice()),
    ] {
        ingest(
            &store,
            identity(name),
            temp.write(&format!("{name}.bin"), bytes),
        );
    }

    let validator = StatusByKey {
        identity: ValidatorIdentity::new("synthetic", "1").expect("validator valid"),
    };
    let report = store
        .validate(SelectionMode::New, &validator)
        .expect("semantic decisions persisted");
    assert_eq!(report.considered, 4);
    assert_eq!(report.changed, 4);

    let records = store
        .verify_integrity()
        .expect("all lifecycle states valid");
    for record in &records {
        match record.validation.as_ref().expect("decision exists").status {
            SemanticStatus::Verified => assert_eq!(record.lifecycle, LifecycleState::Verified),
            SemanticStatus::Rejected | SemanticStatus::Uncertain | SemanticStatus::Corrupt => {
                assert_eq!(record.lifecycle, LifecycleState::Quarantined);
            }
        }
    }
    let current = ValidatorIdentity::new("synthetic", "1").expect("validator valid");
    assert!(
        store
            .select(SelectionMode::New, &current)
            .expect("selection valid")
            .is_empty()
    );
    assert_eq!(
        store.select(SelectionMode::Full, &current).unwrap().len(),
        4
    );
    let upgraded = ValidatorIdentity::new("synthetic", "2").expect("new version valid");
    assert_eq!(
        store.select(SelectionMode::New, &upgraded).unwrap().len(),
        4
    );

    let failing = FixedValidator {
        identity: ValidatorIdentity::new("failed", "1").unwrap(),
        status: SemanticStatus::Verified,
        fail: true,
    };
    let failed = store
        .validate(SelectionMode::New, &failing)
        .expect("domain failure is an outcome, not a store error");
    assert_eq!(failed.considered, 4);
    assert_eq!(failed.changed, 0);
    assert_eq!(failed.blockers, ["synthetic_failure"]);
    assert!(
        store.verify_integrity().unwrap().iter().all(|asset| asset
            .validation
            .as_ref()
            .unwrap()
            .validator
            .id
            == "synthetic")
    );
}

#[test]
fn integrity_failures_are_explicit_and_traversal_is_rejected() {
    let temp = TempDir::new("integrity");
    let root = temp.path().join("store");
    let store = open_store(&root);
    let source = temp.write("asset.bin", b"integrity fixture");
    let record = ingest(&store, identity("one"), source).asset;
    let object = root.join(&record.storage_path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&object, fs::Permissions::from_mode(0o600))
            .expect("test делает owned object доступным для порчи");
    }
    fs::write(&object, b"tampered").expect("synthetic corruption записывается");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::IntegrityMismatch
    );

    let reopened = AssetStore::open(StoreOptions::new(&root));
    assert_eq!(reopened.unwrap_err().code, ErrorCode::IntegrityMismatch);
}

#[test]
fn absent_file_unsupported_schema_corrupt_json_and_traversal_fail_closed() {
    let temp = TempDir::new("manifest-errors");
    let root = temp.path().join("store");
    let store = open_store(&root);
    let record = ingest(&store, identity("one"), temp.write("asset.bin", b"payload")).asset;
    fs::remove_file(root.join(&record.storage_path)).expect("test удаляет owned file");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::MissingAssetFile
    );
    fs::write(root.join(&record.storage_path), b"payload").expect("owned file восстанавливается");

    let manifest_path = root.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("manifest читается"))
            .expect("manifest json valid");
    manifest["schema_version"] = serde_json::json!(999);
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .expect("schema mutation записывается");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::UnsupportedSchemaVersion
    );

    manifest["schema_version"] = serde_json::json!(1);
    manifest["assets"][0]["storage_path"] = serde_json::json!("../../decks/media/x.png");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .expect("traversal mutation записывается");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::PathTraversal
    );

    fs::write(&manifest_path, b"{").expect("corrupt json записывается");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::ManifestCorrupt
    );
}

#[test]
fn store_root_must_not_overlap_protected_decks_tree_or_use_parent_aliases() {
    let temp = TempDir::new("boundary");
    let repository = temp.path().join("repository");
    let decks = repository.join("decks");
    fs::create_dir_all(decks.join("japanese/media")).expect("synthetic decks root");
    let inside = decks.join("assets");
    let error = AssetStore::open(StoreOptions::new(&inside).protect_from(&decks)).unwrap_err();
    assert_eq!(error.code, ErrorCode::BoundaryViolation);
    assert!(!inside.exists(), "запрещённый root не создаётся");

    let error = AssetStore::open(StoreOptions::new(&repository).protect_from(&decks)).unwrap_err();
    assert_eq!(error.code, ErrorCode::BoundaryViolation);

    let error = AssetStore::open(StoreOptions::new(repository.join("../outside"))).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidStoreRoot);

    let unowned = temp.path().join("user-directory");
    fs::create_dir_all(&unowned).unwrap();
    fs::write(unowned.join("important.txt"), b"user-owned").unwrap();
    let error = AssetStore::open(StoreOptions::new(&unowned)).unwrap_err();
    assert_eq!(error.code, ErrorCode::StoreNotOwned);
    assert!(
        !unowned.join(".lock").exists(),
        "отказ не оставляет lock-файл"
    );

    fs::write(decks.join("japanese/media/user.bin"), b"user-owned").unwrap();
    let store = open_store(&repository.join(".asset-store/kanji"));
    assert!(store.verify_integrity().unwrap().is_empty());
    let validator = ValidatorIdentity::new("fixture", "1").unwrap();
    assert!(
        store
            .select(SelectionMode::Full, &validator)
            .unwrap()
            .is_empty(),
        "full не индексирует соседний decks/media"
    );
}

#[cfg(unix)]
#[test]
fn symlink_store_root_and_object_symlink_are_rejected() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new("symlink-boundary");
    let root = temp.path().join("store");
    let store = open_store(&root);
    let record = ingest(
        &store,
        identity("linked"),
        temp.write("asset.bin", b"asset"),
    )
    .asset;
    let object_path = root.join(&record.storage_path);
    let external = temp.write("outside.bin", b"outside");
    fs::remove_file(&object_path).expect("owned object удалён для symlink fixture");
    symlink(&external, &object_path).expect("object symlink создаётся");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::BoundaryViolation
    );

    let target = temp.path().join("real-store");
    open_store(&target);
    let alias = temp.path().join("store-alias");
    symlink(&target, &alias).expect("root symlink создаётся");
    assert_eq!(
        AssetStore::open(StoreOptions::new(alias)).unwrap_err().code,
        ErrorCode::BoundaryViolation
    );

    let repository = temp.path().join("linked-repository");
    let actual_decks = temp.path().join("external-decks");
    fs::create_dir_all(&repository).unwrap();
    fs::create_dir_all(&actual_decks).unwrap();
    symlink(&actual_decks, repository.join("decks")).expect("decks symlink created");
    let outside_alias = actual_decks.join("candidate-store");
    let error =
        AssetStore::open(StoreOptions::new(&outside_alias).protect_from(repository.join("decks")))
            .unwrap_err();
    assert_eq!(error.code, ErrorCode::BoundaryViolation);
    assert!(!outside_alias.exists());
}

#[test]
fn concurrent_writers_serialize_manifest_updates_without_lost_records() {
    let temp = TempDir::new("concurrent");
    let root = temp.path().join("store");
    open_store(&root);
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for (key, bytes) in [
        ("left", b"left-bytes".as_slice()),
        ("right", b"right-bytes".as_slice()),
    ] {
        let root = root.clone();
        let source = temp.write(&format!("{key}.bin"), bytes);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            let store = open_store(&root);
            barrier.wait();
            ingest(&store, identity(key), source)
        }));
    }
    for worker in workers {
        worker.join().expect("writer completes");
    }
    let records = open_store(&root)
        .verify_integrity()
        .expect("concurrent manifest valid");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].identity.key, "left");
    assert_eq!(records[1].identity.key, "right");
}

#[test]
fn concurrent_different_bytes_for_one_identity_have_one_winner_and_one_conflict() {
    let temp = TempDir::new("concurrent-identity");
    let root = temp.path().join("store");
    open_store(&root);
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for bytes in [b"first-version".as_slice(), b"second-version".as_slice()] {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        let source = temp.write(&format!("candidate-{}.bin", workers.len()), bytes);
        workers.push(thread::spawn(move || {
            let store = open_store(&root);
            barrier.wait();
            store.ingest(IngestRequest {
                identity: identity("contested"),
                source_path: source,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
        }));
    }

    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().expect("writer completes"))
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let conflicts: Vec<_> = results
        .iter()
        .filter_map(|result| result.as_ref().err())
        .collect();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].code, ErrorCode::IdentityConflict);

    let records = open_store(&root)
        .verify_integrity()
        .expect("winner state valid");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].identity.key, "contested");
}

#[test]
fn failed_validator_never_publishes_verified_state_and_unicode_identity_roundtrips() {
    let temp = TempDir::new("failure-unicode");
    let root = temp.path().join("store");
    let store = open_store(&root);
    let unicode = AssetIdentity::new("kanji", "𠮷").unwrap();
    let source = temp.write("kanji.dat", b"unicode bytes");
    let record = ingest(&store, unicode.clone(), source).asset;
    let failed = FixedValidator {
        identity: ValidatorIdentity::new("broken", "1").unwrap(),
        status: SemanticStatus::Verified,
        fail: true,
    };
    let report = store.validate(SelectionMode::Full, &failed).unwrap();
    assert_eq!(report.changed, 0);
    let reopened = open_store(&root);
    let restored = reopened.verify_integrity().unwrap();
    assert_eq!(restored[0].identity, unicode);
    assert_eq!(restored[0].sha256, record.sha256);
    assert_eq!(restored[0].lifecycle, LifecycleState::Pending);
    assert!(restored[0].validation.is_none());
}

#[test]
fn verified_without_nonempty_evidence_is_rejected() {
    let temp = TempDir::new("empty-evidence");
    let root = temp.path().join("store");
    let store = open_store(&root);
    ingest(
        &store,
        identity("candidate"),
        temp.write("candidate.bin", b"bytes"),
    );

    let error = store
        .validate(SelectionMode::Full, &NoEvidenceValidator)
        .expect_err("VERIFIED без evidence запрещён");
    assert_eq!(error.code, ErrorCode::InvalidValidationEvidence);
    let record = open_store(&root).verify_integrity().unwrap().remove(0);
    assert_eq!(record.lifecycle, LifecycleState::Pending);
    assert!(record.validation.is_none());
}
