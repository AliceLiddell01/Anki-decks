//! Структурная сверка исходника и кандидата по путям JSON.
//!
//! Структурные операции toolkit'а (`create`, `retire`) отличаются от `edit`
//! тем, что меняют не одно значение поля, а набор сущностей: добавляют заметки
//! или дописывают тег. Для них мало «байтовый diff равен ожидаемому»: нужно
//! доказать, что **ничего другого не поехало** — ни модели, ни конфигурации
//! колод, ни порядок существующих заметок, ни `media_files`.
//!
//! Здесь считается не diff ради показа, а множество путей различий: его
//! сравнивает с разрешённым набором вызывающая операция. Значения по путям не
//! собираются — их проверяет сама операция точным равенством, потому что только
//! она знает, что именно собиралась записать.
//!
//! Каноническая форма `deck.json` и то, что исходник ей соответствует, дают
//! более сильный инвариант, чем diff байтов: `candidate_bytes` — это
//! канонический рендер значения кандидата, `source_bytes` — канонический рендер
//! значения исходника. Значит байты различаются ровно там, где различаются
//! значения, и никакого «массового переформатирования» остального файла при
//! этом быть не может.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde_json::{Map, Value};

/// Предел числа записанных путей различий по умолчанию.
pub const DEFAULT_PATH_LIMIT: usize = 4096;

/// Предельная глубина обхода: защита от неожиданной структуры и от рекурсии.
const MAX_DEPTH: usize = 64;

/// Пути различий двух JSON-деревьев.
#[derive(Debug, Default, Clone)]
pub struct ChangedPaths {
    /// Пути (RFC 6901 JSON Pointer) различий, не больше запрошенного предела.
    pub paths: Vec<String>,
    /// Сколько различий найдено всего.
    pub total: usize,
    /// Найдено больше различий, чем записанный предел.
    pub truncated: bool,
}

impl ChangedPaths {
    /// Есть ли различия вообще.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Первый путь в порядке обхода, в котором собран список различий.
    ///
    /// Порядок детерминирован, но не лексикографичен: обход идёт по дереву
    /// `after`, а ключи объектов перебираются отсортированными. Вызывающий код
    /// не должен придавать этому порядку смысла — для сравнения нужен
    /// [`Self::first_outside`].
    #[must_use]
    pub fn first(&self) -> Option<&str> {
        self.paths.first().map(String::as_str)
    }

    /// Все ли найденные пути начинаются с одной из разрешённых основ.
    ///
    /// Возвращает первый путь, который не подходит.
    #[must_use]
    pub fn first_outside<'a>(&'a self, allowed_prefixes: &[&str]) -> Option<&'a str> {
        self.paths.iter().map(String::as_str).find(|path| {
            !allowed_prefixes
                .iter()
                .any(|prefix| path.starts_with(prefix))
        })
    }
}

/// Собирает пути различий двух JSON-деревьев.
///
/// Массивы сравниваются как последовательности с общим префиксом: добавленный
/// хвост даёт путь каждого добавленного элемента, а любое изменение внутри
/// общего префикса — путь этого элемента. Поэтому «вставили элемент в
/// середину» и «переставили элементы» не могут выглядеть как добавление хвоста.
#[must_use]
pub fn changed_paths(before: &Value, after: &Value, limit: usize) -> ChangedPaths {
    let mut found = ChangedPaths::default();
    let mut path = String::new();
    walk(before, after, &mut path, &mut found, limit, 0);
    found
}

fn walk(
    before: &Value,
    after: &Value,
    path: &mut String,
    out: &mut ChangedPaths,
    limit: usize,
    depth: usize,
) {
    if before == after || out.truncated {
        return;
    }
    if depth >= MAX_DEPTH || out.paths.len() >= limit {
        // Остаток дерева считаем одним различием: доказать «всё остальное
        // не менялось» на этом уровне уже нельзя, и операция обязана упасть.
        record(path, out, limit);
        return;
    }

    match (before, after) {
        (Value::Object(left), Value::Object(right)) => {
            for key in union_keys(left, right) {
                let restore = push_key(path, key);
                match (left.get(key), right.get(key)) {
                    (Some(before), Some(after)) => walk(before, after, path, out, limit, depth + 1),
                    (Some(_), None) | (None, Some(_)) => record(path, out, limit),
                    (None, None) => {}
                }
                path.truncate(restore);
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            let common = left.len().min(right.len());
            for index in 0..common {
                let restore = push_index(path, index);
                walk(&left[index], &right[index], path, out, limit, depth + 1);
                path.truncate(restore);
            }
            for index in common..right.len() {
                let restore = push_index(path, index);
                record(path, out, limit);
                path.truncate(restore);
            }
            for index in common..left.len() {
                let restore = push_index(path, index);
                record(path, out, limit);
                path.truncate(restore);
            }
        }
        _ => record(path, out, limit),
    }
}

fn union_keys<'a>(
    left: &'a Map<String, Value>,
    right: &'a Map<String, Value>,
) -> BTreeSet<&'a str> {
    left.keys()
        .chain(right.keys())
        .map(String::as_str)
        .collect()
}

