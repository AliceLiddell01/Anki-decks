//! Контракт CLI-подкоманды `code-review learning` на собственной синтетике.
//!
//! Ни один тест не читает `decks/**`: фикстуры строятся во временных
//! Git-репозиториях, а свидетельства создаёт реальный конвейер
//! `code-review collect`. Проверяются свойства, определяющие доверие к CLI:
//! штатный режим без истории, различимость ошибок, ограничение вывода и
//! пагинация, безопасная публикация производных артефактов и неизменность
//! исходных документов ревью.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::common::{TempDir, run_cli_in};
use anki_repo::code_review::learning::{LearningStore, StoreOptions};

/// Синтетический Git-репозиторий с production- и test-исходником.
struct Fixture {
    temp: TempDir,
    base_sha: String,
    head_sha: String,
}

/// Базовая версия production-исходника: без кандидатов ревью.
const BASE_PRODUCTION: &str =
    "pub fn parse(value: &str) -> Option<u32> {\n    value.parse().ok()\n}\n";
/// Базовая версия тестового исходника.
const BASE_TESTS: &str = "#[test]\nfn parses() {\n    assert!(crate::parse(\"0\").is_some());\n}\n";
/// Правка с тремя однотипными кандидатами `error_path` в production-коде.
const HEAD_PRODUCTION: &str = "pub fn parse(value: &str) -> Option<u32> {\n    let digits = value.trim();\n    Some(digits.parse::<u32>().unwrap())\n}\n\npub fn first(value: &str) -> u32 {\n    value.trim().parse::<u32>().unwrap()\n}\n\npub fn last(value: &str) -> u32 {\n    value.trim().parse::<u32>().unwrap()\n}\n";
/// Правка тестового исходника.
const HEAD_TESTS: &str = "#[test]\nfn parses() {\n    assert!(crate::parse(\"1\").is_some());\n}\n";
/// Правка с единственным кандидатом `error_path`: срез ровно из одной единицы.
const SINGLE_PRODUCTION: &str = "pub fn parse(value: &str) -> Option<u32> {\n    let digits = value.trim();\n    Some(digits.parse::<u32>().unwrap())\n}\n";
/// Вторая правка: та же структурная сигнатура при других именах функций.
const SECOND_PRODUCTION: &str = "pub fn decode(value: &str) -> Option<u32> {\n    let digits = value.trim();\n    Some(digits.parse::<u32>().unwrap())\n}\n\npub fn head(value: &str) -> u32 {\n    value.trim().parse::<u32>().unwrap()\n}\n\npub fn tail(value: &str) -> u32 {\n    value.trim().parse::<u32>().unwrap()\n}\n";

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

fn write_sources(root: &Path, production: &str, tests: &str) {
    fs::write(root.join("src/lib.rs"), production).unwrap();
    fs::write(root.join("src/tests/parse.rs"), tests).unwrap();
}

fn commit_all(root: &Path, message: &str) {
    git(root, &["add", "--", "."]);
    git(root, &["commit", "-qm", message]);
}

/// Запускает CLI в каталоге фикстуры и возвращает код, stdout и stderr.
fn cli(fixture: &Fixture, args: &[&str]) -> (i32, String, String) {
    run_cli_in(Some(fixture.path()), args)
}

/// Запускает CLI из подкаталога репозитория: путь базы считается от корня.
fn cli_from_subdirectory(fixture: &Fixture, args: &[&str]) -> (i32, String, String) {
    let mut argv = vec!["--json"];
    argv.extend_from_slice(args);
    run_cli_in(Some(&fixture.path().join("src")), &argv)
}

/// Запускает CLI в режиме машинного JSON и разбирает stdout.
fn cli_json(fixture: &Fixture, args: &[&str]) -> (i32, Value, String) {
    let mut argv = vec!["--json"];
    argv.extend_from_slice(args);
    let (code, stdout, stderr) = run_cli_in(Some(fixture.path()), &argv);
    let value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("вывод CLI не является JSON: {error}"));
    (code, value, stderr)
}

/// Разбирает машинный JSON успешного ответа.
fn ok_json(fixture: &Fixture, args: &[&str]) -> Value {
    let (code, value, _stderr) = cli_json(fixture, args);
    assert!(code == 0, "команда должна завершиться успешно, код: {code}");
    value
}

/// Машиночитаемый код ошибки из ответа.
fn error_code(value: &Value) -> String {
    value["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("в ответе нет кода ошибки"))
        .to_owned()
}

/// Проверяет, что команда отказывает с ожидаемым кодом ошибки.
fn assert_error(fixture: &Fixture, args: &[&str], expected: &str) {
    let (code, value, _stderr) = cli_json(fixture, args);
    assert!(
        code != 0,
        "команда должна завершиться с ошибкой; код: {code}"
    );
    let actual = error_code(&value);
    assert!(
        actual == expected,
        "код ошибки команды не совпал с ожидаемым: ожидался {expected}, получен {actual}"
    );
}

impl Fixture {
    fn create(label: &str) -> Self {
        let temp = TempDir::new(label);
        let root = temp.path();
        git(root, &["init", "-q"]);
        git(
            root,
            &["config", "user.email", "cli-learning@example.invalid"],
        );
        git(root, &["config", "user.name", "CLI Learning Contract"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        git(root, &["config", "core.autocrlf", "false"]);
        fs::create_dir_all(root.join("src/tests")).unwrap();
        write_sources(root, BASE_PRODUCTION, BASE_TESTS);
        commit_all(root, "Синтетическая база");
        let base_sha = git(root, &["rev-parse", "HEAD"]);
        write_sources(root, HEAD_PRODUCTION, HEAD_TESTS);
        commit_all(root, "Синтетическая правка");
        let head_sha = git(root, &["rev-parse", "HEAD"]);
        let fixture = Self {
            temp,
            base_sha,
            head_sha,
        };
        fixture.collect(&fixture.base_sha, &fixture.head_sha);
        fixture
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }

    /// Каталог снимка в канонической рабочей области ревью.
    fn workspace(&self, head: &str) -> PathBuf {
        self.path().join(".anki-repo/review/local").join(head)
    }

    fn artifact(&self, head: &str, name: &str) -> String {
        format!(".anki-repo/review/local/{head}/{name}")
    }

    fn pack(&self, head: &str) -> String {
        self.artifact(head, "review.json")
    }

    fn queue(&self, head: &str) -> String {
        self.artifact(head, "review-queue.json")
    }

    fn triage(&self, head: &str) -> String {
        self.artifact(head, "semantic-triage.json")
    }

    fn collect(&self, base: &str, head: &str) {
        let (code, _, _stderr) = cli(
            self,
            &["code-review", "collect", "--base", base, "--head", head],
        );
        assert_eq!(code, 0, "collect должен проходить");
    }

    /// Добавляет второй снимок с той же структурной сигнатурой.
    fn add_second_head(&self) -> String {
        write_sources(self.path(), SECOND_PRODUCTION, HEAD_TESTS);
        commit_all(self.path(), "Синтетическая вторая правка");
        let head = git(self.path(), &["rev-parse", "HEAD"]);
        self.collect(&self.base_sha, &head);
        head
    }

    /// Идентификаторы кандидатов одного детектора в пакете снимка.
    fn candidate_ids(&self, head: &str, detector: &str) -> Vec<String> {
        let pack: Value =
            serde_json::from_slice(&fs::read(self.path().join(self.pack(head))).unwrap())
                .expect("review.json фикстуры должен разбираться");
        pack["candidates"]
            .as_array()
            .expect("в пакете есть список кандидатов")
            .iter()
            .filter(|candidate| candidate["detector"] == json!(detector))
            .map(|candidate| candidate["id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Создаёт проверенный семантический разбор с заданными решениями.
    fn write_triage(&self, head: &str, decisions: &[(String, &str)]) {
        let input = self.workspace(head).join("semantic-triage.input.json");
        if input.exists() {
            fs::remove_file(&input).unwrap();
        }
        let (code, _, _stderr) = cli(
            self,
            &[
                "code-review",
                "triage",
                "init",
                "--pack",
                &self.pack(head),
                "--out",
                &self.artifact(head, "semantic-triage.input.json"),
            ],
        );
        assert_eq!(code, 0, "triage init должен проходить");
        let mut document: Value =
            serde_json::from_slice(&fs::read(&input).unwrap()).expect("документ разбора");
        for (candidate_id, disposition) in decisions {
            decide(&mut document, candidate_id, disposition);
        }
        fs::write(&input, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        let (code, _, _stderr) = cli(
            self,
            &[
                "code-review",
                "triage",
                "validate",
                "--pack",
                &self.pack(head),
                "--triage",
                &self.artifact(head, "semantic-triage.input.json"),
                "--canonical-out",
                &self.triage(head),
            ],
        );
        assert_eq!(code, 0, "triage validate должен проходить");
    }

    /// Импортирует проверенную историю снимка и возвращает запись истории.
    fn import(&self, head: &str) -> Value {
        let value = ok_json(
            self,
            &[
                "code-review",
                "learning",
                "import",
                "--pack",
                &self.pack(head),
                "--queue",
                &self.queue(head),
                "--triage",
                &self.triage(head),
            ],
        );
        value["result"].clone()
    }
}

/// Добавляет решение кандидата в документ семантического разбора.
fn decide(document: &mut Value, candidate_id: &str, disposition: &str) {
    let unreviewed = document["unreviewed_candidate_ids"]
        .as_array_mut()
        .expect("в документе есть список нерассмотренных");
    unreviewed.retain(|id| id.as_str() != Some(candidate_id));
    let mut decision = json!({
        "candidate_id": candidate_id,
        "disposition": disposition,
        "reason_code": "expected_failure_path",
        "explanation": "Синтетическое решение для контракта CLI learning.",
        "finding_ids": [],
    });
    if disposition == "confirmed" {
        let finding_id = format!("finding-{candidate_id}");
        decision["finding_ids"] = json!([finding_id]);
        document["findings"]
            .as_array_mut()
            .expect("в документе есть список замечаний")
            .push(json!({
                "id": finding_id,
                "severity": "major",
                "title": "Синтетическое подтверждённое замечание",
                "description": format!("{candidate_id}: подтверждённый дефект синтетического фикстура."),
                "provenance": "candidate_assisted",
                "candidate_ids": [candidate_id],
            }));
    }
    document["individual_decisions"]
        .as_array_mut()
        .expect("в документе есть список решений")
        .push(decision);
}

/// Все кандидаты `error_path` снимка с одним решением.
fn error_path_decisions<'a>(
    fixture: &Fixture,
    head: &str,
    disposition: &'a str,
) -> Vec<(String, &'a str)> {
    fixture
        .candidate_ids(head, "error_path")
        .into_iter()
        .map(|candidate_id| (candidate_id, disposition))
        .collect()
}

/// Проверяет, что в выводе нет неограниченных списков идентификаторов.
fn assert_bounded_candidate_ids(value: &Value) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if key == "candidate_ids" {
                    let length = item.as_array().map_or(0, Vec::len);
                    assert!(
                        length <= 3,
                        "список candidate_ids в выводе не ограничен: {length} элементов"
                    );
                }
                assert_bounded_candidate_ids(item);
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_bounded_candidate_ids(item);
            }
        }
        _ => {}
    }
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).expect("файл фикстуры должен читаться")
}

/// Пересчитывает digest тела по формату архива до версии 4.
fn legacy_archive_digest(
    archive: &anki_repo::code_review::learning::model::LearningExport,
) -> String {
    #[derive(serde::Serialize)]
    struct Body<'a> {
        reviews: &'a [anki_repo::code_review::learning::model::ImportRecord],
        units: &'a [anki_repo::code_review::learning::model::ReviewUnitRecord],
        candidates: &'a [anki_repo::code_review::learning::model::ExportedCandidate],
        findings: &'a [anki_repo::code_review::learning::model::ExportedFinding],
        finding_links: &'a [anki_repo::code_review::learning::model::ExportedFindingLink],
        case_links: &'a [anki_repo::code_review::learning::model::ExportedCaseLink],
        feedback_events: &'a [anki_repo::code_review::learning::model::FeedbackEvent],
        policy_proposals: &'a [anki_repo::code_review::learning::model::PolicyProposal],
        search: &'a [anki_repo::code_review::learning::model::ExportedSearchCase],
    }
    let body = Body {
        reviews: &archive.reviews,
        units: &archive.units,
        candidates: &archive.candidates,
        findings: &archive.findings,
        finding_links: &archive.finding_links,
        case_links: &archive.case_links,
        feedback_events: &archive.feedback_events,
        policy_proposals: &archive.policy_proposals,
        search: &archive.search,
    };
    anki_repo::code_review::learning::import::sha256_hex(
        &serde_json::to_vec(&body).expect("legacy body сериализуется"),
    )
}

