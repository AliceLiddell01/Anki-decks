//! Контракт learning-подсистемы code review на собственной синтетике.
//!
//! Все проверки строятся на собственных фикстурах и временных Git-репозиториях:
//! `decks/**` не читается. Проверяются ровно те свойства, которые определяют
//! доверие к истории: идемпотентность, аудируемые ревизии, отказ при
//! повреждённой и более новой схеме, AST-аутентичность очереди, раздельные
//! единицы наблюдения, консервативная статистика, переносимость и аудит правок.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anki_repo::code_review::learning;
use anki_repo::code_review::learning::import::LoadedReview;
use anki_repo::code_review::learning::model::{
    FeedbackAction, FeedbackEvent, FeedbackKind, SupportLevel, TrustLevel,
};
use anki_repo::code_review::model::{
    CandidateEvidence, CandidateOrigin, REVIEW_SCHEMA_VERSION, ReviewFile, ReviewPack, ReviewScope,
    ToolRunEvidence,
};
use anki_repo::code_review::review_queue::{self, REPRESENTATIVE_LIMIT};
use anki_repo::code_review::scope::{FileCategory, GitTarget, ImageState, LineRange};
use anki_repo::code_review::semantic_triage::{
    self, Disposition, FindingProvenance, ReasonCode, SemanticFinding, Severity,
};
use anki_repo::error::ErrorCode;

use crate::common::TempDir;

/// Мьютекс последовательных проверок: часть из них работает через временный
/// Git-репозиторий, поэтому они не запускаются одновременно.
static REPOSITORY_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    REPOSITORY_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("не удалось запустить тестовую команду Git");
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

/// Синтетический Git-репозиторий с production- и test-исходником.
struct SyntheticRepo {
    temp: TempDir,
    base_sha: String,
    head_sha: String,
}

impl SyntheticRepo {
    fn create(label: &str) -> Self {
        let temp = TempDir::new(label);
        let root = temp.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "learning@example.invalid"]);
        git(root, &["config", "user.name", "Learning Contract"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        git(root, &["config", "core.autocrlf", "false"]);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("src/tests")).unwrap();
        // Метка фикстуры делает базу уникальной: два синтетических репозитория
        // не могут совпасть по SHA и, значит, по идентичности диапазона.
        fs::write(root.join("SYNTHETIC_LABEL"), label).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn parse(value: &str) -> Option<u32> {\n    value.parse().ok()\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/tests/parse.rs"),
            "#[test]\nfn parses() {\n    assert!(crate::parse(\"0\").is_some());\n}\n",
        )
        .unwrap();
        git(root, &["add", "--", "."]);
        git(root, &["commit", "-qm", "Синтетическая база"]);
        let base_sha = git(root, &["rev-parse", "HEAD"]);
        fs::write(
            root.join("src/lib.rs"),
            "pub fn parse(value: &str) -> Option<u32> {\n    let digits = value.trim();\n    digits.parse().ok()\n}\n",
        )
        .unwrap();
        // Тестовая поверхность тоже изменена в диапазоне: иначе её исходник не
        // попадает в образы диапазона и синтаксический контекст недоступен.
        fs::write(
            root.join("src/tests/parse.rs"),
            "#[test]\nfn parses() {\n    assert!(crate::parse(\"1\").is_some());\n}\n",
        )
        .unwrap();
        git(root, &["add", "--", "."]);
        git(root, &["commit", "-qm", "Синтетическая правка"]);
        let head_sha = git(root, &["rev-parse", "HEAD"]);
        Self {
            temp,
            base_sha,
            head_sha,
        }
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }

    fn target(&self) -> GitTarget {
        self.target_for(&self.base_sha, &self.head_sha)
    }

    /// Идентичность совместимости повторяет формулу репозитория: SHA-256 от base.
    fn target_for(&self, base_sha: &str, head_sha: &str) -> GitTarget {
        GitTarget {
            repository_id: learning::import::sha256_hex(
                format!("anki-repo:git-base:v1:{base_sha}").as_bytes(),
            ),
            base_sha: base_sha.to_owned(),
            head_sha: head_sha.to_owned(),
            merge_base_sha: self.base_sha.clone(),
        }
    }
}

/// Кандидат с локализуемой строкой и сниппетом.
///
/// Столбец важен: контекст строки разрешается по точной позиции, поэтому
/// кандидат тестовой поверхности указывает на `assert!`, а не на начало файла.
fn candidate(
    id: &str,
    detector: &str,
    path: &str,
    line: usize,
    column: usize,
    snippet: &str,
) -> CandidateEvidence {
    CandidateEvidence {
        id: id.into(),
        detector: detector.into(),
        path: path.into(),
        line: Some(line),
        column: Some(column),
        snippet: Some(snippet.into()),
        origin: CandidateOrigin::IntroducedOrChanged,
        signals: vec![detector.into()],
        source: "synthetic_detector".into(),
        metadata: BTreeMap::new(),
    }
}

fn scope_file(path: &str, category: FileCategory) -> ReviewFile {
    // Поверхности берутся из той же классификации пути, что использует конвейер:
    // пустой список сделал бы контекст файла неизвестным.
    let (classified, surfaces) = anki_repo::code_review::scope::classify_path(path);
    debug_assert_eq!(classified, category);
    ReviewFile {
        path: path.into(),
        previous_path: None,
        status: anki_repo::code_review::scope::FileStatus::Modified,
        additions: None,
        deletions: None,
        category,
        surfaces,
        binary: false,
        base_state: ImageState::Text,
        base_size: 0,
        base_object_id: None,
        base_changed_lines: Vec::new(),
        post_state: ImageState::Text,
        post_size: 64,
        post_object_id: None,
        post_changed_lines: vec![LineRange { start: 1, end: 4 }],
    }
}

/// Пакет с production- и test-кандидатами, привязанный к синтетическому репозиторию.
fn synthetic_pack(repo: &SyntheticRepo, production: usize, tests: usize) -> ReviewPack {
    let mut candidates = Vec::new();
    for index in 0..production {
        candidates.push(candidate(
            &format!("production-{index}"),
            "error_path",
            "src/lib.rs",
            2,
            5,
            "let digits = value.trim();",
        ));
    }
    for index in 0..tests {
        candidates.push(candidate(
            &format!("tests-{index}"),
            "error_path",
            "src/tests/parse.rs",
            3,
            5,
            "assert!(crate::parse(\"1\").is_some());",
        ));
    }
    let files = vec![
        scope_file("src/lib.rs", FileCategory::Rust),
        scope_file("src/tests/parse.rs", FileCategory::Rust),
    ];
    ReviewPack {
        schema_version: REVIEW_SCHEMA_VERSION,
        target: repo.target(),
        scope: ReviewScope {
            merge_base_sha: repo.base_sha.clone(),
            text_image_limit_bytes: 1024,
            files,
        },
        diagnostics: Vec::new(),
        candidates,
        language: anki_repo::code_review::language::LanguageScan {
            schema_version: anki_repo::code_review::language::LANGUAGE_SCHEMA_VERSION,
            files: Vec::new(),
            candidates: Vec::new(),
            skipped: Vec::new(),
        },
        dependencies: Vec::new(),
        tests: Vec::new(),
        suppressions: Vec::new(),
        risk_surfaces: Vec::new(),
        tool_runs: vec![ToolRunEvidence {
            tool: "clippy".into(),
            status: "success".into(),
            exit_status: None,
            diagnostic_count: 0,
            malformed_lines: 0,
            ignored_records: 0,
            stderr_summary: None,
            message: None,
        }],
    }
}

/// Строит очередь в каталоге репозитория, где доступны точные Git-образы.
fn build_authoritative_queue(repo: &SyntheticRepo, pack: &ReviewPack) -> review_queue::ReviewQueue {
    let root = repo.path().to_path_buf();
    let pack = pack.clone();
    in_directory(&root, move || {
        let digest = digest_of(&pack);
        learning::queue_contexts_for_pack(&pack)
            .and_then(|contexts| review_queue::build(&pack, &digest, &contexts))
            .expect("очередь синтетического фикстура должна строиться")
    })
}

/// Разворачивает ожидаемый отказ загрузки без требования `Debug` у результата.
fn expect_load_error(
    result: Result<LoadedReview, anki_repo::error::DomainError>,
    message: &str,
) -> anki_repo::error::DomainError {
    match result {
        Ok(_) => panic!("{message}"),
        Err(error) => error,
    }
}

/// Строит очередь без авторитетных контекстов: уровень структурной проверки.
fn build_structure_only_queue(pack: &ReviewPack) -> review_queue::ReviewQueue {
    let digest = digest_of(pack);
    review_queue::build(pack, &digest, &BTreeMap::new())
        .expect("структурная очередь синтетического фикстура должна строиться")
}

struct RestoreCurrentDirectory(PathBuf);

impl Drop for RestoreCurrentDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::env::set_current_dir(&self.0)
            && !std::thread::panicking()
        {
            panic!("возврат в каталог запуска тестов: {error}");
        }
    }
}

fn in_directory<T>(path: &Path, action: impl FnOnce() -> T) -> T {
    let original = std::env::current_dir().expect("текущий каталог теста");
    std::env::set_current_dir(path).expect("переход в синтетический репозиторий");
    let _restore = RestoreCurrentDirectory(original);
    action()
}

#[test]
fn in_directory_restores_current_directory_after_panic() {
    let _guard = guard();
    let original = std::env::current_dir().expect("текущий каталог теста");
    let directory = TempDir::new("learning-directory-panic");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        in_directory(directory.path(), || panic!("синтетическая паника"));
    }));
    assert!(result.is_err());
    assert_eq!(
        std::env::current_dir().expect("каталог после паники"),
        original
    );
}

/// Digest точных байтов пакета: аналог SHA-256 `review.json`.
fn digest_of(pack: &ReviewPack) -> String {
    learning::import::sha256_hex(&serde_json::to_vec(pack).unwrap())
}

/// Заменяет решение одиночного кандидата, сохраняя взаимность связей.
fn resolve_individual(
    triage: &mut semantic_triage::SemanticTriage,
    candidate_id: &str,
    disposition: Disposition,
    finding: Option<&SemanticFinding>,
) {
    triage
        .unreviewed_candidate_ids
        .retain(|id| id != candidate_id);
    // Решение confirmed обязано опираться на связанное замечание, поэтому при
    // отсутствии явного замечания синтетика создаёт его сама.
    let owned = if disposition == Disposition::Confirmed && finding.is_none() {
        Some(SemanticFinding {
            id: format!("finding-{candidate_id}"),
            severity: Severity::Major,
            title: "Синтетическое подтверждённое замечание".into(),
            description: format!("{candidate_id}: подтверждённый дефект синтетического фикстура."),
            provenance: FindingProvenance::CandidateAssisted,
            candidate_ids: vec![candidate_id.to_owned()],
        })
    } else {
        None
    };
    let finding = finding.or(owned.as_ref());
    triage
        .individual_decisions
        .push(semantic_triage::CandidateDecision {
            candidate_id: candidate_id.into(),
            disposition,
            reason_code: ReasonCode::ExpectedFailurePath,
            explanation: "Синтетическое решение для проверки контракта learning.".into(),
            finding_ids: finding.map_or_else(Vec::new, |finding| vec![finding.id.clone()]),
        });
    if let Some(finding) = finding {
        triage.findings.push(finding.clone());
    }
}

/// Заменяет решение группы кандидатов на решение с указанным disposition.
fn resolve_group(
    triage: &mut semantic_triage::SemanticTriage,
    group_id: &str,
    candidate_ids: &[String],
    disposition: Disposition,
) {
    triage
        .unreviewed_candidate_ids
        .retain(|id| !candidate_ids.contains(id));
    triage.group_decisions.push(semantic_triage::GroupDecision {
        id: group_id.into(),
        candidate_ids: candidate_ids.to_vec(),
        representative_candidate_ids: candidate_ids
            .iter()
            .take(REPRESENTATIVE_LIMIT)
            .cloned()
            .collect(),
        disposition,
        reason_code: ReasonCode::TestFixture,
        explanation: "Синтетическая группа для проверки единиц наблюдения.".into(),
        finding_ids: Vec::new(),
    });
}

/// Готовит каталог артефактов и возвращает пути к трём документам.
struct Artifacts {
    directory: PathBuf,
}

impl Artifacts {
    fn write(
        directory: &Path,
        pack: &ReviewPack,
        queue: &review_queue::ReviewQueue,
        triage: &semantic_triage::SemanticTriage,
    ) -> Self {
        fs::create_dir_all(directory).unwrap();
        fs::write(
            directory.join("review.json"),
            serde_json::to_vec(pack).unwrap(),
        )
        .unwrap();
        let mut queue_bytes = serde_json::to_vec_pretty(queue).unwrap();
        queue_bytes.push(b'\n');
        fs::write(directory.join("review-queue.json"), queue_bytes).unwrap();
        fs::write(
            directory.join("semantic-triage.json"),
            serde_json::to_vec(triage).unwrap(),
        )
        .unwrap();
        Self {
            directory: directory.to_path_buf(),
        }
    }

    fn pack(&self) -> PathBuf {
        self.directory.join("review.json")
    }

    fn queue(&self) -> PathBuf {
        self.directory.join("review-queue.json")
    }

    fn triage(&self) -> PathBuf {
        self.directory.join("semantic-triage.json")
    }
}

/// Загружает артефакты так, как это делает вызывающая сторона из корня
/// синтетического репозитория: точные Git-образы ищутся в текущем каталоге.
fn load_in_repo(
    repo: &SyntheticRepo,
    artifacts: &Artifacts,
    structure_only: bool,
) -> Result<LoadedReview, anki_repo::error::DomainError> {
    let root = repo.path().to_path_buf();
    in_directory(&root, || {
        learning::load_review(
            &artifacts.pack(),
            &artifacts.queue(),
            Some(&artifacts.triage()),
            structure_only,
        )
    })
}

/// Загружает артефакты вместе с необязательным результатом изоляции.
fn load_in_repo_with_execution(
    repo: &SyntheticRepo,
    artifacts: &Artifacts,
    execution: Option<&Path>,
    structure_only: bool,
) -> Result<LoadedReview, anki_repo::error::DomainError> {
    let root = repo.path().to_path_buf();
    let execution = execution.map(Path::to_path_buf);
    in_directory(&root, || {
        learning::load_review_with_execution(
            &artifacts.pack(),
            &artifacts.queue(),
            Some(&artifacts.triage()),
            execution.as_deref(),
            structure_only,
        )
    })
}

/// Загружает артефакты без документа семантического разбора.
fn load_in_repo_without_triage(
    repo: &SyntheticRepo,
    artifacts: &Artifacts,
) -> Result<LoadedReview, anki_repo::error::DomainError> {
    let root = repo.path().to_path_buf();
    in_directory(&root, || {
        learning::load_review(&artifacts.pack(), &artifacts.queue(), None, false)
    })
}

