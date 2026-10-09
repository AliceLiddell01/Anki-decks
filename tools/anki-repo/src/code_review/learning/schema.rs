//! DDL, версия схемы и проверяемые миграции локальной базы learning.
//!
//! Версия схемы хранится в `PRAGMA user_version`. База, созданная более новой
//! сборкой (`user_version > LEARNING_SCHEMA_VERSION`), не открывается для
//! записи: вместо попытки «исправить» её возвращается
//! [`ErrorCode::LearningSchemaUnsupported`]. Миграция выполняется внутри одной
//! транзакции и фиксируется в таблице `learning_migration`, поэтому повторный
//! запуск той же версии ничего не меняет.

use rusqlite::{Connection, Transaction, params};

use crate::error::{DomainError, ErrorCode};

use super::LEARNING_POLICY_VERSION;

/// Текущая версия схемы базы learning.
pub const LEARNING_SCHEMA_VERSION: u32 = 2;

/// Имя таблицы журнала миграций.
pub const MIGRATION_TABLE: &str = "learning_migration";

/// Базовый DDL версии 1.
///
/// Таблицы разделены так, чтобы разные единицы наблюдения нельзя было сложить по
/// ошибке: `learning_import` — записи ревью, `learning_unit` — независимые
/// единицы, `learning_decision` — решения ревьюера, `learning_finding` —
/// замечания, `learning_finding_link` — связи замечаний с кандидатами.
const SCHEMA_V1: &[&str] = &[
    r"CREATE TABLE IF NOT EXISTS learning_meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    )",
    r"CREATE TABLE IF NOT EXISTS learning_import (
        review_id TEXT PRIMARY KEY,
        repository_id TEXT NOT NULL,
        base_sha TEXT NOT NULL,
        head_sha TEXT NOT NULL,
        merge_base_sha TEXT NOT NULL,
        workspace_variant TEXT NOT NULL,
        workspace_label TEXT,
        review_pack_sha256 TEXT NOT NULL,
        queue_sha256 TEXT NOT NULL,
        triage_sha256 TEXT,
        execution_result_sha256 TEXT,
        review_schema_version INTEGER NOT NULL,
        queue_schema_version INTEGER NOT NULL,
        triage_schema_version INTEGER,
        execution_schema_version INTEGER,
        analyzer_digest TEXT NOT NULL,
        classifier_digest TEXT NOT NULL,
        trust TEXT NOT NULL,
        outcome TEXT NOT NULL,
        limitations_json TEXT NOT NULL,
        revision_of TEXT,
        superseded_by TEXT,
        revision INTEGER NOT NULL,
        imported_at INTEGER NOT NULL,
        observations_json TEXT NOT NULL,
        identity_key TEXT NOT NULL
    )",
    r"CREATE INDEX IF NOT EXISTS learning_import_identity
        ON learning_import (identity_key, revision)",
    r"CREATE INDEX IF NOT EXISTS learning_import_repository
        ON learning_import (repository_id, head_sha)",
    r"CREATE TABLE IF NOT EXISTS learning_unit (
        review_id TEXT NOT NULL,
        unit_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        candidate_count INTEGER NOT NULL,
        priority TEXT NOT NULL,
        representatives_json TEXT NOT NULL,
        disposition TEXT,
        reason_code TEXT,
        detector TEXT NOT NULL,
        source TEXT NOT NULL,
        role TEXT NOT NULL,
        code_role TEXT NOT NULL,
        surfaces_json TEXT NOT NULL,
        signature TEXT NOT NULL,
        feature_json TEXT NOT NULL,
        PRIMARY KEY (review_id, unit_id)
    )",
    r"CREATE INDEX IF NOT EXISTS learning_unit_signature
        ON learning_unit (signature)",
    r"CREATE INDEX IF NOT EXISTS learning_unit_detector
        ON learning_unit (detector, role)",
    r"CREATE TABLE IF NOT EXISTS learning_candidate (
        review_id TEXT NOT NULL,
        candidate_id TEXT NOT NULL,
        unit_id TEXT NOT NULL,
        detector TEXT NOT NULL,
        source TEXT NOT NULL,
        path TEXT NOT NULL,
        path_family TEXT NOT NULL,
        origin TEXT NOT NULL,
        execution TEXT NOT NULL,
        snippet TEXT,
        classification_json TEXT NOT NULL,
        PRIMARY KEY (review_id, candidate_id)
    )",
    r"CREATE INDEX IF NOT EXISTS learning_candidate_unit
        ON learning_candidate (review_id, unit_id)",
    r"CREATE TABLE IF NOT EXISTS learning_decision (
        review_id TEXT NOT NULL,
        decision_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        disposition TEXT NOT NULL,
        reason_code TEXT NOT NULL,
        explanation TEXT NOT NULL,
        candidate_count INTEGER NOT NULL,
        covered_json TEXT NOT NULL,
        PRIMARY KEY (review_id, decision_id)
    )",
    r"CREATE TABLE IF NOT EXISTS learning_finding (
        review_id TEXT NOT NULL,
        finding_id TEXT NOT NULL,
        severity TEXT NOT NULL,
        provenance TEXT NOT NULL,
        title TEXT NOT NULL,
        description TEXT NOT NULL,
        signature TEXT NOT NULL,
        linked_unit_ids_json TEXT NOT NULL,
        PRIMARY KEY (review_id, finding_id)
    )",
    r"CREATE INDEX IF NOT EXISTS learning_finding_signature
        ON learning_finding (signature)",
    r"CREATE TABLE IF NOT EXISTS learning_finding_link (
        review_id TEXT NOT NULL,
        candidate_id TEXT NOT NULL,
        finding_id TEXT NOT NULL,
        PRIMARY KEY (review_id, candidate_id, finding_id)
    )",
    r"CREATE TABLE IF NOT EXISTS learning_case_link (
        review_id TEXT NOT NULL,
        finding_id TEXT NOT NULL,
        linked_review_id TEXT NOT NULL,
        linked_finding_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        basis TEXT NOT NULL,
        PRIMARY KEY (review_id, finding_id, linked_review_id, linked_finding_id)
    )",
    r"CREATE TABLE IF NOT EXISTS learning_feedback (
        event_id TEXT PRIMARY KEY,
        review_id TEXT NOT NULL,
        unit_id TEXT NOT NULL,
        candidate_id TEXT,
        kind TEXT NOT NULL,
        action TEXT NOT NULL,
        supersedes_event_id TEXT,
        retracted_event_id TEXT,
        effective_disposition TEXT,
        usefulness TEXT,
        explanation TEXT NOT NULL,
        provenance TEXT NOT NULL,
        recorded_at INTEGER NOT NULL
    )",
    r"CREATE INDEX IF NOT EXISTS learning_feedback_target
        ON learning_feedback (review_id, unit_id, kind)",
    r"CREATE TABLE IF NOT EXISTS learning_policy_proposal (
        proposal_id TEXT PRIMARY KEY,
        rule_id TEXT NOT NULL,
        feature_json TEXT NOT NULL,
        document_json TEXT NOT NULL
    )",
    r"CREATE TABLE IF NOT EXISTS learning_search (
        case_id TEXT PRIMARY KEY,
        review_id TEXT NOT NULL,
        unit_id TEXT NOT NULL,
        candidate_id TEXT,
        finding_id TEXT,
        kind TEXT NOT NULL,
        disposition TEXT,
        severity TEXT,
        provenance TEXT,
        text TEXT NOT NULL
    )",
    r"CREATE INDEX IF NOT EXISTS learning_search_review
        ON learning_search (review_id)",
];