#[test]
fn review_workflow_without_learning_is_unchanged_and_creates_no_database() {
    let fixture = Fixture::create("learning-cli-disabled");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);

    let pack_bytes = read(&fixture.path().join(fixture.pack(&head)));
    let queue_bytes = read(&fixture.path().join(fixture.queue(&head)));
    let triage_bytes = read(&fixture.path().join(fixture.triage(&head)));

    // Обычные команды ревью работают без базы learning.
    let (code, _, _stderr) = cli(
        &fixture,
        &[
            "code-review",
            "queue",
            "validate",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
        ],
    );
    assert_eq!(code, 0, "queue validate без learning должен проходить");
    let (code, _, _stderr) = cli(
        &fixture,
        &[
            "code-review",
            "triage",
            "validate",
            "--pack",
            &fixture.pack(&head),
            "--triage",
            &fixture.triage(&head),
        ],
    );
    assert_eq!(code, 0, "triage validate без learning должен проходить");
    let before = ok_json(
        &fixture,
        &[
            "code-review",
            "queue",
            "list",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
        ],
    );

    // Задание изоляции тоже не создаёт базу learning.
    let prepared = ok_json(
        &fixture,
        &[
            "code-review",
            "execution",
            "prepare",
            "--pack",
            &fixture.pack(&head),
            "--mode",
            "isolated_checks",
            "--scope",
            "tests",
        ],
    );
    let job = prepared["result"]["job_directory"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, _, _stderr) = cli(&fixture, &["code-review", "execution", "inspect", &job]);
    assert_eq!(code, 0, "execution inspect без learning должен проходить");
    let (code, _, _stderr) = cli(
        &fixture,
        &[
            "code-review",
            "execution",
            "cleanup",
            &job,
            "--confirm-no-live-descendants",
        ],
    );
    assert_eq!(code, 0, "execution cleanup без learning должен проходить");

    let database = fixture.path().join(".anki-repo/learning");
    assert!(
        !database.exists(),
        "обычное ревью не должно создавать каталог learning"
    );

    // Состояние отсутствующей истории — это состояние, а не ошибка.
    let status = ok_json(&fixture, &["code-review", "learning", "status"]);
    assert_eq!(status["result"]["present"], json!(false));
    assert_eq!(status["result"]["user_version"], json!(0));
    assert!(!database.exists(), "status не создаёт базу");

    // Рекомендации без истории совпадают с детерминированным порядком очереди.
    let without = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--without-learning",
        ],
    );
    assert_eq!(without["result"]["learning_disabled"], json!(true));
    assert!(!database.exists(), "recommend без истории не создаёт базу");

    // Импорт истории создаёт базу, но не меняет исходные документы ревью.
    let record = fixture.import(&head);
    assert_eq!(record["status"], json!("imported"));
    assert!(
        fixture
            .path()
            .join(".anki-repo/learning/state.sqlite")
            .is_file()
    );

    // Команды работают из любого каталога репозитория: путь базы считается от корня.
    let (code, stdout, _stderr) =
        cli_from_subdirectory(&fixture, &["code-review", "learning", "status"]);
    assert_eq!(code, 0, "status из подкаталога должен проходить");
    let from_subdirectory: Value = serde_json::from_str(&stdout).expect("JSON из подкаталога");
    assert_eq!(from_subdirectory["result"]["present"], json!(true));
    assert_eq!(
        from_subdirectory["result"]["database_path"],
        json!(".anki-repo/learning/state.sqlite")
    );

    let after = ok_json(
        &fixture,
        &[
            "code-review",
            "queue",
            "list",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
        ],
    );
    assert_eq!(
        before, after,
        "наличие истории learning не меняет обычный просмотр очереди"
    );
    assert_eq!(read(&fixture.path().join(fixture.pack(&head))), pack_bytes);
    assert_eq!(
        read(&fixture.path().join(fixture.queue(&head))),
        queue_bytes
    );
    assert_eq!(
        read(&fixture.path().join(fixture.triage(&head))),
        triage_bytes
    );
}

#[test]
fn import_is_idempotent_reports_revisions_and_distinguishable_errors() {
    let fixture = Fixture::create("learning-cli-import");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);

    let first = fixture.import(&head);
    assert_eq!(first["status"], json!("imported"));
    assert_eq!(first["record"]["trust"], json!("ast_authenticated"));
    assert_eq!(first["record"]["outcome"], json!("fully_reviewed"));
    let review_id = first["record"]["review_id"].as_str().unwrap().to_owned();

    // Точный повтор того же набора свидетельств не удваивает историю.
    let repeated = fixture.import(&head);
    assert_eq!(repeated["status"], json!("noop_existing"));
    assert_eq!(repeated["record"]["review_id"], json!(review_id));
    let stats = ok_json(&fixture, &["code-review", "learning", "stats"]);
    assert_eq!(stats["result"]["reviews_total"], json!(1));

    // Другие байты разбора при той же identity — аудируемая ревизия, а не перезапись.
    fixture.write_triage(&head, &error_path_decisions(&fixture, &head, "acceptable"));
    let revision = fixture.import(&head);
    assert_eq!(revision["status"], json!("revision_created"));
    assert_ne!(revision["record"]["review_id"], json!(review_id));
    assert_eq!(revision["record"]["revision_of"], json!(review_id));
    let stats = ok_json(&fixture, &["code-review", "learning", "stats"]);
    assert_eq!(stats["result"]["reviews_total"], json!(2));

    // Карантинная запись видна отдельно и не входит в доверенную историю.
    let quarantined = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--structure-only",
            "--variant",
            "snapshot-0123456789abcdef0123456789abcdef",
        ],
    );
    assert_eq!(quarantined["result"]["status"], json!("quarantined"));
    assert_eq!(
        quarantined["result"]["record"]["trust"],
        json!("structure_only_quarantine")
    );
    let trusted = ok_json(&fixture, &["code-review", "learning", "stats"]);
    assert_eq!(trusted["result"]["reviews_total"], json!(2));
    let with_quarantine = ok_json(
        &fixture,
        &["code-review", "learning", "stats", "--include-quarantine"],
    );
    assert_eq!(with_quarantine["result"]["reviews_total"], json!(3));

    // Различимые отказы не маскируются друг под друга.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
            "--execution",
            "result-one.json",
            "--execution",
            "result-two.json",
        ],
        "invalid_request",
    );
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--variant",
            "snapshot-short",
        ],
        "invalid_request",
    );
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            "missing-queue.json",
        ],
        "input_unreadable",
    );

    // Очередь, заявляющая чужой digest пакета, — это `source_changed`.
    let mut queue: Value =
        serde_json::from_slice(&read(&fixture.path().join(fixture.queue(&head))))
            .expect("очередь фикстуры");
    queue["source"]["review_pack_sha256"] = json!("0".repeat(64));
    let tampered = fixture.path().join("tampered-queue.json");
    fs::write(&tampered, serde_json::to_vec(&queue).unwrap()).unwrap();
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            "tampered-queue.json",
        ],
        "source_changed",
    );

    // Читающая команда на отсутствующей базе — это `not_found`, а не пустой успех.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "stats",
            "--db",
            ".anki-repo/learning/absent.sqlite",
        ],
        "not_found",
    );
    let absent_policy_database = ".anki-repo/learning/missing-policy.sqlite";
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "policy",
            "propose",
            "--db",
            absent_policy_database,
            "--signature",
            "signature-that-does-not-exist",
            "--rule-id",
            "rule-missing-database",
        ],
        "not_found",
    );
    assert!(
        !fixture.path().join(absent_policy_database).exists(),
        "policy propose не создаёт базу до чтения существующей истории"
    );
}

