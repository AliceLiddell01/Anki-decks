//! Владение каталогом отчёта и замена его содержимого целиком.
//!
//! Каталог отчёта — артефакт прогона, который человек открывает в браузере. Его
//! нельзя ни затирать вслепую, ни дописывать поверх: повторная генерация обязана
//! оставить ровно файлы текущего отчёта, а чужой каталог — не потерять ни одного
//! файла. Поэтому у каталога есть **доказательство владения** — манифест
//! [`MANIFEST_FILE`] со списком файлов отчёта, — и две операции, которые с ним
//! работают:
//!
//! - [`inspect`] отвечает, что это за каталог: пустой, свой (с манифестом) или
//!   чужой. Чужой каталог и испорченный манифест — отказ **до** любых изменений:
//!   «каталог не пуст, и в нём лежит обычный `index.html`» больше не выглядит
//!   доказательством, потому что таким каталог может быть у чего угодно;
//! - [`Staging`] собирает новый отчёт **рядом** с целевым каталогом и переносит его
//!   на место одним шагом ([`Staging::commit`]). Все отказы генерации — разбор
//!   экспорта, планирование media, проверка границы доверия, нехватка места —
//!   случаются до переноса, поэтому предыдущий отчёт остаётся целым.
//!
//! Порядок переноса выбран так, чтобы в любой момент на диске был читаемый отчёт:
//! сначала новые файлы встают на свои места (файл заменяется целиком, одним
//! `rename`), потом удаляются устаревшие файлы **из списка владения**, и только
//! последним записывается манифест. Прерывание в середине оставляет смесь старых и
//! новых документов со старым манифестом: такой каталог читается, а следующий
//! успешный прогон сходится ровно к текущему набору файлов.
//!
//! Что манифест **не** делает: он не удаляет и не перезаписывает ничего, чего в нём
//! нет. Файлы, появившиеся в каталоге отчёта помимо отчёта, называются в
//! диагностике и остаются на месте.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::write;

/// Имя файла владения внутри каталога отчёта.
pub const MANIFEST_FILE: &str = "report-manifest.json";

/// Метка формата: манифест другого инструмента владением не является.
const MAGIC: &str = "anki-repo-visual-report";

/// Версия формата манифеста.
const VERSION: u64 = 1;

/// Доказательство владения каталогом отчёта.
///
/// Список файлов — это и есть владение: удаляется и перезаписывается только то,
/// что в нём перечислено. Отсортированный список без времени и без путей запуска
/// делает манифест детерминированным: один и тот же отчёт даёт один и тот же файл.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Метка формата.
    pub magic: String,
    /// Версия формата.
    pub version: u64,
    /// Относительные пути файлов отчёта, отсортированные.
    pub files: Vec<String>,
}

impl Manifest {
    /// Манифест для набора файлов.
    #[must_use]
    pub fn new(files: &[String]) -> Self {
        let mut sorted: BTreeSet<String> = files.iter().cloned().collect();
        sorted.remove(MANIFEST_FILE);
        Self {
            magic: MAGIC.to_string(),
            version: VERSION,
            files: sorted.into_iter().collect(),
        }
    }