/// FTS5-индекс текстового поиска; создаётся, если сборка SQLite поддерживает FTS5.
const SEARCH_FTS5: &str = r"CREATE VIRTUAL TABLE IF NOT EXISTS learning_search_fts USING fts5 (
    case_id UNINDEXED,
    body
)";

/// Миграция v2: сохраняет минимизированную сводку проверенного ExecutionResult.
const SCHEMA_V2: &str = "ALTER TABLE learning_import ADD COLUMN execution_evidence_json TEXT";

/// Читает версию схемы базы.
pub fn read_schema_version(connection: &Connection) -> Result<u32, DomainError> {
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| schema_error("не удалось прочитать PRAGMA user_version", &error))?;
    u32::try_from(version).map_err(|_| {
        DomainError::new(
            ErrorCode::LearningSchemaUnsupported,
            format!("Значение PRAGMA user_version вне поддерживаемого диапазона: {version}"),
        )
    })
}

/// Определяет доступность FTS5 в текущей сборке SQLite.
pub fn fts5_available(connection: &Connection) -> Result<bool, DomainError> {
    let mut statement = connection
        .prepare("PRAGMA compile_options")
        .map_err(|error| schema_error("не удалось прочитать PRAGMA compile_options", &error))?;
    let options = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| schema_error("не удалось прочитать PRAGMA compile_options", &error))?;
    for option in options {
        let option = option
            .map_err(|error| schema_error("не удалось прочитать PRAGMA compile_options", &error))?;
        if option.eq_ignore_ascii_case("ENABLE_FTS5") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Применяет миграции до [`LEARNING_SCHEMA_VERSION`] внутри одной транзакции.
///
/// Возвращает фактическую версию схемы после работы. Более новая версия schema
/// отвергается до любых изменений.
pub fn apply_migrations(connection: &mut Connection) -> Result<u32, DomainError> {
    let current = read_schema_version(connection)?;
    if current > LEARNING_SCHEMA_VERSION {
        return Err(DomainError::with_details(
            ErrorCode::LearningSchemaUnsupported,
            format!(
                "База learning создана более новой схемой ({current}); эта сборка поддерживает {LEARNING_SCHEMA_VERSION}"
            ),
            crate::details! {
                "database_user_version" => current,
                "supported_schema_version" => LEARNING_SCHEMA_VERSION,
            },
        ));
    }
    let transaction = connection
        .transaction()
        .map_err(|error| schema_error("не удалось начать транзакцию миграции", &error))?;
    create_migration_table(&transaction)?;
    for version in current.saturating_add(1)..=LEARNING_SCHEMA_VERSION {
        if migration_applied(&transaction, version)? {
            continue;
        }
        match version {
            1 => apply_v1(&transaction)?,
            2 => transaction
                .execute_batch(SCHEMA_V2)
                .map_err(|error| schema_error("не удалось добавить сводку execution", &error))?,
            _ => {
                return Err(DomainError::new(
                    ErrorCode::LearningSchemaUnsupported,
                    format!("Нет миграции для версии схемы learning {version}"),
                ));
            }
        }
        transaction
            .execute(
                &format!("INSERT INTO {MIGRATION_TABLE} (version, applied_at) VALUES (?1, ?2)"),
                params![version, 0i64],
            )
            .map_err(|error| schema_error("не удалось записать журнал миграции", &error))?;
    }
    set_meta(
        &transaction,
        "policy_version",
        &LEARNING_POLICY_VERSION.to_string(),
    )?;
    set_meta(
        &transaction,
        "schema_version",
        &LEARNING_SCHEMA_VERSION.to_string(),
    )?;
    transaction
        .pragma_update(None, "user_version", LEARNING_SCHEMA_VERSION)
        .map_err(|error| schema_error("не удалось записать PRAGMA user_version", &error))?;
    transaction
        .commit()
        .map_err(|error| schema_error("не удалось зафиксировать транзакцию миграции", &error))?;
    Ok(LEARNING_SCHEMA_VERSION)
}

fn migration_applied(transaction: &Transaction<'_>, version: u32) -> Result<bool, DomainError> {
    let count: i64 = transaction
        .query_row(
            &format!("SELECT COUNT(*) FROM {MIGRATION_TABLE} WHERE version = ?1"),
            params![version],
            |row| row.get(0),
        )
        .map_err(|error| schema_error("не удалось прочитать журнал миграций", &error))?;
    Ok(count > 0)
}

fn create_migration_table(transaction: &Transaction<'_>) -> Result<(), DomainError> {
    transaction
        .execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {MIGRATION_TABLE} (
                    version INTEGER PRIMARY KEY,
                    applied_at INTEGER NOT NULL
                )"
            ),
            [],
        )
        .map_err(|error| schema_error("не удалось создать журнал миграций", &error))?;
    Ok(())
}