#[test]
fn execution_import_uses_the_job_manifest_and_keeps_only_a_compact_summary() {
    let fixture = Fixture::create("learning-cli-execution-evidence");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);
    let pack = fixture.pack(&head);
    let queue = fixture.queue(&head);
    let triage = fixture.triage(&head);

    let prepared = ok_json(
        &fixture,
        &[
            "code-review",
            "execution",
            "prepare",
            "--pack",
            &pack,
            "--mode",
            "isolated_checks",
            "--scope",
            "tests",
        ],
    );
    let job = prepared["result"]["job_directory"]
        .as_str()
        .unwrap()
        .to_owned();
    let run = ok_json(
        &fixture,
        &[
            "code-review",
            "execution",
            "run",
            &job,
            "--timeout-seconds",
            "10",
            "--",
            "/bin/true",
        ],
    );
    assert_eq!(run["result"]["status"], json!("passed"));
    let result_path = fixture.path().join(&job).join("result.json");
    let result_arg = result_path.to_str().unwrap().to_owned();

    let imported = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--pack",
            &pack,
            "--queue",
            &queue,
            "--triage",
            &triage,
            "--execution",
            &result_arg,
        ],
    );
    let evidence = &imported["result"]["record"]["inputs"]["execution_evidence"];
    assert_eq!(evidence["status"], json!("passed"));
    assert_eq!(evidence["security_sandbox"], json!("absent"));
    assert!(evidence["stdout_total_bytes"].is_number());
    assert!(
        evidence.get("stdout").is_none(),
        "сырые логи не импортируются"
    );
    assert!(evidence["limitations"].as_array().unwrap().len() >= 2);

    // Подмена job_id в файле при сохранённых source hashes не проходит
    // проверку результата против job.json и не создаёт второй импорт.
    let mut forged: Value = serde_json::from_slice(&read(&result_path)).unwrap();
    forged["job_id"] = json!("0".repeat(32));
    fs::write(&result_path, serde_json::to_vec_pretty(&forged).unwrap()).unwrap();
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--pack",
            &pack,
            "--queue",
            &queue,
            "--triage",
            &triage,
            "--execution",
            &result_arg,
        ],
        "review_artifact_invalid",
    );
    let stats = ok_json(&fixture, &["code-review", "learning", "stats"]);
    assert_eq!(stats["result"]["reviews_total"], json!(1));
}

#[test]
fn learning_forget_requires_confirmation_and_removes_only_the_requested_run() {
    let fixture = Fixture::create("learning-cli-forget");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);
    let imported = fixture.import(&head);
    let review_id = imported["record"]["review_id"].as_str().unwrap().to_owned();

    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "forget",
            "--review-id",
            &review_id,
        ],
        "invalid_request",
    );
    let preserved = ok_json(&fixture, &["code-review", "learning", "stats"]);
    assert_eq!(preserved["result"]["reviews_total"], json!(1));

    let forgotten = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "forget",
            "--review-id",
            &review_id,
            "--confirm",
        ],
    );
    assert_eq!(forgotten["result"]["removed"]["reviews"], json!(1));
    let queue: Value =
        serde_json::from_slice(&read(&fixture.path().join(fixture.queue(&head)))).unwrap();
    assert_eq!(
        forgotten["result"]["removed"]["units"],
        json!(queue["units"].as_array().unwrap().len())
    );
    let stats = ok_json(&fixture, &["code-review", "learning", "stats"]);
    assert_eq!(stats["result"]["reviews_total"], json!(0));
    assert_eq!(stats["result"]["observations"]["raw_candidates"], json!(0));
    assert!(fixture.path().join(fixture.pack(&head)).is_file());
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "forget",
            "--review-id",
            &review_id,
            "--confirm",
        ],
        "not_found",
    );
}

#[test]
fn status_validate_stats_patterns_and_search_bound_their_output() {
    let fixture = Fixture::create("learning-cli-read");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);
    fixture.import(&head);

    let status = ok_json(&fixture, &["code-review", "learning", "status"]);
    assert_eq!(status["result"]["present"], json!(true));
    assert_eq!(
        status["result"]["user_version"],
        json!(anki_repo::code_review::learning::LEARNING_SCHEMA_VERSION)
    );
    assert_eq!(status["result"]["integrity_ok"], json!(true));
    assert!(status["result"]["fts5_available"].is_boolean());
    assert!(
        status["result"]["journal_mode"] == json!("wal")
            || status["result"]["journal_mode"] == json!("delete")
    );

    let validated = ok_json(&fixture, &["code-review", "learning", "validate"]);
    assert_eq!(validated["result"]["present"], json!(true));

    let stats = ok_json(
        &fixture,
        &["code-review", "learning", "stats", "--limit", "1"],
    );
    assert_eq!(stats["result"]["reviews_total"], json!(1));
    assert_eq!(stats["result"]["reviews_shown"], json!(1));
    assert_eq!(stats["result"]["reviews_truncated"], json!(false));
    assert_eq!(stats["result"]["dispositions"]["confirmed"], json!(3));
    assert_bounded_candidate_ids(&stats);

    // Ограничение страницы: запрошенный лимит соблюдается, остаток виден по счётчику.
    let patterns = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "patterns",
            "--limit",
            "1",
            "--detector",
            "error_path",
        ],
    );
    assert!(
        patterns["result"]["rules"].as_array().unwrap().len() <= 1,
        "лимит паттернов не соблюдён: {patterns}"
    );
    assert_bounded_candidate_ids(&patterns);

    // Подтверждённая история проходит явное требование поддержки.
    let supported = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "patterns",
            "--require-supported",
            "--detector",
            "error_path",
        ],
    );
    assert_eq!(supported["result"]["abstained"], json!(false));
    // Числа поддержки видны и в машинном выводе, и в человекочитаемом: сколько
    // независимых линий, сколько записей их наблюдало и сколько повторов свёрнуто.
    let rule = &supported["result"]["rules"][0]["support"]["summary"];
    for field in [
        "support_units",
        "support_reviews",
        "revised_units",
        "unresolved_units",
    ] {
        assert!(
            rule[field].is_u64(),
            "в сводке поддержки нет поля {field}: {rule}"
        );
    }
    assert!(
        rule["explanation"]
            .as_str()
            .unwrap()
            .contains("повторных наблюдений того же случая исключено"),
        "объяснение обязано называть свёрнутые повторы: {rule}"
    );
    let (code, human, _stderr) = cli(
        &fixture,
        &[
            "code-review",
            "learning",
            "patterns",
            "--detector",
            "error_path",
        ],
    );
    assert_eq!(code, 0, "человекочитаемый вывод паттернов должен проходить");
    assert!(
        human.contains("независимых единиц") && human.contains("свёрнутых повторов"),
        "человекочитаемый вывод обязан показывать свёрнутые повторы: {human}"
    );

    // Фильтр, под который ничего не подходит, честно воздерживается.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "patterns",
            "--require-supported",
            "--detector",
            "detector-that-does-not-exist",
        ],
        "insufficient_evidence",
    );

    let first_page = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "search",
            "--detector",
            "error_path",
            "--limit",
            "2",
            "--offset",
            "0",
        ],
    );
    assert_eq!(first_page["result"]["limit"], json!(2));
    assert_eq!(first_page["result"]["offset"], json!(0));
    let cases = first_page["result"]["cases"].as_array().unwrap();
    assert!(
        !cases.is_empty(),
        "поиск обязан находить исторические случаи"
    );
    assert!(cases.len() <= 2, "страница поиска не ограничена: {cases:?}");
    assert_bounded_candidate_ids(&first_page);
    assert_eq!(
        first_page["result"]["has_more"],
        json!(true),
        "три подтверждённых кандидата при лимите 2 обязаны давать продолжение"
    );
    let second_page = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "search",
            "--detector",
            "error_path",
            "--limit",
            "2",
            "--offset",
            "2",
        ],
    );
    assert_ne!(
        first_page["result"]["cases"], second_page["result"]["cases"],
        "смещение обязано менять страницу"
    );

    // Текстовая подстрока короче двух символов — это `invalid_request`.
    assert_error(
        &fixture,
        &["code-review", "learning", "search", "--text", "a"],
        "invalid_request",
    );

    // Выход за жёсткий максимум страницы отвергается разбором аргументов.
    let (code, _, _stderr) = cli(
        &fixture,
        &["code-review", "learning", "search", "--limit", "100000"],
    );
    assert_eq!(code, 2, "жёсткий максимум страницы должен отвергаться");
}

