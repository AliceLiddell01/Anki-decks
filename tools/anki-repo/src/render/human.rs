//! Human renderer: компактный текст на русском языке.
//!
//! Renderer не выполняет доменную работу: он только печатает уже готовый
//! domain result. Значения полей в human-режиме приводятся к одной строке и
//! ограничиваются по длине; полные значения доступны в `--json`.

use crate::ops::NamedField;
use crate::ops::find::FindResult;
use crate::ops::inspect::InspectResult;
use crate::ops::stats::StatsResult;
use crate::ops::validate::{Severity, ValidateResult};

/// Предел длины значения поля в human-режиме.
pub const VALUE_LIMIT: usize = 300;

#[derive(Default)]
struct Out {
    buffer: String,
}

impl Out {
    fn line(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.buffer.push_str(text.as_ref());
        self.buffer.push('\n');
        self
    }

    fn blank(&mut self) -> &mut Self {
        self.buffer.push('\n');
        self
    }

    fn finish(mut self) -> String {
        while self.buffer.ends_with("\n\n") {
            self.buffer.pop();
        }
        self.buffer
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "да" } else { "нет" }
}

fn optional(value: Option<&str>) -> &str {
    value.unwrap_or("—")
}

/// Приводит значение поля к одной строке с ограничением длины.
fn compact_value(value: &str) -> String {
    let single: String = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut result: String = single.chars().take(VALUE_LIMIT).collect();
    if single.chars().count() > VALUE_LIMIT {
        result.push('…');
    }
    result
}

/// Печатает результат `inspect`.
pub fn inspect(result: &InspectResult) -> String {
    let mut out = Out::default();
    let verbose = result.verbose.is_some();

    out.line(format!("Экспорт: {}", result.export_dir));
    if let Some(details) = &result.verbose {
        out.line(format!("deck.json: {}", details.deck_json));
    }
    out.line(format!("Корневая колода: {}", result.root_deck_name));
    out.line(format!("Узлов колоды: {}", result.deck_nodes));
    out.line(format!("Заметок всего: {}", result.notes_total));
    out.blank();

    out.line(format!("Модели заметок ({}):", result.models.len()));
    if result.models.is_empty() {
        out.line("  —");
    }
    for model in &result.models {
        out.line(format!("  {}", model.name));
        if verbose {
            out.line(format!(
                "    crowdanki_uuid: {}",
                optional(model.crowdanki_uuid.as_deref())
            ));
        }
        out.line(format!(
            "    полей: {}, шаблонов: {}, заметок: {}",
            model.field_count, model.template_count, model.used_notes
        ));
        for field in &model.fields {
            out.line(format!(
                "    ord={}  {}",
                field
                    .ord
                    .map_or_else(|| "?".to_string(), |ord| ord.to_string()),
                field.name
            ));
        }
    }
    out.blank();

    out.line(format!("Конфигурации колод ({}):", result.configs.len()));
    if result.configs.is_empty() {
        out.line("  —");
    }
    for config in &result.configs {
        out.line(format!("  {}", config.name));
        if verbose {
            out.line(format!(
                "    crowdanki_uuid: {}",
                optional(config.crowdanki_uuid.as_deref())
            ));
        }
        out.line(format!("    узлов: {}", config.nodes_using));
    }
    out.blank();

    let media = &result.media;
    out.line("Media:");
    out.line(format!(
        "  объявлено имён: {} (уникальных: {}, повторов: {})",
        media.declared_total, media.declared_unique, media.duplicate_declared
    ));
    out.line(format!(
        "  каталог media/: {}",
        if media.dir_present {
            "есть"
        } else {
            "нет"
        }
    ));
    out.line(format!("  файлов в media/: {}", media.physical_total));
    out.line(format!(
        "  объявлено, но нет физически: {}",
        media.missing_physical
    ));
    out.line(format!(
        "  есть физически, но не объявлено: {}",
        media.undeclared_physical
    ));

    if let Some(details) = &result.verbose {
        push_sample(
            &mut out,
            "  образец отсутствующих физически",
            &details.media_missing_physical_sample,
        );
        push_sample(
            &mut out,
            "  образец необъявленных физически",
            &details.media_undeclared_physical_sample,
        );
        push_sample(
            &mut out,
            "  образец повторяющихся объявлений",
            &details.media_duplicate_declared_sample,
        );
    }
    out.blank();

    out.line("Заметки по узлам колоды:");
    if result.notes_by_deck.is_empty() {
        out.line("  —");
    }
    for entry in &result.notes_by_deck {
        out.line(format!("  {}: {}", entry.key, entry.count));
    }

    if let Some(details) = &result.verbose {
        out.blank();
        out.line("Дерево колод (preorder):");
        for node in &details.nodes {
            out.line(format!(
                "  {indent}{path}  [заметок: {notes}, uuid: {uuid}]",
                indent = "  ".repeat(node.depth),
                path = node.path,
                notes = node.notes,
                uuid = optional(node.crowdanki_uuid.as_deref()),
            ));
        }

        out.blank();
        out.line("Шаблоны карточек:");
        if details.model_templates.is_empty() {
            out.line("  —");
        }
        for model in &details.model_templates {
            out.line(format!("  {}", model.model_name));
            for template in &model.templates {
                out.line(format!(
                    "    ord={}  {}",
                    template
                        .ord
                        .map_or_else(|| "?".to_string(), |ord| ord.to_string()),
                    optional(template.name.as_deref())
                ));
            }
        }

        out.blank();
        out.line("Идентификаторы заметок:");
        out.line(format!("  уникальных guid: {}", details.guids_unique));
        out.line(format!("  guid с повторами: {}", details.guid_duplicates));

        out.blank();
        out.line("Выборка заметок:");
        if details.notes_sample.is_empty() {
            out.line("  —");
        }
        for sample in &details.notes_sample {
            out.line(format!(
                "  guid={} колода={} полей={}",
                optional(sample.guid.as_deref()),
                sample.deck_path,
                sample.field_count
            ));
        }
    }

    out.finish()
}