fn apply_v1(transaction: &Transaction<'_>) -> Result<(), DomainError> {
    for statement in SCHEMA_V1 {
        transaction
            .execute_batch(statement)
            .map_err(|error| schema_error("не удалось создать таблицу схемы learning", &error))?;
    }
    Ok(())
}

/// Создаёт FTS5-индекс и наполняет его уже сохранённым текстом.
///
/// Вызывается владельцем соединения после миграций: при отсутствии FTS5 в
/// сборке индекс не создаётся, а поиск использует документированный fallback.
pub fn ensure_fts_index(connection: &Connection) -> Result<bool, DomainError> {
    if !fts5_available(connection)? {
        return Ok(false);
    }
    connection
        .execute_batch(SEARCH_FTS5)
        .map_err(|error| schema_error("не удалось создать FTS5-индекс learning", &error))?;
    connection
        .execute(
            "INSERT INTO learning_search_fts (case_id, body)
             SELECT case_id, text FROM learning_search
             WHERE case_id NOT IN (SELECT case_id FROM learning_search_fts)",
            [],
        )
        .map_err(|error| schema_error("не удалось наполнить FTS5-индекс learning", &error))?;
    Ok(true)
}

/// Пересобирает FTS5-индекс из таблицы `learning_search`.
pub fn rebuild_fts_index(connection: &Connection) -> Result<bool, DomainError> {
    if !fts5_available(connection)? {
        return Ok(false);
    }
    connection
        .execute_batch(SEARCH_FTS5)
        .map_err(|error| schema_error("не удалось создать FTS5-индекс learning", &error))?;
    connection
        .execute("DELETE FROM learning_search_fts", [])
        .map_err(|error| schema_error("не удалось очистить FTS5-индекс learning", &error))?;
    connection
        .execute(
            "INSERT INTO learning_search_fts (case_id, body) SELECT case_id, text FROM learning_search",
            [],
        )
        .map_err(|error| schema_error("не удалось пересобрать FTS5-индекс learning", &error))?;
    Ok(true)
}

