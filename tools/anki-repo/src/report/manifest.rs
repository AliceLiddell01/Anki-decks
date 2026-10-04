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
//!   «каталог не пуст, и в нём лежит обычный `index.html`» не выглядит
//!   доказательством, потому что таким каталог может быть у чего угодно;
//! - [`Staging`] собирает новый отчёт **рядом** с целевым каталогом и переносит его
//!   на место одним шагом ([`Staging::commit`]). Все отказы генерации — разбор
//!   экспорта, планирование media, проверка границы доверия, нехватка места —
//!   случаются до переноса, поэтому предыдущий отчёт остаётся целым.
//!
//! Манифест выдаёт право **удалить** и **заменить** — то есть право изменить
//! чужой файл. Поэтому право выдаётся имени, а не ссылке, и не выдаётся
//! каталогам: запись, чтение и удаление идут по настоящим путям внутри каталога
//! отчёта, а символическая ссылка внутри каталога — это отказ, а не путь. Обход
//! каталога не следует за ссылками на каталоги: иначе «внутри отчёта» перестало
//! бы означать «внутри отчёта».
//!
//! Порядок переноса выбран так, чтобы в любой момент на диске был читаемый отчёт:
//! сначала **проверяется целиком весь** новый набор — каждый целевой путь и все
//! каталоги, через которые он получается, — потом новые файлы встают на свои
//! места (файл заменяется целиком, одним `rename`), потом удаляются устаревшие
//! файлы **из списка владения**, и только последним записывается манифест.
//! Проверка до первой мутации нужна потому, что частично применённый набор — это
//! не «почти отчёт», а каталог, где часть новых документов уже заняла места тех,
//! которые отказ обязан был сохранить.
//!
//! Что манифест **не** делает: он не удаляет и не перезаписывает ничего, чего в нём
//! нет. Файлы, появившиеся в каталоге отчёта помимо отчёта, называются в
//! диагностике и остаются на месте. Исключение ровно одно и названо прямо: имена
//! внутри `cards/` и `media/` формирует сам отчёт, поэтому файл с таким путём —
//! это либо файл отчёта, либо остаток прерванного прогона, и он заменяется. Точка
//! входа `index.html` лежит в корне рядом с чем угодно и в это исключение не
//! входит: заменить её можно только по доказательству владения.
//!
//! Прерывание между переносом файлов и записью манифеста поэтому остаётся
//! восстановимым: следующий прогон снова соберёт тот же набор, заменит свои же
//! файлы по именам внутри `cards/` и `media/`, удалит устаревшее по прежнему
//! манифесту и запишет новый. Всё, что оказалось в каталоге не по этим правилам,
//! остаётся на месте и называется в диагностике.

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

/// Что представляет собой запись внутри каталога отчёта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Обычный файл.
    File,
    /// Каталог.
    Directory,
    /// Символическая ссылка — на файл или на каталог.
    Symlink,
    /// Что-то ещё: сокет, устройство, именованный канал.
    Other,
}

impl EntryKind {
    /// Имя вида для диагностики.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Other => "other",
        }
    }
}

/// Запись внутри каталога отчёта.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Относительный путь от корня каталога.
    pub path: String,
    /// Что это.
    pub kind: EntryKind,
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
        /// Чужие записи не удаляются и не перезаписываются: они принадлежат не
        /// отчёту, а тому, кто их положил. В списке есть и файлы, и каталоги:
        /// «не наш» — это свойство записи, а не её вида. Символической ссылки
        /// здесь быть не может — она отвергается раньше, потому что право
        /// заменить имя ссылке не выдаётся.
        foreign: Vec<String>,
    },
}