#[test]
fn recommendations_are_deterministic_guardrailed_and_publish_safely() {
    let fixture = Fixture::create("learning-cli-recommend");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);

    // Без истории рекомендации доступны, но требуют явного согласия.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--require-history",
        ],
        "not_found",
    );

    fixture.import(&head);
    let pack_bytes = read(&fixture.path().join(fixture.pack(&head)));
    let queue_bytes = read(&fixture.path().join(fixture.queue(&head)));

    let first = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
        ],
    );
    let second = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
        ],
    );
    assert_eq!(
        first, second,
        "рекомендации детерминированы для тех же входов"
    );

    let result = &first["result"];
    assert_eq!(result["learning_disabled"], json!(false));
    assert_eq!(result["generation"]["trusted_reviews"], json!(1));
    assert!(result["policy_version"].is_number());
    assert_bounded_candidate_ids(&first);

    // Ни одна единица очереди не исчезает из подсказок и не теряет приоритет.
    let queue: Value = serde_json::from_slice(&queue_bytes).expect("очередь фикстуры");
    let units = queue["units"].as_array().unwrap();
    assert!(!units.is_empty());
    let recommended: Vec<&str> = result["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["unit_id"].as_str().unwrap())
        .collect();
    assert_eq!(recommended.len(), units.len());
    for unit in units {
        let unit_id = unit["id"].as_str().unwrap();
        let item = result["recommendations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["unit_id"] == json!(unit_id))
            .unwrap_or_else(|| panic!("единица {unit_id} потеряна в рекомендациях"));
        assert_eq!(item["queue_priority"], unit["priority"]);
        assert!(item["candidate_count"].as_u64().unwrap() >= 1);
        assert!(
            result["suggested_order"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == &json!(unit_id)),
            "единица {unit_id} отсутствует в предложенном порядке"
        );
    }

    // Публикация производного артефакта привязана к рабочей области снимка.
    let expected = fixture.artifact(&head, "recommendations.json");
    let published = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
            "--out",
            &expected,
        ],
    );
    assert_eq!(published["result"]["artifact"]["created"], json!(true));
    assert_eq!(published["result"]["artifact"]["path"], json!(expected));
    let artifact_bytes = read(&fixture.path().join(&expected));
    let repeat = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
            "--out",
            &expected,
        ],
    );
    assert_eq!(
        repeat["result"]["artifact"]["created"],
        json!(false),
        "повтор с теми же байтами идемпотентен"
    );
    assert_eq!(read(&fixture.path().join(&expected)), artifact_bytes);
    fixture.collect(&fixture.base_sha, &head);
    assert_eq!(
        read(&fixture.path().join(&expected)),
        artifact_bytes,
        "повторный collect сохраняет рекомендации"
    );

    // Произвольный --out не перезаписывает чужие файлы.
    fs::write(fixture.path().join(&expected), b"{\"foreign\": true}").unwrap();
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
            "--out",
            &expected,
        ],
        "review_artifact_conflict",
    );
    assert_eq!(
        read(&fixture.path().join(&expected)),
        b"{\"foreign\": true}".to_vec(),
        "чужой файл остаётся нетронутым"
    );

    // Путь вне рабочей области снимка отвергается.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
            "--out",
            "recommendations.json",
        ],
        "invalid_request",
    );

    // Явная версия истории и политики проверяются, а не игнорируются.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--history-revision",
            "999",
        ],
        "source_changed",
    );
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--policy-version",
            "999",
        ],
        "learning_schema_unsupported",
    );

    // Рекомендации не мутируют исходные документы.
    assert_eq!(read(&fixture.path().join(fixture.pack(&head))), pack_bytes);
    assert_eq!(
        read(&fixture.path().join(fixture.queue(&head))),
        queue_bytes
    );
}

#[test]
fn low_noise_history_never_hides_a_runtime_risk() {
    let fixture = Fixture::create("learning-cli-guardrails");
    let noisy_head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &noisy_head, "acceptable");
    fixture.write_triage(&noisy_head, &decisions);
    fixture.import(&noisy_head);

    // Тот же структурный класс в новом снимке: прошлая статистика «допустимо».
    let head = fixture.add_second_head();
    let patterns = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "patterns",
            "--detector",
            "error_path",
        ],
    );
    assert_eq!(patterns["result"]["abstained"], json!(false));
    let rules = patterns["result"]["rules"].as_array().unwrap();
    assert!(
        rules
            .iter()
            .any(|rule| rule["support"]["summary"]["acceptable_units"] == json!(3)),
        "ожидался низкошумный исторический паттерн: {rules:?}"
    );

    let recommended = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
        ],
    );
    let items = recommended["result"]["recommendations"].as_array().unwrap();
    let mut guardrailed = 0;
    for item in items {
        assert_ne!(
            item["suggested_position"],
            json!("last"),
            "историческая статистика не понижает обязательность: {item}"
        );
        assert_ne!(
            item["granularity"],
            json!("skip"),
            "безопасного пропуска не существует: {item}"
        );
        if item["queue_priority"] == json!("high") {
            guardrailed += 1;
            let reason = item["reason"].as_str().unwrap_or_default();
            let limitations: Vec<&str> = item["limitations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap_or_default())
                .collect();
            assert!(
                reason.contains("guardrails")
                    || limitations
                        .iter()
                        .any(|text| text.contains("не понижает обязательность")),
                "единица высокого приоритета обязана нести guardrails: {item}"
            );
        }
    }
    assert!(
        guardrailed >= 3,
        "синтетика обязана дать высокоприоритетные runtime-единицы: {items:?}"
    );

    // Обычный просмотр очереди по-прежнему показывает все единицы.
    let listed = ok_json(
        &fixture,
        &[
            "code-review",
            "queue",
            "list",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--limit",
            "200",
        ],
    );
    let queue: Value = serde_json::from_slice(&read(&fixture.path().join(fixture.queue(&head))))
        .expect("очередь фикстуры");
    let expected = queue["units"].as_array().unwrap().len();
    assert_eq!(
        listed["result"]["units"].as_array().unwrap().len(),
        expected,
        "ни одна единица не исчезает из обычного просмотра"
    );
}

#[test]
fn feedback_and_policy_lifecycle_is_audited_and_never_auto_applied() {
    let fixture = Fixture::create("learning-cli-feedback");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);
    let record = fixture.import(&head);
    let review_id = record["record"]["review_id"].as_str().unwrap().to_owned();
    let candidate = fixture.candidate_ids(&head, "error_path")[0].clone();

    // Единица очереди, к которой относится наблюдение, берётся из истории.
    let stats = ok_json(&fixture, &["code-review", "learning", "stats"]);
    assert_eq!(stats["result"]["dispositions"]["confirmed"], json!(3));
    let unit_id = format!("individual-{candidate}");

    // Содержательная правка semantic outcome хранится аудируемо.
    let revised = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--candidate-id",
            &candidate,
            "--kind",
            "semantic-outcome-revision",
            "--disposition",
            "false-positive",
            "--explanation",
            "Синтетическая метка подтверждения оказалась ошибочной.\nУточнение во второй строке.",
        ],
    );
    let event_id = revised["result"]["event_id"].as_str().unwrap().to_owned();
    assert_eq!(
        revised["result"]["outcome"]["effective_disposition"],
        json!("false_positive")
    );

    // Повтор того же утверждения идемпотентен и не плодит дубликатов.
    let repeated = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--candidate-id",
            &candidate,
            "--kind",
            "semantic-outcome-revision",
            "--disposition",
            "false-positive",
            "--explanation",
            "Синтетическая метка подтверждения оказалась ошибочной.\nУточнение во второй строке.",
        ],
    );
    assert_eq!(repeated["result"]["event_id"], json!(event_id));
    let audit = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "list",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
        ],
    );
    assert_eq!(audit["result"]["events"].as_array().unwrap().len(), 1);

    // Противоречащее утверждение видно, а не разрешается молча.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--candidate-id",
            &candidate,
            "--kind",
            "semantic-outcome-revision",
            "--disposition",
            "acceptable",
            "--explanation",
            "Противоречащее утверждение.",
        ],
        "learning_conflict",
    );

    // Оценка полезности рекомендации отделена от содержательной правки.
    let usefulness = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--kind",
            "usefulness",
            "--usefulness",
            "partially-useful",
            "--explanation",
            "Подсказка помогла частично.",
        ],
    );
    assert_eq!(
        usefulness["result"]["outcome"]["effective_disposition"],
        json!("false_positive"),
        "оценка полезности не меняет семантический исход"
    );

    // Пересчёт паттернов учитывает действующие правки.
    let patterns = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "patterns",
            "--detector",
            "error_path",
        ],
    );
    assert!(
        patterns["result"]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| rule["support"]["summary"]["false_positive_units"] == json!(1)),
        "правка обязана влиять на пересчёт: {patterns}"
    );

    // Отзыв ошибочного утверждения убирает его влияние, а исход возвращается
    // к исходному решению ревьюера.
    let retracted = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--candidate-id",
            &candidate,
            "--kind",
            "semantic-outcome-revision",
            "--action",
            "retract",
            "--supersedes-event-id",
            &event_id,
            "--explanation",
            "Ошибочная правка отозвана.",
        ],
    );
    assert_eq!(
        retracted["result"]["outcome"]["effective_disposition"],
        json!(null)
    );
    assert_eq!(
        retracted["result"]["outcome"]["original_disposition"],
        json!("confirmed")
    );
    let patterns = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "patterns",
            "--detector",
            "error_path",
        ],
    );
    assert!(
        patterns["result"]["rules"].as_array().unwrap().iter().any(
            |rule| rule["support"]["summary"]["confirmed_units"] == json!(3)
                && rule["support"]["summary"]["false_positive_units"] == json!(0)
        ),
        "отозванная правка не должна влиять на пересчёт: {patterns}"
    );

    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "show",
            "--event-id",
            "event-that-does-not-exist",
        ],
        "not_found",
    );
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            "individual-unknown",
            "--kind",
            "usefulness",
            "--usefulness",
            "useful",
            "--explanation",
            "Неизвестный случай.",
        ],
        "not_found",
    );

    // Жизненный цикл предложения политики: без автопромоции и автоприменения.
    let supported = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "patterns",
            "--require-supported",
            "--detector",
            "error_path",
        ],
    );
    let signature = supported["result"]["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|rule| rule["support"]["summary"]["level"] == json!("supported"))
        .map(|rule| rule["signature"].as_str().unwrap().to_owned())
        .unwrap_or_else(|| panic!("нужен подтверждённый паттерн: {supported}"));

    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "policy",
            "propose",
            "--signature",
            "signature-that-does-not-exist",
            "--rule-id",
            "rule-unknown",
        ],
        "not_found",
    );

    let proposal = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "policy",
            "propose",
            "--signature",
            &signature,
            "--rule-id",
            "rule-runtime-error-path",
        ],
    );
    assert_eq!(proposal["result"]["auto_applied"], json!(false));
    let proposal_id = proposal["result"]["proposal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        proposal["result"]["cautions"].as_array().unwrap().len() >= 2,
        "предложение обязано перечислять ограничения: {proposal}"
    );

    let listed = ok_json(&fixture, &["code-review", "learning", "policy", "list"]);
    assert_eq!(listed["result"]["proposals"].as_array().unwrap().len(), 1);
    let shown = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "policy",
            "show",
            "--id",
            &proposal_id,
        ],
    );
    assert_eq!(shown["result"]["proposal_id"], json!(proposal_id));

    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "policy",
            "approve",
            "--id",
            "proposal-that-does-not-exist",
            "--out",
            ".anki-repo/policy/code-review.json",
        ],
        "not_found",
    );

    let approved = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "policy",
            "approve",
            "--id",
            &proposal_id,
            "--out",
            ".anki-repo/policy/code-review.json",
        ],
    );
    assert_eq!(approved["result"]["auto_applied"], json!(false));
    assert_eq!(approved["result"]["artifact"]["created"], json!(true));
    let artifact: Value = serde_json::from_slice(&read(
        &fixture.path().join(".anki-repo/policy/code-review.json"),
    ))
    .expect("утверждённый артефакт политики");
    assert_eq!(artifact["approved"], json!(true));
    assert_eq!(artifact["auto_applied"], json!(false));
    assert_eq!(artifact["proposal"]["proposal_id"], json!(proposal_id));

    // Повтор утверждения идемпотентен, а база не становится владельцем политики.
    let repeated_approval = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "policy",
            "approve",
            "--id",
            &proposal_id,
            "--out",
            ".anki-repo/policy/code-review.json",
        ],
    );
    assert_eq!(
        repeated_approval["result"]["artifact"]["created"],
        json!(false)
    );

    // Повтор уже отозванного содержания требует нового event_id вместо ложного noop.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--candidate-id",
            &candidate,
            "--kind",
            "semantic-outcome-revision",
            "--disposition",
            "false-positive",
            "--explanation",
            "Синтетическая метка подтверждения оказалась ошибочной.\nУточнение во второй строке.",
        ],
        "learning_conflict",
    );

    // Повтор утверждения, которое заменили через supersede, тоже не должен быть noop.
    let appended = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--candidate-id",
            &candidate,
            "--kind",
            "semantic-outcome-revision",
            "--disposition",
            "acceptable",
            "--explanation",
            "Новое содержательное утверждение.",
        ],
    );
    let appended_event_id = appended["result"]["event_id"].as_str().unwrap();
    ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--candidate-id",
            &candidate,
            "--kind",
            "semantic-outcome-revision",
            "--action",
            "supersede",
            "--supersedes-event-id",
            appended_event_id,
            "--disposition",
            "uncertain",
            "--explanation",
            "Уточнённое утверждение заменяет прежнее.",
        ],
    );
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--candidate-id",
            &candidate,
            "--kind",
            "semantic-outcome-revision",
            "--disposition",
            "acceptable",
            "--explanation",
            "Новое содержательное утверждение.",
        ],
        "learning_conflict",
    );
}

