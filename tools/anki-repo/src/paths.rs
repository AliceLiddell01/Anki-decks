//! Сравнение путей по факту, а не по написанию.
//!
//! Tool принимает пути от пользователя и обязан один раз ответить на вопрос
//! «это один и тот же файл?» — до того, как что-то запишет. Текстовое равенство
//! на него не отвечает: `decks/x/deck.json`, `./decks/x/deck.json`,
//! `decks/x/../x/deck.json` и символическая ссылка — это один файл, а
//! `decks/x/deck.json` и `/абсолютный/путь/decks/x/deck.json` — тоже.
//!
//! Отсюда одна реализация на весь toolkit, а не по копии в каждой операции.

use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Канонизирует настолько, насколько это возможно для ещё не созданного пути.
///
/// [`fs::canonicalize`] требует, чтобы путь существовал. Целевой файл отчёта
/// или sidecar'а обычно ещё не создан, но его родитель уже есть, поэтому
/// канонизируется самая длинная существующая часть, а несуществующий хвост
/// приписывается к ней как есть. Символические ссылки в существующей части при
/// этом раскрываются: `--out link-to-deck` и настоящий каталог экспорта дают
/// один путь, а не два разных.
#[must_use]
pub fn canonical_ish(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    let mut existing = absolute.clone();
    let mut tail: Vec<OsString> = Vec::new();
    // Хвост собирается по компонентам, а не по `file_name()`: путь, кончающийся
    // на `..`, имени файла не имеет, и именно так выглядит `не_создано/../f`.
    while !existing.exists() {
        let Some(component) = existing.components().next_back() else {
            break;
        };
        match component {
            Component::Normal(name) => tail.push(name.to_os_string()),
            Component::CurDir => tail.push(OsString::from(".")),
            Component::ParentDir => tail.push(OsString::from("..")),
            Component::RootDir | Component::Prefix(_) => break,
        }
        match existing.parent() {
            Some(parent) => existing = parent.to_path_buf(),
            None => break,
        }
    }

    let mut result = fs::canonicalize(&existing).unwrap_or(existing);
    for name in tail.iter().rev() {
        push_normalized(&mut result, name);
    }
    result
}

/// Приписывает компонент к пути, разбираясь с `.` и `..` лексически.
///
/// Хвост пути ещё не существует, поэтому файловая система `..` в нём не
/// разбирает: `не_создано/../sidecar.json` и `sidecar.json` обязаны дать один
/// путь, иначе проверка алиаса пропустила бы перезапись `deck.json` через
/// несуществующий промежуточный компонент.
fn push_normalized(path: &mut PathBuf, name: &OsString) {
    match name.to_str() {
        Some(".") => {}
        Some("..") => {
            let popped = path
                .components()
                .next_back()
                .is_some_and(|component| matches!(component, std::path::Component::Normal(_)));
            if popped {
                path.pop();
            } else {
                path.push(name);
            }
        }
        _ => path.push(name),
    }
}

/// Указывают ли два пути на один и тот же объект файловой системы.
///
/// Сравнение идёт по [`canonical_ish`], поэтому покрывает относительность,
/// `.`/`..` внутри пути и символические ссылки. Это проверка перед записью, а
/// не замена файловой семантике: она отвечает на вопрос «не затрём ли мы тот
/// самый файл», а не «какой из двух путей короче».
#[must_use]
pub fn paths_alias(first: &Path, second: &Path) -> bool {
    canonical_ish(first) == canonical_ish(second)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TempDir;

    #[test]
    fn dot_segments_and_spelling_do_not_hide_an_alias() {
        let dir = TempDir::new("paths-alias");
        let file = dir.path().join("deck.json");
        fs::write(&file, b"{}").expect("файл");

        assert!(paths_alias(&file, &file));
        assert!(paths_alias(&file, &dir.path().join(".").join("deck.json")));
        assert!(paths_alias(
            &file,
            &dir.path().join("sub").join("..").join("deck.json")
        ));
        assert!(paths_alias(&file, &Path::new("./").join(&file)));

        let other = dir.path().join("другой.json");
        assert!(!paths_alias(&file, &other));
    }

    #[test]
    fn a_symlink_is_the_same_file() {
        let dir = TempDir::new("paths-symlink");
        let file = dir.path().join("deck.json");
        fs::write(&file, b"{}").expect("файл");
        let link = dir.path().join("link.json");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&file, &link).expect("ссылка");
            assert!(paths_alias(&file, &link));
        }
    }

    #[test]
    fn a_path_that_does_not_exist_yet_is_still_compared() {
        let dir = TempDir::new("paths-missing");
        let missing = dir.path().join("нет").join("..").join("sidecar.json");
        assert_eq!(
            canonical_ish(&missing),
            canonical_ish(&dir.path().join("sidecar.json"))
        );
    }
}
