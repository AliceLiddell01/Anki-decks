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
//!
//! Media каждого состояния лежит в своём подкаталоге (`media/before`,
//! `media/after`), потому что состояния не обязаны совпадать содержимым.
//! Одинаковое имя файла может быть в обоих экспортах с разными байтами, а
//! отсутствующий файл «до» не имеет права «исправиться» файлом из «после»:
//! превью «до» обязано показывать то, что было в «до».

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::media::normalize_media_name;
use crate::report::SideState;

/// Подкаталог отчёта, в который копируются файлы.
pub const MEDIA_SUBDIR: &str = "media";

/// Максимум копируемых файлов.
pub const MAX_MEDIA_COPIES: usize = 4096;

/// Максимум размера одного копируемого файла.
pub const MAX_MEDIA_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Работа с media одного состояния.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateMedia {
    /// Нормализованные имена скопированных файлов.
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
}

/// План работы с media: что скопировано и что с этим делать в HTML.
#[derive(Debug, Clone, Default)]
pub struct MediaPlan {
    /// Состояние и сырая ссылка → путь относительно каталога отчёта.
    resolved: BTreeMap<(SideState, String), String>,
    /// Работа с media по состояниям.
    pub states: BTreeMap<SideState, StateMedia>,
    /// Сколько ссылок не обработано из-за предела [`MAX_MEDIA_COPIES`].
    pub budget_skipped: usize,
}

impl MediaPlan {
    /// Путь файла внутри каталога отчёта, если он скопирован для этого состояния.
    #[must_use]
    pub fn resolved_path(&self, state: SideState, reference: &str) -> Option<&str> {
        self.resolved
            .get(&(state, reference.to_string()))
            .map(String::as_str)
    }

    /// Работа с media состояния.
    #[must_use]
    pub fn state(&self, state: SideState) -> StateMedia {
        self.states.get(&state).cloned().unwrap_or_default()
    }

    /// Сколько файлов скопировано всего.
    #[must_use]
    pub fn copied_total(&self) -> usize {
        self.states.values().map(|state| state.copied.len()).sum()
    }

    /// Имена отсутствующих файлов обоих состояний без повторов.
    #[must_use]
    pub fn missing_union(&self) -> Vec<String> {
        self.union(|state| &state.missing)
    }

    /// Ссылки, выходившие за пределы `media/`, без повторов.
    #[must_use]
    pub fn traversal_union(&self) -> Vec<String> {
        self.union(|state| &state.traversal)
    }

    /// Внешние ссылки обоих состояний без повторов.
    #[must_use]
    pub fn remote_union(&self) -> Vec<String> {
        self.union(|state| &state.remote)
    }

    /// Имена символических ссылок обоих состояний без повторов.
    #[must_use]
    pub fn symlinks_union(&self) -> Vec<String> {
        self.union(|state| &state.symlinks)
    }

    /// Имена слишком больших файлов обоих состояний без повторов.
    #[must_use]
    pub fn oversized_union(&self) -> Vec<String> {
        self.union(|state| &state.oversized)
    }

    fn union(&self, pick: impl Fn(&StateMedia) -> &Vec<String>) -> Vec<String> {
        let mut found: BTreeSet<String> = BTreeSet::new();
        for state in self.states.values() {
            found.extend(pick(state).iter().cloned());
        }
        found.into_iter().collect()
    }

    /// Пустая ли работа с media.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.copied_total() == 0
    }
}