fn push_sample(out: &mut Out, title: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    out.line(format!("{title}: {}", values.join(", ")));
}

/// Печатает результат `find`.
pub fn find(result: &FindResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir));
    out.line(format!("Критерий: {}", describe_criteria(result)));
    out.line(format!(
        "Найдено заметок: {}, возвращено: {}, усечено: {}",
        result.matched_total,
        result.returned,
        yes_no(result.truncated)
    ));

    for (position, note) in result.notes.iter().enumerate() {
        out.blank();
        out.line(format!(
            "[{}] guid: {}",
            position + 1,
            optional(note.guid.as_deref())
        ));
        out.line(format!("    колода: {}", note.deck_path));
        out.line(format!(
            "    модель: {}",
            optional(note.note_model_name.as_deref())
        ));
        out.line(format!(
            "    uuid модели: {}",
            optional(note.note_model_uuid.as_deref())
        ));
        out.line(format!(
            "    теги: {}",
            if note.tags.is_empty() {
                "—".to_string()
            } else {
                note.tags.join(", ")
            }
        ));
        for field in &note.fields {
            out.line(format!("    {}: {}", field.name, render_field(field)));
        }
    }

    out.finish()
}

fn render_field(field: &NamedField) -> String {
    match &field.value {
        Some(value) => compact_value(value),
        None => "(значение отсутствует)".to_string(),
    }
}

fn describe_criteria(result: &FindResult) -> String {
    let mut text = match (
        result.criteria.field.as_deref(),
        result.criteria.guid.as_deref(),
    ) {
        (Some(field), _) => format!(
            "поле {:?} {mode} {:?}",
            field,
            result.criteria.value.as_deref().unwrap_or(""),
            mode = match result.criteria.match_mode {
                Some("exact") => "точно равно",
                _ => "содержит",
            }
        ),
        (None, Some(guid)) => format!("guid {guid:?}"),
        (None, None) => "не задан".to_string(),
    };
    if let Some(deck) = &result.criteria.deck {
        text.push_str(&format!(", колода {deck:?}"));
    }
    text
}

