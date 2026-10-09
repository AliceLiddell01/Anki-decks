//! Идентичность SQLite проверяется до записи; колоды и внешние данные не нужны.

use std::fs;
use std::path::Path;

use anki_repo::code_review::learning::schema::{
    LEARNING_APPLICATION_ID, LEARNING_SCHEMA_VERSION, apply_migrations,
};
use anki_repo::code_review::learning::{LearningStore, StoreOptions};
use anki_repo::error::ErrorCode;
use rusqlite::Connection;

use crate::common::TempDir;

fn assert_refused_without_changes(database: &Path, expected: ErrorCode) {
    let before = fs::read(database).unwrap();
    let before_entries = fs::read_dir(database.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<std::collections::BTreeSet<_>>();
    for create in [false, true] {
        let mut options = StoreOptions::at(database);
        options.create = create;
        let error = LearningStore::open(options).unwrap_err();
        assert_eq!(error.code, expected);
        assert_eq!(
            fs::read(database).unwrap(),
            before,
            "Отказ не меняет байты файла"
        );
        let after_entries = fs::read_dir(database.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            after_entries, before_entries,
            "Отказ не создаёт журналы SQLite"
        );
    }
}

fn create_current(database: &Path) {
    let mut connection = Connection::open(database).unwrap();
    apply_migrations(&mut connection).unwrap();
}

#[test]
fn existing_empty_sqlite_and_empty_files_cannot_be_initialized_implicitly() {
    let directory = TempDir::new("learning-store-empty");
    for name in ["empty.sqlite", "zero-length.sqlite"] {
        let database = directory.path().join(name);
        if name == "empty.sqlite" {
            Connection::open(&database)
                .unwrap()
                .execute_batch("VACUUM")
                .unwrap();
        } else {
            fs::write(&database, []).unwrap();
        }
        assert_refused_without_changes(&database, ErrorCode::LearningCorrupt);
    }
}

#[test]
fn foreign_user_data_is_preserved_and_user_version_cannot_spoof_identity() {
    let directory = TempDir::new("learning-store-foreign");
    for version in [0, 1, LEARNING_SCHEMA_VERSION] {
        let database = directory.path().join(format!("foreign-{version}.sqlite"));
        {
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE personal_notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
                 INSERT INTO personal_notes VALUES (1, 'данные владельца');",
                )
                .unwrap();
            connection
                .pragma_update(None, "user_version", version)
                .unwrap();
        }
        assert_refused_without_changes(&database, ErrorCode::LearningCorrupt);
        let connection = Connection::open(&database).unwrap();
        let (body, count): (String, i64) = connection.query_row(
            "SELECT body, (SELECT count(*) FROM sqlite_master WHERE type='table') FROM personal_notes",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(body, "данные владельца");
        assert_eq!(count, 1);
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            version
        );
    }
}

#[test]
fn closed_foreign_wal_database_does_not_gain_sidecar_files_after_refusal() {
    let directory = TempDir::new("learning-store-foreign-wal");
    let database = directory.path().join("foreign.sqlite");
    {
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
             CREATE TABLE personal_notes (body TEXT);
             INSERT INTO personal_notes VALUES ('сохранить');",
            )
            .unwrap();
    }
    assert_refused_without_changes(&database, ErrorCode::LearningCorrupt);
}

#[test]
fn partial_learning_schema_and_inconsistent_markers_are_refused_without_changes() {
    let directory = TempDir::new("learning-store-partial");
    for (name, damage) in [
        ("missing-table", "DROP TABLE learning_decision"),
        ("missing-index", "DROP INDEX learning_candidate_unit"),
        (
            "changed-column",
            "ALTER TABLE learning_unit ADD COLUMN unexpected TEXT",
        ),
        (
            "missing-migration",
            "DELETE FROM learning_migration WHERE version=2",
        ),
        (
            "missing-meta",
            "DELETE FROM learning_meta WHERE key='schema_version'",
        ),
        (
            "wrong-meta",
            "UPDATE learning_meta SET value='2' WHERE key='schema_version'",
        ),
        (
            "wrong-generation",
            "UPDATE learning_meta SET value='bad' WHERE key='history_generation'",
        ),
        ("foreign-app", "PRAGMA application_id=42"),
        ("zero-version", "PRAGMA user_version=0"),
    ] {
        let database = directory.path().join(format!("{name}.sqlite"));
        create_current(&database);
        Connection::open(&database)
            .unwrap()
            .execute_batch(damage)
            .unwrap();
        assert_refused_without_changes(&database, ErrorCode::LearningCorrupt);
    }
}