#[test]
fn export_backup_restore_roundtrip_and_foreign_files_are_protected() {
    let fixture = Fixture::create("learning-cli-transfer");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);
    fixture.import(&head);

    let archive = ".anki-repo/learning/exports/archive.json";
    let exported = ok_json(
        &fixture,
        &["code-review", "learning", "export", "--out", archive],
    );
    assert_eq!(exported["result"]["artifact"]["created"], json!(true));
    assert_eq!(exported["result"]["manifest"]["reviews"], json!(1));
    let repeated = ok_json(
        &fixture,
        &["code-review", "learning", "export", "--out", archive],
    );
    assert_eq!(repeated["result"]["artifact"]["created"], json!(false));

    // Служебные каталоги репозитория для артефактов learning закрыты.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "export",
            "--out",
            "decks/archive.json",
        ],
        "invalid_request",
    );

    // Транзакционный backup не перезаписывает существующий файл.
    let backup = ".anki-repo/learning/backups/state.sqlite";
    let backed_up = ok_json(
        &fixture,
        &["code-review", "learning", "backup", "--out", backup],
    );
    assert!(backed_up["result"]["bytes"].as_u64().unwrap() > 0);
    assert_error(
        &fixture,
        &["code-review", "learning", "backup", "--out", backup],
        "review_artifact_conflict",
    );

    // Восстановление в другую базу повторяет проверку digest архива.
    let restored = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "restore",
            "--db",
            ".anki-repo/learning/restored.sqlite",
            "--archive",
            archive,
        ],
    );
    assert_eq!(restored["result"]["summary"]["restored_reviews"], json!(1));
    let stats = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "stats",
            "--db",
            ".anki-repo/learning/restored.sqlite",
        ],
    );
    assert_eq!(stats["result"]["reviews_total"], json!(1));

    // Повторное восстановление идемпотентно и не удваивает историю.
    let again = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "restore",
            "--db",
            ".anki-repo/learning/restored.sqlite",
            "--archive",
            archive,
        ],
    );
    assert_eq!(again["result"]["summary"]["restored_reviews"], json!(0));
    assert_eq!(again["result"]["summary"]["unchanged_reviews"], json!(1));

    fs::write(fixture.path().join("broken-archive.json"), b"not json").unwrap();
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "restore",
            "--db",
            ".anki-repo/learning/other.sqlite",
            "--archive",
            "broken-archive.json",
        ],
        "learning_export_invalid",
    );
    assert!(
        !fixture
            .path()
            .join(".anki-repo/learning/other.sqlite")
            .exists(),
        "нечитаемый архив не создаёт базу learning"
    );

    let mut invalid_digest: Value = serde_json::from_slice(&read(&fixture.path().join(archive)))
        .expect("экспортированный архив должен быть JSON");
    invalid_digest["manifest"]["payload_sha256"] = json!("0".repeat(64));
    fs::write(
        fixture.path().join("invalid-digest-archive.json"),
        serde_json::to_vec(&invalid_digest).unwrap(),
    )
    .unwrap();
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "restore",
            "--db",
            ".anki-repo/learning/invalid-digest.sqlite",
            "--archive",
            "invalid-digest-archive.json",
        ],
        "learning_export_invalid",
    );
    assert!(
        !fixture
            .path()
            .join(".anki-repo/learning/invalid-digest.sqlite")
            .exists(),
        "архив с неверным digest не создаёт базу learning"
    );

    // Чужой файл на пути архива не перезаписывается и остаётся нетронутым.
    fs::write(fixture.path().join(archive), b"{\"foreign\": true}").unwrap();
    assert_error(
        &fixture,
        &["code-review", "learning", "export", "--out", archive],
        "review_artifact_conflict",
    );
    assert_eq!(
        read(&fixture.path().join(archive)),
        b"{\"foreign\": true}".to_vec()
    );
}

#[test]
fn absent_empty_corrupt_and_unavailable_databases_do_not_break_review() {
    let fixture = Fixture::create("learning-cli-broken");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);

    let review_commands = |fixture: &Fixture| {
        let (code, _, _stderr) = cli(
            fixture,
            &[
                "code-review",
                "queue",
                "validate",
                "--pack",
                &fixture.pack(&head),
                "--queue",
                &fixture.queue(&head),
            ],
        );
        assert_eq!(code, 0, "queue validate должен проходить");
        let (code, _, _stderr) = cli(
            fixture,
            &[
                "code-review",
                "queue",
                "list",
                "--pack",
                &fixture.pack(&head),
                "--queue",
                &fixture.queue(&head),
            ],
        );
        assert_eq!(code, 0, "queue list должен проходить");
        let (code, _, _stderr) = cli(
            fixture,
            &[
                "code-review",
                "triage",
                "validate",
                "--pack",
                &fixture.pack(&head),
                "--triage",
                &fixture.triage(&head),
            ],
        );
        assert_eq!(code, 0, "triage validate должен проходить");
    };

    // Отсутствующая база: обычное ревью работает, каталог не создаётся.
    review_commands(&fixture);
    assert!(!fixture.path().join(".anki-repo/learning").exists());

    let learning_dir = fixture.path().join(".anki-repo/learning");
    fs::create_dir_all(&learning_dir).unwrap();

    // Произвольный пустой файл не удостоверяет формат learning и не меняется.
    let foreign_empty = learning_dir.join("foreign-empty.sqlite");
    fs::write(&foreign_empty, b"").unwrap();
    for command in ["status", "stats"] {
        assert_error(
            &fixture,
            &[
                "code-review",
                "learning",
                command,
                "--db",
                ".anki-repo/learning/foreign-empty.sqlite",
            ],
            "learning_corrupt",
        );
    }
    assert_eq!(read(&foreign_empty), b"");
    review_commands(&fixture);

    // Состояние корректно созданной пустой learning-базы остаётся доступно.
    let empty = learning_dir.join("empty.sqlite");
    drop(LearningStore::open(StoreOptions::at(&empty)).unwrap());
    let status = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "status",
            "--db",
            ".anki-repo/learning/empty.sqlite",
        ],
    );
    assert_eq!(status["result"]["present"], json!(true));
    let stats = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "stats",
            "--db",
            ".anki-repo/learning/empty.sqlite",
        ],
    );
    assert_eq!(stats["result"]["reviews_total"], json!(0));
    review_commands(&fixture);

    // Повреждённая база: различимый отказ и сохранённые исходные байты.
    let corrupt = learning_dir.join("corrupt.sqlite");
    fs::write(&corrupt, "это не база SQLite").unwrap();
    for command in ["status", "validate", "stats"] {
        assert_error(
            &fixture,
            &[
                "code-review",
                "learning",
                command,
                "--db",
                ".anki-repo/learning/corrupt.sqlite",
            ],
            "learning_corrupt",
        );
    }
    assert_eq!(
        read(&corrupt),
        "это не база SQLite".as_bytes().to_vec(),
        "повреждённая база не удаляется"
    );
    review_commands(&fixture);

    // Более новая схема: `learning_schema_unsupported` без разрушения данных.
    let future = learning_dir.join("future.sqlite");
    {
        let connection = rusqlite::Connection::open(&future).unwrap();
        connection
            .pragma_update(None, "user_version", 99u32)
            .unwrap();
        connection
            .execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        connection
            .execute("INSERT INTO marker (value) VALUES ('preserved')", [])
            .unwrap();
    }
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "status",
            "--db",
            ".anki-repo/learning/future.sqlite",
        ],
        "learning_schema_unsupported",
    );
    review_commands(&fixture);

    // Недоступная база: запись невозможна, обычное ревью продолжает работать.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--db",
            "src/lib.rs/unavailable.sqlite",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
        ],
        "learning_storage_unavailable",
    );
    review_commands(&fixture);

    // Путь базы под обычным файлом не может стать каталогом ни под root,
    // ни при иных режимах доступа.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--db",
            "src/lib.rs/unavailable.sqlite",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
        ],
        "learning_storage_unavailable",
    );
    review_commands(&fixture);

    // Символическая ссылка на служебный каталог не подменяет путь базы.
    fs::create_dir_all(fixture.path().join("decks")).unwrap();
    symlink("decks", &fixture.path().join("link-decks"));
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--db",
            "link-decks/state.sqlite",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
        ],
        "invalid_request",
    );
    assert!(!fixture.path().join("decks/state.sqlite").exists());
    review_commands(&fixture);
}