/// Печатает результат `stats`.
pub fn stats(result: &StatsResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir));
    out.line(format!("Узлов колоды: {}", result.deck_nodes));
    out.line(format!("Заметок всего: {}", result.notes_total));
    out.line(format!(
        "Заметок с неразрешённой моделью: {}",
        result.unresolved_model_notes
    ));

    out.blank();
    out.line("Заметки по колодам:");
    if result.notes_by_deck.is_empty() {
        out.line("  —");
    }
    for entry in &result.notes_by_deck {
        out.line(format!("  {}: {}", entry.key, entry.count));
    }

    out.blank();
    out.line("Заметки по моделям:");
    if result.notes_by_model.is_empty() {
        out.line("  —");
    }
    for entry in &result.notes_by_model {
        out.line(format!("  {}: {}", entry.key, entry.count));
    }

    out.blank();
    out.line("Использование конфигураций (по узлам колод):");
    if result.config_usage.is_empty() {
        out.line("  —");
    }
    for entry in &result.config_usage {
        out.line(format!("  {}: {}", entry.key, entry.count));
    }

    out.blank();
    out.line("Поля:");
    if result.fields.is_empty() {
        out.line("  —");
    }
    for field in &result.fields {
        out.line(format!(
            "  {}: всего {}, непустых {}, пустых {}",
            field.name, field.total, field.nonempty, field.empty
        ));
    }

    let media = &result.media;
    out.blank();
    out.line("Media:");
    out.line(format!(
        "  объявлено имён: {} (уникальных: {}, повторов: {})",
        media.declared_total, media.declared_unique, media.duplicate_declared
    ));
    out.line(format!("  файлов в media/: {}", media.physical_total));
    out.line(format!(
        "  объявлено, но нет физически: {}",
        media.missing_physical
    ));
    out.line(format!(
        "  есть физически, но не объявлено: {}",
        media.undeclared_physical
    ));

    if let Some(group) = &result.group_by {
        out.blank();
        out.line(format!(
            "Распределение по полю {:?} (различных значений: {}, заметок без поля: {}, усечено: {}):",
            group.field,
            group.distinct_values,
            group.notes_without_field,
            yes_no(group.truncated)
        ));
        if group.buckets.is_empty() {
            out.line("  —");
        }
        for bucket in &group.buckets {
            let value = if bucket.key.is_empty() {
                "(пусто)".to_string()
            } else {
                compact_value(&bucket.key)
            };
            out.line(format!("  {:>6}  {}", bucket.count, value));
        }
    }

    out.finish()
}

/// Печатает результат `validate`.
pub fn validate(result: &ValidateResult) -> String {
    let mut out = Out::default();

    out.line(format!("Валиден: {}", yes_no(result.valid)));
    out.line(format!(
        "ERROR: {}, WARNING: {}, INFO: {}",
        result.summary.errors, result.summary.warnings, result.summary.info
    ));

    for severity in [Severity::Error, Severity::Warning, Severity::Info] {
        let issues: Vec<_> = result
            .issues
            .iter()
            .filter(|issue| issue.severity == severity)
            .collect();
        if issues.is_empty() {
            continue;
        }
        out.blank();
        out.line(format!(
            "{} ({}):",
            severity.as_str().to_uppercase(),
            issues.len()
        ));
        for issue in issues {
            let scope = describe_scope(issue.deck_path.as_deref(), &issue.location);
            if scope.is_empty() {
                out.line(format!("  {} — {}", issue.code, issue.message));
            } else {
                out.line(format!("  {} {} — {}", issue.code, scope, issue.message));
            }
        }
    }

    out.finish()
}

fn describe_scope(deck_path: Option<&str>, location: &str) -> String {
    match (deck_path, location.is_empty()) {
        (Some(path), false) => format!("{path} {location}"),
        (Some(path), true) => path.to_string(),
        (None, false) => location.to_string(),
        (None, true) => String::new(),
    }
}