/// Готовит media для отчёта.
///
/// `sources` — каталоги экспорта вместе с их состоянием: файл ищется только в
/// экспорте своего состояния. `references` — сырые ссылки из значений полей по
/// состояниям.
///
/// # Errors
///
/// [`ErrorCode::WriteFailed`], если каталог `media` в отчёте нельзя создать или
/// файл нельзя записать.
pub fn plan(
    sources: &[(SideState, PathBuf)],
    references: &BTreeMap<SideState, BTreeSet<String>>,
    out_dir: &Path,
) -> Result<MediaPlan, DomainError> {
    let mut plan = MediaPlan::default();
    let mut copied_total = 0usize;

    for state in SideState::ALL {
        let empty: BTreeSet<String> = BTreeSet::new();
        let state_references = references.get(&state).unwrap_or(&empty);
        if state_references.is_empty() {
            continue;
        }

        let source = sources
            .iter()
            .find(|(candidate, _)| *candidate == state)
            .map(|(_, path)| path.clone());
        let state_media_dir = out_dir.join(state.media_dir());
        let mut created = false;
        let entry = plan.states.entry(state).or_default();

        for reference in state_references {
            if is_remote(reference) {
                entry.remote.push(reference.clone());
                continue;
            }

            let name = normalize_media_name(reference);
            if name != *reference {
                entry.traversal.push(reference.clone());
            }

            ensure_plain_name(&name)?;

            if !entry.copied.contains(&name) {
                if copied_total >= MAX_MEDIA_COPIES {
                    plan.budget_skipped += 1;
                    continue;
                }

                let found = source.as_ref().and_then(|source| find_file(source, &name));
                let Some(found) = found else {
                    if !entry.missing.contains(&name) {
                        entry.missing.push(name.clone());
                    }
                    continue;
                };

                let size = fs::metadata(&found)
                    .map_err(|error| write_error(&found, &error))?
                    .len();
                if size > MAX_MEDIA_FILE_BYTES {
                    entry.oversized.push(name.clone());
                    continue;
                }

                if !created {
                    fs::create_dir_all(&state_media_dir)
                        .map_err(|error| write_error(&state_media_dir, &error))?;
                    created = true;
                }

                let bytes = fs::read(&found).map_err(|error| write_error(&found, &error))?;
                let target = state_media_dir.join(&name);
                fs::write(&target, &bytes).map_err(|error| write_error(&target, &error))?;
                entry.copied.push(name.clone());
                copied_total += 1;
            }

            if entry.copied.contains(&name) {
                plan.resolved.insert(
                    (state, reference.clone()),
                    format!("{}/{name}", state.media_dir()),
                );
            }
        }

        entry.copied.sort();
        entry.missing.sort();
        entry.traversal.sort();
        entry.oversized.sort();
        entry.remote.sort();
    }

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

/// Ищет файл в каталоге-источнике одного состояния.
fn find_file(source: &Path, name: &str) -> Option<PathBuf> {
    let candidate = source.join(MEDIA_SUBDIR).join(name);
    let metadata = fs::symlink_metadata(&candidate).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    Some(candidate)
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
pub fn note_symlinks(
    plan: &mut MediaPlan,
    sources: &[(SideState, PathBuf)],
    references: &BTreeMap<SideState, BTreeSet<String>>,
) {
    for (state, source) in sources {
        let Some(state_references) = references.get(state) else {
            continue;
        };
        let entry = plan.states.entry(*state).or_default();
        for reference in state_references {
            if is_remote(reference) {
                continue;
            }
            let name = normalize_media_name(reference);
            let symlink = fs::symlink_metadata(source.join(MEDIA_SUBDIR).join(&name))
                .is_ok_and(|metadata| metadata.file_type().is_symlink());
            if symlink && !entry.symlinks.contains(&name) {
                entry.symlinks.push(name);
            }
        }
        entry.symlinks.sort();
    }
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

    fn references(state: SideState, names: &[&str]) -> BTreeMap<SideState, BTreeSet<String>> {
        let mut map: BTreeMap<SideState, BTreeSet<String>> = BTreeMap::new();
        map.insert(
            state,
            names.iter().map(|name| (*name).to_string()).collect(),
        );
        map
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
    fn each_state_copies_from_its_own_export() {
        let dir = crate::test_support::TempDir::new("report-media-states");
        let before = dir.path().join("before");
        let after = dir.path().join("after");
        let out = dir.path().join("out");
        write(&before.join("media/a.png"), b"before");
        write(&after.join("media/a.png"), b"after");

        let mut references: BTreeMap<SideState, BTreeSet<String>> = BTreeMap::new();
        for state in SideState::ALL {
            references.insert(state, ["a.png".to_string()].into_iter().collect());
        }

        let plan = plan(
            &[
                (SideState::Before, before.clone()),
                (SideState::After, after.clone()),
            ],
            &references,
            &out,
        )
        .expect("план");

        // Одинаковое имя — два разных файла: состояние не имеет права взять
        // содержимое соседа.
        assert_eq!(
            fs::read(out.join(SideState::Before.media_dir()).join("a.png")).expect("файл"),
            b"before".to_vec()
        );
        assert_eq!(
            fs::read(out.join(SideState::After.media_dir()).join("a.png")).expect("файл"),
            b"after".to_vec()
        );
        assert_eq!(plan.copied_total(), 2);
        assert_eq!(
            plan.resolved_path(SideState::Before, "a.png"),
            Some("media/before/a.png")
        );
        assert_eq!(
            plan.resolved_path(SideState::After, "a.png"),
            Some("media/after/a.png")
        );
    }

    #[test]
    fn a_file_missing_in_before_is_not_taken_from_after() {
        let dir = crate::test_support::TempDir::new("report-media-before-missing");
        let before = dir.path().join("before");
        let after = dir.path().join("after");
        let out = dir.path().join("out");
        write(&after.join("media/a.png"), b"after");

        let mut references: BTreeMap<SideState, BTreeSet<String>> = BTreeMap::new();
        for state in SideState::ALL {
            references.insert(state, ["a.png".to_string()].into_iter().collect());
        }

        let plan = plan(
            &[
                (SideState::Before, before.clone()),
                (SideState::After, after.clone()),
            ],
            &references,
            &out,
        )
        .expect("план");

        assert_eq!(plan.missing_union(), vec!["a.png".to_string()]);
        assert_eq!(
            plan.state(SideState::Before).missing,
            vec!["a.png".to_string()]
        );
        assert!(plan.state(SideState::Before).copied.is_empty());
        assert_eq!(
            plan.state(SideState::After).copied,
            vec!["a.png".to_string()]
        );
        assert_eq!(plan.resolved_path(SideState::Before, "a.png"), None);
        assert!(
            !out.join(SideState::Before.media_dir()).exists(),
            "отсутствующий файл не выдумывается"
        );
    }

    #[test]
    fn traversal_is_reported_and_reduced_to_the_basename() {
        let dir = crate::test_support::TempDir::new("report-media-traversal");
        let after = dir.path().join("after");
        let out = dir.path().join("out");
        write(&after.join("media/a.png"), b"x");

        let plan = plan(
            &[(SideState::After, after)],
            &references(SideState::After, &["../../a.png"]),
            &out,
        )
        .expect("план");

        assert_eq!(plan.traversal_union(), vec!["../../a.png".to_string()]);
        assert_eq!(
            plan.state(SideState::After).copied,
            vec!["a.png".to_string()]
        );
        assert_eq!(
            plan.resolved_path(SideState::After, "../../a.png"),
            Some("media/after/a.png"),
            "для подстановки в HTML берётся только базовое имя"
        );
    }

    #[test]
    fn missing_files_are_reported_not_invented() {
        let dir = crate::test_support::TempDir::new("report-media-missing");
        let after = dir.path().join("after");
        let out = dir.path().join("out");

        let plan = plan(
            &[(SideState::After, after)],
            &references(SideState::After, &["нет.png"]),
            &out,
        )
        .expect("план");

        assert_eq!(plan.copied_total(), 0);
        assert_eq!(plan.missing_union(), vec!["нет.png".to_string()]);
        assert_eq!(plan.resolved_path(SideState::After, "нет.png"), None);
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

        let references = references(SideState::After, &["a.png"]);
        let sources = [(SideState::After, after)];
        let mut plan = plan(&sources, &references, &out).expect("план");
        note_symlinks(&mut plan, &sources, &references);

        assert_eq!(plan.copied_total(), 0);
        assert_eq!(plan.symlinks_union(), vec!["a.png".to_string()]);
        assert!(plan.missing_union().contains(&"a.png".to_string()));
    }
}