#[test]
fn sqlite_lock_contention_is_temporary_and_distinguishable() {
    let fixture = Fixture::create("learning-cli-busy");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);
    let record = fixture.import(&head);
    let review_id = record["record"]["review_id"].as_str().unwrap().to_owned();
    let candidate = fixture.candidate_ids(&head, "error_path")[0].clone();
    let unit_id = format!("individual-{candidate}");

    // Удержанная чужая транзакция записи — временная занятость, а не конфликт.
    let database = fixture.path().join(".anki-repo/learning/state.sqlite");
    let holder = rusqlite::Connection::open(&database).unwrap();
    holder
        .busy_timeout(std::time::Duration::from_millis(0))
        .unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (code, value, _stderr) = cli_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--candidate-id",
            &candidate,
            "--kind",
            "usefulness",
            "--usefulness",
            "useful",
            "--explanation",
            "Проверка временной занятости базы.",
        ],
    );
    assert_eq!(code, 13, "занятость базы обязана иметь отдельный код");
    assert_eq!(error_code(&value), "learning_storage_busy");
    assert_eq!(value["error"]["details"]["retryable"], json!(true));
    assert_eq!(value["error"]["details"]["resource"], json!("learning"));
    holder.execute_batch("ROLLBACK").unwrap();
    drop(holder);

    // После снятия удержания та же самая команда проходит.
    let accepted = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--candidate-id",
            &candidate,
            "--kind",
            "usefulness",
            "--usefulness",
            "useful",
            "--explanation",
            "Проверка временной занятости базы.",
        ],
    );
    assert_eq!(
        accepted["result"]["outcome"]["original_disposition"],
        json!("confirmed")
    );
    assert_eq!(
        accepted["result"]["outcome"]["effective_disposition"],
        json!(null),
        "оценка полезности не меняет семантический исход случая"
    );
}

#[test]
fn restore_returns_the_same_search_cases_and_stays_honest_for_old_archives() {
    let fixture = Fixture::create("learning-cli-restore-search");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);
    fixture.import(&head);

    let archive = ".anki-repo/learning/exports/archive.json";
    let exported = ok_json(
        &fixture,
        &["code-review", "learning", "export", "--out", archive],
    );
    assert!(
        exported["result"]["manifest"]["search_cases"]
            .as_u64()
            .unwrap()
            > 0,
        "поисковые случаи обязаны входить в архив: {exported}"
    );

    // Поиск до переноса: текстовый и по решению.
    let before_text = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "search",
            "--text",
            "подтверждённый дефект",
            "--limit",
            "20",
        ],
    );
    let before_decisions = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "search",
            "--disposition",
            "confirmed",
            "--limit",
            "20",
        ],
    );
    let before_ids = case_ids(&before_text);
    assert!(!before_ids.is_empty(), "поиск обязан находить случаи");
    assert!(
        kinds(&before_text).iter().any(|kind| kind == "finding"),
        "текстовый поиск находит случаи замечаний: {before_text}"
    );
    assert!(
        kinds(&before_decisions)
            .iter()
            .any(|kind| kind == "decision"),
        "поиск по решению находит случаи решений: {before_decisions}"
    );

    let restored_db = ".anki-repo/learning/restored.sqlite";
    let restored = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "restore",
            "--db",
            restored_db,
            "--archive",
            archive,
        ],
    );
    assert!(
        restored["result"]["summary"]["limitations"]
            .as_array()
            .expect("ограничения восстановления")
            .iter()
            .any(|limitation| limitation
                .as_str()
                .is_some_and(|text| text.contains("переносятся вместе с архивом"))),
        "архив второй версии обязан честно описывать перенос поиска: {restored}"
    );

    // Тот же поиск после восстановления в чистую базу.
    let after_text = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "search",
            "--db",
            restored_db,
            "--text",
            "подтверждённый дефект",
            "--limit",
            "20",
        ],
    );
    let after_decisions = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "search",
            "--db",
            restored_db,
            "--disposition",
            "confirmed",
            "--limit",
            "20",
        ],
    );
    assert_eq!(
        case_ids(&after_text),
        before_ids,
        "восстановленный поиск обязан находить те же случаи"
    );
    assert_eq!(case_ids(&after_decisions), case_ids(&before_decisions));

    // Повторное восстановление не удваивает ни строки поиска, ни индекс FTS5.
    let counts = search_row_counts(&fixture.path().join(restored_db));
    ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "restore",
            "--db",
            restored_db,
            "--archive",
            archive,
        ],
    );
    assert_eq!(
        search_row_counts(&fixture.path().join(restored_db)),
        counts,
        "повторное восстановление не удваивает поисковые строки и строки индекса"
    );

    // Архив первой версии не роняет восстановление и не обещает ложного.
    let old_archive = ".anki-repo/learning/exports/archive-v1.json";
    let plain = Fixture::create("learning-cli-restore-search-v1");
    ok_json(
        &plain,
        &[
            "code-review",
            "learning",
            "import",
            "--pack",
            &plain.pack(&plain.head_sha),
            "--queue",
            &plain.queue(&plain.head_sha),
        ],
    );
    ok_json(
        &plain,
        &["code-review", "learning", "export", "--out", old_archive],
    );
    let mut document: Value =
        serde_json::from_slice(&fs::read(plain.path().join(old_archive)).unwrap()).unwrap();
    assert_eq!(
        document["search"],
        json!([]),
        "импорт без разбора не создаёт поисковых случаев"
    );
    document["manifest"]["export_schema_version"] = json!(1);
    document["manifest"]
        .as_object_mut()
        .unwrap()
        .remove("search_cases");
    document.as_object_mut().unwrap().remove("search");
    document.as_object_mut().unwrap().remove("decisions");
    let legacy: anki_repo::code_review::learning::model::LearningExport =
        serde_json::from_value(document.clone()).expect("legacy v1 archive shape");
    document["manifest"]["payload_sha256"] = json!(legacy_archive_digest(&legacy));
    fs::write(
        plain.path().join(old_archive),
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();
    let restored = ok_json(
        &plain,
        &[
            "code-review",
            "learning",
            "restore",
            "--db",
            ".anki-repo/learning/restored.sqlite",
            "--archive",
            old_archive,
        ],
    );
    assert_eq!(restored["result"]["summary"]["restored_reviews"], json!(1));
    let limitations = restored["result"]["summary"]["limitations"]
        .as_array()
        .expect("ограничения восстановления")
        .iter()
        .filter_map(|limitation| limitation.as_str())
        .collect::<Vec<_>>();
    assert!(
        limitations
            .iter()
            .any(|text| text.contains("в архив этой версии не входят")),
        "старый архив обязан честно сообщать, что поиска в нём нет: {limitations:?}"
    );
    assert!(
        !limitations
            .iter()
            .any(|text| text.contains("при следующем поиске")),
        "ложное обещание пересборки индекса обязано исчезнуть: {limitations:?}"
    );
}

/// Идентификаторы найденных случаев в детерминированном порядке.
fn case_ids(page: &Value) -> Vec<String> {
    let mut ids = page["result"]["cases"]
        .as_array()
        .expect("страница поиска")
        .iter()
        .map(|case| {
            case["case_id"]
                .as_str()
                .expect("у случая есть идентификатор")
                .to_owned()
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

/// Виды найденных случаев.
fn kinds(page: &Value) -> Vec<String> {
    page["result"]["cases"]
        .as_array()
        .expect("страница поиска")
        .iter()
        .filter_map(|case| case["kind"].as_str().map(str::to_owned))
        .collect()
}

/// Число строк поиска и строк FTS5-индекса в базе.
fn search_row_counts(path: &Path) -> (i64, i64) {
    let connection = rusqlite::Connection::open(path).expect("база восстановления открывается");
    let search: i64 = connection
        .query_row("SELECT COUNT(*) FROM learning_search", [], |row| row.get(0))
        .expect("таблица поиска существует");
    let fts: i64 = connection
        .query_row("SELECT COUNT(*) FROM learning_search_fts", [], |row| {
            row.get(0)
        })
        .expect("таблица индекса существует");
    (search, fts)
}

#[test]
fn symlinked_paths_never_redirect_learning_artifacts() {
    let fixture = Fixture::create("learning-cli-symlink");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);
    fixture.import(&head);

    // Служебные каталоги репозитория и каталог вне клона доступны по ссылкам.
    fs::create_dir_all(fixture.path().join("decks")).unwrap();
    symlink("decks", &fixture.path().join("link-decks"));
    symlink(".git", &fixture.path().join("link-git"));
    let outside = TempDir::new("learning-cli-symlink-outside");
    symlink(outside.path(), &fixture.path().join("link-outside"));

    // Символическая ссылка на decks/** не обходит запрет служебного каталога.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "export",
            "--out",
            "link-decks/archive.json",
        ],
        "invalid_request",
    );
    assert!(!fixture.path().join("decks/archive.json").exists());

    // Символическая ссылка на служебные данные Git тоже закрыта.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "export",
            "--out",
            "link-git/archive.json",
        ],
        "invalid_request",
    );
    assert!(!fixture.path().join(".git/archive.json").exists());

    // Снимок базы следует той же политике путей, что и архив.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "backup",
            "--out",
            "link-decks/state.sqlite",
        ],
        "invalid_request",
    );
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "backup",
            "--out",
            "link-git/state.sqlite",
        ],
        "invalid_request",
    );

    // Ссылка за пределы клона не уводит артефакт наружу.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "export",
            "--out",
            "link-outside/sub/archive.json",
        ],
        "invalid_request",
    );
    assert!(!outside.path().join("sub/archive.json").exists());

    // Путь вывода рекомендаций ограничен своей рабочей областью: ни ссылка на
    // служебный каталог, ни ссылка на рабочую область не подменяют цель.
    fs::create_dir_all(fixture.workspace(&head)).unwrap();
    symlink(
        fixture.path().join("decks"),
        &fixture.workspace(&head).join("link-decks"),
    );
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
            "--out",
            &format!(
                "{}/link-decks/recommendations.json",
                fixture.workspace(&head).display()
            ),
        ],
        "invalid_request",
    );
    symlink(
        fixture.workspace(&head),
        &fixture.path().join("link-workspace"),
    );
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
            "--out",
            "link-workspace/recommendations.json",
        ],
        "invalid_request",
    );

    // Символическая ссылка вместо самого файла рекомендаций запрещена.
    symlink(
        fixture.path().join("foreign.json"),
        &fixture.workspace(&head).join("recommendations.json"),
    );
    // Символическая ссылка вместо самого файла — конфликт артефакта: так его
    // классифицирует канонический владелец политики путей.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
            "--out",
            &fixture.artifact(&head, "recommendations.json"),
        ],
        "review_artifact_conflict",
    );
    assert!(!fixture.path().join("foreign.json").exists());
}