/// Импортирует подготовленные артефакты в локальную историю.
fn import(loaded: &LoadedReview, store: &learning::LearningStore) -> learning::ImportRecord {
    learning::import_history(store, loaded, &learning::ImportRequest::default())
        .expect("проверенный импорт должен проходить")
}

/// Открывает базу learning во временном каталоге.
fn open_store(directory: &Path) -> learning::LearningStore {
    learning::LearningStore::open(learning::StoreOptions::at(directory.join("state.sqlite")))
        .expect("база learning должна открываться")
}

#[test]
fn idempotent_double_import_does_not_double_counts_or_findings() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-idempotent");
    let pack = synthetic_pack(&repo, 2, 2);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    let finding = SemanticFinding {
        id: "finding-1".into(),
        severity: Severity::Major,
        title: "Потеря ошибки разбора".into(),
        description: "src/lib.rs:2: результат parse отбрасывается без обработки.".into(),
        provenance: FindingProvenance::CandidateAssisted,
        candidate_ids: vec!["production-0".into()],
    };
    resolve_individual(
        &mut triage,
        "production-0",
        Disposition::Confirmed,
        Some(&finding),
    );
    semantic_triage::canonicalize(&mut triage);
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-idempotent-store");
    let store = open_store(store_dir.path());

    let loaded = load_in_repo(&repo, &artifacts, false)
        .expect("доверенный импорт должен находить точные Git-образы");
    assert_eq!(loaded.trust, TrustLevel::AstAuthenticated);
    assert_eq!(loaded.review_pack_sha256, digest);
    assert_eq!(
        loaded.queue_sha256,
        learning::import::sha256_hex(&fs::read(artifacts.queue()).unwrap()),
        "digest очереди считается по точным байтам файла"
    );

    let first = import(&loaded, &store);
    assert_eq!(first.observations.findings, 1);
    let generation_after_first = learning::import::generation(&store).unwrap();

    let second = import(&loaded, &store);
    assert_eq!(second.review_id, first.review_id);
    assert_eq!(second.revision, first.revision);
    let generation_after_second = learning::import::generation(&store).unwrap();
    assert_eq!(
        generation_after_first.trusted_reviews,
        generation_after_second.trusted_reviews
    );
    assert_eq!(
        generation_after_second.trusted_reviews, 1,
        "точный повтор не должен создавать вторую запись"
    );

    // Числа наблюдений не удваиваются.
    assert_eq!(second.observations.findings, first.observations.findings);
    assert_eq!(
        second.observations.raw_candidates,
        first.observations.raw_candidates
    );
    assert_eq!(
        learning::import::list_imports(&store, true, 10)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn concurrent_exact_imports_commit_one_review_and_one_finding() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-concurrent-idempotent");
    let pack = synthetic_pack(&repo, 2, 2);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    let finding = SemanticFinding {
        id: "finding-concurrent".into(),
        severity: Severity::Major,
        title: "Синтетическое подтверждённое замечание".into(),
        description: "Проверка параллельной идемпотентности импорта.".into(),
        provenance: FindingProvenance::CandidateAssisted,
        candidate_ids: vec!["production-0".into()],
    };
    resolve_individual(
        &mut triage,
        "production-0",
        Disposition::Confirmed,
        Some(&finding),
    );
    semantic_triage::canonicalize(&mut triage);
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let loaded_first = load_in_repo(&repo, &artifacts, false).unwrap();
    let loaded_second = load_in_repo(&repo, &artifacts, false).unwrap();
    let store_dir = TempDir::new("learning-concurrent-idempotent-store");
    let database = store_dir.path().join("state.sqlite");
    let store_first = learning::LearningStore::open(learning::StoreOptions::at(&database)).unwrap();
    let store_second =
        learning::LearningStore::open(learning::StoreOptions::at(&database)).unwrap();
    let verification_store =
        learning::LearningStore::open(learning::StoreOptions::at(&database)).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut handles = Vec::new();
    for (store, loaded) in [(store_first, loaded_first), (store_second, loaded_second)] {
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            learning::import::import_with_outcome(
                &store,
                &loaded,
                &learning::ImportRequest::default(),
            )
        }));
    }
    barrier.wait();
    let mut outcomes = Vec::new();
    for handle in handles {
        outcomes.push(
            handle
                .join()
                .expect("импортёр не должен паниковать")
                .unwrap_or_else(|error| {
                    panic!("конкурентный импорт должен дождаться записи: {error:?}")
                }),
        );
    }
    assert_eq!(outcomes[0].record.review_id, outcomes[1].record.review_id);
    assert_eq!(outcomes[0].record.revision, outcomes[1].record.revision);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.status == learning::ImportStatus::Imported)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.status == learning::ImportStatus::NoopExisting)
            .count(),
        1
    );
    let history = learning::import::list_imports(&verification_store, true, 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].observations.findings, 1);
    assert_eq!(
        learning::import::generation(&verification_store)
            .unwrap()
            .trusted_reviews,
        1
    );
}

#[test]
fn conflicting_bytes_at_same_identity_create_audited_revision() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-revision");
    let pack = synthetic_pack(&repo, 2, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Acceptable, None);
    semantic_triage::canonicalize(&mut triage);
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-revision-store");
    let store = open_store(store_dir.path());

    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let first = import(&loaded, &store);

    // Другие байты triage при той же source identity не перезаписывают молча.
    let mut changed = triage.clone();
    changed.individual_decisions[0].disposition = Disposition::FalsePositive;
    changed.individual_decisions[0].reason_code = ReasonCode::DuplicateSignal;
    semantic_triage::canonicalize(&mut changed);
    semantic_triage::validate(&changed, &pack, &digest).unwrap();
    let artifacts_changed =
        Artifacts::write(&repo.path().join("artifacts-2"), &pack, &queue, &changed);
    let loaded_changed = load_in_repo(&repo, &artifacts_changed, false).unwrap();
    let second = import(&loaded_changed, &store);

    assert_ne!(second.review_id, first.review_id);
    assert_eq!(
        second.revision_of.as_deref(),
        Some(first.review_id.as_str())
    );
    assert_eq!(second.revision, first.revision + 1);
    let preserved = learning::show_import(&store, &first.review_id).unwrap();
    assert_eq!(
        preserved.superseded_by.as_deref(),
        Some(second.review_id.as_str()),
        "прежняя запись сохраняется и помечается вытесненной"
    );
    assert_eq!(
        preserved.observations.raw_candidates,
        first.observations.raw_candidates
    );
    assert!(
        second
            .limitations
            .iter()
            .any(|text| text.contains("аудируемой ревизией"))
    );
    assert_eq!(
        learning::import::generation(&store)
            .unwrap()
            .trusted_reviews,
        2
    );
}

#[test]
fn different_source_identity_is_not_mixed_with_previous_run() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-identity");
    let pack = synthetic_pack(&repo, 2, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let triage = semantic_triage::initialize(&pack, &digest);
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-identity-store");
    let store = open_store(store_dir.path());

    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let first = import(&loaded, &store);

    // Другой вариант рабочего пространства при тех же байтах: отдельная identity.
    let snapshot = format!("snapshot-{}", "b".repeat(32));
    let snapshot_record = learning::import::import_history(
        &store,
        &loaded,
        &learning::ImportRequest {
            workspace_variant: snapshot.clone(),
            workspace_label: Some(snapshot.clone()),
        },
    )
    .unwrap();
    assert_ne!(snapshot_record.review_id, first.review_id);
    assert_eq!(snapshot_record.workspace_variant, snapshot);
    assert!(snapshot_record.revision_of.is_none());

    // Другой набор анализаторов при той же Git identity: тоже отдельный run.
    let mut analyzer_variant = pack.clone();
    analyzer_variant.tool_runs.push(ToolRunEvidence {
        tool: "rustc".into(),
        status: "diagnostics".into(),
        exit_status: None,
        diagnostic_count: 1,
        malformed_lines: 0,
        ignored_records: 0,
        stderr_summary: None,
        message: None,
    });
    let variant_digest = digest_of(&analyzer_variant);
    let variant_triage = semantic_triage::initialize(&analyzer_variant, &variant_digest);
    let variant_queue = build_authoritative_queue(&repo, &analyzer_variant);
    let variant_artifacts = Artifacts::write(
        &repo.path().join("artifacts-analyzer"),
        &analyzer_variant,
        &variant_queue,
        &variant_triage,
    );
    let variant_loaded = load_in_repo(&repo, &variant_artifacts, false).unwrap();
    let variant_record = import(&variant_loaded, &store);
    assert_ne!(variant_record.review_id, first.review_id);
    assert_ne!(
        variant_record.inputs.analyzer_digest,
        first.inputs.analyzer_digest
    );
    assert!(variant_record.revision_of.is_none());

    assert_eq!(
        learning::import::generation(&store)
            .unwrap()
            .trusted_reviews,
        3
    );
}

#[test]
fn tampered_queue_never_becomes_validated_ast_history() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-tamper");
    let pack = synthetic_pack(&repo, 3, 2);
    let authoritative = build_authoritative_queue(&repo, &pack);
    let structure_only = build_structure_only_queue(&pack);
    let digest = digest_of(&pack);
    let triage = semantic_triage::initialize(&pack, &digest);
    let store_dir = TempDir::new("learning-tamper-store");
    let store = open_store(store_dir.path());

    // Подмена production → tests в копии очереди: структурная сверка её пропускает,
    // независимая проверка по точным Git-образам — нет.
    let mut forged = authoritative.clone();
    let mut flipped = false;
    for unit in &mut forged.units {
        if unit.signature.classification.execution
            == Some(anki_repo::code_review::scope::FileSurface::Production)
        {
            unit.signature.classification.execution =
                Some(anki_repo::code_review::scope::FileSurface::Tests);
            unit.signature.classification.code_role = review_queue::CodeRole::TestAssertion;
            flipped = true;
        }
    }
    assert!(
        flipped,
        "подмена обязана найти production-единицу в очереди"
    );
    assert_ne!(
        forged, authoritative,
        "подмена обязана менять сохранённую очередь"
    );
    let forged_dir = repo.path().join("forged");
    let forged_artifacts = Artifacts::write(&forged_dir, &pack, &forged, &triage);
    let error = expect_load_error(
        load_in_repo(&repo, &forged_artifacts, false),
        "подменённая очередь не должна становиться проверенной историей",
    );
    assert_eq!(error.code, ErrorCode::ReviewArtifactInvalid);
    assert_eq!(
        learning::import::generation(&store)
            .unwrap()
            .trusted_reviews,
        0
    );

    // Ослабленный уровень возможен только по явному запросу и маркируется
    // карантином: запись не участвует в learning.
    let quarantine_artifacts = Artifacts::write(
        &repo.path().join("quarantine"),
        &pack,
        &structure_only,
        &triage,
    );
    let quarantined = expect_quarantine(&repo, &quarantine_artifacts);
    assert_eq!(quarantined.trust, TrustLevel::StructureOnlyQuarantine);
    assert!(!quarantined.trust.participates_in_learning());
    let record = import(&quarantined, &store);
    assert_eq!(
        record.outcome,
        learning::model::ReviewedOutcome::Quarantined
    );
    assert!(
        record
            .limitations
            .iter()
            .any(|text| text.contains("карантин"))
    );
    let generation = learning::import::generation(&store).unwrap();
    assert_eq!(generation.trusted_reviews, 0);
    assert_eq!(generation.quarantined_reviews, 1);
    assert_eq!(generation.trusted_units, 0);

    // Та же структурная очередь не принимается доверенным путём: молчаливого
    // ослабления уровня доверия нет.
    let error = expect_load_error(
        load_in_repo(&repo, &quarantine_artifacts, false),
        "структурная очередь не должна приниматься доверенным путём",
    );
    assert_eq!(error.code, ErrorCode::ReviewArtifactInvalid);

    // Подлинная очередь принимается доверенным путём.
    let accepted_artifacts = Artifacts::write(
        &repo.path().join("accepted"),
        &pack,
        &authoritative,
        &triage,
    );
    let accepted = load_in_repo(&repo, &accepted_artifacts, false)
        .expect("подлинная очередь должна приниматься");
    assert_eq!(accepted.trust, TrustLevel::AstAuthenticated);
    import(&accepted, &store);
    assert_eq!(
        learning::import::generation(&store)
            .unwrap()
            .trusted_reviews,
        1
    );
}

/// Ожидаемый карантинный импорт по явно запрошенной структурной проверке.
fn expect_quarantine(repo: &SyntheticRepo, artifacts: &Artifacts) -> LoadedReview {
    let root = repo.path().to_path_buf();
    match in_directory(&root, || {
        learning::load_review(
            &artifacts.pack(),
            &artifacts.queue(),
            Some(&artifacts.triage()),
            true,
        )
    }) {
        Ok(loaded) => loaded,
        Err(error) => panic!("ослабленный импорт обязан выполняться по явному запросу: {error:?}"),
    }
}

#[test]
fn missing_git_objects_do_not_silently_downgrade_to_structure_only() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-no-git");
    let pack = synthetic_pack(&repo, 1, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let triage = semantic_triage::initialize(&pack, &digest);
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    // Вне Git-репозитория точные образы недоступны: ослабления до структурной
    // проверки без явного запроса быть не должно.
    let outside = TempDir::new("learning-no-git-outside");
    let error = expect_load_error(
        in_directory(outside.path(), || {
            learning::load_review(
                &artifacts.pack(),
                &artifacts.queue(),
                Some(&artifacts.triage()),
                false,
            )
        }),
        "без точных Git-образов доверенный путь обязан отказать",
    );
    assert_eq!(error.code, ErrorCode::SyntaxAuthenticityUnavailable);

    // По явному запросу та же загрузка возможна только как карантин.
    let quarantined = in_directory(outside.path(), || {
        learning::load_review(
            &artifacts.pack(),
            &artifacts.queue(),
            Some(&artifacts.triage()),
            true,
        )
    })
    .expect("явная структурная проверка не зависит от образов Git");
    assert_eq!(quarantined.trust, TrustLevel::StructureOnlyQuarantine);
}

#[test]
fn source_digest_mismatch_is_reported_as_source_changed() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-digest");
    let pack = synthetic_pack(&repo, 2, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let triage = semantic_triage::initialize(&pack, &digest);
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);

    // Перезаписываем review.json другими байтами при прежней identity артефактов:
    // добавляем ещё одного кандидата, что не меняет Git-identity пакета.
    let mut changed = pack.clone();
    changed.candidates.push(candidate(
        "production-extra",
        "error_path",
        "src/lib.rs",
        3,
        5,
        "digits.parse().ok()",
    ));
    fs::write(artifacts.pack(), serde_json::to_vec(&changed).unwrap()).unwrap();
    let error = expect_load_error(
        load_in_repo(&repo, &artifacts, false),
        "несовпадающий digest источника обязан отвергаться",
    );
    assert_eq!(error.code, ErrorCode::SourceChanged);
}

