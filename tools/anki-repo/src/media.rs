//! Media-счётчики и безопасная проверка наличия файлов.
//!
//! `media_files` — это недоверенный список имён, а не содержимое media.
//! Tool никогда не конструирует из него filesystem path: сравниваются только
//! множества имён, а физические имена берутся листингом каталога `media/`.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::index::ExportIndex;

/// Имя каталога media внутри экспорта.
pub const MEDIA_DIR: &str = "media";

/// Сводка объявленного и физически присутствующего media.
#[derive(Debug)]
pub struct MediaReport {
    /// Существует ли каталог `media/`.
    pub dir_present: bool,
    /// Сколько имён объявлено суммарно по всем узлам (`media_files`).
    pub declared_total: usize,
    /// Уникальные объявленные basenames.
    pub declared: BTreeSet<String>,
    /// Имена, объявленные более одного раза.
    pub duplicate_declared: BTreeSet<String>,
    /// Объявленные значения, не являющиеся простым basename.
    pub not_plain_names: BTreeSet<String>,
    /// Фактические basenames из `media/`.
    pub physical: BTreeSet<String>,
}

impl MediaReport {
    /// Объявленные имена, для которых нет физического файла.
    pub fn missing_physical(&self) -> Vec<String> {
        self.declared.difference(&self.physical).cloned().collect()
    }

    /// Физические файлы, которые не объявлены ни в одном `media_files`.
    pub fn undeclared_physical(&self) -> Vec<String> {
        self.physical.difference(&self.declared).cloned().collect()
    }
}

/// Собирает media-сводку экспорта.
pub fn collect_media(export_dir: &Path, index: &ExportIndex<'_>) -> MediaReport {
    let mut declared_total = 0usize;
    let mut declared: BTreeSet<String> = BTreeSet::new();
    let mut duplicate_declared: BTreeSet<String> = BTreeSet::new();
    let mut not_plain_names: BTreeSet<String> = BTreeSet::new();

    for entry in &index.nodes {
        let mut seen_here: BTreeSet<&str> = BTreeSet::new();
        for name in &entry.node.media_files {
            declared_total += 1;
            let basename = normalize_media_name(name);
            if basename != name.as_str() {
                not_plain_names.insert(name.clone());
            }
            if !seen_here.insert(name.as_str()) {
                duplicate_declared.insert(name.clone());
            }
            declared.insert(basename);
        }
    }

    let media_dir = export_dir.join(MEDIA_DIR);
    let dir_present = media_dir.is_dir();
    let mut physical: BTreeSet<String> = BTreeSet::new();
    if let Ok(entries) = fs::read_dir(&media_dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                physical.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }

    MediaReport {
        dir_present,
        declared_total,
        declared,
        duplicate_declared,
        not_plain_names,
        physical,
    }
}

/// Приводит объявленное значение к простому basename.
///
/// Path из недоверенного значения не конструируется: берётся последний
/// компонент, а всё остальное игнорируется.
pub fn normalize_media_name(name: &str) -> String {
    Path::new(name).file_name().map_or_else(
        || name.to_string(),
        |part| part.to_string_lossy().into_owned(),
    )
}

/// Извлекает ссылки на media из HTML-значения поля заметки.
///
/// Поддерживаются только очевидные конструкции: `[sound:NAME]` и
/// `src="NAME"` / `src='NAME'`. Полноценный Anki/HTML parser не используется.
pub fn extract_media_references(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    collect_sound_references(text, &mut found);
    collect_src_references(text, &mut found);
    found
}

fn collect_sound_references(text: &str, found: &mut Vec<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("[sound:") {
        let after = &rest[start + "[sound:".len()..];
        let Some(end) = after.find(']') else {
            return;
        };
        let name = after[..end].trim();
        if !name.is_empty() {
            found.push(name.to_string());
        }
        rest = &after[end + 1..];
    }
}

fn collect_src_references(text: &str, found: &mut Vec<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("src=") {
        let after = &rest[start + "src=".len()..];
        let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            rest = after;
            continue;
        };
        let after_quote = &after[quote.len_utf8()..];
        let Some(end) = after_quote.find(quote) else {
            return;
        };
        let name = after_quote[..end].trim();
        if !name.is_empty() {
            found.push(name.to_string());
        }
        rest = &after_quote[end + quote.len_utf8()..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_sound_and_src_references() {
        let text = r#"[sound:a.mp3]<img src="b.png">и <img src='c.gif'>"#;
        let mut found = extract_media_references(text);
        found.sort();
        assert_eq!(found, vec!["a.mp3", "b.png", "c.gif"]);
    }

    #[test]
    fn ignores_unrelated_src_and_unterminated_constructs() {
        assert!(extract_media_references("src=noquotes").is_empty());
        assert!(extract_media_references("<img src=\"незакрытый>").is_empty());
        assert!(extract_media_references("[sound:без конца").is_empty());
        assert!(extract_media_references("обычный текст").is_empty());
    }

    #[test]
    fn normalizes_only_basenames() {
        assert_eq!(normalize_media_name("a.mp3"), "a.mp3");
        assert_eq!(normalize_media_name("dir/a.mp3"), "a.mp3");
        assert_eq!(normalize_media_name("../evil/a.mp3"), "a.mp3");
        assert_eq!(normalize_media_name(".."), "..");
    }

    #[test]
    fn media_report_compares_sets_not_paths() {
        let report = MediaReport {
            dir_present: true,
            declared_total: 3,
            declared: ["a.mp3".to_string(), "b.mp3".to_string()]
                .into_iter()
                .collect(),
            duplicate_declared: BTreeSet::new(),
            not_plain_names: BTreeSet::new(),
            physical: ["b.mp3".to_string(), "c.mp3".to_string()]
                .into_iter()
                .collect(),
        };
        assert_eq!(report.missing_physical(), vec!["a.mp3".to_string()]);
        assert_eq!(report.undeclared_physical(), vec!["c.mp3".to_string()]);
    }
}