/// Проверяет, кому принадлежит каталог отчёта.
///
/// # Errors
///
/// [`ErrorCode::InvalidRequest`], если каталог не пуст и не доказывает владение
/// (`out_dir_not_owned`), если манифест испорчен, чужой или ссылается на путь
/// вне каталога (`out_dir_manifest_invalid`), если `--out` сам является
/// символической ссылкой (`out_dir_is_symlink`) или если внутри каталога лежит
/// ссылка либо запись, противоречащая доказательству владения
/// (`out_dir_entry_conflict`). Отказ приходит **до** любых изменений.
pub fn inspect(out: &Path) -> Result<Ownership, DomainError> {
    match fs::symlink_metadata(out) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Ownership::Free),
        Err(error) => {
            return Err(DomainError::with_details(
                ErrorCode::WriteFailed,
                format!("{} не читается: {error}", out.display()),
                details! {
                    "reason" => "out_dir_unreadable",
                    "path" => out.display().to_string(),
                },
            ));
        }
        Ok(metadata) => {
            // Каталог отчёта — это каталог, а не ссылка на него: за ссылкой
            // владение оказалось бы владением над тем, чего мы не выбирали.
            if metadata.file_type().is_symlink() {
                return Err(DomainError::with_details(
                    ErrorCode::InvalidRequest,
                    format!(
                        "каталог отчёта {} является символической ссылкой: отчёт пишется только \
                         в настоящий каталог",
                        out.display()
                    ),
                    details! {
                        "reason" => "out_dir_is_symlink",
                        "out_dir" => out.display().to_string(),
                    },
                ));
            }
            if !metadata.is_dir() {
                return Err(DomainError::with_details(
                    ErrorCode::InvalidRequest,
                    format!("{} существует и не является каталогом", out.display()),
                    details! {
                        "reason" => "out_dir_not_a_directory",
                        "out_dir" => out.display().to_string(),
                    },
                ));
            }
        }
    }

    let entries = entries_of(out)?;
    if entries.is_empty() {
        return Ok(Ownership::Free);
    }

    let manifest_path = out.join(MANIFEST_FILE);
    match fs::symlink_metadata(&manifest_path) {
        Err(_) => {
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
                    "entries" => entries
                        .iter()
                        .map(|entry| entry.path.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                },
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(manifest_invalid(
                &manifest_path,
                "доказательство владения не является обычным файлом",
                None,
            ));
        }
        Ok(_) => {}
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
    // что лежит внутри каталога отчёта, поэтому «../» здесь отвергается. Сам
    // манифест в списке быть не может: он не файл отчёта, а доказательство.
    let mut unique: BTreeSet<&str> = BTreeSet::new();
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
        if file == MANIFEST_FILE || !unique.insert(file.as_str()) {
            return Err(manifest_invalid(
                &manifest_path,
                "доказательство владения ссылается на себя или повторяется",
                Some(file),
            ));
        }
    }

    let owned: BTreeSet<&str> = manifest.files.iter().map(String::as_str).collect();
    let expected = expected_directories(manifest.files.iter());

    // Владение выдаёт право заменить имя внутри каталога отчёта. Значит, ни одна
    // запись не может быть ссылкой: ссылка — это другое имя для чего-то другого,
    // и по ней право ушло бы за пределы каталога. Каталог обязан быть каталогом, а
    // файл — файлом: «карточка» на месте каталога `cards` или каталог на месте
    // `index.html` — это две разные записи с одним именем, и отчёт не вправе
    // решать, какая из них его.
    for entry in &entries {
        let conflict = entry.kind == EntryKind::Symlink
            || (expected.contains(&entry.path) && entry.kind != EntryKind::Directory)
            || (owned.contains(entry.path.as_str()) && entry.kind != EntryKind::File);
        if conflict {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!(
                    "каталог отчёта {} содержит {} {}, и заменить её отчёт не имеет права: {}",
                    out.display(),
                    entry.kind.as_str(),
                    entry.path,
                    if entry.kind == EntryKind::Symlink {
                        "ссылка увела бы запись за пределы каталога отчёта"
                    } else {
                        "имя обязано означать ту же запись, что и в доказательстве владения"
                    }
                ),
                details! {
                    "reason" => "out_dir_entry_conflict",
                    "out_dir" => out.display().to_string(),
                    "entry" => entry.path.clone(),
                    "kind" => entry.kind.as_str(),
                },
            ));
        }
    }

    let foreign: Vec<String> = entries
        .iter()
        .filter(|entry| {
            let path = entry.path.as_str();
            path != MANIFEST_FILE && !owned.contains(path) && !expected.contains(path)
        })
        .map(|entry| entry.path.clone())
        .collect();

    Ok(Ownership::Owned {
        files: manifest.files.clone(),
        foreign,
    })
}

/// Все относительные пути каталога вместе с их видом.
///
/// Обход идёт по настоящим каталогам: ссылка на каталог называется ссылкой и
/// дальше не раскрывается, иначе содержимое за ссылкой попало бы в «внутри
/// отчёта», которым оно не является.
fn entries_of(root: &Path) -> Result<Vec<Entry>, DomainError> {
    let mut found: Vec<Entry> = Vec::new();
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
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                DomainError::with_details(
                    ErrorCode::WriteFailed,
                    format!("{} не читается: {error}", path.display()),
                    details! {
                        "reason" => "out_dir_unreadable",
                        "path" => path.display().to_string(),
                    },
                )
            })?;
            let file_type = metadata.file_type();
            let kind = if file_type.is_symlink() {
                EntryKind::Symlink
            } else if file_type.is_dir() {
                EntryKind::Directory
            } else if file_type.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            if kind == EntryKind::Directory {
                stack.push((path, relative.clone()));
            }
            found.push(Entry {
                path: relative,
                kind,
            });
        }
    }

    found.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(found)
}