#[test]
fn large_group_decision_is_one_independent_unit() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-large-group");
    let pack = synthetic_pack(&repo, 0, 1200);
    let queue = build_authoritative_queue(&repo, &pack);
    assert_eq!(
        queue.units.len(),
        1,
        "1200 однородных кандидатов образуют одну единицу очереди"
    );
    let unit = &queue.units[0];
    assert_eq!(unit.candidate_ids().len(), 1200);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_group(
        &mut triage,
        &unit.id,
        unit.candidate_ids(),
        Disposition::Acceptable,
    );
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    assert_eq!(triage.group_decisions.len(), 1);
    assert_eq!(triage.individual_decisions.len(), 0);

    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-large-group-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let record = import(&loaded, &store);

    assert_eq!(record.observations.raw_candidates, 1200);
    assert_eq!(record.observations.covered_candidates, 1200);
    assert_eq!(record.observations.group_decisions, 1);
    assert_eq!(
        record.observations.reviewed_units, 1,
        "групповое решение даёт одну независимую единицу наблюдения, а не 1200"
    );
    assert_eq!(record.observations.unresolved_units, 0);
    assert_eq!(record.observations.confirmed_candidates, 0);
    assert_eq!(record.observations.individual_decisions, 0);
}

#[test]
fn one_finding_with_many_candidates_stays_one_finding() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-one-finding");
    // Однородные кандидаты тестовой поверхности образуют одну единицу очереди.
    let pack = synthetic_pack(&repo, 0, 3);
    let queue = build_authoritative_queue(&repo, &pack);
    let unit = &queue.units[0];
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    let ids: Vec<String> = unit.candidate_ids().to_vec();
    let finding = SemanticFinding {
        id: "finding-shared".into(),
        severity: Severity::Critical,
        title: "Общая причина отказов".into(),
        description: "src/lib.rs:2: один дефект объясняет несколько сигналов.".into(),
        provenance: FindingProvenance::CandidateAssisted,
        candidate_ids: ids.clone(),
    };
    triage.unreviewed_candidate_ids.clear();
    triage.group_decisions.push(semantic_triage::GroupDecision {
        id: unit.id.clone(),
        candidate_ids: ids.clone(),
        representative_candidate_ids: ids.iter().take(1).cloned().collect(),
        disposition: Disposition::Confirmed,
        reason_code: ReasonCode::Other,
        explanation: "Один дефект подтверждён группой сигналов.".into(),
        finding_ids: vec![finding.id.clone()],
    });
    triage.findings.push(finding);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();

    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-one-finding-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let record = import(&loaded, &store);
    assert_eq!(record.observations.findings, 1);
    assert_eq!(record.observations.findings_candidate_assisted, 1);
    assert_eq!(record.observations.reviewed_units, 1);

    // Поиск по описанию замечания возвращает ровно один случай.
    let page = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            text: Some("общая причина".into()),
            limit: 10,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert_eq!(page.cases.len(), 1);
    assert_eq!(page.cases[0].finding_id.as_deref(), Some("finding-shared"));

    let detector_filtered = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            detector: Some("error_path".into()),
            limit: 10,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert!(
        detector_filtered.cases.iter().any(|case| {
            case.finding_id.as_deref() == Some("finding-shared")
                && case.unit_id.as_deref() == Some(unit.id.as_str())
                && case.detector.as_deref() == Some("error_path")
        }),
        "замечание по кандидату связывается с единицей для фильтра detector"
    );
}

#[test]
fn independent_finding_is_not_a_positive_detector_outcome() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-independent");
    let pack = synthetic_pack(&repo, 2, 0);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    let finding = SemanticFinding {
        id: "finding-independent".into(),
        severity: Severity::Major,
        title: "Пропущенная проверка остатка".into(),
        description: "src/lib.rs:2: ревьюер нашёл дефект вне сигналов детектора.".into(),
        provenance: FindingProvenance::Independent,
        candidate_ids: Vec::new(),
    };
    triage.findings.push(finding);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();

    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-independent-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let record = import(&loaded, &store);

    assert_eq!(record.observations.findings, 1);
    assert_eq!(record.observations.findings_independent, 1);
    assert_eq!(record.observations.findings_direct_candidate, 0);
    assert_eq!(record.observations.findings_candidate_assisted, 0);
    // Ни один кандидат не получил положительного исхода от независимого finding.
    assert_eq!(record.observations.confirmed_candidates, 0);
    assert_eq!(record.observations.covered_candidates, 0);
    assert_eq!(record.observations.unreviewed_candidates, 2);
    assert_eq!(record.observations.reviewed_units, 0);
    assert_eq!(record.observations.unresolved_units, 2);
}

#[test]
fn unreviewed_and_uncertain_history_gets_no_invented_outcomes() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-partial");
    let pack = synthetic_pack(&repo, 2, 2);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Uncertain, None);
    resolve_individual(&mut triage, "tests-0", Disposition::NotApplicable, None);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    assert!(!triage.unreviewed_candidate_ids.is_empty());

    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-partial-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let record = import(&loaded, &store);

    assert_eq!(
        record.outcome,
        learning::model::ReviewedOutcome::PartiallyReviewed
    );
    assert_eq!(record.observations.uncertain_candidates, 1);
    assert_eq!(record.observations.not_applicable_candidates, 1);
    assert_eq!(record.observations.confirmed_candidates, 0);
    assert_eq!(record.observations.acceptable_candidates, 0);
    assert!(record.observations.unreviewed_candidates > 0);
    assert!(record.observations.unresolved_units > 0);

    // Отсутствующий semantic-triage не выдумывает решения вовсе.
    let no_triage = load_in_repo_without_triage(&repo, &artifacts).unwrap();
    let record_no_triage = import(&no_triage, &store);
    assert_eq!(
        record_no_triage.outcome,
        learning::model::ReviewedOutcome::NoTriage
    );
    assert_eq!(record_no_triage.observations.covered_candidates, 0);
    assert_eq!(record_no_triage.observations.findings, 0);
    assert_eq!(
        record_no_triage.observations.unreviewed_candidates,
        record_no_triage.observations.raw_candidates
    );
    assert_eq!(record_no_triage.observations.reviewed_units, 0);
}

#[test]
fn corrupted_and_newer_databases_fail_without_destroying_data() {
    let directory = TempDir::new("learning-corrupt");
    let database = directory.path().join("state.sqlite");
    fs::write(&database, "это не база SQLite").unwrap();
    let error = learning::LearningStore::open(learning::StoreOptions::at(&database)).unwrap_err();
    assert_eq!(
        error.code,
        ErrorCode::LearningCorrupt,
        "неожиданный код отказа: {error:?}"
    );
    assert!(database.is_file(), "исходный файл не удаляется");

    // Будущая версия схемы отвергается до любых изменений.
    let future = directory.path().join("future.sqlite");
    let connection = rusqlite::Connection::open(&future).unwrap();
    let original_journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    connection
        .pragma_update(None, "user_version", 99u32)
        .unwrap();
    connection
        .execute("CREATE TABLE marker (value TEXT)", [])
        .unwrap();
    connection
        .execute("INSERT INTO marker (value) VALUES ('preserved')", [])
        .unwrap();
    drop(connection);
    let error = learning::LearningStore::open(learning::StoreOptions::at(&future)).unwrap_err();
    assert_eq!(error.code, ErrorCode::LearningSchemaUnsupported);
    let connection = rusqlite::Connection::open(&future).unwrap();
    let marker: String = connection
        .query_row("SELECT value FROM marker", [], |row| row.get(0))
        .unwrap();
    assert_eq!(marker, "preserved", "данные будущей схемы не разрушаются");
    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(
        journal_mode, original_journal_mode,
        "будущая схема отвергается до смены режима журнала"
    );
}

#[test]
fn concurrent_readers_and_writer_keep_history_consistent() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-concurrency");
    let pack = synthetic_pack(&repo, 2, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let triage = semantic_triage::initialize(&pack, &digest);
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-concurrency-store");
    let database = store_dir.path().join("state.sqlite");
    let store = learning::LearningStore::open(learning::StoreOptions::at(&database)).unwrap();
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    import(&loaded, &store);

    // Открываем соединения заранее, чтобы проверять параллельные транзакции
    // чтения и записи, а не гонки старта команд.
    let readers: Vec<_> = (0..3)
        .map(|_| {
            learning::LearningStore::open(learning::StoreOptions {
                create: false,
                ..learning::StoreOptions::at(database.clone())
            })
            .expect("читатель должен открывать существующую базу")
        })
        .collect();
    let writer = learning::LearningStore::open(learning::StoreOptions::at(&database)).unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(readers.len() + 1));
    let mut handles = Vec::new();
    for reader in readers {
        let barrier = std::sync::Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let mut previous_count = 0;
            for _ in 0..20 {
                let records = learning::import::list_imports(&reader, true, 50).unwrap();
                assert!((1..=11).contains(&records.len()));
                assert!(records.len() >= previous_count, "число записей не убывает");
                previous_count = records.len();
                for record in records {
                    assert_eq!(record.observations.raw_candidates, 3);
                    assert!(!record.review_id.is_empty());
                }
                let _ = learning::import::generation(&reader).unwrap();
            }
        }));
    }
    barrier.wait();
    for index in 0..10 {
        let variant = format!("snapshot-{index:032x}");
        let record = learning::import::import_history(
            &writer,
            &loaded,
            &learning::ImportRequest {
                workspace_variant: variant.clone(),
                workspace_label: Some(variant),
            },
        )
        .unwrap();
        assert!(!record.review_id.is_empty());
    }
    for handle in handles {
        handle.join().expect("читатель не должен падать");
    }
    let records = learning::import::list_imports(&store, true, 50).unwrap();
    assert_eq!(records.len(), 11, "все неповторные импорты сохранены");
    for record in &records {
        assert_eq!(record.observations.raw_candidates, 3);
    }
}

#[test]
fn queue_with_wrong_order_is_rejected_without_changing_history() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-queue-revision");
    let pack = synthetic_pack(&repo, 2, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let triage = semantic_triage::initialize(&pack, &digest);
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-queue-revision-store");
    let store = open_store(store_dir.path());

    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let first = import(&loaded, &store);

    // Неверный порядок очереди отвергается до записи ревизии.
    let mut second_queue = queue.clone();
    let mut resorted = second_queue.units.clone();
    resorted.reverse();
    second_queue.units = resorted;
    let second_artifacts = Artifacts::write(
        &repo.path().join("artifacts-2"),
        &pack,
        &second_queue,
        &triage,
    );
    let error = expect_load_error(
        load_in_repo(&repo, &second_artifacts, false),
        "очередь с неверным порядком не должна проходить проверку",
    );
    assert_eq!(error.code, ErrorCode::ReviewArtifactInvalid);

    let digest_after = learning::import::generation(&store).unwrap();
    assert_eq!(digest_after.trusted_reviews, 1);
    let preserved = learning::show_import(&store, &first.review_id).unwrap();
    assert!(preserved.superseded_by.is_none());
}

#[test]
fn patterns_keep_contexts_separate_and_abstain_on_weak_support() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-patterns");
    let pack = synthetic_pack(&repo, 2, 2);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Confirmed, None);
    resolve_individual(&mut triage, "tests-0", Disposition::Acceptable, None);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-patterns-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    import(&loaded, &store);

    let report = learning::pattern_report(&store, &learning::patterns::PatternQuery::default())
        .expect("отчёт по паттернам должен строиться на согласованном снимке");
    assert!(!report.rules.is_empty());
    for rule in &report.rules {
        assert!(
            rule.support.level != SupportLevel::Supported,
            "малая выборка обязана воздерживаться, а не выдавать псевдоточную оценку"
        );
        assert_eq!(rule.support.confirmed_share_lower_bound, None);
        assert!(rule.support.support_units <= 4);
    }

    // Production и tests не сливаются в один ключ признаков.
    let mut executions = BTreeSet::new();
    let mut origins = BTreeSet::new();
    for rule in &report.rules {
        executions.insert(rule.key.get("execution").cloned().unwrap_or_default());
        origins.insert(rule.key.get("origin").cloned().unwrap_or_default());
    }
    assert!(
        executions.len() > 1,
        "production и tests обязаны различаться: {executions:?}"
    );
    assert!(!origins.is_empty());

    // Формула нижней границы согласованности закреплена тестом.
    let lower = learning::patterns::wilson_lower_bound(1, 7);
    assert!((0.0..0.3).contains(&lower), "нижняя граница: {lower}");
    assert_eq!(learning::patterns::wilson_lower_bound(0, 0), 0.0);
    let strong = learning::patterns::wilson_lower_bound(50, 100);
    assert!(strong > lower, "большая выборка даёт более высокую границу");
}

#[test]
fn contradictory_support_is_reported_as_contradictory() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-contradiction");
    let pack = synthetic_pack(&repo, 4, 0);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Confirmed, None);
    resolve_individual(&mut triage, "production-1", Disposition::Confirmed, None);
    resolve_individual(&mut triage, "production-2", Disposition::Confirmed, None);
    resolve_individual(
        &mut triage,
        "production-3",
        Disposition::FalsePositive,
        None,
    );
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-contradiction-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    import(&loaded, &store);

    let report =
        learning::pattern_report(&store, &learning::patterns::PatternQuery::default()).unwrap();
    let rule = report
        .rules
        .iter()
        .find(|rule| rule.support.support_units >= 4)
        .expect("должна существовать поддерживающая правило единица");
    assert_eq!(rule.support.level, SupportLevel::Contradictory);
    assert_eq!(rule.support.contradicting_unit_ids.len(), 1);
    assert_eq!(rule.support.confirmed_units, 3);
    assert_eq!(rule.support.false_positive_units, 1);
    assert!(rule.support.explanation.contains("противоречат"));
}

#[test]
fn production_test_and_human_machine_contexts_do_not_merge() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-contexts");
    let pack = synthetic_pack(&repo, 1, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let triage = semantic_triage::initialize(&pack, &digest);
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-contexts-store");
    let store = open_store(store_dir.path());

    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    import(&loaded, &store);

    let keys: Vec<BTreeMap<String, String>> = store
        .read(|read| {
            let mut statement = read
                .transaction()
                .prepare("SELECT feature_json FROM learning_unit ORDER BY unit_id")
                .unwrap();
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap();
            let mut values = Vec::new();
            for row in rows {
                values.push(row.unwrap());
            }
            Ok(values)
        })
        .unwrap()
        .into_iter()
        .map(|raw| serde_json::from_str::<BTreeMap<String, String>>(&raw).unwrap())
        .collect();
    assert_eq!(keys.len(), 2);
    let mut distinguishable = false;
    for key in &keys {
        assert!(key.contains_key("origin"));
        assert!(key.contains_key("role"));
        assert!(key.contains_key("code_role"));
        assert!(key.contains_key("text_role"));
        assert!(key.contains_key("execution"));
        assert!(key.contains_key("file_category"));
        assert!(
            keys.iter()
                .all(|other| other.get("classification_unknown")
                    == key.get("classification_unknown"))
        );
    }
    if keys[0] != keys[1] {
        distinguishable = true;
    }
    assert!(
        distinguishable,
        "production/test, human/machine и known/unknown не сливаются в один ключ"
    );
}