#[test]
fn current_and_legacy_unmarked_current_schemas_keep_data_without_writes() {
    let directory = TempDir::new("learning-store-current");
    for application_id in [0, LEARNING_APPLICATION_ID] {
        let database = directory
            .path()
            .join(format!("current-{application_id}.sqlite"));
        create_current(&database);
        let observer = Connection::open(&database).unwrap();
        observer
            .pragma_update(None, "application_id", application_id)
            .unwrap();
        observer
            .execute(
                "INSERT INTO learning_meta(key, value) VALUES ('custom-audit', 'сохранить')",
                [],
            )
            .unwrap();
        // Согласованный режим исключает законное изменение journal_mode при открытии.
        drop(LearningStore::open(StoreOptions::at(&database)).unwrap());
        let before: i64 = observer
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .unwrap();
        let mut options = StoreOptions::at(&database);
        options.create = false;
        let store = LearningStore::open(options).unwrap();
        store.integrity_check().unwrap();
        assert_eq!(
            store.status().unwrap().user_version,
            LEARNING_SCHEMA_VERSION
        );
        drop(store);
        let after: i64 = observer
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .unwrap();
        assert_eq!(after, before);
        assert_eq!(
            observer
                .query_row(
                    "SELECT value FROM learning_meta WHERE key='custom-audit'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "сохранить"
        );
        assert_eq!(
            observer
                .pragma_query_value(None, "application_id", |row| row.get::<_, u32>(0))
                .unwrap(),
            application_id
        );
    }
}

#[test]
fn recognized_checkpoint_is_followed_by_reading_the_active_wal_snapshot() {
    let directory = TempDir::new("learning-store-active-wal");
    let database = directory.path().join("state.sqlite");
    let writer = LearningStore::open(StoreOptions::at(&database)).unwrap();
    writer
        .connection()
        .execute_batch("PRAGMA wal_autocheckpoint=0")
        .unwrap();
    writer
        .write(|write| {
            write.execute(
                "INSERT INTO learning_meta(key, value) VALUES ('wal-audit', 'запись из WAL')",
                [],
            )
        })
        .unwrap();
    let mut options = StoreOptions::at(&database);
    options.create = false;
    let reader = LearningStore::open(options).unwrap();
    assert_eq!(
        reader
            .connection()
            .query_row(
                "SELECT value FROM learning_meta WHERE key='wal-audit'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "запись из WAL"
    );
    assert_eq!(reader.status().unwrap().generation.revision, 1);
}

#[test]
fn future_schema_is_refused_without_changes() {
    let directory = TempDir::new("learning-store-future");
    let database = directory.path().join("future.sqlite");
    create_current(&database);
    Connection::open(&database)
        .unwrap()
        .pragma_update(None, "user_version", LEARNING_SCHEMA_VERSION + 1)
        .unwrap();
    assert_refused_without_changes(&database, ErrorCode::LearningSchemaUnsupported);
}

#[test]
fn missing_store_is_created_only_when_creation_is_requested() {
    let directory = TempDir::new("learning-store-missing");
    let database = directory.path().join("nested/state.sqlite");
    let mut options = StoreOptions::at(&database);
    options.create = false;
    assert_eq!(
        LearningStore::open(options).unwrap_err().code,
        ErrorCode::LearningStorageUnavailable
    );
    assert!(!database.parent().unwrap().exists());
    let store = LearningStore::open(StoreOptions::at(&database)).unwrap();
    assert_eq!(
        store
            .connection()
            .pragma_query_value(None, "application_id", |row| row.get::<_, u32>(0))
            .unwrap(),
        LEARNING_APPLICATION_ID
    );
    assert_eq!(
        store.status().unwrap().user_version,
        LEARNING_SCHEMA_VERSION
    );
}