/// Создаёт символическую ссылку в синтетическом репозитории.
fn symlink(target: impl AsRef<Path>, link: &Path) {
    std::os::unix::fs::symlink(target.as_ref(), link).expect("символическая ссылка фикстуры");
}

#[test]
fn snapshot_variant_publishes_into_its_own_workspace() {
    let fixture = Fixture::create("learning-cli-snapshot");
    let head = fixture.head_sha.clone();
    let snapshot_relative = format!(
        "{}/snapshot-0123456789abcdef0123456789abcdef",
        fixture.artifact(&head, "").trim_end_matches('/')
    );
    let snapshot = format!(
        "{}/snapshot-0123456789abcdef0123456789abcdef",
        fixture.workspace(&head).display()
    );
    let (code, _, _stderr) = cli(
        &fixture,
        &[
            "code-review",
            "collect",
            "--base",
            &fixture.base_sha,
            "--head",
            &head,
            "--out-dir",
            &snapshot,
        ],
    );
    assert_eq!(code, 0, "снимок варианта обязан собираться");
    assert!(fixture.path().join(&snapshot).join("review.json").is_file());

    // Проверенный разбор и импорт истории снимка.
    let pack = format!("{snapshot}/review.json");
    let queue = format!("{snapshot}/review-queue.json");
    let input = format!("{snapshot}/semantic-triage.input.json");
    let triage = format!("{snapshot}/semantic-triage.json");
    let (code, _, _stderr) = cli(
        &fixture,
        &[
            "code-review",
            "triage",
            "init",
            "--pack",
            &pack,
            "--out",
            &input,
        ],
    );
    assert_eq!(code, 0, "triage init в снимке обязан проходить");
    let mut document: Value =
        serde_json::from_slice(&fs::read(fixture.path().join(&input)).unwrap()).unwrap();
    let candidates = document["unreviewed_candidate_ids"]
        .as_array()
        .expect("нерассмотренные кандидаты снимка")
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    for candidate in &candidates {
        decide(&mut document, candidate, "confirmed");
    }
    fs::write(
        fixture.path().join(&input),
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();
    let (code, _, _stderr) = cli(
        &fixture,
        &[
            "code-review",
            "triage",
            "validate",
            "--pack",
            &pack,
            "--triage",
            &input,
            "--canonical-out",
            &triage,
        ],
    );
    assert_eq!(code, 0, "triage validate в снимке обязан проходить");
    ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--pack",
            &pack,
            "--queue",
            &queue,
            "--triage",
            &triage,
        ],
    );

    // Рекомендации публикуются ровно в рабочую область снимка.
    let published = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &pack,
            "--queue",
            &queue,
            "--triage",
            &triage,
            "--out",
            &format!("{snapshot}/recommendations.json"),
        ],
    );
    assert_eq!(
        published["result"]["artifact"]["path"],
        json!(format!("{snapshot_relative}/recommendations.json"))
    );
    assert_eq!(published["result"]["artifact"]["created"], json!(true));

    // Каноническая рабочая область того же HEAD — чужая для этого пакета.
    assert_error(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &pack,
            "--queue",
            &queue,
            "--triage",
            &triage,
            "--out",
            &fixture.artifact(&head, "recommendations.json"),
        ],
        "invalid_request",
    );
    assert!(
        !fixture
            .workspace(&head)
            .join("recommendations.json")
            .exists()
    );
}

#[test]
fn repeated_variants_of_one_snapshot_are_one_observation_line() {
    let fixture = Fixture::create("learning-cli-variants");
    // Срез ровно из одной единицы: так повторный импорт одного снимка виден
    // напрямую, без вклада других единиц того же среза.
    write_sources(fixture.path(), SINGLE_PRODUCTION, HEAD_TESTS);
    commit_all(fixture.path(), "Синтетическая правка с одним кандидатом");
    let head = git(fixture.path(), &["rev-parse", "HEAD"]);
    fixture.collect(&fixture.base_sha, &head);
    assert_eq!(fixture.candidate_ids(&head, "error_path").len(), 1);
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);

    // Один и тот же неизменный снимок импортируется четыре раза: различается
    // только вариант источника. Это повторные чтения одного случая, а не четыре
    // независимых наблюдения.
    let variants = [
        "root",
        "snapshot-0123456789abcdef0123456789abcdef",
        "snapshot-fedcba9876543210fedcba9876543210",
        "snapshot-00112233445566778899aabbccddeeff",
    ];
    let mut review_ids = Vec::new();
    for variant in variants {
        let record = ok_json(
            &fixture,
            &[
                "code-review",
                "learning",
                "import",
                "--pack",
                &fixture.pack(&head),
                "--queue",
                &fixture.queue(&head),
                "--triage",
                &fixture.triage(&head),
                "--variant",
                variant,
            ],
        );
        review_ids.push(
            record["result"]["record"]["review_id"]
                .as_str()
                .expect("запись истории")
                .to_owned(),
        );
    }
    assert_eq!(
        review_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        4,
        "вариант источника даёт отдельные записи истории: {review_ids:?}"
    );

    let patterns = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "patterns",
            "--detector",
            "error_path",
        ],
    );
    let rule = &patterns["result"]["rules"][0]["support"]["summary"];
    assert_eq!(
        rule["support_units"],
        json!(1),
        "повторные варианты одного снимка не набирают независимую поддержку: {rule}"
    );
    assert_eq!(rule["support_reviews"], json!(4));
    assert_eq!(rule["revised_units"], json!(3));
    assert_eq!(
        rule["level"],
        json!("insufficient_evidence"),
        "одно независимое наблюдение не достигает порога поддержки: {rule}"
    );
    assert_eq!(rule["confirmed_share_lower_bound"], json!(null));
    assert_eq!(
        rule["confirmed_units"],
        json!(1),
        "действует одно наблюдение линии"
    );
    assert_eq!(rule["confirmed_findings"], json!(1));
    assert_eq!(rule["false_positive_units"], json!(0));
}

#[test]
fn search_hides_quarantine_until_explicitly_requested() {
    let fixture = Fixture::create("learning-cli-search-trust");
    let head = fixture.head_sha.clone();
    let decisions = error_path_decisions(&fixture, &head, "confirmed");
    fixture.write_triage(&head, &decisions);
    fixture.import(&head);

    // Тот же снимок импортируется ослабленно и помечается карантином: записи
    // лежат в одной базе и различаются только уровнем доверия.
    let quarantined = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "import",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
            "--structure-only",
            "--variant",
            "snapshot-0123456789abcdef0123456789abcdef",
        ],
    );
    assert_eq!(quarantined["result"]["status"], json!("quarantined"));
    assert_eq!(
        quarantined["result"]["record"]["trust"],
        json!("structure_only_quarantine")
    );

    // По умолчанию карантин в результаты поиска не попадает.
    let trusted_page = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "search",
            "--detector",
            "error_path",
            "--limit",
            "200",
        ],
    );
    assert_eq!(
        trusted_page["result"]["include_quarantine"],
        json!(false),
        "машинный JSON обязан сообщать фактический фильтр"
    );
    let cases = trusted_page["result"]["cases"].as_array().unwrap();
    assert!(!cases.is_empty(), "доверенные случаи обязаны находиться");
    assert!(
        cases
            .iter()
            .all(|case| case["trust"] == json!("ast_authenticated")),
        "карантин не возвращается без явного согласия: {cases:?}"
    );

    // Явный флаг возвращает карантин, не подменяя доверенные случаи.
    let with_quarantine = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "search",
            "--detector",
            "error_path",
            "--include-quarantine",
            "--limit",
            "200",
        ],
    );
    assert_eq!(with_quarantine["result"]["include_quarantine"], json!(true));
    assert!(
        with_quarantine["result"]["matched"].as_u64().unwrap()
            > trusted_page["result"]["matched"].as_u64().unwrap(),
        "с флагом карантинные случаи обязаны возвращаться"
    );
    assert!(
        with_quarantine["result"]["cases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|case| case["trust"] == json!("structure_only_quarantine")),
        "карантинная запись обязана быть видна: {}",
        with_quarantine["result"]["cases"]
    );

    // Человеческий вывод не выдаёт карантин за обычную историю.
    let (code, human, _stderr) = cli(
        &fixture,
        &[
            "code-review",
            "learning",
            "search",
            "--detector",
            "error_path",
            "--limit",
            "200",
        ],
    );
    assert_eq!(code, 0, "человекочитаемый поиск должен проходить");
    assert!(
        human.contains("Карантин: исключён по умолчанию"),
        "фильтр обязан быть назван в выводе: {human}"
    );
    assert!(
        !human.contains("structure_only_quarantine"),
        "карантин не показывается как обычная история: {human}"
    );
    assert!(
        human.contains("доверие: ast_authenticated — проверенная история"),
        "доверие возвращённого случая обязано быть видно: {human}"
    );

    let (code, flagged, _stderr) = cli(
        &fixture,
        &[
            "code-review",
            "learning",
            "search",
            "--detector",
            "error_path",
            "--include-quarantine",
            "--limit",
            "200",
        ],
    );
    assert_eq!(code, 0, "человекочитаемый поиск с флагом должен проходить");
    assert!(
        flagged.contains("Карантин: включён по явному --include-quarantine"),
        "явное согласие обязано быть названо: {flagged}"
    );
    assert!(
        flagged.contains("доверие: structure_only_quarantine — карантин, не полноценное основание"),
        "карантинная запись обязана быть помечена: {flagged}"
    );

    // Псевдоним `similar` — та же команда с тем же фильтром доверия.
    let alias = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "similar",
            "--detector",
            "error_path",
            "--limit",
            "200",
        ],
    );
    assert_eq!(
        alias["result"]["matched"], trusted_page["result"]["matched"],
        "псевдоним обязан соблюдать тот же фильтр"
    );
}