#[test]
fn optional_execution_result_is_verified_before_it_enters_history() {
    use anki_repo::code_review::execution::{
        CommandRequest, ExecutionMode, PrepareOptions, RunOptions, run_job,
    };

    let _guard = guard();
    let repo = SyntheticRepo::create("learning-execution");
    let pack = synthetic_pack(&repo, 2, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let triage = semantic_triage::initialize(&pack, &digest);
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-execution-store");
    let store = open_store(store_dir.path());

    let source_pack = repo
        .path()
        .join(".anki-repo/review/local")
        .join(&pack.target.head_sha)
        .join("review.json");
    fs::create_dir_all(source_pack.parent().unwrap()).unwrap();
    let pack_bytes = fs::read(artifacts.pack()).unwrap();
    fs::write(&source_pack, &pack_bytes).unwrap();
    let job = anki_repo::code_review::execution::prepare_job(
        repo.path(),
        &pack,
        &pack_bytes,
        PrepareOptions {
            mode: ExecutionMode::IsolatedChecks,
            scope: "learning-contract".into(),
            source_pack,
            pr_number: None,
        },
    )
    .unwrap();
    let result_path = job.directory().join("result.json");

    // Подготовленное, но не исполненное задание не является источником истории.
    let error = expect_load_error(
        load_in_repo_with_execution(&repo, &artifacts, Some(&result_path), false),
        "незавершённый результат не должен приниматься",
    );
    assert_eq!(error.code, ErrorCode::ExecutionNotCompleted);

    let result = run_job(
        &job,
        &CommandRequest {
            argv: vec!["/bin/true".into()],
            cwd: ".".into(),
            options: RunOptions::default(),
        },
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        result.status,
        anki_repo::code_review::execution::ExecutionStatus::Passed
    );
    let bytes = fs::read(&result_path).unwrap();

    // Завершённый результат проверяется по манифесту, state и source snapshot.
    let loaded = load_in_repo_with_execution(&repo, &artifacts, Some(&result_path), false).unwrap();
    let record = import(&loaded, &store);
    assert_eq!(
        record.inputs.execution_result_sha256.as_deref(),
        Some(learning::import::sha256_hex(&bytes).as_str())
    );
    assert_eq!(
        record.inputs.execution_schema_version,
        Some(anki_repo::code_review::execution::EXECUTION_SCHEMA_VERSION)
    );
    assert_eq!(
        record.inputs.execution_evidence.as_ref().unwrap().status,
        anki_repo::code_review::execution::ExecutionStatus::Passed
    );
    assert_eq!(record.trust, TrustLevel::AstAuthenticated);

    // Валидный результат другого набора байтов review.json не принимается.
    let mut foreign_pack = pack.clone();
    foreign_pack.scope.text_image_limit_bytes += 1;
    let foreign_digest = digest_of(&foreign_pack);
    let foreign_queue = build_authoritative_queue(&repo, &foreign_pack);
    let foreign_triage = semantic_triage::initialize(&foreign_pack, &foreign_digest);
    let foreign_artifacts = Artifacts::write(
        &repo.path().join("foreign-artifacts"),
        &foreign_pack,
        &foreign_queue,
        &foreign_triage,
    );
    let error = expect_load_error(
        load_in_repo_with_execution(&repo, &foreign_artifacts, Some(&result_path), false),
        "результат другого ревью не должен приниматься",
    );
    assert_eq!(error.code, ErrorCode::SourceChanged);
}

#[test]
fn export_and_restore_preserve_provenance_versions_and_no_absolute_paths() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-transfer");
    let pack = synthetic_pack(&repo, 2, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Confirmed, None);
    let long_comment = "界".repeat(400);
    assert!(long_comment.len() > learning::import::MAX_STORED_TEXT_BYTES);
    triage
        .individual_decisions
        .iter_mut()
        .find(|decision| decision.candidate_id == "production-0")
        .unwrap()
        .explanation = long_comment;
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let source_dir = TempDir::new("learning-transfer-source");
    let source_store = open_store(source_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let record = import(&loaded, &source_store);

    // Правка семантического исхода и её отзыв остаются аудируемыми.
    let event = FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: "event-correction".into(),
        review_id: record.review_id.clone(),
        unit_id: "individual-production-0".to_owned(),
        candidate_id: Some("production-0".into()),
        kind: FeedbackKind::SemanticOutcomeRevision,
        action: FeedbackAction::Append,
        supersedes_event_id: None,
        effective_disposition: Some("false_positive".into()),
        usefulness: None,
        explanation: "Ревьюер уточнил вывод после проверки контекста.".into(),
        provenance: "reviewer".into(),
        recorded_at: 1,
    };
    learning::feedback::record_feedback(&source_store, &event).unwrap();

    let exported = learning::transfer::export_history(&source_store).unwrap();
    assert_eq!(
        exported.archive.manifest.export_schema_version,
        learning::transfer::EXPORT_SCHEMA_VERSION
    );
    assert!(
        exported
            .archive
            .candidates
            .iter()
            .all(|candidate| candidate.snippet.is_none()),
        "переносимый архив содержит ссылки, но не фрагменты исходного кода"
    );
    assert!(exported.archive.search.iter().any(|case| {
        case.kind == "decision" && case.text.len() <= learning::import::MAX_STORED_TEXT_BYTES
    }));
    let encoded = serde_json::to_vec(&exported.archive).unwrap();
    let text = String::from_utf8(encoded).unwrap();
    for absolute in [repo.path(), source_dir.path()] {
        let absolute = absolute.to_str().unwrap();
        let encoded_absolute = serde_json::to_string(absolute).unwrap();
        let encoded_absolute = &encoded_absolute[1..encoded_absolute.len() - 1];
        assert!(
            !text.contains(encoded_absolute),
            "переносимый архив не содержит абсолютный путь фикстуры"
        );
    }
    assert!(!text.contains("\\Users\\"));

    let restore_dir = TempDir::new("learning-transfer-restore");
    let restored_store = open_store(restore_dir.path());
    let summary = learning::transfer::restore_history(&restored_store, &exported.archive).unwrap();
    assert_eq!(summary.restored_reviews, 1);
    let generation_after_restore = summary.generation.revision;
    let restored = learning::import::list_imports(&restored_store, true, 10).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].review_id, record.review_id);
    assert_eq!(restored[0].repository_id, record.repository_id);
    assert_eq!(restored[0].head_sha, record.head_sha);
    assert_eq!(restored[0].trust, TrustLevel::AstAuthenticated);
    assert_eq!(restored[0].revision, record.revision);
    assert_eq!(restored[0].observations.raw_candidates, 3);
    assert_eq!(
        restored[0].inputs.execution_evidence,
        record.inputs.execution_evidence
    );
    assert_eq!(
        restored[0].inputs.classifier_digest,
        record.inputs.classifier_digest
    );

    // Событие правки и действующий исход восстановлены.
    let outcome = learning::feedback::outcome(
        &restored_store,
        &record.review_id,
        "individual-production-0",
    )
    .unwrap();
    assert_eq!(
        outcome.original_disposition.as_deref(),
        Some("confirmed"),
        "исходный вывод виден рядом с действующим"
    );
    assert_eq!(
        outcome.effective_disposition.as_deref(),
        Some("false_positive")
    );
    assert!(
        outcome
            .effective_event_ids
            .contains(&"event-correction".to_owned())
    );

    // Повторное восстановление не затирает более новую локальную дочернюю строку.
    restored_store
        .write(|write| {
            write.execute(
                "UPDATE learning_candidate SET path = 'src/local-update.rs'
                 WHERE review_id = ?1 AND candidate_id = 'production-0'",
                rusqlite::params![record.review_id],
            )?;
            write.execute(
                "UPDATE learning_feedback SET explanation = 'локальное обновление'
                 WHERE event_id = 'event-correction'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let generation_after_local_update = learning::import::generation(&restored_store)
        .unwrap()
        .revision;
    assert!(generation_after_local_update > generation_after_restore);
    let repeated = learning::transfer::restore_history(&restored_store, &exported.archive).unwrap();
    assert_eq!(repeated.unchanged_reviews, 1);
    assert_eq!(repeated.restored_reviews, 0);
    assert_eq!(repeated.generation.revision, generation_after_local_update);
    let preserved_path: String = restored_store
        .connection()
        .query_row(
            "SELECT path FROM learning_candidate WHERE review_id = ?1 AND candidate_id = 'production-0'",
            rusqlite::params![record.review_id],
            |row| row.get(0),
        )
        .unwrap();
    let preserved_explanation: String = restored_store
        .connection()
        .query_row(
            "SELECT explanation FROM learning_feedback WHERE event_id = 'event-correction'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(preserved_path, "src/local-update.rs");
    assert_eq!(preserved_explanation, "локальное обновление");

    restored_store
        .write(|write| {
            write.execute(
                "UPDATE learning_import SET review_pack_sha256 = ?2 WHERE review_id = ?1",
                rusqlite::params![record.review_id, "f".repeat(64)],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        learning::transfer::restore_history(&restored_store, &exported.archive)
            .unwrap_err()
            .code,
        ErrorCode::LearningConflict,
        "конфликтующий review_pack_sha256 не заменяет строку импорта поверх дочерних данных"
    );
    let preserved_after_conflict: String = restored_store
        .connection()
        .query_row(
            "SELECT path FROM learning_candidate WHERE review_id = ?1 AND candidate_id = 'production-0'",
            rusqlite::params![record.review_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(preserved_after_conflict, "src/local-update.rs");
}

#[test]
fn forget_removes_one_review_run_and_its_derived_history_atomically() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-forget");
    let pack = synthetic_pack(&repo, 2, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Confirmed, None);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-forget-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let record = import(&loaded, &store);
    let generation_after_import = learning::import::generation(&store).unwrap().revision;
    let event = FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: "forget-feedback-event".into(),
        review_id: record.review_id.clone(),
        unit_id: "individual-production-0".to_owned(),
        candidate_id: Some("production-0".into()),
        kind: FeedbackKind::RecommendationUsefulness,
        action: FeedbackAction::Append,
        supersedes_event_id: None,
        effective_disposition: None,
        usefulness: Some("useful".into()),
        explanation: "Подсказка помогла найти контекст.".into(),
        provenance: "reviewer".into(),
        recorded_at: 1,
    };
    learning::feedback::record_feedback(&store, &event).unwrap();
    let generation_after_feedback = learning::import::generation(&store).unwrap().revision;
    assert!(generation_after_feedback > generation_after_import);

    let forgotten = learning::forget_review(&store, &record.review_id).unwrap();
    let generation_after_forget = learning::import::generation(&store).unwrap().revision;
    assert!(generation_after_forget > generation_after_feedback);
    assert_eq!(forgotten.removed.reviews, 1);
    assert_eq!(forgotten.removed.units, queue.units.len());
    assert!(forgotten.removed.candidates > 0);
    assert!(forgotten.removed.search_cases > 0);
    assert_eq!(forgotten.removed.feedback_events, 1);
    assert!(
        learning::import::list_imports(&store, true, 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        learning::import::generation(&store)
            .unwrap()
            .trusted_reviews,
        0
    );
    let remaining = store
        .read(|read| {
            read.query_row(
                "SELECT (SELECT COUNT(*) FROM learning_unit) +
                        (SELECT COUNT(*) FROM learning_candidate) +
                        (SELECT COUNT(*) FROM learning_decision) +
                        (SELECT COUNT(*) FROM learning_finding) +
                        (SELECT COUNT(*) FROM learning_search) +
                        (SELECT COUNT(*) FROM learning_feedback)",
                [],
                |row| row.get::<_, i64>(0),
            )
        })
        .unwrap();
    assert_eq!(remaining, 0);
    assert!(
        artifacts.pack().is_file(),
        "исходный review artifact сохраняется"
    );
    assert_eq!(
        learning::forget_review(&store, &record.review_id)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    import(&loaded, &store);
    assert!(learning::import::generation(&store).unwrap().revision > generation_after_forget);
}

#[test]
fn feedback_correction_and_retraction_remove_wrong_label_after_recompute() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-feedback");
    let pack = synthetic_pack(&repo, 2, 0);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Confirmed, None);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-feedback-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let record = import(&loaded, &store);
    let generation_after_import = learning::import::generation(&store).unwrap().revision;
    let unit_id = "individual-production-0".to_owned();

    let correction = FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: "event-1".into(),
        review_id: record.review_id.clone(),
        unit_id: unit_id.clone(),
        candidate_id: Some("production-0".into()),
        kind: FeedbackKind::SemanticOutcomeRevision,
        action: FeedbackAction::Append,
        supersedes_event_id: None,
        effective_disposition: Some("false_positive".into()),
        usefulness: None,
        explanation: "Метка подтверждения оказалась ошибочной.".into(),
        provenance: "reviewer".into(),
        recorded_at: 1,
    };
    let applied = learning::feedback::record_feedback(&store, &correction).unwrap();
    let generation_after_correction = learning::import::generation(&store).unwrap().revision;
    assert!(generation_after_correction > generation_after_import);
    assert_eq!(
        applied.outcome.effective_disposition.as_deref(),
        Some("false_positive")
    );

    // Повторный feedback не плодит дубликаты: он отклоняется как конфликт.
    let mut conflicting = correction.clone();
    conflicting.event_id = "event-2".into();
    conflicting.effective_disposition = Some("acceptable".into());
    let error = learning::feedback::record_feedback(&store, &conflicting).unwrap_err();
    assert_eq!(
        learning::import::generation(&store).unwrap().revision,
        generation_after_correction,
        "отклонённое событие не меняет поколение"
    );
    assert_eq!(error.code, ErrorCode::LearningConflict);
    assert!(
        error.details["conflicting_event_ids"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|id| id == "event-1"))
    );

    // Отзыв ошибочного утверждения убирает его влияние.
    let retraction = FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: "event-3".into(),
        review_id: record.review_id.clone(),
        unit_id: unit_id.clone(),
        candidate_id: Some("production-0".into()),
        kind: FeedbackKind::SemanticOutcomeRevision,
        action: FeedbackAction::Retract,
        supersedes_event_id: Some("event-1".into()),
        effective_disposition: None,
        usefulness: None,
        explanation: "Ошибочная правка отозвана.".into(),
        provenance: "reviewer".into(),
        recorded_at: 2,
    };
    let retracted = learning::feedback::record_feedback(&store, &retraction).unwrap();
    assert!(learning::import::generation(&store).unwrap().revision > generation_after_correction);
    assert_eq!(retracted.outcome.effective_disposition, None);
    assert_eq!(
        retracted.outcome.original_disposition.as_deref(),
        Some("confirmed")
    );
    assert!(
        retracted
            .outcome
            .retracted_event_ids
            .contains(&"event-1".to_owned())
    );

    // Audit trail сохраняет все события.
    let audit = learning::feedback::audit_case(&store, &record.review_id, &unit_id).unwrap();
    assert_eq!(audit.events.len(), 2);
    assert!(audit.events.iter().any(|event| event.event_id == "event-1"));
    assert!(audit.events.iter().any(|event| event.event_id == "event-3"));

    // Оценка полезности не меняет семантический исход и не конфликтует с правкой.
    let usefulness = FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: "event-4".into(),
        review_id: record.review_id.clone(),
        unit_id: unit_id.clone(),
        candidate_id: Some("production-0".into()),
        kind: FeedbackKind::RecommendationUsefulness,
        action: FeedbackAction::Append,
        supersedes_event_id: None,
        effective_disposition: None,
        usefulness: Some("useful".into()),
        explanation: "Подсказка помогла найти случай раньше.".into(),
        provenance: "reviewer".into(),
        recorded_at: 3,
    };
    learning::feedback::record_feedback(&store, &usefulness).unwrap();
    let outcome = learning::feedback::outcome(&store, &record.review_id, &unit_id).unwrap();
    assert_eq!(outcome.effective_disposition, None);

    let usefulness_retraction = FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: "event-7".into(),
        review_id: record.review_id.clone(),
        unit_id: unit_id.clone(),
        candidate_id: Some("production-0".into()),
        kind: FeedbackKind::RecommendationUsefulness,
        action: FeedbackAction::Retract,
        supersedes_event_id: Some("event-4".into()),
        effective_disposition: None,
        usefulness: None,
        explanation: "Оценка полезности отозвана.".into(),
        provenance: "reviewer".into(),
        recorded_at: 4,
    };
    let retracted_usefulness =
        learning::feedback::record_feedback(&store, &usefulness_retraction).unwrap();
    assert_eq!(
        retracted_usefulness.retracted_event_id.as_deref(),
        Some("event-4")
    );
    let mut invalid_usefulness_retraction = usefulness_retraction.clone();
    invalid_usefulness_retraction.event_id = "event-8".into();
    invalid_usefulness_retraction.usefulness = Some("useful".into());
    assert_eq!(
        learning::feedback::record_feedback(&store, &invalid_usefulness_retraction)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest,
        "отзыв не может назначить новую оценку полезности"
    );

    // После отзыва прежняя позиция освобождена для нового утверждения.
    let mut appended_after_retraction = correction.clone();
    appended_after_retraction.event_id = "event-5".into();
    appended_after_retraction.effective_disposition = Some("acceptable".into());
    appended_after_retraction.recorded_at = 4;
    let appended = learning::feedback::record_feedback(&store, &appended_after_retraction).unwrap();
    assert_eq!(
        appended.outcome.effective_disposition.as_deref(),
        Some("acceptable")
    );
    assert_eq!(appended.outcome.effective_event_ids, ["event-5"]);
    assert!(!appended.outcome.has_conflict);

    // supersede выводит целевое событие из эффективной истории.
    let superseding = FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: "event-6".into(),
        review_id: record.review_id.clone(),
        unit_id: unit_id.clone(),
        candidate_id: Some("production-0".into()),
        kind: FeedbackKind::SemanticOutcomeRevision,
        action: FeedbackAction::Supersede,
        supersedes_event_id: Some("event-5".into()),
        effective_disposition: Some("false_positive".into()),
        usefulness: None,
        explanation: "Уточнённый исход заменяет прежнюю оценку.".into(),
        provenance: "reviewer".into(),
        recorded_at: 5,
    };
    let superseded = learning::feedback::record_feedback(&store, &superseding).unwrap();
    assert_eq!(
        superseded.outcome.effective_disposition.as_deref(),
        Some("false_positive")
    );
    assert_eq!(superseded.outcome.effective_event_ids, ["event-6"]);
    assert!(!superseded.outcome.has_conflict);
    assert_eq!(
        learning::feedback::outcome_distribution(&store)
            .unwrap()
            .get("false_positive"),
        Some(&1)
    );

    store
        .write(|write| {
            write.execute(
                "UPDATE learning_feedback SET kind = 'future_feedback_kind' WHERE event_id = 'event-4'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        learning::feedback::show_event(&store, "event-4")
            .unwrap_err()
            .code,
        ErrorCode::LearningCorrupt,
        "неизвестный kind не подменяется другим видом feedback"
    );
    store
        .write(|write| {
            write.execute(
                "UPDATE learning_feedback SET kind = 'recommendation_usefulness', action = 'future_feedback_action' WHERE event_id = 'event-4'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        learning::transfer::export_history(&store).unwrap_err().code,
        ErrorCode::LearningCorrupt,
        "неизвестный action не экспортируется как append"
    );
}

#[test]
fn corrections_change_recomputed_statistics_and_retraction_restores_them() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-recompute");
    let pack = synthetic_pack(&repo, 3, 0);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    for id in ["production-0", "production-1", "production-2"] {
        resolve_individual(&mut triage, id, Disposition::Confirmed, None);
    }
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-recompute-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let record = import(&loaded, &store);

    let rule = || {
        let report =
            learning::pattern_report(&store, &learning::patterns::PatternQuery::default()).unwrap();
        report
            .rules
            .iter()
            .max_by_key(|rule| rule.support.support_units)
            .cloned()
            .expect("паттерн обязан существовать")
    };
    let before = rule();
    assert_eq!(before.support.confirmed_units, 3);
    assert_eq!(before.support.level, SupportLevel::Supported);
    assert_eq!(before.support.support_units, 3);

    // Содержательная правка одного случая меняет пересчитанную статистику.
    let correction = FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: "event-recompute".into(),
        review_id: record.review_id.clone(),
        unit_id: "individual-production-0".into(),
        candidate_id: Some("production-0".into()),
        kind: FeedbackKind::SemanticOutcomeRevision,
        action: FeedbackAction::Append,
        supersedes_event_id: None,
        effective_disposition: Some("false_positive".into()),
        usefulness: None,
        explanation: "Метка подтверждения оказалась ошибочной.".into(),
        provenance: "reviewer".into(),
        recorded_at: 1,
    };
    learning::feedback::record_feedback(&store, &correction).unwrap();
    let corrected = rule();
    assert_eq!(corrected.support.confirmed_units, 2);
    assert_eq!(corrected.support.false_positive_units, 1);
    assert_eq!(corrected.support.level, SupportLevel::Contradictory);
    assert_eq!(
        learning::feedback::outcome_distribution(&store)
            .unwrap()
            .get("false_positive"),
        Some(&1)
    );

    // Отзыв правки убирает её влияние при следующем пересчёте.
    let retraction = FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: "event-recompute-retract".into(),
        review_id: record.review_id.clone(),
        unit_id: "individual-production-0".into(),
        candidate_id: Some("production-0".into()),
        kind: FeedbackKind::SemanticOutcomeRevision,
        action: FeedbackAction::Retract,
        supersedes_event_id: Some("event-recompute".into()),
        effective_disposition: None,
        usefulness: None,
        explanation: "Ошибочная правка отозвана.".into(),
        provenance: "reviewer".into(),
        recorded_at: 2,
    };
    learning::feedback::record_feedback(&store, &retraction).unwrap();
    let restored = rule();
    assert_eq!(restored.support.confirmed_units, 3);
    assert_eq!(restored.support.false_positive_units, 0);
    assert_eq!(restored.support.level, before.support.level);
    assert_eq!(
        restored.support.revised_units,
        corrected.support.revised_units
    );

    // Журнал правок сохраняется целиком.
    let audit =
        learning::feedback::audit_case(&store, &record.review_id, "individual-production-0")
            .unwrap();
    assert_eq!(audit.events.len(), 2);
}