    /// Байты манифеста: детерминированные и читаемые человеком.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::WriteFailed`], если сериализация не удалась.
    pub fn bytes(&self) -> Result<Vec<u8>, DomainError> {
        let mut bytes = serde_json::to_vec_pretty(self).map_err(|error| {
            DomainError::with_details(
                ErrorCode::WriteFailed,
                format!("манифест отчёта не сериализуется: {error}"),
                details! { "reason" => "manifest_not_serializable" },
            )
        })?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Что лежит в каталоге отчёта до записи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// Каталога нет или он пуст: отчёт создаёт его сам.
    Free,
    /// Каталог принадлежит отчёту: в нём есть манифест.
    Owned {
        /// Файлы из манифеста.
        files: Vec<String>,
        /// Записи внутри каталога, которых в манифесте нет.
        ///
        /// Чужие файлы не удаляются и не перезаписываются: они принадлежат не
        /// отчёту, а тому, кто их положил.
        foreign: Vec<String>,
    },
}

/// Проверяет, кому принадлежит каталог отчёта.
///
/// # Errors
///
/// [`ErrorCode::InvalidRequest`], если каталог не пуст и не доказывает владение
/// (`out_dir_not_owned`), или если манифест испорчен либо чужой
/// (`out_dir_manifest_invalid`). Отказ приходит **до** любых изменений.
pub fn inspect(out: &Path) -> Result<Ownership, DomainError> {
    if !out.exists() {
        return Ok(Ownership::Free);
    }
    if !out.is_dir() {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!("{} существует и не является каталогом", out.display()),
            details! {
                "reason" => "out_dir_not_a_directory",
                "out_dir" => out.display().to_string(),
            },
        ));
    }

    let entries = entries_of(out)?;
    if entries.is_empty() {
        return Ok(Ownership::Free);
    }

    let manifest_path = out.join(MANIFEST_FILE);
    if !manifest_path.is_file() {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "каталог отчёта {} не пуст и не принадлежит отчёту: нет {MANIFEST_FILE}",
                out.display()
            ),
            details! {
                "reason" => "out_dir_not_owned",
                "out_dir" => out.display().to_string(),
                "manifest" => MANIFEST_FILE,
                "entries" => entries.join(", "),
            },
        ));
    }

    let raw = fs::read(&manifest_path).map_err(|error| {
        DomainError::with_details(
            ErrorCode::WriteFailed,
            format!("{MANIFEST_FILE} не читается: {error}"),
            details! {
                "reason" => "out_dir_manifest_invalid",
                "path" => manifest_path.display().to_string(),
            },
        )
    })?;
    let manifest: Manifest = serde_json::from_slice(&raw).map_err(|error| {
        DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!("{MANIFEST_FILE} испорчен: {error}"),
            details! {
                "reason" => "out_dir_manifest_invalid",
                "path" => manifest_path.display().to_string(),
            },
        )
    })?;

    if manifest.magic != MAGIC || manifest.version != VERSION {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "{MANIFEST_FILE} описывает чужой или незнакомый формат отчёта: {} v{}",
                manifest.magic, manifest.version
            ),
            details! {
                "reason" => "out_dir_manifest_invalid",
                "path" => manifest_path.display().to_string(),
                "magic" => manifest.magic.clone(),
                "version" => manifest.version,
            },
        ));
    }

    // Путь из манифеста — это право удалить файл. Право выдаётся только на то,
    // что лежит внутри каталога отчёта, поэтому «../» здесь отвергается.
    for file in &manifest.files {
        if !is_inner_path(file) {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("{MANIFEST_FILE} ссылается на путь вне каталога отчёта: {file}"),
                details! {
                    "reason" => "out_dir_manifest_invalid",
                    "path" => manifest_path.display().to_string(),
                    "entry" => file.clone(),
                },
            ));
        }
    }

    let owned: BTreeSet<&str> = manifest.files.iter().map(String::as_str).collect();
    let foreign: Vec<String> = entries
        .into_iter()
        .filter(|entry| entry != MANIFEST_FILE && !owned.contains(entry.as_str()))
        .collect();

    Ok(Ownership::Owned {
        files: manifest.files.clone(),
        foreign,
    })
}

/// Все относительные пути каталога, включая содержимое подкаталогов.
fn entries_of(root: &Path) -> Result<Vec<String>, DomainError> {
    let mut found: Vec<String> = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];

    while let Some((directory, prefix)) = stack.pop() {
        let listing = fs::read_dir(&directory).map_err(|error| {
            DomainError::with_details(
                ErrorCode::WriteFailed,
                format!("{} не читается: {error}", directory.display()),
                details! {
                    "reason" => "out_dir_unreadable",
                    "path" => directory.display().to_string(),
                },
            )
        })?;
        for entry in listing {
            let entry = entry.map_err(|error| {
                DomainError::with_details(
                    ErrorCode::WriteFailed,
                    format!("{} не читается: {error}", directory.display()),
                    details! { "reason" => "out_dir_unreadable" },
                )
            })?;
            let name = entry.file_name().to_string_lossy().to_string();
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let path = entry.path();
            if path.is_dir() {
                stack.push((path, relative));
            } else {
                found.push(relative);
            }
        }
    }

    found.sort();
    Ok(found)
}

/// Путь внутри каталога отчёта: относительный, без подъёма наверх.
///
/// Один предикат на две задачи: он же защищает удаление устаревших файлов, и он же
/// формирует список файлов отчёта. Второй реализации «безопасного относительного
/// пути» быть не должно.
#[must_use]
pub fn is_inner_path(relative: &str) -> bool {
    if relative.is_empty() || relative.contains('\\') || relative.contains('\0') {
        return false;
    }
    let path = Path::new(relative);
    path.components().all(|component| match component {
        Component::Normal(name) => !name.is_empty(),
        Component::CurDir | Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
            false
        }
    })
}