#[test]
fn complete_learning_lifecycle_preserves_decisions_feedback_and_independent_findings() {
    let fixture = Fixture::create("learning-lifecycle-transfer");
    let head = fixture.head_sha.clone();
    let candidate = fixture.candidate_ids(&head, "error_path")[0].clone();
    fixture.write_triage(&head, &[(candidate.clone(), "confirmed")]);

    // Независимая находка имеет место и обоснование в собственном evidence,
    // но не получает выдуманную связь с одним из кандидатов очереди.
    let triage_input = fixture.workspace(&head).join("semantic-triage.input.json");
    let mut triage: Value = serde_json::from_slice(&read(&triage_input)).unwrap();
    triage["findings"].as_array_mut().unwrap().push(json!({
        "id": "finding-independent",
        "severity": "major",
        "title": "Независимая ошибка на границе разбора",
        "description": "src/lib.rs:2 — значение теряется при ошибке разбора; место и краткое основание проверены по исходнику.",
        "provenance": "independent",
        "candidate_ids": [],
    }));
    fs::write(&triage_input, serde_json::to_vec_pretty(&triage).unwrap()).unwrap();
    let (code, _, stderr) = cli(
        &fixture,
        &[
            "code-review",
            "triage",
            "validate",
            "--pack",
            &fixture.pack(&head),
            "--triage",
            &fixture.artifact(&head, "semantic-triage.input.json"),
            "--canonical-out",
            &fixture.triage(&head),
        ],
    );
    assert_eq!(code, 0, "triage с independent finding проходит: {stderr}");

    // Навигация показывает очередь, а semantic summary отдельно показывает,
    // что рассмотрен только один кандидат и остальные явно остались открыты.
    let queue: Value =
        serde_json::from_slice(&read(&fixture.path().join(fixture.queue(&head)))).unwrap();
    assert!(!queue["units"].as_array().unwrap().is_empty());
    let triage_summary = ok_json(
        &fixture,
        &[
            "code-review",
            "triage",
            "summary",
            "--pack",
            &fixture.pack(&head),
            "--triage",
            &fixture.triage(&head),
        ],
    );
    let summary = &triage_summary["result"];
    assert_eq!(summary["reviewed_candidates"], json!(1));
    assert!(summary["unreviewed_candidates"].as_u64().unwrap() > 0);
    assert_eq!(
        summary["findings"]["by_provenance"]["independent"],
        json!(1)
    );

    let imported = fixture.import(&head);
    assert_eq!(imported["status"], json!("imported"));
    assert_eq!(
        imported["record"]["observations"]["individual_decisions"],
        json!(1)
    );
    assert_eq!(
        imported["record"]["observations"]["findings_independent"],
        json!(1)
    );
    let review_id = imported["record"]["review_id"].as_str().unwrap().to_owned();

    let queue_unit = queue["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| {
            unit["members"]["candidate_id"].as_str() == Some(candidate.as_str())
                || unit["members"]["candidate_ids"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(candidate.as_str())))
        })
        .expect("кандидат принадлежит ровно одной единице очереди");
    let unit_id = queue_unit["id"].as_str().unwrap().to_owned();

    let search = |database: Option<&str>,
                  text: &str,
                  disposition: Option<&str>,
                  provenance: Option<&str>| {
        let mut args = vec!["code-review", "learning", "search"];
        if let Some(database) = database {
            args.extend(["--db", database]);
        }
        args.extend(["--text", text]);
        if let Some(disposition) = disposition {
            args.extend(["--disposition", disposition]);
        }
        if let Some(provenance) = provenance {
            args.extend(["--provenance", provenance]);
        }
        args.extend(["--limit", "20"]);
        ok_json(&fixture, &args)
    };

    let decision_text = "Синтетическое решение для контракта CLI learning.";
    let before_revision = search(None, decision_text, Some("confirmed"), None);
    assert_eq!(before_revision["result"]["matched"], json!(1));
    let independent_before = search(None, "src/lib.rs:2", None, Some("independent"));
    assert_eq!(independent_before["result"]["matched"], json!(1));
    let independent_case = &independent_before["result"]["cases"][0];
    assert_eq!(independent_case["finding_id"], json!("finding-independent"));
    assert_eq!(independent_case["candidate_id"], Value::Null);
    assert_eq!(independent_case["provenance"], json!("independent"));
    assert!(
        independent_case["snippet"]
            .as_str()
            .unwrap()
            .contains("src/lib.rs:2")
    );

    // Feedback меняет effective disposition, сохраняя исходное решение и audit event.
    let correction = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "feedback",
            "record",
            "--review-id",
            &review_id,
            "--unit-id",
            &unit_id,
            "--candidate-id",
            &candidate,
            "--kind",
            "semantic-outcome-revision",
            "--disposition",
            "false-positive",
            "--explanation",
            "Повторно проверена синтетическая метка.",
        ],
    );
    assert_eq!(
        correction["result"]["outcome"]["original_disposition"],
        json!("confirmed")
    );
    assert_eq!(
        correction["result"]["outcome"]["effective_disposition"],
        json!("false_positive")
    );
    assert_eq!(
        search(None, decision_text, Some("confirmed"), None)["result"]["matched"],
        json!(0)
    );
    assert_eq!(
        search(None, decision_text, Some("false-positive"), None)["result"]["matched"],
        json!(1)
    );

    let source_stats = ok_json(&fixture, &["code-review", "learning", "stats"]);
    assert_eq!(
        source_stats["result"]["observations"]["findings_independent"],
        json!(1)
    );
    let history_revision = source_stats["result"]["generation"]["revision"]
        .as_u64()
        .unwrap()
        .to_string();
    let source_patterns = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "patterns",
            "--detector",
            "error_path",
        ],
    );
    let source_recommendations = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
            "--history-revision",
            &history_revision,
            "--require-history",
        ],
    );

    let archive_path = ".anki-repo/learning/exports/full-lifecycle.json";
    let exported = ok_json(
        &fixture,
        &["code-review", "learning", "export", "--out", archive_path],
    );
    assert_eq!(
        exported["result"]["manifest"]["export_schema_version"],
        json!(anki_repo::code_review::learning::transfer::EXPORT_SCHEMA_VERSION)
    );
    let archive: Value = serde_json::from_slice(&read(&fixture.path().join(archive_path))).unwrap();
    assert_eq!(archive["decisions"].as_array().unwrap().len(), 1);
    assert_eq!(archive["feedback_events"].as_array().unwrap().len(), 1);
    assert!(
        archive["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| {
                finding["finding_id"] == json!("finding-independent")
                    && finding["provenance"] == json!("independent")
            })
    );

    let restored_db = ".anki-repo/learning/restored-lifecycle.sqlite";
    let restored = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "restore",
            "--db",
            restored_db,
            "--archive",
            archive_path,
        ],
    );
    assert_eq!(restored["result"]["summary"]["restored_reviews"], json!(1));
    let restored_stats = ok_json(
        &fixture,
        &["code-review", "learning", "stats", "--db", restored_db],
    );
    assert_eq!(
        restored_stats["result"]["observations"],
        source_stats["result"]["observations"]
    );
    let restored_history_revision = restored_stats["result"]["generation"]["revision"]
        .as_u64()
        .expect("восстановленная база публикует собственную ревизию")
        .to_string();

    let independent_after = search(Some(restored_db), "src/lib.rs:2", None, Some("independent"));
    assert_eq!(
        case_ids(&independent_after),
        case_ids(&independent_before),
        "independent finding сохраняется и находится после restore"
    );
    assert_eq!(
        search(Some(restored_db), decision_text, Some("confirmed"), None)["result"]["matched"],
        json!(0)
    );
    assert_eq!(
        search(
            Some(restored_db),
            decision_text,
            Some("false-positive"),
            None
        )["result"]["matched"],
        json!(1)
    );

    let restored_patterns = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "patterns",
            "--db",
            restored_db,
            "--detector",
            "error_path",
        ],
    );
    assert_eq!(
        restored_patterns["result"]["rules"],
        source_patterns["result"]["rules"]
    );
    let restored_recommendations = ok_json(
        &fixture,
        &[
            "code-review",
            "learning",
            "recommend",
            "--db",
            restored_db,
            "--pack",
            &fixture.pack(&head),
            "--queue",
            &fixture.queue(&head),
            "--triage",
            &fixture.triage(&head),
            "--history-revision",
            &restored_history_revision,
            "--require-history",
        ],
    );
    assert_eq!(
        restored_recommendations["result"]["recommendations"],
        source_recommendations["result"]["recommendations"]
    );
    assert_eq!(
        restored_recommendations["result"]["suggested_order"],
        source_recommendations["result"]["suggested_order"]
    );
}