#[test]
fn repeated_defect_across_versions_is_linked_not_counted_twice() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-case-link");
    let pack = synthetic_pack(&repo, 2, 0);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Confirmed, None);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let first_dir = repo.path().join("artifacts-first");
    let first = Artifacts::write(&first_dir, &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-case-link-store");
    let store = open_store(store_dir.path());
    let loaded_first = load_in_repo(&repo, &first, false).unwrap();
    let first_record = import(&loaded_first, &store);

    // Повторное чтение неизменной записи не создаёт новую единицу наблюдения.
    let repeated = import(&loaded_first, &store);
    assert_eq!(repeated.review_id, first_record.review_id);
    let units = store
        .read(|read| {
            let count: i64 =
                read.query_row("SELECT COUNT(*) FROM learning_unit", [], |row| row.get(0))?;
            let links: i64 =
                read.query_row("SELECT COUNT(*) FROM learning_case_link", [], |row| {
                    row.get(0)
                })?;
            Ok((count, links))
        })
        .unwrap();
    assert_eq!(units.0, 2, "точный повтор не удваивает единицы наблюдения");
    assert_eq!(
        units.1, 0,
        "повтор в пределах одного запуска не создаёт связь"
    );

    // Другой запуск того же репозитория со следующим head связывается явно.
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn parse(value: &str) -> Option<u32> {\n    let digits = value.trim();\n    digits.parse().ok()\n}\n\npub fn third() -> u32 { 3 }\n",
    )
    .unwrap();
    git(repo.path(), &["add", "--", "."]);
    git(repo.path(), &["commit", "-qm", "Синтетическое продолжение"]);
    let next_head = git(repo.path(), &["rev-parse", "HEAD"]);
    // Та же база и тот же repository_id, но следующий head: та же линия версий.
    let mut moved = pack.clone();
    moved.target = GitTarget {
        head_sha: next_head.clone(),
        ..repo.target()
    };
    moved.scope.files = vec![scope_file("src/lib.rs", FileCategory::Rust)];
    let moved_digest = digest_of(&moved);
    let mut moved_triage = semantic_triage::initialize(&moved, &moved_digest);
    // Тот же дефект подтверждён повторно в следующей версии: это не новое
    // независимое наблюдение, а связанный повтор.
    resolve_individual(
        &mut moved_triage,
        "production-0",
        Disposition::Confirmed,
        None,
    );
    semantic_triage::canonicalize(&mut moved_triage);
    semantic_triage::validate(&moved_triage, &moved, &moved_digest).unwrap();
    let moved_queue = build_authoritative_queue(&repo, &moved);
    let moved_artifacts = Artifacts::write(
        &repo.path().join("artifacts-second"),
        &moved,
        &moved_queue,
        &moved_triage,
    );
    let moved_loaded = load_in_repo(&repo, &moved_artifacts, false).unwrap();
    let second_record = import(&moved_loaded, &store);
    assert_ne!(second_record.review_id, first_record.review_id);

    let links = store
        .read(|read| {
            let mut statement = read
                .transaction()
                .prepare(
                    "SELECT review_id, linked_review_id, kind, basis
                     FROM learning_case_link ORDER BY review_id, linked_review_id",
                )
                .unwrap();
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .unwrap();
            let mut values = Vec::new();
            for row in rows {
                values.push(row.unwrap());
            }
            Ok(values)
        })
        .unwrap();
    assert!(
        !links.is_empty(),
        "повтор дефекта между версиями связывается явной записью"
    );
    for (left, right, kind, basis) in &links {
        assert!(left != right);
        assert!(
            matches!(
                kind.as_str(),
                "structural_repeat" | "same_iteration" | "unknown"
            ),
            "неожиданный вид связи: {kind}"
        );
        assert!(
            !basis.trim().is_empty(),
            "основание связи обязано быть записано"
        );
        assert!(
            !basis.contains("snippet"),
            "основание не опирается на совпадение сниппета"
        );
    }

    // Свёрнутый повтор не попадает в независимую поддержку среза: две версии
    // одного диапазона — одно наблюдение, а не два.
    let rule = learning::pattern_report(&store, &learning::patterns::PatternQuery::default())
        .unwrap()
        .rules
        .into_iter()
        .max_by_key(|rule| rule.support.support_units)
        .expect("паттерн обязан существовать");
    // Срез содержит две единицы очереди, и каждая из них наблюдалась дважды:
    // независимых наблюдений два, свёрнутых повторов два.
    assert_eq!(
        rule.support.support_units, 2,
        "повтор версии не добавляет независимую единицу поддержки"
    );
    assert_eq!(
        rule.support.revised_units, 2,
        "две единицы среза наблюдались дважды"
    );
    assert_eq!(rule.support.support_reviews, 2);
    assert_eq!(
        rule.support.support_units + rule.support.revised_units,
        4,
        "сырые единицы среза делятся на независимые и свёрнутые повторы"
    );
    assert_eq!(rule.support.confirmed_units, 1);
    assert_eq!(
        rule.support.level,
        SupportLevel::InsufficientEvidence,
        "двух независимых единиц мало для вывода"
    );
    assert!(
        rule.support.confirmed_share_lower_bound.is_none(),
        "вывод воздерживается, значит нижней границы нет"
    );
}