fn record(path: &str, out: &mut ChangedPaths, limit: usize) {
    out.total += 1;
    if out.paths.len() < limit {
        out.paths.push(path.to_string());
    } else {
        out.truncated = true;
    }
}

/// Дописывает шаг пути и возвращает длину, до которой его нужно откатить.
fn push_key(path: &mut String, key: &str) -> usize {
    let restore = path.len();
    path.push('/');
    for character in key.chars() {
        match character {
            '~' => path.push_str("~0"),
            '/' => path.push_str("~1"),
            other => path.push(other),
        }
    }
    restore
}

/// Дописывает индекс массива и возвращает длину для отката.
fn push_index(path: &mut String, index: usize) -> usize {
    let restore = path.len();
    path.push('/');
    let _ = write!(path, "{index}");
    restore
}

/// Ссылка на узел колоды по `children`-пути внутри JSON-дерева.
#[must_use]
pub fn deck_path_pointer(children: &[usize]) -> String {
    let mut pointer = String::new();
    for step in children {
        pointer.push_str("/children/");
        let _ = write!(pointer, "{step}");
    }
    pointer
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(before: &Value, after: &Value) -> ChangedPaths {
        changed_paths(before, after, DEFAULT_PATH_LIMIT)
    }

    #[test]
    fn identical_trees_have_no_differences() {
        let tree = json!({"notes": [{"guid": "a"}], "name": "x"});
        assert!(paths(&tree, &tree).is_empty());
    }

    #[test]
    fn appended_array_element_is_reported_by_its_own_path() {
        let before = json!({"notes": [1, 2]});
        let after = json!({"notes": [1, 2, 3, 4]});
        let found = paths(&before, &after);
        assert_eq!(found.paths, vec!["/notes/2", "/notes/3"]);
        assert_eq!(found.total, 2);
        assert!(!found.truncated);
    }

    #[test]
    fn shortened_array_reports_removed_tail() {
        let found = paths(&json!({"tags": ["a", "b"]}), &json!({"tags": ["a"]}));
        assert_eq!(found.paths, vec!["/tags/1"]);
    }

    #[test]
    fn reordered_array_is_not_mistaken_for_an_append() {
        let found = paths(&json!({"tags": ["a", "b"]}), &json!({"tags": ["b", "a"]}));
        assert_eq!(found.paths, vec!["/tags/0", "/tags/1"]);
    }

    #[test]
    fn added_and_removed_object_keys_are_reported() {
        let found = paths(&json!({"a": 1, "b": 2}), &json!({"a": 1, "c": 3}));
        assert_eq!(found.paths, vec!["/b", "/c"]);
    }

    #[test]
    fn nested_paths_use_json_pointer_escaping() {
        let before = json!({"a/b": {"~x": 1}});
        let after = json!({"a/b": {"~x": 2}});
        assert_eq!(paths(&before, &after).paths, vec!["/a~1b/~0x"]);
    }

    #[test]
    fn scalar_type_change_is_a_single_path() {
        let found = paths(&json!({"n": 1}), &json!({"n": "1"}));
        assert_eq!(found.paths, vec!["/n"]);
    }

    #[test]
    fn limit_marks_truncation_but_keeps_the_total() {
        let before = json!({"tags": []});
        let after = json!({"tags": [1, 2, 3, 4]});
        let found = changed_paths(&before, &after, 2);
        assert_eq!(found.paths, vec!["/tags/0", "/tags/1"]);
        assert_eq!(found.total, 4);
        assert!(found.truncated);
    }

    #[test]
    fn first_outside_finds_the_unexpected_path() {
        let found = paths(
            &json!({"notes": [1]}),
            &json!({"notes": [1, 2], "name": "x"}),
        );
        assert_eq!(found.first_outside(&["/notes"]), Some("/name"));
        assert_eq!(found.paths, vec!["/name", "/notes/1"]);
        assert_eq!(found.first(), Some("/name"));
    }

    #[test]
    fn depth_guard_stops_recursion_without_panicking() {
        let mut before = json!(1);
        let mut after = json!(2);
        for _ in 0..(MAX_DEPTH + 8) {
            before = json!([before]);
            after = json!([after]);
        }
        let found = paths(&before, &after);
        assert!(found.total >= 1);
    }

    #[test]
    fn deck_path_pointer_addresses_children() {
        assert_eq!(deck_path_pointer(&[]), "");
        assert_eq!(deck_path_pointer(&[0, 2]), "/children/0/children/2");
    }
}