/// Записывает пару «ключ — значение» в служебную таблицу.
pub fn set_meta(connection: &Connection, key: &str, value: &str) -> Result<(), DomainError> {
    connection
        .execute(
            "INSERT INTO learning_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(|error| {
            schema_error("не удалось записать служебные метаданные learning", &error)
        })?;
    Ok(())
}

/// Читает значение служебной пары, если она есть.
pub fn read_meta(connection: &Connection, key: &str) -> Result<Option<String>, DomainError> {
    let mut statement = connection
        .prepare("SELECT value FROM learning_meta WHERE key = ?1")
        .map_err(|error| {
            schema_error("не удалось прочитать служебные метаданные learning", &error)
        })?;
    let mut rows = statement.query(params![key]).map_err(|error| {
        schema_error("не удалось прочитать служебные метаданные learning", &error)
    })?;
    match rows.next().map_err(|error| {
        schema_error("не удалось прочитать служебные метаданные learning", &error)
    })? {
        Some(row) => row.get::<_, String>(0).map(Some).map_err(|error| {
            schema_error("не удалось прочитать служебные метаданные learning", &error)
        }),
        None => Ok(None),
    }
}

fn schema_error(context: &str, error: &rusqlite::Error) -> DomainError {
    // Классификация та же, что у остальных операций хранилища: повреждением
    // считается только доказанное повреждение файла, а временная занятость и
    // недоступный каталог — нет. Иначе конкурентный писатель выглядел бы как
    // испорченная база и подталкивал к восстановлению исправной истории.
    if matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
    ) {
        return DomainError::with_details(
            ErrorCode::LearningCorrupt,
            format!("{context}: {error}"),
            crate::details! { "sqlite_error" => error.to_string() },
        );
    }
    super::store::map_error(error, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_failures_are_classified_by_cause_not_as_corruption() {
        use rusqlite::ffi;
        let failure = |code: ffi::ErrorCode, extended: i32| {
            rusqlite::Error::SqliteFailure(
                ffi::Error {
                    code,
                    extended_code: extended,
                },
                None,
            )
        };
        // Доказанное повреждение файла — единственный случай `learning_corrupt`.
        for code in [
            ffi::ErrorCode::NotADatabase,
            ffi::ErrorCode::DatabaseCorrupt,
        ] {
            assert_eq!(
                schema_error("миграция", &failure(code, 26)).code,
                ErrorCode::LearningCorrupt,
                "код {code:?} обязан оставаться повреждением"
            );
        }
        // Занятость базы временна и различима.
        for (code, extended) in [
            (ffi::ErrorCode::DatabaseBusy, 5),
            (ffi::ErrorCode::DatabaseLocked, 6),
        ] {
            let error = schema_error("миграция", &failure(code, extended));
            assert_eq!(error.code, ErrorCode::LearningStorageBusy);
            assert!(error.details["busy_timeout_ms"].is_number());
        }
        // Недоступный, только для чтения или переполненный носитель — не порча.
        for (code, extended) in [
            (ffi::ErrorCode::CannotOpen, 14),
            (ffi::ErrorCode::ReadOnly, 8),
            (ffi::ErrorCode::DiskFull, 13),
            (ffi::ErrorCode::SystemIoFailure, 10),
        ] {
            let error = schema_error("миграция", &failure(code, extended));
            assert_eq!(
                error.code,
                ErrorCode::LearningStorageUnavailable,
                "код {code:?} не является повреждением базы"
            );
        }
    }

    #[test]
    fn current_schema_is_applied_once_and_recorded() {
        let mut connection = Connection::open_in_memory().unwrap();
        assert_eq!(
            apply_migrations(&mut connection).unwrap(),
            LEARNING_SCHEMA_VERSION
        );
        assert_eq!(
            read_schema_version(&connection).unwrap(),
            LEARNING_SCHEMA_VERSION
        );
        // Повторный вызов идемпотентен и не плодит записей журнала.
        assert_eq!(
            apply_migrations(&mut connection).unwrap(),
            LEARNING_SCHEMA_VERSION
        );
        let recorded: i64 = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {MIGRATION_TABLE}"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, i64::from(LEARNING_SCHEMA_VERSION));
        assert_eq!(
            read_meta(&connection, "policy_version").unwrap().as_deref(),
            Some("1")
        );
    }

    #[test]
    fn version_one_database_migrates_atomically_to_current_schema() {
        let mut connection = Connection::open_in_memory().unwrap();
        // Применяем исходную схему и записываем корректную точку миграции v1.
        {
            let transaction = connection.transaction().unwrap();
            create_migration_table(&transaction).unwrap();
            apply_v1(&transaction).unwrap();
            transaction
                .execute(
                    &format!("INSERT INTO {MIGRATION_TABLE} (version, applied_at) VALUES (1, 0)"),
                    [],
                )
                .unwrap();
            transaction
                .pragma_update(None, "user_version", 1u32)
                .unwrap();
            transaction.commit().unwrap();
        }
        assert_eq!(
            apply_migrations(&mut connection).unwrap(),
            LEARNING_SCHEMA_VERSION
        );
        assert_eq!(
            read_schema_version(&connection).unwrap(),
            LEARNING_SCHEMA_VERSION
        );
        let has_column: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('learning_import') WHERE name = 'execution_evidence_json'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_column, 1);
        let migrations: i64 = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {MIGRATION_TABLE}"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migrations, i64::from(LEARNING_SCHEMA_VERSION));
    }

    #[test]
    fn newer_schema_is_refused_without_changes() {
        let mut connection = Connection::open_in_memory().unwrap();
        apply_migrations(&mut connection).unwrap();
        connection
            .pragma_update(None, "user_version", 99u32)
            .unwrap();
        let error = apply_migrations(&mut connection).unwrap_err();
        assert_eq!(error.code, ErrorCode::LearningSchemaUnsupported);
        assert_eq!(error.code.as_str(), "learning_schema_unsupported");
        assert_eq!(read_schema_version(&connection).unwrap(), 99);
    }

    #[test]
    fn fts5_presence_is_reported_honestly() {
        let mut connection = Connection::open_in_memory().unwrap();
        apply_migrations(&mut connection).unwrap();
        let available = fts5_available(&connection).unwrap();
        // Bundled-сборка объявляет ENABLE_FTS5; тест фиксирует факт, а не предположение.
        assert!(available, "bundled libsqlite3 должен объявлять ENABLE_FTS5");
        assert!(ensure_fts_index(&connection).unwrap());
        assert!(rebuild_fts_index(&connection).unwrap());
    }
}