#[test]
fn policy_proposal_is_never_auto_applied() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-policy");
    let pack = synthetic_pack(&repo, 4, 0);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Acceptable, None);
    resolve_individual(&mut triage, "production-1", Disposition::Acceptable, None);
    resolve_individual(&mut triage, "production-2", Disposition::Acceptable, None);
    resolve_individual(&mut triage, "production-3", Disposition::Acceptable, None);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-policy-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let record = import(&loaded, &store);

    store
        .write(|write| {
            write.execute(
                "UPDATE learning_import SET imported_at = 1000000 WHERE review_id = ?1",
                rusqlite::params![record.review_id],
            )?;
            Ok(())
        })
        .unwrap();
    let report =
        learning::pattern_report(&store, &learning::patterns::PatternQuery::default()).unwrap();
    assert!(
        report.rules.iter().all(|rule| {
            rule.support.freshest_age_days == 0 && rule.support.oldest_age_days == 0
        })
    );
    let rule = report
        .rules
        .iter()
        .max_by_key(|rule| rule.support.support_units)
        .expect("паттерн обязан существовать");
    let proposal = learning::feedback::propose_policy(
        &store,
        &rule.signature,
        "learning.observe.error_path",
        0,
    )
    .unwrap();
    let generation_after_proposal = learning::import::generation(&store).unwrap().revision;
    assert!(generation_after_proposal > proposal.generation.revision);
    assert!(!proposal.auto_applied);
    assert_eq!(
        proposal.artifact_path,
        learning::feedback::POLICY_ARTIFACT_PATH
    );
    assert!(proposal.artifact_path.ends_with(".json"));
    assert!(
        proposal
            .cautions
            .iter()
            .any(|text| text.contains("не применяется автоматически"))
    );
    assert!(!proposal.supporting_cases.is_empty());
    // Предложение не создаёт решений и не выполняет suppression.
    assert_eq!(
        learning::import::generation(&store)
            .unwrap()
            .trusted_reviews,
        1
    );
    // Повторное предложение не плодит дубликаты.
    let repeated = learning::feedback::propose_policy(
        &store,
        &rule.signature,
        "learning.observe.error_path",
        0,
    )
    .unwrap();
    assert_eq!(repeated.proposal_id, proposal.proposal_id);
    assert_eq!(
        learning::import::generation(&store).unwrap().revision,
        generation_after_proposal,
        "точный повтор предложения не меняет поколение"
    );
    assert_eq!(
        learning::feedback::list_proposals(&store, 10)
            .unwrap()
            .len(),
        1
    );

    let exported = learning::transfer::export_history(&store).unwrap();
    let mut local_proposal =
        learning::feedback::show_proposal(&store, &proposal.proposal_id).unwrap();
    local_proposal
        .cautions
        .push("Локальное обновление предложения после экспорта.".into());
    let local_document = serde_json::to_string(&local_proposal).unwrap();
    store
        .write(|write| {
            write.execute(
                "UPDATE learning_policy_proposal SET document_json = ?2 WHERE proposal_id = ?1",
                rusqlite::params![proposal.proposal_id, local_document],
            )?;
            Ok(())
        })
        .unwrap();
    let restore_error = learning::transfer::restore_history(&store, &exported.archive).unwrap_err();
    assert_eq!(restore_error.code, ErrorCode::LearningConflict);
    assert_eq!(
        learning::feedback::show_proposal(&store, &proposal.proposal_id).unwrap(),
        local_proposal,
        "конфликт архива не заменяет локальное предложение с тем же proposal_id"
    );

    // Ключ политики находится и у паттерна за пределами стандартной страницы отчёта.
    let synthetic_patterns: Vec<(String, String)> = (0..24)
        .map(|index| {
            let features = BTreeMap::from([
                (
                    "classifier_compatibility".to_owned(),
                    record.inputs.classifier_digest.clone(),
                ),
                ("fixture_pattern".to_owned(), format!("pattern-{index:02}")),
            ]);
            let signature = learning::patterns::feature_signature(&features);
            let feature_json = serde_json::to_string(&features).unwrap();
            (signature, feature_json)
        })
        .collect();
    store
        .write(|write| {
            for (index, (signature, feature_json)) in synthetic_patterns.iter().enumerate() {
                write.execute(
                    "INSERT INTO learning_unit (
                        review_id, unit_id, kind, candidate_count, priority,
                        representatives_json, disposition, reason_code, detector, source,
                        role, code_role, surfaces_json, signature, feature_json
                     ) VALUES (?1, ?2, 'individual', 0, 'normal', '[]', 'acceptable', NULL,
                               'fixture-detector', 'fixture-source', 'unknown', 'unknown',
                               '[]', ?3, ?4)",
                    rusqlite::params![
                        record.review_id,
                        format!("fixture-unit-{index:02}"),
                        signature,
                        feature_json,
                    ],
                )?;
            }
            Ok(())
        })
        .unwrap();
    let visible: BTreeSet<String> =
        learning::pattern_report(&store, &learning::patterns::PatternQuery::default())
            .unwrap()
            .rules
            .into_iter()
            .map(|rule| rule.signature)
            .collect();
    let (hidden_signature, hidden_feature_json) = synthetic_patterns
        .iter()
        .find(|(signature, _)| !visible.contains(signature))
        .expect("синтетический паттерн должен оказаться за пределами первых 20");
    let hidden_proposal =
        learning::feedback::propose_policy(&store, hidden_signature, "learning.observe.hidden", 0)
            .unwrap();
    assert_eq!(
        hidden_proposal.key,
        serde_json::from_str::<BTreeMap<String, String>>(hidden_feature_json).unwrap()
    );

    let mut truncated = learning::feedback::show_proposal(&store, &proposal.proposal_id).unwrap();
    let exemplar = truncated
        .supporting_cases
        .first()
        .expect("предложение содержит поддерживающий случай")
        .clone();
    truncated.supporting_cases = (0..10)
        .map(|index| {
            let mut case = exemplar.clone();
            case.review_id = format!("unlisted-support-{index}");
            case
        })
        .collect();
    truncated.contradicting_cases.clear();
    let truncated_document = serde_json::to_string(&truncated).unwrap();
    store
        .write(|write| {
            write.execute(
                "UPDATE learning_policy_proposal SET document_json = ?2 WHERE proposal_id = ?1",
                rusqlite::params![proposal.proposal_id, truncated_document],
            )?;
            Ok(())
        })
        .unwrap();
    let forgotten = learning::lifecycle::forget_review(&store, &record.review_id).unwrap();
    assert_eq!(forgotten.removed.policy_proposals, 2);
    assert_eq!(
        learning::feedback::show_proposal(&store, &proposal.proposal_id)
            .unwrap_err()
            .code,
        ErrorCode::NotFound,
        "удаление записи находит proposal по полной подписи, даже если случай не показан"
    );
    assert_eq!(
        learning::feedback::show_proposal(&store, &hidden_proposal.proposal_id)
            .unwrap_err()
            .code,
        ErrorCode::NotFound,
        "предложение вне страницы отчёта также удаляется по подписи"
    );
}

#[test]
fn recommendations_are_deterministic_and_guardrailed() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-recommend");
    let pack = synthetic_pack(&repo, 3, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Confirmed, None);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-recommend-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    import(&loaded, &store);

    let request = learning::recommend::RecommendRequest {
        now: 1_000_000,
        ..learning::recommend::RecommendRequest::default()
    };
    let first = learning::recommend(Some(&store), &loaded, &request).unwrap();
    let second = learning::recommend(Some(&store), &loaded, &request).unwrap();
    assert_eq!(first, second, "результат детерминирован для тех же входов");
    assert_eq!(first.review_pack_sha256, digest);
    assert_eq!(first.queue_sha256, loaded.queue_sha256);
    assert!(!first.learning_disabled);
    assert_eq!(first.generation.trusted_reviews, 1);

    let limited = learning::recommend(
        Some(&store),
        &loaded,
        &learning::recommend::RecommendRequest {
            limit: 1,
            ..request.clone()
        },
    )
    .unwrap();
    assert_eq!(limited.recommendations.len(), 1);
    assert_eq!(limited.suggested_order.len(), queue.units.len());
    assert_eq!(limited.suggested_order, first.suggested_order);
    assert!(limited.limitations.iter().any(|limitation| {
        limitation.contains("единиц остаются в suggested_order")
    }));

    // Если вызывающая сторона не передала now, возраст считается от самой
    // позднейшей доверенной записи, а не от часов процесса.
    store
        .write(|write| write.execute("UPDATE learning_import SET imported_at = 1_000_000", []))
        .unwrap();
    let stable_default_time = learning::recommend(
        Some(&store),
        &loaded,
        &learning::recommend::RecommendRequest::default(),
    )
    .unwrap();
    assert!(
        stable_default_time
            .recommendations
            .iter()
            .any(|item| !item.historical_cases.is_empty())
    );
    for item in &stable_default_time.recommendations {
        assert!(item.historical_cases.iter().all(|case| case.age_days == 0));
        if let Some(support) = &item.support {
            assert_eq!(support.freshest_age_days, 0);
            assert_eq!(support.oldest_age_days, 0);
        }
    }
    assert_eq!(
        stable_default_time,
        learning::recommend(
            Some(&store),
            &loaded,
            &learning::recommend::RecommendRequest::default(),
        )
        .unwrap()
    );

    // Каждая рекомендация ссылается на первичный контекст и не теряет кандидатов.
    let queue_units: BTreeSet<&str> = queue.units.iter().map(|unit| unit.id.as_str()).collect();
    let mut ordered: BTreeSet<&str> = BTreeSet::new();
    for item in &first.recommendations {
        assert!(queue_units.contains(item.unit_id.as_str()));
        assert!(!item.source_reference.is_empty());
        assert!(!item.candidate_ids.is_empty());
        assert!(item.reason.contains("Кандидатов в единице"));
        ordered.insert(item.unit_id.as_str());
    }
    assert_eq!(ordered.len(), queue.units.len());
    assert_eq!(first.suggested_order.len(), queue.units.len());
    assert_eq!(
        first.suggested_order,
        first
            .recommendations
            .iter()
            .map(|item| item.unit_id.clone())
            .collect::<Vec<_>>(),
        "подсказки и рекомендации должны иметь один и тот же порядок"
    );

    // Guardrails: high/unknown/security остаются обязательными к просмотру.
    for unit in &queue.units {
        let item = first
            .recommendations
            .iter()
            .find(|item| item.unit_id == unit.id)
            .unwrap();
        assert_eq!(item.queue_priority, unit.priority.as_str());
        if unit.priority.as_str() == "high" || unit.signature.classification.is_unknown() {
            assert_eq!(item.suggested_position, "first");
            assert!(
                item.limitations
                    .iter()
                    .any(|text| text.contains("не понижает обязательность")
                        || text.contains("не исключается из обязательного"))
            );
        }
        assert_eq!(item.candidate_ids, unit.candidate_ids());
    }

    // Штатный режим без learning не ломает порядок очереди.
    let without = learning::recommend(None, &loaded, &request).unwrap();
    assert!(without.learning_disabled);
    assert_eq!(without.generation.trusted_reviews, 0);
    let queue_order: Vec<String> = queue.units.iter().map(|unit| unit.id.clone()).collect();
    assert_eq!(
        without.suggested_order, queue_order,
        "без learning предложенный порядок совпадает с детерминированным порядком очереди"
    );
    for item in &without.recommendations {
        assert_eq!(item.granularity, "no_recommendation");
        assert!(item.historical_cases.is_empty());
    }
}

#[test]
fn search_distinguishes_match_kinds_and_paginates() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-search");
    let pack = synthetic_pack(&repo, 3, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    let finding = SemanticFinding {
        id: "finding-search".into(),
        severity: Severity::Minor,
        title: "Незначительное замечание".into(),
        description: "src/lib.rs:2: parse и описание для локального текстового поиска.".into(),
        provenance: FindingProvenance::CandidateAssisted,
        candidate_ids: vec!["production-0".into()],
    };
    resolve_individual(
        &mut triage,
        "production-0",
        Disposition::Confirmed,
        Some(&finding),
    );
    resolve_individual(&mut triage, "production-1", Disposition::Acceptable, None);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-search-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    import(&loaded, &store);

    let textual = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            text: Some("текстового поиска".into()),
            limit: 5,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert_eq!(textual.cases.len(), 1);
    assert_eq!(
        textual.cases[0].match_kind,
        learning::search::SearchMatchKind::Textual
    );
    assert!(
        textual.cases[0]
            .snippet
            .contains("локального текстового поиска")
    );
    assert!(textual.cases[0].source_reference.contains("review.json"));
    assert!(textual.fts5_used);

    let partial_word = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            text: Some("pars".into()),
            limit: 5,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert_eq!(partial_word.cases.len(), 1);
    assert!(partial_word.fts5_used);

    let middle_of_token = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            text: Some("екстов".into()),
            limit: 5,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert!(!middle_of_token.cases.is_empty());
    assert!(middle_of_token.fts5_used);

    let two_character_substring = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            text: Some("ar".into()),
            limit: 5,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert!(!two_character_substring.cases.is_empty());
    assert!(!two_character_substring.fts5_used);

    let thematic = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            disposition: Some("acceptable".into()),
            limit: 5,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert!(!thematic.cases.is_empty());
    assert_eq!(
        thematic.cases[0].match_kind,
        learning::search::SearchMatchKind::Thematic
    );

    let exact = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            text: Some("локального текстового поиска".into()),
            disposition: Some("confirmed".into()),
            severity: Some("minor".into()),
            provenance: Some("candidate_assisted".into()),
            limit: 5,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert_eq!(exact.cases.len(), 1, "{:#?}", exact.cases);
    assert_eq!(
        exact.cases[0].match_kind,
        learning::search::SearchMatchKind::ExactStructural
    );
    assert_eq!(exact.cases[0].disposition.as_deref(), Some("confirmed"));
    assert_eq!(
        exact.cases[0].provenance.as_deref(),
        Some("candidate_assisted")
    );
    assert_eq!(exact.cases[0].severity.as_deref(), Some("minor"));

    // Постраничный вывод ограничен и сообщает о продолжении.
    let page = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            disposition: Some("confirmed".into()),
            limit: 1,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert_eq!(page.cases.len(), 1);
    assert_eq!(page.limit, 1);
    assert_eq!(
        page.matched, 2,
        "подтверждённых случаев два: решение и замечание"
    );
    assert!(page.has_more, "страница обязана сообщать о продолжении");

    let second_page = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            disposition: Some("confirmed".into()),
            limit: 1,
            offset: 1,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert_eq!(second_page.cases.len(), 1);
    assert!(!second_page.has_more);
    assert_ne!(
        second_page.cases[0].case_id, page.cases[0].case_id,
        "страницы не повторяют один и тот же случай"
    );

    // Слишком короткая подстрока отвергается как некорректный запрос.
    let error = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            text: Some("a".into()),
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);

    // Отсутствующий случай даёт not_found, а не пустой успех.
    let error = learning::show_import(&store, "review-отсутствует").unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
}

#[test]
fn search_falls_back_reproducibly_without_the_fts_index() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-search-fallback");
    let pack = synthetic_pack(&repo, 1, 1);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    let finding = SemanticFinding {
        id: "finding-fallback".into(),
        severity: Severity::Major,
        title: "Замечание для проверки fallback".into(),
        description:
            "src/lib.rs:2: локальный поиск без FTS5 и парсинг обязаны дать тот же результат.".into(),
        provenance: FindingProvenance::CandidateAssisted,
        candidate_ids: vec!["production-0".into()],
    };
    resolve_individual(
        &mut triage,
        "production-0",
        Disposition::Confirmed,
        Some(&finding),
    );
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-search-fallback-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    import(&loaded, &store);

    let query = learning::search::SearchQuery {
        text: Some("локальный поиск".into()),
        limit: 5,
        ..learning::search::SearchQuery::default()
    };
    let with_fts = learning::search_history(&store, &query).unwrap();
    assert!(with_fts.fts5_used);
    assert_eq!(with_fts.cases.len(), 1);

    let cyrillic_short_query = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            text: Some("Па".into()),
            limit: 5,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert!(!cyrillic_short_query.fts5_used);
    assert!(!cyrillic_short_query.cases.is_empty());

    // Тот же корпус без FTS5-таблицы: результат воспроизводится через `instr`.
    store
        .write(|write| write.execute("DROP TABLE learning_search_fts", []))
        .unwrap();
    let without_fts = learning::search_history(&store, &query).unwrap();
    assert!(
        !without_fts.fts5_used,
        "отсутствие индекса сообщается честно"
    );
    assert_eq!(without_fts.cases.len(), with_fts.cases.len());
    assert_eq!(without_fts.cases[0].case_id, with_fts.cases[0].case_id);
    assert_eq!(without_fts.matched, with_fts.matched);
    assert_eq!(
        without_fts.cases[0].match_kind,
        learning::search::SearchMatchKind::Textual
    );
    let cyrillic_without_fts = learning::search_history(
        &store,
        &learning::search::SearchQuery {
            text: Some("Па".into()),
            limit: 5,
            ..learning::search::SearchQuery::default()
        },
    )
    .unwrap();
    assert!(!cyrillic_without_fts.fts5_used);
    assert_eq!(
        cyrillic_without_fts.cases, cyrillic_short_query.cases,
        "fallback сохраняет Unicode-поиск без учёта регистра"
    );

    // Индекс можно восстановить из таблицы поиска без потери случаев.
    let rebuilt = learning::schema::rebuild_fts_index(store.connection()).unwrap();
    assert!(rebuilt);
    let restored = learning::search_history(&store, &query).unwrap();
    assert!(restored.fts5_used);
    assert_eq!(restored.cases.len(), 1);
}

