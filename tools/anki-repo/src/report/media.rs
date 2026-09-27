//! Копирование media в каталог отчёта — с сохранением границы каталога.
//!
//! Отчёт обязан открываться офлайн, а превью обязано показывать те же картинки и
//! звуки, что увидит Anki. Значит, файлы нужно положить рядом с отчётом, и
//! именно на этом шаге появляется риск: ссылка в поле заметки может указывать
//! куда угодно. Правила поэтому жёсткие и без исключений:
//!
//! - имя нормализуется до базового ([`crate::media::normalize_media_name`]);
//!   всё, что было путём, остаётся диагностикой, а не путём для чтения;
//! - символическая ссылка не читается вовсе: иначе проверка «только базовое имя»
//!   обходится ссылкой внутри `media/`;
//! - запись возможна только в подкаталог `media` каталога отчёта, а имена
//!   проверяются повторно — второй, независимой проверкой нельзя пренебречь,
//!   потому что ошибка здесь означает запись за пределы каталога отчёта;
//! - удалённые ссылки (`http://`, `https://`, `//`, `data:`) не скачиваются и не
//!   подменяются: инструмент не ходит в сеть, а отчёт не должен тихо ломаться.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::media::normalize_media_name;

/// Подкаталог отчёта, в который копируются файлы.
pub const MEDIA_SUBDIR: &str = "media";

/// Максимум копируемых файлов.
pub const MAX_MEDIA_COPIES: usize = 4096;

/// Максимум размера одного копируемого файла.
pub const MAX_MEDIA_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// План работы с media: что скопировано и что с этим делать в HTML.
#[derive(Debug, Clone, Default)]
pub struct MediaPlan {
    /// Сырая ссылка → нормализованное имя скопированного файла.
    resolved: BTreeMap<String, String>,
    /// Относительные пути скопированных файлов внутри каталога отчёта.
    pub copied: Vec<String>,
    /// Нормализованные имена, для которых файл не найден.
    pub missing: Vec<String>,
    /// Ссылки, пытавшиеся выйти за пределы каталога `media` источника.
    pub traversal: Vec<String>,
    /// Имена, файл которых превысил [`MAX_MEDIA_FILE_BYTES`].
    pub oversized: Vec<String>,
    /// Ссылки на внешние ресурсы: не скачиваются.
    pub remote: Vec<String>,
    /// Имена, пропущенные из-за символической ссылки.
    pub symlinks: Vec<String>,
    /// Сколько ссылок не обработано из-за предела [`MAX_MEDIA_COPIES`].
    pub budget_skipped: usize,
}

impl MediaPlan {
    /// Нормализованное имя файла, если он скопирован в отчёт.
    #[must_use]
    pub fn resolved_name(&self, reference: &str) -> Option<&str> {
        self.resolved.get(reference).map(String::as_str)
    }

    /// Пустая ли работа с media.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.copied.is_empty()
    }
}

/// Готовит media для отчёта.
///
/// `sources` — каталоги экспорта в порядке приоритета: файл берётся из первого,
/// где он есть. `references` — сырые ссылки из значений полей.
///
/// # Errors
///
/// [`ErrorCode::WriteFailed`], если каталог `media` в отчёте нельзя создать или
/// файл нельзя записать.
pub fn plan(
    sources: &[PathBuf],
    references: &BTreeSet<String>,
    out_dir: &Path,
) -> Result<MediaPlan, DomainError> {
    let mut plan = MediaPlan::default();
    let media_dir = out_dir.join(MEDIA_SUBDIR);
    let mut created = false;

    for reference in references {
        if is_remote(reference) {
            plan.remote.push(reference.clone());
            continue;
        }

        let name = normalize_media_name(reference);
        if name != *reference {
            plan.traversal.push(reference.clone());
        }

        ensure_plain_name(&name)?;

        if !plan.copied.iter().any(|copied| copied == &name) {
            if plan.copied.len() >= MAX_MEDIA_COPIES {
                plan.budget_skipped += 1;
                continue;
            }

            let Some(found) = find_file(sources, &name) else {
                if !plan.missing.contains(&name) {
                    plan.missing.push(name.clone());
                }
                continue;
            };

            let size = fs::metadata(&found)
                .map_err(|error| write_error(&found, &error))?
                .len();
            if size > MAX_MEDIA_FILE_BYTES {
                plan.oversized.push(name.clone());
                continue;
            }

            if !created {
                fs::create_dir_all(&media_dir).map_err(|error| write_error(&media_dir, &error))?;
                created = true;
            }

            let bytes = fs::read(&found).map_err(|error| write_error(&found, &error))?;
            let target = media_dir.join(&name);
            fs::write(&target, &bytes).map_err(|error| write_error(&target, &error))?;
            plan.copied.push(name.clone());
        }

        if plan.copied.iter().any(|copied| copied == &name) {
            plan.resolved.insert(reference.clone(), name);
        }
    }

    plan.copied.sort();
    plan.missing.sort();
    plan.traversal.sort();
    plan.oversized.sort();
    plan.remote.sort();
    plan.symlinks.sort();

    Ok(plan)
}

/// Внешняя ли ссылка: скачивать её инструмент не будет.
#[must_use]
pub fn is_remote(reference: &str) -> bool {
    let trimmed = reference.trim();
    trimmed.starts_with("//")
        || trimmed.starts_with('/')
        || trimmed.contains("://")
        || trimmed.starts_with("data:")
        || trimmed.starts_with("mailto:")
        || trimmed.starts_with('#')
}