/// Сборка нового отчёта рядом с целевым каталогом.
///
/// Все файлы пишутся в [`Staging::path`], а в целевой каталог попадают только по
/// [`Staging::commit`]. Пока сборка идёт, предыдущий отчёт на месте; если сборка
/// не дошла до переноса, каталог отчёта не меняется вообще.
#[derive(Debug)]
pub struct Staging {
    /// Куда собирается отчёт.
    directory: PathBuf,
    /// Куда он переезжает.
    destination: PathBuf,
    /// Что было в целевом каталоге до записи, по манифесту.
    owned: Vec<String>,
}

impl Staging {
    /// Готовит каталог сборки рядом с целевым.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::WriteFailed`], если каталог сборки нельзя создать.
    pub fn begin(destination: &Path, ownership: &Ownership) -> Result<Self, DomainError> {
        let name = destination.file_name().map_or_else(
            || "report".to_string(),
            |name| name.to_string_lossy().to_string(),
        );
        let directory =
            destination.with_file_name(format!(".{name}.staging-{}", std::process::id()));

        // Остатки прерванного прогона с тем же pid: каталог сборки — не артефакт.
        if directory.exists() {
            let _ = fs::remove_dir_all(&directory);
        }
        fs::create_dir_all(&directory).map_err(|error| {
            DomainError::with_details(
                ErrorCode::WriteFailed,
                format!(
                    "каталог сборки {} не создаётся: {error}",
                    directory.display()
                ),
                details! {
                    "reason" => "staging_not_created",
                    "path" => directory.display().to_string(),
                },
            )
        })?;

        let owned = match ownership {
            Ownership::Free => Vec::new(),
            Ownership::Owned { files, .. } => files.clone(),
        };

        Ok(Self {
            directory,
            destination: destination.to_path_buf(),
            owned,
        })
    }

    /// Куда писать файлы отчёта.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.directory
    }

    /// Переносит собранный отчёт в целевой каталог.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::WriteFailed`], если файл нельзя перенести или манифест нельзя
    /// записать. Список `files` — это полный набор файлов отчёта: всё, что было в
    /// манифесте и не входит в него, удаляется как устаревшее.
    pub fn commit(self, files: &[String]) -> Result<CommitReport, DomainError> {
        let manifest = Manifest::new(files);
        let mut moved: Vec<String> = Vec::new();

        fs::create_dir_all(&self.destination).map_err(|error| {
            write_failure("staging_destination_not_created", &self.destination, &error)
        })?;

        for relative in &manifest.files {
            if !is_inner_path(relative) {
                return Err(DomainError::with_details(
                    ErrorCode::WriteFailed,
                    format!("файл отчёта вне каталога отчёта: {relative}"),
                    details! {
                        "reason" => "report_file_outside_out_dir",
                        "file" => relative.clone(),
                    },
                ));
            }
            let source = self.directory.join(relative);
            let target = self.destination.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    write_failure("staging_destination_not_created", parent, &error)
                })?;
            }
            // `rename` заменяет файл целиком: читатель никогда не видит половину.
            fs::rename(&source, &target)
                .map_err(|error| write_failure("report_file_not_moved", &target, &error))?;
            moved.push(relative.clone());
        }

        let current: BTreeSet<&str> = manifest.files.iter().map(String::as_str).collect();
        let mut removed: Vec<String> = Vec::new();
        for relative in &self.owned {
            if current.contains(relative.as_str()) {
                continue;
            }
            let stale = self.destination.join(relative);
            if stale.is_file() {
                fs::remove_file(&stale)
                    .map_err(|error| write_failure("stale_file_not_removed", &stale, &error))?;
                removed.push(relative.clone());
            }
        }

        // Пустые подкаталоги прежнего отчёта (`cards`, `media/<состояние>`)
        // убираются только если они действительно пусты: `remove_dir` не трогает
        // каталог с содержимым.
        prune_empty_directories(&self.destination);

        // Манифест — последним: пока его нет, каталог считается прежним отчётом, и
        // следующий прогон не примет смесь за свой набор файлов.
        let manifest_path = self.destination.join(MANIFEST_FILE);
        let bytes = manifest.bytes()?;
        write::replace_document_atomically(&manifest_path, &bytes)?;

        Ok(CommitReport {
            files: manifest.files,
            moved,
            removed,
        })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // Каталог сборки — не артефакт отчёта: и после отказа, и после переноса от
        // него не должно остаться ничего. Перенесённых файлов здесь уже нет.
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Что произошло при переносе.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReport {
    /// Файлы отчёта после переноса.
    pub files: Vec<String>,
    /// Перенесённые файлы.
    pub moved: Vec<String>,
    /// Удалённые устаревшие файлы прежнего отчёта.
    pub removed: Vec<String>,
}