#[test]
fn schema_and_policy_versions_are_recorded_with_the_history() {
    let directory = TempDir::new("learning-status");
    let store = open_store(directory.path());
    let status = store.status().unwrap();
    assert_eq!(status.user_version, learning::LEARNING_SCHEMA_VERSION);
    assert_eq!(status.policy_version, learning::LEARNING_POLICY_VERSION);
    match status.journal_mode.as_str() {
        "wal" => assert!(status.journal_mode_reason.contains("локальная")),
        "delete" => {
            assert!(status.journal_mode_reason.contains("DELETE"));
            assert!(status.journal_mode_reason.contains("выбран"));
        }
        mode => panic!("неизвестный режим журнала SQLite: {mode}"),
    }
    assert_eq!(
        status.busy_timeout_ms,
        learning::store::DEFAULT_BUSY_TIMEOUT_MS
    );
    assert!(status.integrity_ok);
    assert!(status.recovery_paths.len() >= 2);
    assert!(!status.database_path.contains("/home/"));
    // Путь по умолчанию лежит внутри каталога learning и не раскрывает клон.
    let options = learning::StoreOptions::in_repository(directory.path());
    assert!(
        options.database
            == directory
                .path()
                .join(learning::DEFAULT_LEARNING_DIRECTORY)
                .join(learning::DEFAULT_LEARNING_DATABASE),
        "база данных должна находиться в каталоге learning репозитория"
    );
    assert_eq!(
        options.display_path,
        format!(
            "{}/{}",
            learning::DEFAULT_LEARNING_DIRECTORY,
            learning::DEFAULT_LEARNING_DATABASE
        )
    );

    // Проверка целостности не разрушает данные, а backup — транзакционный снимок.
    let snapshot = directory.path().join("snapshot.sqlite");
    store.backup_to(&snapshot).unwrap();
    assert!(snapshot.is_file());
    let restored = learning::LearningStore::open(learning::StoreOptions::at(&snapshot)).unwrap();
    assert!(restored.integrity_check().is_ok());
}

#[test]
fn stored_paths_and_snippets_are_minimised() {
    assert_eq!(
        learning::import::sanitize_path("/home/someone/project/src/lib.rs"),
        "lib.rs"
    );
    assert_eq!(
        learning::import::sanitize_path("C:\\Users\\someone\\src\\lib.rs"),
        "lib.rs"
    );
    assert_eq!(learning::import::sanitize_path("src/lib.rs"), "src/lib.rs");
    assert_eq!(
        learning::import::sanitize_path("src\\nested\\lib.rs"),
        "src/nested/lib.rs"
    );
    assert_eq!(
        learning::import::path_family("src/tests/parse.rs"),
        "src/tests.rs"
    );

    let long = "слово ".repeat(500);
    let snippet = learning::import::sanitize_snippet(Some(&long)).unwrap();
    assert!(snippet.len() <= learning::import::MAX_STORED_SNIPPET_BYTES);
    assert!(!snippet.contains('\n'));

    let text = learning::import::sanitize_text("первая строка\nвторая\u{0} строка");
    assert!(!text.contains('\n'));
    assert!(!text.contains('\u{0}'));
    assert!(learning::import::sanitize_snippet(Some("   ")).is_none());
}

#[test]
fn import_request_requires_explicit_variant() {
    assert_eq!(
        learning::workspace_variant("").unwrap(),
        learning::ROOT_WORKSPACE_VARIANT
    );
    assert_eq!(
        learning::workspace_variant("root").unwrap(),
        learning::ROOT_WORKSPACE_VARIANT
    );
    let snapshot = format!("snapshot-{}", "a".repeat(32));
    assert_eq!(learning::workspace_variant(&snapshot).unwrap(), snapshot);
    for invalid in ["snapshot-short", "snapshot-", "branch"] {
        let error = learning::workspace_variant(invalid).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }
}

#[test]
fn pattern_report_refuses_mismatched_policy_versions() {
    let mut report = learning::patterns::empty_report();
    learning::patterns::require_supported(&report).unwrap();
    report.policy_version += 1;
    let error = learning::patterns::require_supported(&report).unwrap_err();
    assert_eq!(error.code, ErrorCode::InsufficientEvidence);
}

#[test]
fn storage_busy_is_not_masked_as_success() {
    let directory = TempDir::new("learning-busy");
    let database = directory.path().join("state.sqlite");
    let store = learning::LearningStore::open(learning::StoreOptions::at(&database)).unwrap();
    // Другое соединение удерживает исключительную блокировку записи.
    let blocker = rusqlite::Connection::open(&database).unwrap();
    blocker
        .busy_timeout(std::time::Duration::from_millis(1))
        .unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let error = store
        .write(|write| {
            write.execute(
                "INSERT INTO learning_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params!["busy-probe", "value"],
            )
        })
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::LearningStorageBusy);
    assert!(blocker.execute_batch("ROLLBACK").is_ok());
}

/// Единица очереди, содержащая указанного кандидата.
fn unit_of(queue: &review_queue::ReviewQueue, candidate_id: &str) -> review_queue::ReviewUnit {
    queue
        .units
        .iter()
        .find(|unit| unit.candidate_ids().iter().any(|id| id == candidate_id))
        .cloned()
        .unwrap_or_else(|| panic!("кандидат {candidate_id} обязан входить в единицу очереди"))
}

/// Сводка поддержки среза единицы.
fn support_of(
    store: &learning::LearningStore,
    unit: &review_queue::ReviewUnit,
) -> learning::model::SupportSummary {
    let candidate_id = unit
        .candidate_ids()
        .first()
        .unwrap_or_else(|| panic!("единица {} обязана содержать кандидата", unit.id));
    learning::patterns::support_for_signature(store, &stored_unit_signature(store, candidate_id), 0)
        .unwrap()
        .unwrap_or_else(|| panic!("срез единицы {} обязан существовать", unit.id))
}

/// Число независимых решений в сводке: отсутствие решения в него не входит.
fn reviewed_units(summary: &learning::model::SupportSummary) -> usize {
    summary.confirmed_units
        + summary.acceptable_units
        + summary.false_positive_units
        + summary.not_applicable_units
        + summary.uncertain_units
}

/// Находка синтетического фикстура с заданными осями.
fn finding(
    id: &str,
    severity: Severity,
    provenance: FindingProvenance,
    candidate_ids: Vec<String>,
) -> SemanticFinding {
    SemanticFinding {
        id: id.into(),
        severity,
        title: format!("Синтетическое замечание {id}"),
        description: format!("{id}: наблюдение синтетического фикстура."),
        provenance,
        candidate_ids,
    }
}

#[test]
fn findings_are_linked_to_a_slice_by_units_and_confirmed_by_decisions() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-finding-link");
    let pack = synthetic_pack(&repo, 2, 2);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);

    // Мелкое замечание с подтверждающим решением: подтверждённость не зависит
    // от серьёзности, поэтому оно обязано попасть в счётчик подтверждённых.
    let minor = finding(
        "finding-minor",
        Severity::Minor,
        FindingProvenance::CandidateAssisted,
        vec!["production-0".into()],
    );
    resolve_individual(
        &mut triage,
        "production-0",
        Disposition::Confirmed,
        Some(&minor),
    );
    // Серьёзное замечание при отказе от кандидата подтверждённым не становится.
    let rejected = finding(
        "finding-rejected",
        Severity::Critical,
        FindingProvenance::CandidateAssisted,
        vec!["production-1".into()],
    );
    resolve_individual(
        &mut triage,
        "production-1",
        Disposition::FalsePositive,
        Some(&rejected),
    );
    // Замечание другого среза не смешивается с production-срезом.
    let direct = finding(
        "finding-direct",
        Severity::Major,
        FindingProvenance::DirectCandidate,
        vec!["tests-0".into()],
    );
    resolve_individual(
        &mut triage,
        "tests-0",
        Disposition::Confirmed,
        Some(&direct),
    );
    // Независимое наблюдение без связанных единиц не привязывается ни к срезу.
    triage.findings.push(finding(
        "finding-independent",
        Severity::Critical,
        FindingProvenance::Independent,
        Vec::new(),
    ));

    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-finding-link-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    let record = import(&loaded, &store);

    let production = support_of(&store, &unit_of(&queue, "production-0"));
    assert_eq!(
        production.confirmed_findings, 1,
        "подтверждённой считается находка, на которую ссылается решение, а не серьёзная"
    );
    assert_eq!(
        production.findings_by_provenance.get("findings_total"),
        Some(&2),
        "срез считает только свои находки"
    );
    assert_eq!(
        production.findings_by_provenance.get("candidate_assisted"),
        Some(&2)
    );
    assert_eq!(
        production.findings_by_provenance.get("independent"),
        None,
        "находка без связанных единиц не привязывается к срезу"
    );
    assert_eq!(
        production.findings_by_provenance.get("direct_candidate"),
        None,
        "находка другого среза не попадает в этот"
    );
    assert_eq!(production.support_units, 2);
    assert_eq!(
        production.support_units + production.revised_units,
        2,
        "каждая единица среза либо независима, либо свёрнутый повтор"
    );

    let tests = support_of(&store, &unit_of(&queue, "tests-0"));
    assert_eq!(tests.findings_by_provenance.get("findings_total"), Some(&1));
    assert_eq!(
        tests.findings_by_provenance.get("direct_candidate"),
        Some(&1)
    );
    assert_eq!(tests.confirmed_findings, 1);
    assert_eq!(
        tests.findings_by_provenance.get("candidate_assisted"),
        None,
        "срезы не обмениваются находками"
    );

    // Идентичность находки — отпечаток наблюдения, а не ключ среза.
    let (severity, provenance, signature) = stored_finding(&store, "finding-minor");
    assert_eq!(severity, "minor");
    assert_eq!(provenance, "candidate_assisted");
    let unit_signature = stored_unit_signature(&store, "production-0");
    assert_ne!(
        signature, unit_signature,
        "подпись находки и подпись среза обязаны оставаться разными доменами"
    );
    let classifier_compatibility = learning::import::classifier_digest(&pack, &queue);
    let current_slice_signature =
        learning::patterns::feature_signature(&learning::patterns::unit_features_with_classifier(
            &unit_of(&queue, "production-0").signature,
            &classifier_compatibility,
        ));
    assert_eq!(unit_signature, current_slice_signature);
    assert!(
        signature.starts_with("finding-v2-"),
        "подпись находки включает полное свидетельство и версию контракта"
    );

    // Повторный импорт того же источника не удваивает счётчики среза.
    import(&loaded, &store);
    let repeated = support_of(&store, &unit_of(&queue, "production-0"));
    assert_eq!(repeated.confirmed_findings, 1);
    assert_eq!(
        repeated.findings_by_provenance.get("findings_total"),
        Some(&2)
    );
    assert_eq!(repeated.support_units, 2);

    let correction = FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: "finding-outcome-revision".into(),
        review_id: record.review_id,
        unit_id: unit_of(&queue, "production-0").id,
        candidate_id: Some("production-0".into()),
        kind: FeedbackKind::SemanticOutcomeRevision,
        action: FeedbackAction::Append,
        supersedes_event_id: None,
        effective_disposition: Some("false_positive".into()),
        usefulness: None,
        explanation: "Подтверждение было пересмотрено после проверки.".into(),
        provenance: "reviewer".into(),
        recorded_at: 1,
    };
    learning::feedback::record_feedback(&store, &correction).unwrap();
    let corrected = support_of(&store, &unit_of(&queue, "production-0"));
    assert_eq!(corrected.confirmed_findings, 0);
}

/// Путь базы, открытой в синтетическом каталоге истории.
fn database_path(store: &learning::LearningStore) -> PathBuf {
    let options = store.options();
    options.database.clone()
}