/// Ищет файл в каталогах-источниках по приоритету.
fn find_file(sources: &[PathBuf], name: &str) -> Option<PathBuf> {
    for source in sources {
        let candidate = source.join(MEDIA_SUBDIR).join(name);
        let Ok(metadata) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        return Some(candidate);
    }
    None
}

/// Проверяет, что имя пригодно для записи внутрь каталога отчёта.
fn ensure_plain_name(name: &str) -> Result<(), DomainError> {
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0');

    if ok {
        return Ok(());
    }

    Err(DomainError::with_details(
        ErrorCode::Internal,
        format!("нормализованное имя media {name:?} не является базовым именем файла"),
        details! {
            "reason" => "media_name_not_plain",
            "name" => name,
        },
    ))
}

/// Диагностическая запись о найденной символической ссылке.
pub fn note_symlinks(plan: &mut MediaPlan, sources: &[PathBuf], references: &BTreeSet<String>) {
    for reference in references {
        if is_remote(reference) {
            continue;
        }
        let name = normalize_media_name(reference);
        let symlink = sources.iter().any(|source| {
            fs::symlink_metadata(source.join(MEDIA_SUBDIR).join(&name))
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
        });
        if symlink && !plan.symlinks.contains(&name) {
            plan.symlinks.push(name);
        }
    }
    plan.symlinks.sort();
}

fn write_error(path: &Path, error: &std::io::Error) -> DomainError {
    DomainError::with_details(
        ErrorCode::WriteFailed,
        format!(
            "не удалось записать media отчёта {}: {error}",
            path.display()
        ),
        details! {
            "reason" => "media_copy_failed",
            "path" => path.display().to_string(),
            "message" => error.to_string(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("каталог");
        }
        fs::write(path, bytes).expect("файл");
    }

    #[test]
    fn remote_references_are_recognised() {
        assert!(is_remote("https://example.com/a.png"));
        assert!(is_remote("//example.com/a.png"));
        assert!(is_remote("/etc/passwd"));
        assert!(is_remote("data:image/png;base64,AAAA"));
        assert!(!is_remote("a.png"));
        assert!(!is_remote("音.mp3"));
    }

    #[test]
    fn copies_from_the_first_source_that_has_the_file() {
        let dir = crate::test_support::TempDir::new("report-media-first");
        let before = dir.path().join("before");
        let after = dir.path().join("after");
        let out = dir.path().join("out");
        write(&before.join("media/a.png"), b"before");
        write(&after.join("media/a.png"), b"after");

        let references: BTreeSet<String> = ["a.png".to_string()].into_iter().collect();
        let plan = plan(&[after.clone(), before], &references, &out).expect("план");

        assert_eq!(plan.copied, vec!["a.png".to_string()]);
        assert_eq!(
            fs::read(out.join("media/a.png")).expect("файл"),
            b"after".to_vec()
        );
        assert_eq!(plan.resolved_name("a.png"), Some("a.png"));
    }

    #[test]
    fn traversal_is_reported_and_reduced_to_the_basename() {
        let dir = crate::test_support::TempDir::new("report-media-traversal");
        let after = dir.path().join("after");
        let out = dir.path().join("out");
        write(&after.join("media/a.png"), b"x");

        let references: BTreeSet<String> = ["../../a.png".to_string()].into_iter().collect();
        let plan = plan(&[after], &references, &out).expect("план");

        assert_eq!(plan.traversal, vec!["../../a.png".to_string()]);
        assert_eq!(plan.copied, vec!["a.png".to_string()]);
        assert_eq!(
            plan.resolved_name("../../a.png"),
            Some("a.png"),
            "для подстановки в HTML берётся только базовое имя"
        );
    }

    #[test]
    fn missing_files_are_reported_not_invented() {
        let dir = crate::test_support::TempDir::new("report-media-missing");
        let after = dir.path().join("after");
        let out = dir.path().join("out");
        let references: BTreeSet<String> = ["нет.png".to_string()].into_iter().collect();

        let plan = plan(&[after], &references, &out).expect("план");

        assert!(plan.copied.is_empty());
        assert_eq!(plan.missing, vec!["нет.png".to_string()]);
        assert_eq!(plan.resolved_name("нет.png"), None);
        assert!(!out.join("media").exists(), "пустой каталог не создаётся");
    }

    #[test]
    fn symlinked_media_is_not_read() {
        let dir = crate::test_support::TempDir::new("report-media-symlink");
        let after = dir.path().join("after");
        let out = dir.path().join("out");
        let secret = dir.path().join("secret.png");
        write(&secret, b"secret");
        fs::create_dir_all(after.join("media")).expect("каталог");
        std::os::unix::fs::symlink(&secret, after.join("media/a.png")).expect("ссылка");

        let references: BTreeSet<String> = ["a.png".to_string()].into_iter().collect();
        let mut plan = plan(std::slice::from_ref(&after), &references, &out).expect("план");
        note_symlinks(&mut plan, &[after], &references);

        assert!(plan.copied.is_empty());
        assert_eq!(plan.symlinks, vec!["a.png".to_string()]);
        assert!(plan.missing.contains(&"a.png".to_string()));
    }
}