/// Каталоги, которые обязаны существовать ради набора файлов.
///
/// Учитываются все предки каждого пути, а не только первый: `media/before/…`
/// требует и `media`, и `media/before`. Каталог отчёта в набор не входит: он не
/// создаётся ради файлов, он и есть то, что ими владеет.
fn expected_directories<'a>(files: impl IntoIterator<Item = &'a String>) -> BTreeSet<String> {
    let mut directories: BTreeSet<String> = BTreeSet::new();
    for file in files {
        let mut ancestor = Path::new(file.as_str()).parent();
        while let Some(directory) = ancestor {
            let name = directory.to_string_lossy().replace('\\', "/");
            if name.is_empty() {
                break;
            }
            directories.insert(name);
            ancestor = directory.parent();
        }
    }
    directories
}

/// Пространство имён отчёта: каталоги, имена внутри которых формирует только он.
///
/// Внутри них файл с таким путём — это либо файл отчёта, либо остаток
/// прерванного прогона, и заменить его можно. Корень каталога в это пространство
/// не входит: `index.html` лежит рядом с чем угодно, и заменить его можно только
/// по доказательству владения.
fn is_report_namespace(relative: &str) -> bool {
    relative.starts_with("cards/") || relative.starts_with("media/")
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
    /// [`ErrorCode::WriteFailed`], если целевой путь занят чужой записью, если
    /// файл нельзя перенести, если манифест нельзя записать или если записанный
    /// файл оказался вне каталога отчёта. Список `files` — это полный набор файлов
    /// отчёта: всё, что было в манифесте и не входит в него, удаляется как
    /// устаревшее.
    pub fn commit(self, files: &[String]) -> Result<CommitReport, DomainError> {
        let manifest = Manifest::new(files);

        // Проверка всего набора до первой мутации: каталог отчёта не меняется,
        // пока не выяснено, что ни один целевой путь не занят чужой записью и
        // что все каталоги, через которые пройдёт запись, — настоящие каталоги.
        let mut planned: Vec<(String, PathBuf)> = Vec::new();
        self.check_destination()?;
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
            self.check_parents(relative)?;
            let source = self.directory.join(relative);
            let metadata = fs::symlink_metadata(&source)
                .map_err(|error| write_failure("report_file_not_moved", &source, &error))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(
                    self.conflict(relative, "собранный файл отчёта не является обычным файлом")
                );
            }
            self.check_target(relative)?;
            planned.push((relative.clone(), source));
        }

        let current: BTreeSet<&str> = manifest.files.iter().map(String::as_str).collect();
        let mut stale: Vec<String> = Vec::new();
        for relative in &self.owned {
            if current.contains(relative.as_str()) || !is_inner_path(relative) {
                continue;
            }
            self.check_parents(relative)?;
            let path = self.destination.join(relative);
            match fs::symlink_metadata(&path) {
                Err(_) => {}
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_file() {
                        return Err(
                            self.conflict(relative, "устаревший файл отчёта заменён чужой записью")
                        );
                    }
                    stale.push(relative.clone());
                }
            }
        }

        // --- изменения начинаются здесь ---
        fs::create_dir_all(&self.destination).map_err(|error| {
            write_failure("staging_destination_not_created", &self.destination, &error)
        })?;

        for (relative, source) in &planned {
            let target = self.destination.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    write_failure("staging_destination_not_created", parent, &error)
                })?;
            }
            // `rename` заменяет файл целиком: читатель никогда не видит половину.
            fs::rename(source, &target)
                .map_err(|error| write_failure("report_file_not_moved", &target, &error))?;
        }

        let mut removed: Vec<String> = Vec::new();
        for relative in &stale {
            let path = self.destination.join(relative);
            fs::remove_file(&path)
                .map_err(|error| write_failure("stale_file_not_removed", &path, &error))?;
            removed.push(relative.clone());
        }

        // Пустые подкаталоги прежнего набора убираются только если они
        // действительно пусты: `remove_dir` не трогает каталог с содержимым. И
        // только те, что принадлежат структуре отчёта: каталог, заведённый не им,
        // отчёт не убирает.
        let mut expected = expected_directories(self.owned.iter());
        expected.extend(expected_directories(manifest.files.iter()));
        prune_empty_directories(&self.destination, &expected);

        // Манифест — последним: пока его нет, каталог считается прежним отчётом, и
        // следующий прогон не примет смесь за свой набор файлов.
        let manifest_path = self.destination.join(MANIFEST_FILE);
        let bytes = manifest.bytes()?;
        write::replace_document_atomically(&manifest_path, &bytes)?;

        // Последняя проверка отвечает на вопрос, на который проверки путей
        // ответить не могут: где файл оказался на самом деле.
        self.verify_committed(&manifest)?;

        Ok(CommitReport {
            files: manifest.files,
            moved: planned.into_iter().map(|(relative, _)| relative).collect(),
            removed,
        })
    }

    /// Проверяет сам целевой каталог.
    fn check_destination(&self) -> Result<(), DomainError> {
        match fs::symlink_metadata(&self.destination) {
            Err(_) => Ok(()),
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(self.conflict(
                        &self.destination.to_string_lossy(),
                        "каталог отчёта является символической ссылкой",
                    ));
                }
                if metadata.is_dir() {
                    return Ok(());
                }
                Err(DomainError::with_details(
                    ErrorCode::WriteFailed,
                    format!(
                        "{} существует и не является каталогом",
                        self.destination.display()
                    ),
                    details! {
                        "reason" => "out_dir_not_a_directory",
                        "out_dir" => self.destination.display().to_string(),
                    },
                ))
            }
        }
    }

    /// Проверяет каталоги, через которые пройдёт запись.
    ///
    /// Каталог, которого ещё нет, будет создан. Существующий обязан быть настоящим
    /// каталогом: символическая ссылка увела бы запись за пределы каталога отчёта,
    /// а не-каталог на месте родителя — это чужая запись, которую отчёт не вправе
    /// ни заменить, ни обойти.
    fn check_parents(&self, relative: &str) -> Result<(), DomainError> {
        let components: Vec<&str> = relative.split('/').collect();
        let mut prefix = PathBuf::new();
        for component in &components[..components.len().saturating_sub(1)] {
            prefix.push(component);
            let full = self.destination.join(&prefix);
            let Ok(metadata) = fs::symlink_metadata(&full) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                return Err(self.conflict(
                    &prefix.to_string_lossy(),
                    "каталог на пути файла отчёта является символической ссылкой",
                ));
            }
            if !metadata.is_dir() {
                return Err(self.conflict(
                    &prefix.to_string_lossy(),
                    "на пути файла отчёта лежит не каталог",
                ));
            }
        }
        Ok(())
    }

    /// Проверяет, свободен ли целевой путь.
    ///
    /// Заменить можно только файл отчёта: перечисленный в прежнем манифесте или
    /// лежащий в пространстве имён отчёта. Всё остальное — чужая запись, и отказ
    /// приходит до того, как хоть что-то изменилось.
    fn check_target(&self, relative: &str) -> Result<(), DomainError> {
        let target = self.destination.join(relative);
        let Ok(metadata) = fs::symlink_metadata(&target) else {
            return Ok(());
        };
        if metadata.file_type().is_symlink() {
            return Err(self.conflict(relative, "целевой путь занят символической ссылкой"));
        }
        if !metadata.is_file() {
            return Err(self.conflict(relative, "целевой путь занят не обычным файлом"));
        }
        if !self.owned.iter().any(|owned| owned == relative) && !is_report_namespace(relative) {
            return Err(self.conflict(
                relative,
                "целевой файл не перечислен в доказательстве владения",
            ));
        }
        Ok(())
    }

    /// Проверяет, что записанный отчёт физически лежит внутри каталога отчёта.
    ///
    /// Проверка идёт по настоящим путям, а не по строкам: если какой-то компонент
    /// пути оказался ссылкой, файл окажется снаружи — и это отказ, а не «перенос
    /// прошёл». Это последняя проверка переноса, и она единственная, которая
    /// отвечает на вопрос о самом файле, а не о намерении его записать.
    fn verify_committed(&self, manifest: &Manifest) -> Result<(), DomainError> {
        let root = fs::canonicalize(&self.destination).map_err(|error| {
            write_failure("report_file_not_confined", &self.destination, &error)
        })?;
        let mut managed: Vec<&str> = manifest.files.iter().map(String::as_str).collect();
        managed.push(MANIFEST_FILE);

        for relative in managed {
            let path = self.destination.join(relative);
            let real = fs::canonicalize(&path)
                .map_err(|error| write_failure("report_file_not_confined", &path, &error))?;
            if !real.starts_with(&root) || !real.is_file() {
                return Err(DomainError::with_details(
                    ErrorCode::WriteFailed,
                    format!(
                        "файл отчёта {relative} оказался не внутри каталога отчёта: {}",
                        real.display()
                    ),
                    details! {
                        "reason" => "report_file_not_confined",
                        "file" => relative.to_string(),
                        "real_path" => real.display().to_string(),
                        "out_dir" => root.display().to_string(),
                    },
                ));
            }
        }
        Ok(())
    }

    /// Отказ из-за чужой или противоречащей записи в каталоге отчёта.
    fn conflict(&self, relative: &str, reason: &str) -> DomainError {
        DomainError::with_details(
            ErrorCode::WriteFailed,
            format!(
                "каталог отчёта {} не изменён: {reason} ({relative})",
                self.destination.display()
            ),
            details! {
                "reason" => "out_dir_entry_conflict",
                "out_dir" => self.destination.display().to_string(),
                "entry" => relative.to_string(),
            },
        )
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

/// Убирает пустые каталоги структуры отчёта, оставшиеся от устаревших файлов.
///
/// Порядок — от самых глубоких к самым верхним: `media/before` убирается до
/// `media`, иначе `media` ещё не пуст. `remove_dir` не трогает каталог с
/// содержимым, поэтому каталог, заведённый не отчётом, остаётся на месте.
fn prune_empty_directories(root: &Path, expected: &BTreeSet<String>) {
    let mut candidates: Vec<&String> = expected.iter().collect();
    candidates.sort_by_key(|directory| std::cmp::Reverse(directory.matches('/').count()));
    for directory in candidates {
        let _ = fs::remove_dir(root.join(directory));
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

/// Отказ из-за негодного доказательства владения.
fn manifest_invalid(path: &Path, message: &str, entry: Option<&String>) -> DomainError {
    let mut map = serde_json::Map::new();
    map.insert(
        "reason".to_string(),
        serde_json::Value::String("out_dir_manifest_invalid".to_string()),
    );
    map.insert(
        "path".to_string(),
        serde_json::Value::String(path.display().to_string()),
    );
    if let Some(entry) = entry {
        map.insert(
            "entry".to_string(),
            serde_json::Value::String(entry.clone()),
        );
    }
    DomainError::with_details(
        ErrorCode::InvalidRequest,
        format!("{}: {message}", path.display()),
        serde_json::Value::Object(map),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use asset_store::temp_workspace::TempWorkspace;
    use std::ops::Deref;

    struct TempRoot {
        _workspace: TempWorkspace,
        path: PathBuf,
    }

    impl Deref for TempRoot {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            &self.path
        }
    }

    fn temp(label: &str) -> TempRoot {
        let workspace = TempWorkspace::create(&format!("anki-manifest-{label}"))
            .expect("временный каталог должен создаваться");
        let path = workspace.path().join("report");
        fs::create_dir(&path).expect("папка отчёта создаётся внутри workspace");
        TempRoot {
            _workspace: workspace,
            path,
        }
    }

    fn paths(entries: Vec<Entry>) -> Vec<String> {
        entries.into_iter().map(|entry| entry.path).collect()
    }

    fn symlink(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).expect("ссылка");
        #[cfg(not(unix))]
        drop((target, link));
    }

    fn write_manifest(root: &Path, files: &[&str]) {
        let files: Vec<String> = files.iter().map(|file| (*file).to_string()).collect();
        let manifest = Manifest::new(&files);
        fs::write(root.join(MANIFEST_FILE), manifest.bytes().expect("байты")).expect("манифест");
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
    fn a_manifest_cannot_claim_itself() {
        let root = temp("self-claim");
        fs::write(
            root.join(MANIFEST_FILE),
            format!(
                r#"{{"magic":"{MAGIC}","version":{VERSION},"files":["{MANIFEST_FILE}","index.html"]}}"#
            )
            .as_bytes(),
        )
        .expect("файл");
        fs::write(root.join("index.html"), "свой".as_bytes()).expect("файл");

        let error = inspect(&root).expect_err("манифест не выдаёт право на себя");
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
    fn a_directory_that_is_not_expected_is_named_as_foreign() {
        let root = temp("foreign-dir");
        write_manifest(&root, &["index.html", "media/before/a.png"]);
        fs::write(root.join("index.html"), "свой".as_bytes()).expect("файл");
        fs::create_dir_all(root.join("media/before")).expect("каталог");
        fs::write(root.join("media/before/a.png"), "медиа").expect("файл");
        fs::create_dir_all(root.join("чужая-папка")).expect("каталог");

        match inspect(&root).expect("осмотр") {
            Ownership::Owned { foreign, .. } => {
                // Структурные каталоги отчёта чужими не считаются: они объявлены
                // доказательством владения, а не появились сами.
                assert_eq!(foreign, vec!["чужая-папка".to_string()]);
            }
            Ownership::Free => panic!("каталог обязан быть своим"),
        }
    }

    #[test]
    fn a_symlink_inside_the_report_directory_is_a_conflict() {
        let root = temp("symlink-conflict");
        let outside = temp("symlink-conflict-outside");
        write_manifest(&root, &["index.html"]);
        fs::write(root.join("index.html"), "свой".as_bytes()).expect("файл");
        symlink(&outside, &root.join("ссылка"));

        let error = inspect(&root).expect_err("ссылка внутри каталога отчёта отвергается");
        assert_eq!(error.details["reason"], "out_dir_entry_conflict");
        assert_eq!(error.details["kind"], "symlink");
    }

    #[test]
    fn a_symlink_at_a_manifest_path_is_a_conflict() {
        let root = temp("symlink-at-file");
        let outside = temp("symlink-at-file-outside");
        fs::write(outside.join("index.html"), "снаружи".as_bytes()).expect("файл");
        write_manifest(&root, &["index.html"]);
        symlink(&outside.join("index.html"), &root.join("index.html"));

        let error = inspect(&root).expect_err("право заменить имя не выдаётся ссылке");
        assert_eq!(error.details["reason"], "out_dir_entry_conflict");
        assert_eq!(
            fs::read_to_string(outside.join("index.html")).expect("файл"),
            "снаружи",
            "цель ссылки не тронута"
        );
    }

    #[test]
    fn a_symlinked_directory_is_not_walked_into() {
        let root = temp("symlink-walk");
        let outside = temp("symlink-walk-outside");
        write_manifest(&root, &["index.html"]);
        fs::write(root.join("index.html"), "свой".as_bytes()).expect("файл");
        fs::write(outside.join("чужое.txt"), "снаружи").expect("файл");
        symlink(&outside, &root.join("cards"));

        let entries = entries_of(&root).expect("обход");
        let names = paths(entries);
        assert_eq!(
            names,
            vec![
                "cards".to_string(),
                "index.html".to_string(),
                MANIFEST_FILE.to_string()
            ],
            "содержимое каталога за ссылкой в каталог отчёта не входит"
        );
    }

    #[test]
    fn a_symlinked_out_dir_is_refused() {
        let root = temp("symlink-out");
        let real = temp("symlink-out-real");
        let link = root.join("отчёт");
        symlink(&real, &link);

        let error = inspect(&link).expect_err("ссылка вместо каталога отчёта отвергается");
        assert_eq!(error.details["reason"], "out_dir_is_symlink");
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
        let files = paths(entries_of(&root).expect("обход"));
        assert_eq!(
            files,
            vec![
                "cards".to_string(),
                "cards/card-0009.html".to_string(),
                "index.html".to_string(),
                MANIFEST_FILE.to_string(),
            ]
        );
        assert_eq!(
            fs::read(root.join("index.html")).expect("файл"),
            "новый index".as_bytes()
        );
        assert!(!root.join("cards/card-0001.html").exists());
    }

    #[test]
    fn staging_a_new_report_is_refused_when_a_target_name_is_foreign() {
        // Прежний отчёт владеет только `index.html`. `index.html` переписывается
        // по доказательству владения, а вот чужой файл с тем же именем, что и
        // новый документ отчёта в корне, — нет: отказ приходит до изменений.
        let root = temp("target-foreign");
        write_manifest(&root, &["cards/card-0001.html"]);
        fs::create_dir_all(root.join("cards")).expect("каталог");
        fs::write(root.join("cards/card-0001.html"), "прежний".as_bytes()).expect("файл");
        fs::write(root.join("index.html"), "чужое".as_bytes()).expect("файл");

        let ownership = inspect(&root).expect("осмотр");
        assert!(
            matches!(&ownership, Ownership::Owned { foreign, .. } if foreign == &vec!["index.html".to_string()]),
            "чужой index.html назван: {ownership:?}"
        );
        let staging = Staging::begin(&root, &ownership).expect("сборка");
        fs::create_dir_all(staging.path().join("cards")).expect("каталог");
        fs::write(staging.path().join("index.html"), "новое".as_bytes()).expect("файл");
        fs::write(
            staging.path().join("cards/card-0002.html"),
            "новое".as_bytes(),
        )
        .expect("файл");

        let error = staging
            .commit(&["index.html".to_string(), "cards/card-0002.html".to_string()])
            .expect_err("чужой файл в корне не заменяется");
        assert_eq!(error.details["reason"], "out_dir_entry_conflict");
        assert_eq!(error.details["entry"], "index.html");
        // Ни одна мутация не состоялась.
        assert_eq!(
            fs::read_to_string(root.join("index.html")).expect("файл"),
            "чужое"
        );
        assert_eq!(
            fs::read_to_string(root.join("cards/card-0001.html")).expect("файл"),
            "прежний"
        );
        assert!(!root.join("cards/card-0002.html").exists());
    }

    #[test]
    fn staging_may_replace_its_own_leftovers_after_an_interrupted_run() {
        // Прерывание между переносом файлов и записью манифеста оставляет файл
        // отчёта, которого в прежнем манифесте нет. Имя внутри `cards/` формирует
        // отчёт, поэтому повторный прогон сходится к тому же набору.
        let root = temp("interrupted");
        write_manifest(&root, &["index.html"]);
        fs::write(root.join("index.html"), "прежний".as_bytes()).expect("файл");
        fs::create_dir_all(root.join("cards")).expect("каталог");
        fs::write(root.join("cards/card-0009.html"), "остаток".as_bytes()).expect("файл");

        let ownership = inspect(&root).expect("осмотр");
        let staging = Staging::begin(&root, &ownership).expect("сборка");
        fs::create_dir_all(staging.path().join("cards")).expect("каталог");
        fs::write(staging.path().join("index.html"), "новый".as_bytes()).expect("файл");
        fs::write(
            staging.path().join("cards/card-0009.html"),
            "новый".as_bytes(),
        )
        .expect("файл");
        staging
            .commit(&["index.html".to_string(), "cards/card-0009.html".to_string()])
            .expect("перенос");

        assert_eq!(
            fs::read_to_string(root.join("cards/card-0009.html")).expect("файл"),
            "новый"
        );
        assert!(inspect(&root).is_ok());
    }

    #[test]
    fn staging_is_refused_when_a_parent_is_a_symlink() {
        let root = temp("parent-symlink");
        let outside = temp("parent-symlink-outside");
        write_manifest(&root, &["index.html"]);
        fs::write(root.join("index.html"), "прежний".as_bytes()).expect("файл");
        symlink(&outside, &root.join("cards"));

        let error = inspect(&root).expect_err("ссылка уже отвергнута осмотром");
        assert_eq!(error.details["reason"], "out_dir_entry_conflict");

        // Даже если ссылка появилась после осмотра, перенос её не раскрывает.
        fs::remove_file(root.join("cards")).expect("ссылка");
        let ownership = inspect(&root).expect("осмотр");
        let staging = Staging::begin(&root, &ownership).expect("сборка");
        fs::create_dir_all(staging.path().join("cards")).expect("каталог");
        fs::write(staging.path().join("index.html"), "новый".as_bytes()).expect("файл");
        fs::write(
            staging.path().join("cards/card-0001.html"),
            "новый".as_bytes(),
        )
        .expect("файл");
        symlink(&outside, &root.join("cards"));

        let error = staging
            .commit(&["index.html".to_string(), "cards/card-0001.html".to_string()])
            .expect_err("родитель-ссылка отвергается");
        assert_eq!(error.details["reason"], "out_dir_entry_conflict");
        assert!(
            !outside.join("card-0001.html").exists(),
            "запись не ушла за ссылку"
        );
        assert_eq!(
            fs::read_to_string(root.join("index.html")).expect("файл"),
            "прежний",
            "отказ пришёл до первой мутации"
        );
    }

    #[test]
    fn staging_is_refused_when_a_target_is_a_symlink() {
        let root = temp("target-symlink");
        let outside = temp("target-symlink-outside");
        write_manifest(&root, &["index.html"]);
        fs::write(outside.join("цель.html"), "снаружи".as_bytes()).expect("файл");
        symlink(&outside.join("цель.html"), &root.join("index.html"));

        let error = inspect(&root).expect_err("ссылка уже отвергнута осмотром");
        assert_eq!(error.details["reason"], "out_dir_entry_conflict");
        assert_eq!(
            fs::read_to_string(outside.join("цель.html")).expect("файл"),
            "снаружи",
            "цель ссылки не тронута"
        );
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
        let leftovers: Vec<String> = paths(entries_of(&root).expect("обход"))
            .into_iter()
            .filter(|name| name.contains("staging"))
            .collect();
        assert!(leftovers.is_empty(), "каталог сборки убран: {leftovers:?}");
    }

    #[test]
    fn a_staged_file_that_is_missing_is_refused_before_any_change() {
        let root = temp("missing-staged");
        write_manifest(&root, &["index.html"]);
        fs::write(root.join("index.html"), "прежний".as_bytes()).expect("файл");

        let ownership = inspect(&root).expect("осмотр");
        let staging = Staging::begin(&root, &ownership).expect("сборка");
        fs::write(staging.path().join("index.html"), "новый".as_bytes()).expect("файл");

        let error = staging
            .commit(&["index.html".to_string(), "cards/нет.html".to_string()])
            .expect_err("отсутствующий собранный файл отвергается");
        assert_eq!(error.details["reason"], "report_file_not_moved");
        assert_eq!(
            fs::read_to_string(root.join("index.html")).expect("файл"),
            "прежний",
            "отказ пришёл до первой мутации"
        );
    }

    #[test]
    fn every_committed_file_is_physically_inside_the_report_directory() {
        let root = temp("confined");
        write_manifest(&root, &["index.html"]);
        fs::write(root.join("index.html"), "прежний".as_bytes()).expect("файл");

        let ownership = inspect(&root).expect("осмотр");
        let staging = Staging::begin(&root, &ownership).expect("сборка");
        fs::create_dir_all(staging.path().join("media/before")).expect("каталог");
        fs::write(staging.path().join("index.html"), "новый".as_bytes()).expect("файл");
        fs::write(
            staging.path().join("media/before/a.png"),
            "медиа".as_bytes(),
        )
        .expect("файл");
        staging
            .commit(&["index.html".to_string(), "media/before/a.png".to_string()])
            .expect("перенос");

        let root_real = fs::canonicalize(&*root).expect("настоящий путь");
        for relative in ["index.html", "media/before/a.png", MANIFEST_FILE] {
            let real = fs::canonicalize(root.join(relative)).expect("настоящий путь");
            assert!(real.starts_with(&root_real), "{relative} внутри каталога");
        }
    }

    #[test]
    fn empty_directories_of_the_report_are_pruned_but_foreign_ones_stay() {
        let root = temp("prune");
        write_manifest(&root, &["cards/card-0001.html"]);
        fs::create_dir_all(root.join("cards")).expect("каталог");
        fs::write(root.join("cards/card-0001.html"), "старое".as_bytes()).expect("файл");
        fs::create_dir_all(root.join("чужая-папка")).expect("каталог");

        let ownership = inspect(&root).expect("осмотр");
        let staging = Staging::begin(&root, &ownership).expect("сборка");
        fs::write(staging.path().join("index.html"), "новый".as_bytes()).expect("файл");
        staging
            .commit(&["index.html".to_string()])
            .expect("перенос");

        assert!(
            !root.join("cards").exists(),
            "опустевший каталог отчёта убран"
        );
        assert!(root.join("чужая-папка").is_dir(), "чужой каталог остаётся");
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
    fn a_manifest_may_not_grant_a_directory_named_like_a_file() {
        let root = temp("dir-on-file");
        write_manifest(&root, &["index.html"]);
        fs::create_dir_all(root.join("index.html")).expect("каталог");

        let error = inspect(&root).expect_err("каталог на месте файла отчёта отвергается");
        assert_eq!(error.details["reason"], "out_dir_entry_conflict");
        assert_eq!(error.details["kind"], "directory");
    }

    #[test]
    fn a_file_on_the_place_of_a_structural_directory_is_a_conflict() {
        let root = temp("file-on-dir");
        write_manifest(&root, &["cards/card-0001.html"]);
        fs::write(root.join("cards"), "не каталог".as_bytes()).expect("файл");

        let error = inspect(&root).expect_err("файл на месте каталога отчёта отвергается");
        assert_eq!(error.details["reason"], "out_dir_entry_conflict");
        assert_eq!(error.details["kind"], "file");
    }

    #[test]
    fn the_report_namespace_is_cards_and_media_only() {
        assert!(is_report_namespace("cards/card-0001.html"));
        assert!(is_report_namespace("media/before/a.png"));
        assert!(!is_report_namespace("index.html"));
        assert!(!is_report_namespace("карточки/card-0001.html"));
    }

    #[test]
    fn expected_directories_cover_every_ancestor() {
        let directories = expected_directories(
            [
                "index.html".to_string(),
                "cards/card-0001.html".to_string(),
                "media/before/a.png".to_string(),
            ]
            .iter(),
        );
        assert_eq!(
            directories.into_iter().collect::<Vec<_>>(),
            vec![
                "cards".to_string(),
                "media".to_string(),
                "media/before".to_string()
            ]
        );
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