/// Читает оси и подпись сохранённого замечания.
fn stored_finding(store: &learning::LearningStore, finding_id: &str) -> (String, String, String) {
    let connection = rusqlite::Connection::open(database_path(store)).unwrap();
    connection
        .query_row(
            "SELECT severity, provenance, signature FROM learning_finding WHERE finding_id = ?1",
            [finding_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("замечание обязано быть сохранено")
}

/// Читает ключ признаков единицы наблюдения.
fn stored_unit_signature(store: &learning::LearningStore, candidate_id: &str) -> String {
    let connection = rusqlite::Connection::open(database_path(store)).unwrap();
    connection
        .query_row(
            "SELECT u.signature FROM learning_unit AS u
             JOIN learning_candidate AS c ON c.review_id = u.review_id AND c.unit_id = u.unit_id
             WHERE c.candidate_id = ?1",
            [candidate_id],
            |row| row.get(0),
        )
        .expect("единица кандидата обязана быть сохранена")
}

#[test]
fn unreviewed_units_are_absence_of_evidence_not_a_position() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-unreviewed");
    let pack = synthetic_pack(&repo, 3, 0);
    let queue = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Confirmed, None);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(&repo.path().join("artifacts"), &pack, &queue, &triage);
    let store_dir = TempDir::new("learning-unreviewed-store");
    let store = open_store(store_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    import(&loaded, &store);

    let summary = support_of(&store, &unit_of(&queue, "production-0"));
    assert_eq!(summary.support_units, 3);
    assert_eq!(summary.revised_units, 0);
    assert_eq!(summary.unresolved_units, 2);
    assert_eq!(summary.confirmed_units, 1);
    assert_eq!(reviewed_units(&summary), 1);
    assert!(
        summary.contradicting_unit_ids.is_empty(),
        "нерассмотренная единица не может противоречить решению: {:?}",
        summary.contradicting_unit_ids
    );
    assert_ne!(
        summary.level,
        SupportLevel::Contradictory,
        "нехватка решений — не противоречие"
    );
    assert_eq!(
        summary.level,
        SupportLevel::InsufficientEvidence,
        "одно решение из трёх не даёт вывода"
    );
    assert!(
        summary.confirmed_share_lower_bound.is_none(),
        "вывод воздерживается, значит нижней границы нет"
    );
    assert!(
        !summary.explanation.contains("противоречат"),
        "в объяснении не может быть взаимного противоречия: {}",
        summary.explanation
    );
    assert!(
        summary.explanation.contains("не рассмотрено")
            || summary.explanation.contains("Без решения"),
        "объяснение обязано называть нерассмотренные единицы: {}",
        summary.explanation
    );

    // Настоящее расхождение решений остаётся противоречием: своя история, чтобы
    // ревизии первого сценария не подмешивались в срез.
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", Disposition::Confirmed, None);
    resolve_individual(
        &mut triage,
        "production-1",
        Disposition::FalsePositive,
        None,
    );
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(
        &repo.path().join("artifacts-conflict"),
        &pack,
        &queue,
        &triage,
    );
    let conflict_dir = TempDir::new("learning-unreviewed-conflict-store");
    let conflict_store = open_store(conflict_dir.path());
    let loaded = load_in_repo(&repo, &artifacts, false).unwrap();
    import(&loaded, &conflict_store);
    let conflicting = support_of(&conflict_store, &unit_of(&queue, "production-0"));
    assert_eq!(conflicting.level, SupportLevel::Contradictory);
    assert_eq!(conflicting.unresolved_units, 1);
    assert_eq!(
        conflicting.contradicting_unit_ids,
        vec![unit_of(&queue, "production-1").id],
        "противоречит именно единица с другим решением"
    );
    assert!(conflicting.explanation.contains("противоречат"));
    assert!(
        conflicting.explanation.contains("не рассмотрено: 1"),
        "объяснение обязано называть числа: {}",
        conflicting.explanation
    );
}

/// Импортированный срез одного head вместе с его единицей очереди.
struct ImportedSlice {
    artifacts: Artifacts,
    queue: review_queue::ReviewQueue,
    head: String,
}

/// Коммитит новую правку и возвращает её SHA: линия наблюдения та же.
fn move_head(repo: &SyntheticRepo, label: &str) -> String {
    let path = repo.path().join("src/lib.rs");
    let current = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{current}\n// {label}\n")).unwrap();
    git(repo.path(), &["add", "--", "."]);
    git(repo.path(), &["commit", "-qm", label]);
    git(repo.path(), &["rev-parse", "HEAD"])
}

/// Импортирует срез с новым head того же диапазона и указанным решением.
fn import_slice(
    repo: &SyntheticRepo,
    store: &learning::LearningStore,
    label: &str,
    disposition: Disposition,
) -> ImportedSlice {
    let head = move_head(repo, label);
    let mut pack = synthetic_pack(repo, 1, 0);
    pack.target = repo.target_for(&repo.base_sha, &head);
    let queue = build_authoritative_queue(repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    resolve_individual(&mut triage, "production-0", disposition, None);
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let artifacts = Artifacts::write(
        &repo.path().join(format!("artifacts-{label}")),
        &pack,
        &queue,
        &triage,
    );
    let loaded = load_in_repo(repo, &artifacts, false).unwrap();
    import(&loaded, store);
    ImportedSlice {
        artifacts,
        queue,
        head,
    }
}

/// Число связей повторяющегося случая указанного вида.
fn case_link_count(store: &learning::LearningStore, kind: &str) -> i64 {
    let connection = rusqlite::Connection::open(database_path(store)).unwrap();
    connection
        .query_row(
            "SELECT COUNT(*) FROM learning_case_link WHERE kind = ?1",
            [kind],
            |row| row.get(0),
        )
        .unwrap_or(0)
}

#[test]
fn one_observation_line_counts_once_across_versions() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-line-a");
    let store_dir = TempDir::new("learning-line-store");
    let store = open_store(store_dir.path());

    let first = import_slice(&repo, &store, "first", Disposition::Confirmed);
    let second = import_slice(&repo, &store, "second", Disposition::Confirmed);
    assert_ne!(first.head, second.head);
    let unit = unit_of(&first.queue, "production-0");

    // Две версии одного диапазона — одно независимое наблюдение.
    let summary = support_of(&store, &unit);
    assert_eq!(summary.support_units, 1);
    assert_eq!(summary.revised_units, 1);
    assert_eq!(
        summary.support_units + summary.revised_units,
        2,
        "каждая единица среза либо независима, либо свёрнутый повтор"
    );
    assert_eq!(summary.support_reviews, 2);
    assert_eq!(summary.confirmed_units, 1);
    assert_eq!(
        summary.level,
        SupportLevel::InsufficientEvidence,
        "повторы не набирают независимую поддержку"
    );
    assert!(
        summary.confirmed_share_lower_bound.is_none(),
        "при недостатке независимых данных нижняя граница отсутствует: {:?}",
        summary.confirmed_share_lower_bound
    );
    assert!(
        case_link_count(&store, "structural_repeat") > 0,
        "импорт обязан доказать повтор связью случаев"
    );

    // Третья запись другой линии: две независимые единицы, вывод всё ещё воздерживается.
    let other = SyntheticRepo::create("learning-line-b");
    let other_first = import_slice(&other, &store, "other", Disposition::Confirmed);
    let summary = support_of(&store, &unit);
    assert_eq!(summary.support_units, 2);
    assert_eq!(summary.revised_units, 1);
    assert_eq!(summary.confirmed_units, 2);
    assert_eq!(summary.level, SupportLevel::InsufficientEvidence);

    // Ещё одна версия той же линии ничего не добавляет.
    let _other_second = import_slice(&other, &store, "other-second", Disposition::Confirmed);
    let summary = support_of(&store, &unit);
    assert_eq!(summary.support_units, 2);
    assert_eq!(summary.revised_units, 2);

    // Настоящее расхождение переживает сворачивание повторов.
    let third = SyntheticRepo::create("learning-line-c");
    let _third_first = import_slice(&third, &store, "third", Disposition::FalsePositive);
    let summary = support_of(&store, &unit);
    assert_eq!(summary.support_units, 3);
    assert_eq!(summary.revised_units, 2);
    assert_eq!(summary.level, SupportLevel::Contradictory);
    assert_eq!(summary.contradicting_unit_ids.len(), 1);

    // Точный повтор того же источника не добавляет наблюдений.
    let loaded = load_in_repo(&repo, &first.artifacts, false).unwrap();
    import(&loaded, &store);
    let repeated = support_of(&store, &unit);
    assert_eq!(repeated.support_units, 3);
    assert_eq!(repeated.revised_units, 2);
    assert_eq!(repeated.support_reviews, summary.support_reviews);
    let _ = other_first;
}

#[test]
fn independent_ranges_are_counted_separately() {
    let _guard = guard();
    let first = SyntheticRepo::create("learning-independent-a");
    let second = SyntheticRepo::create("learning-independent-b");
    let store_dir = TempDir::new("learning-independent-store");
    let store = open_store(store_dir.path());
    let imported = import_slice(&first, &store, "a", Disposition::Confirmed);
    let _second = import_slice(&second, &store, "b", Disposition::Confirmed);
    let summary = support_of(&store, &unit_of(&imported.queue, "production-0"));
    assert_eq!(summary.support_units, 2);
    assert_eq!(
        summary.revised_units, 0,
        "разные диапазоны не сворачиваются между собой"
    );
    assert_eq!(summary.confirmed_units, 2);
    assert_eq!(summary.level, SupportLevel::InsufficientEvidence);
}

#[test]
fn the_latest_observation_of_a_line_defines_its_disposition() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-line-latest");
    let store_dir = TempDir::new("learning-line-latest-store");
    let store = open_store(store_dir.path());
    let first = import_slice(&repo, &store, "first", Disposition::Confirmed);
    let _second = import_slice(&repo, &store, "second", Disposition::FalsePositive);

    // Линия — одно наблюдение: действует решение последней версии, и оно не
    // превращается в противоречие само с собой.
    let summary = support_of(&store, &unit_of(&first.queue, "production-0"));
    assert_eq!(summary.support_units, 1);
    assert_eq!(summary.revised_units, 1);
    assert_eq!(
        summary.false_positive_units, 1,
        "действует решение последнего наблюдения линии"
    );
    assert_eq!(summary.confirmed_units, 0);
    assert!(summary.contradicting_unit_ids.is_empty());
    assert_eq!(summary.level, SupportLevel::InsufficientEvidence);
}

#[test]
fn search_does_not_mix_quarantine_with_trusted_history() {
    let _guard = guard();
    let repo = SyntheticRepo::create("learning-search-trust");
    let pack = synthetic_pack(&repo, 3, 2);
    let authoritative = build_authoritative_queue(&repo, &pack);
    let digest = digest_of(&pack);
    let mut triage = semantic_triage::initialize(&pack, &digest);
    for candidate in ["production-0", "production-1", "production-2"] {
        resolve_individual(&mut triage, candidate, Disposition::Confirmed, None);
    }
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &pack, &digest).unwrap();
    let store_dir = TempDir::new("learning-search-trust-store");
    let store = open_store(store_dir.path());

    // Доверенная история и карантин одного и того же снимка лежат в одной базе:
    // различает их только уровень доверия записи.
    let trusted_artifacts =
        Artifacts::write(&repo.path().join("trusted"), &pack, &authoritative, &triage);
    let trusted = load_in_repo(&repo, &trusted_artifacts, false).unwrap();
    assert_eq!(trusted.trust, TrustLevel::AstAuthenticated);
    let trusted_record = import(&trusted, &store);
    let quarantine_artifacts = Artifacts::write(
        &repo.path().join("quarantine"),
        &pack,
        &authoritative,
        &triage,
    );
    let quarantined = expect_quarantine(&repo, &quarantine_artifacts);
    let quarantined_record = import(&quarantined, &store);
    assert_ne!(trusted_record.review_id, quarantined_record.review_id);
    assert!(trusted_record.revision_of.is_none());
    assert!(quarantined_record.revision_of.is_none());
    assert_eq!(trusted_record.superseded_by, None);
    assert_eq!(quarantined_record.superseded_by, None);
    let generation = learning::import::generation(&store).unwrap();
    assert_eq!(generation.trusted_reviews, 1);
    assert_eq!(generation.quarantined_reviews, 1);
    let trusted_patterns =
        learning::pattern_report(&store, &learning::patterns::PatternQuery::default()).unwrap();
    assert!(trusted_patterns.limitations[2].contains("только записи с доверием ast_authenticated"));
    let patterns_with_quarantine = learning::pattern_report(
        &store,
        &learning::patterns::PatternQuery {
            include_quarantine: true,
            ..learning::patterns::PatternQuery::default()
        },
    )
    .unwrap();
    assert!(patterns_with_quarantine.limitations[2].contains("Карантинные записи включены"));
    assert!(patterns_with_quarantine.limitations[2].contains("1"));

    // Порядок импорта не позволяет карантину вытеснить доверенную запись.
    let reverse_store_dir = TempDir::new("learning-search-trust-reverse-store");
    let reverse_store = open_store(reverse_store_dir.path());
    let reverse_quarantine = import(&quarantined, &reverse_store);
    let reverse_trusted = import(&trusted, &reverse_store);
    assert!(reverse_quarantine.revision_of.is_none());
    assert!(reverse_trusted.revision_of.is_none());
    assert_eq!(reverse_quarantine.superseded_by, None);
    assert_eq!(reverse_trusted.superseded_by, None);

    // По умолчанию поиск возвращает только доверенную историю.
    let trusted_page = learning::search_history(&store, &learning::SearchQuery::default())
        .expect("поиск обязан работать");
    assert!(!trusted_page.include_quarantine);
    assert!(
        trusted_page.matched > 0,
        "доверенная история обязана находиться"
    );
    assert!(
        trusted_page
            .cases
            .iter()
            .all(|case| case.trust == "ast_authenticated"),
        "карантин не возвращается без явного согласия"
    );

    // Карантин доступен только явно и не подменяет доверенные случаи.
    let with_quarantine = learning::search_history(
        &store,
        &learning::SearchQuery {
            include_quarantine: true,
            ..learning::SearchQuery::default()
        },
    )
    .expect("поиск обязан работать");
    assert!(with_quarantine.include_quarantine);
    assert!(
        with_quarantine.matched > trusted_page.matched,
        "с явным согласием карантинные случаи возвращаются"
    );
    assert!(
        with_quarantine
            .cases
            .iter()
            .any(|case| case.trust == "structure_only_quarantine"),
        "карантинная запись обязана быть видна в выводе с флагом"
    );
    let trusted_ids: BTreeSet<&str> = trusted_page
        .cases
        .iter()
        .map(|case| case.case_id.as_str())
        .collect();
    let all_ids: BTreeSet<&str> = with_quarantine
        .cases
        .iter()
        .map(|case| case.case_id.as_str())
        .collect();
    assert!(
        trusted_ids.is_subset(&all_ids),
        "доверенные случаи не исчезают при включении карантина"
    );

    // Текстовый путь подчиняется тому же фильтру, а не только структурный.
    let text = trusted_page
        .cases
        .first()
        .map(|case| case.snippet.clone())
        .expect("доверенный случай обязан иметь текст");
    let text_page = learning::search_history(
        &store,
        &learning::SearchQuery {
            text: Some(text.clone()),
            ..learning::SearchQuery::default()
        },
    )
    .expect("текстовый поиск обязан работать");
    assert!(
        !text_page.cases.is_empty(),
        "текстовый поиск обязан находить доверенный случай"
    );
    assert!(
        text_page
            .cases
            .iter()
            .all(|case| case.trust == "ast_authenticated"),
        "текстовый поиск тоже не смешивает карантин: {:?}",
        text_page.cases
    );
    // Тот же текст есть и у карантинной записи: без фильтра она нашлась бы.
    let text_with_quarantine = learning::search_history(
        &store,
        &learning::SearchQuery {
            text: Some(text),
            include_quarantine: true,
            ..learning::SearchQuery::default()
        },
    )
    .expect("текстовый поиск обязан работать");
    assert!(
        text_with_quarantine.matched > text_page.matched,
        "карантинная запись с тем же текстом находится только с явным согласием"
    );

    store
        .write(|write| {
            write.execute(
                "UPDATE learning_import SET trust = 'future_trust_level' WHERE review_id = ?1",
                rusqlite::params![trusted_record.review_id],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        learning::show_import(&store, &trusted_record.review_id)
            .unwrap_err()
            .code,
        ErrorCode::LearningCorrupt,
        "неизвестная метка доверия не считается ast_authenticated"
    );
    assert_eq!(
        learning::pattern_report(&store, &learning::patterns::PatternQuery::default())
            .unwrap_err()
            .code,
        ErrorCode::LearningCorrupt,
        "отчёт паттернов отвергает неизвестную метку доверия"
    );
}