/// Убирает пустые подкаталоги, оставшиеся от устаревших файлов.
fn prune_empty_directories(root: &Path) {
    let Ok(listing) = fs::read_dir(root) else {
        return;
    };
    let mut directories: Vec<PathBuf> = Vec::new();
    for entry in listing.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Второй уровень нужен ради `media/before` и `media/after`.
            if let Ok(nested) = fs::read_dir(&path) {
                for entry in nested.flatten() {
                    let nested_path = entry.path();
                    if nested_path.is_dir() {
                        let _ = fs::remove_dir(&nested_path);
                    }
                }
            }
            directories.push(path);
        }
    }
    for directory in directories {
        let _ = fs::remove_dir(&directory);
    }
}

/// Ошибка записи с путём и причиной.
fn write_failure(reason: &'static str, path: &Path, error: &std::io::Error) -> DomainError {
    DomainError::with_details(
        ErrorCode::WriteFailed,
        format!("{} не записан: {error}", path.display()),
        details! {
            "reason" => reason,
            "path" => path.display().to_string(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "anki-manifest-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("каталог");
        dir
    }

    #[test]
    fn a_missing_or_empty_directory_is_free() {
        let root = temp("free");
        assert_eq!(inspect(&root.join("нет")).expect("осмотр"), Ownership::Free);
        assert_eq!(inspect(&root).expect("осмотр"), Ownership::Free);
    }

    #[test]
    fn a_foreign_directory_is_refused_without_changes() {
        let root = temp("foreign");
        fs::create_dir_all(root.join("cards")).expect("подкаталог");
        fs::write(root.join("index.html"), "<html>чужой</html>".as_bytes()).expect("файл");
        fs::write(root.join("cards/card-0001.html"), "чужое".as_bytes()).expect("файл");

        let error = inspect(&root).expect_err("чужой каталог отвергается");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.details["reason"], "out_dir_not_owned");
        // Отказ ничего не меняет: файлы на месте, побайтово те же.
        assert_eq!(
            fs::read(root.join("index.html")).expect("файл"),
            "<html>чужой</html>".as_bytes()
        );
        assert!(root.join("cards/card-0001.html").is_file());
    }

    #[test]
    fn a_corrupt_or_foreign_manifest_fails_closed() {
        let root = temp("corrupt");
        fs::write(root.join(MANIFEST_FILE), "{ это не JSON".as_bytes()).expect("файл");
        let error = inspect(&root).expect_err("испорченный манифест");
        assert_eq!(error.details["reason"], "out_dir_manifest_invalid");

        fs::write(
            root.join(MANIFEST_FILE),
            r#"{"magic":"другой","version":1,"files":[]}"#.as_bytes(),
        )
        .expect("файл");
        let error = inspect(&root).expect_err("чужой манифест");
        assert_eq!(error.details["reason"], "out_dir_manifest_invalid");
    }

    #[test]
    fn a_manifest_cannot_reach_outside_the_report_directory() {
        let root = temp("escape");
        fs::write(
            root.join(MANIFEST_FILE),
            r#"{"magic":"anki-repo-visual-report","version":1,"files":["../deck.json"]}"#
                .as_bytes(),
        )
        .expect("файл");
        let error = inspect(&root).expect_err("путь наружу отвергается");
        assert_eq!(error.details["reason"], "out_dir_manifest_invalid");
    }

    #[test]
    fn foreign_files_inside_an_owned_directory_are_reported_not_removed() {
        let root = temp("foreign-inside");
        let manifest = Manifest::new(&["index.html".to_string()]);
        fs::write(root.join("index.html"), "свой".as_bytes()).expect("файл");
        fs::write(root.join(MANIFEST_FILE), manifest.bytes().expect("байты")).expect("манифест");
        fs::write(root.join("заметка.txt"), "чужое".as_bytes()).expect("файл");

        match inspect(&root).expect("осмотр") {
            Ownership::Owned { files, foreign } => {
                assert_eq!(files, vec!["index.html".to_string()]);
                assert_eq!(foreign, vec!["заметка.txt".to_string()]);
            }
            Ownership::Free => panic!("каталог обязан быть своим"),
        }
    }

    #[test]
    fn a_repeated_commit_leaves_exactly_the_current_files() {
        let root = temp("repeat");
        fs::create_dir_all(root.join("cards")).expect("подкаталог");
        fs::write(root.join("cards/card-0001.html"), "старое".as_bytes()).expect("файл");
        let manifest =
            Manifest::new(&["index.html".to_string(), "cards/card-0001.html".to_string()]);
        fs::write(root.join("index.html"), "старый index".as_bytes()).expect("файл");
        fs::write(root.join(MANIFEST_FILE), manifest.bytes().expect("байты")).expect("манифест");

        let ownership = inspect(&root).expect("осмотр");
        let staging = Staging::begin(&root, &ownership).expect("сборка");
        fs::create_dir_all(staging.path().join("cards")).expect("подкаталог");
        fs::write(staging.path().join("index.html"), "новый index".as_bytes()).expect("файл");
        fs::write(
            staging.path().join("cards/card-0009.html"),
            "новое".as_bytes(),
        )
        .expect("файл");
        let report = staging
            .commit(&["index.html".to_string(), "cards/card-0009.html".to_string()])
            .expect("перенос");

        assert_eq!(report.removed, vec!["cards/card-0001.html".to_string()]);
        let mut files: Vec<String> = entries_of(&root).expect("обход");
        files.retain(|name| name != MANIFEST_FILE);
        assert_eq!(
            files,
            vec!["cards/card-0009.html".to_string(), "index.html".to_string()]
        );
        assert_eq!(
            fs::read(root.join("index.html")).expect("файл"),
            "новый index".as_bytes()
        );
        assert!(!root.join("cards/card-0001.html").exists());
    }

    #[test]
    fn a_failed_generation_never_touches_the_previous_report() {
        let root = temp("abort");
        let manifest = Manifest::new(&["index.html".to_string()]);
        fs::write(root.join("index.html"), "прежний отчёт".as_bytes()).expect("файл");
        fs::write(root.join(MANIFEST_FILE), manifest.bytes().expect("байты")).expect("манифест");

        {
            let ownership = inspect(&root).expect("осмотр");
            let staging = Staging::begin(&root, &ownership).expect("сборка");
            fs::write(
                staging.path().join("index.html"),
                "наполовину собранный".as_bytes(),
            )
            .expect("файл");
            // Сборка бросается: перенос не выполняется.
        }

        assert_eq!(
            fs::read(root.join("index.html")).expect("файл"),
            "прежний отчёт".as_bytes()
        );
        assert!(inspect(&root).is_ok(), "прежний отчёт остаётся своим");
        let leftovers: Vec<String> = entries_of(&root)
            .expect("обход")
            .into_iter()
            .filter(|name| name.contains("staging"))
            .collect();
        assert!(leftovers.is_empty(), "каталог сборки убран: {leftovers:?}");
    }

    #[test]
    fn inner_paths_are_the_only_writable_ones() {
        for good in ["index.html", "cards/card-0001.html", "media/before/a.png"] {
            assert!(is_inner_path(good), "{good} обязан быть допустим");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../deck.json",
            "cards/../../deck.json",
            "media\\before\\a.png",
            "./index.html",
        ] {
            assert!(!is_inner_path(bad), "{bad} обязан быть отвергнут");
        }
    }

    #[test]
    fn the_manifest_is_deterministic() {
        let first = Manifest::new(&[
            "index.html".to_string(),
            "cards/card-0002.html".to_string(),
            "cards/card-0001.html".to_string(),
        ]);
        let second = Manifest::new(&[
            "cards/card-0001.html".to_string(),
            "index.html".to_string(),
            "cards/card-0002.html".to_string(),
        ]);
        assert_eq!(first, second);
        assert_eq!(
            first.bytes().expect("байты"),
            second.bytes().expect("байты")
        );
        assert_eq!(
            first.files,
            vec![
                "cards/card-0001.html".to_string(),
                "cards/card-0002.html".to_string(),
                "index.html".to_string()
            ]
        );
    }

    #[test]
    fn the_manifest_never_lists_itself() {
        let manifest = Manifest::new(&[MANIFEST_FILE.to_string(), "index.html".to_string()]);
        assert_eq!(manifest.files, vec!["index.html".to_string()]);
    }
}
