//! Human renderer: компактный текст на русском языке.
//!
//! Renderer не выполняет доменную работу: он только печатает уже готовый
//! domain result — включая адресуемость заметок и состав групп, которые команды
//! уже вычислили. Значения полей в human-режиме приводятся к одной строке и
//! ограничиваются по длине.
//!
//! Сообщение об усечении обязано называть реальную причину и реальный способ
//! получить остаток. `--json` сериализует ровно тот же vector, поэтому
//! «полный список доступен в --json» запрещено: это обещание, которого команда
//! не выполняет.

use crate::ops::NamedField;
use crate::ops::edit::{EditResult, EditStatus};
use crate::ops::find::FindResult;
use crate::ops::inspect::InspectResult;
use crate::ops::qa::QaResult;
use crate::ops::review::ReviewResult;
use crate::ops::review_check::{ReviewCheckResult, ReviewOutcome};
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

/// Печатает результат `edit`.
pub fn edit(result: &EditResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir.display()));
    out.line(format!("deck.json: {}", result.deck_json.display()));
    out.line(if result.applied {
        "Режим: --apply, deck.json заменён атомарно".to_string()
    } else if result.dry_run {
        "Режим: dry-run, deck.json не изменён".to_string()
    } else {
        "Режим: --apply, запись не требовалась".to_string()
    });
    out.line(format!("Правок в запросе: {}", result.edits_total));
    out.line(format!("Эффективных правок: {}", result.effective_edits));
    out.line(format!(
        "Статусы: применено {}, dry-run {}, без изменений {}, уже применено {}",
        result.summaries.applied,
        result.summaries.dry_run,
        result.summaries.noop_identical,
        result.summaries.already_applied
    ));
    match result.first_changed_line {
        Some(line) => out.line(format!(
            "Изменённых строк: {} (первая — {line})",
            result.changed_lines
        )),
        None => out.line("Изменённых строк: 0"),
    };
    out.line(format!(
        "Байт: {} → {} ({:+})",
        result.source_bytes, result.candidate_bytes, result.byte_delta
    ));

    out.blank();
    out.line(format!("Правки ({}):", result.outcomes.len()));
    if result.outcomes.is_empty() {
        out.line("  —");
    }
    for outcome in &result.outcomes {
        let edit_id = outcome
            .edit_id
            .as_deref()
            .map_or(String::new(), |id| format!(" [{id}]"));
        out.line(format!(
            "  #{} {}{} — {} (ord {}, заметка {}, колода {})",
            outcome.edit_index,
            outcome.status.as_str(),
            edit_id,
            format_args!("{:?}/{:?}", outcome.guid, outcome.field),
            outcome.field_ord,
            outcome.note_index,
            outcome.deck_path
        ));
        if outcome.status != EditStatus::NoopIdentical {
            out.line(format!(
                "      было: {}",
                compact_value(&outcome.old_sample)
            ));
            out.line(format!(
                "      стало: {}",
                compact_value(&outcome.new_sample)
            ));
        }
    }
    if result.outcomes_truncated {
        out.line(format!(
            "  … в отчёт попало только {} правок из {}; остальные не показаны ни здесь, ни в --json",
            result.outcomes.len(),
            result.edits_total
        ));
    }

    out.blank();
    out.line("Проверки:");
    for (name, value) in [
        ("source_canonical", result.checks.source_canonical),
        ("candidate_reparsed", result.checks.candidate_reparsed),
        (
            "semantic_targets_verified",
            result.checks.semantic_targets_verified,
        ),
        (
            "diff_shape_is_exactly_requested",
            result.checks.diff_shape_is_exactly_requested,
        ),
        (
            "byte_delta_matches_token_delta",
            result.checks.byte_delta_matches_token_delta,
        ),
    ] {
        out.line(format!("  {name}: {}", yes_no(value)));
    }

    out.blank();
    out.line(format!(
        "Валидация до: ERROR {}, WARNING {}, INFO {}",
        result.validation.before.errors,
        result.validation.before.warnings,
        result.validation.before.info
    ));
    out.line(format!(
        "Валидация после: ERROR {}, WARNING {}, INFO {}",
        result.validation.after.errors,
        result.validation.after.warnings,
        result.validation.after.info
    ));
    out.line(format!(
        "Новые ERROR: {}",
        list_or_none(&result.validation.new_error_codes)
    ));
    out.line(format!(
        "Новые WARNING: {}",
        list_or_none(&result.validation.new_warning_codes)
    ));

    out.finish()
}

/// Печатает список кодов или «нет».
fn list_or_none(codes: &[String]) -> String {
    if codes.is_empty() {
        "нет".to_string()
    } else {
        codes.join(", ")
    }
}

/// Печатает результат `qa`.
pub fn qa(result: &QaResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir));
    out.line(format!("Заметок всего: {}", result.notes_total));
    out.line(format!(
        "Findings: всего {}, показано {}, усечено: {} (предел {} на код)",
        result.findings_total,
        result.findings_returned,
        yes_no(result.truncated),
        result.max_per_code
    ));
    if result.unaddressable_findings > 0 {
        out.line(format!(
            "Неадресуемых findings: {} (нет guid, guid не уникален или модель не разрешается; \
             это диагностика содержимого, структурные причины — в validate)",
            result.unaddressable_findings
        ));
    }
    out.line(format!(
        "Коды: {}",
        if result.codes.is_empty() {
            "все правила".to_string()
        } else {
            result.codes.join(", ")
        }
    ));

    out.blank();
    out.line("По кодам:");
    if result.by_code.is_empty() {
        out.line("  —");
    }
    for entry in &result.by_code {
        out.line(format!(
            "  {} [{}]: {}",
            entry.code,
            entry.severity.as_str(),
            entry.count
        ));
    }

    out.blank();
    out.line("Findings:");
    if result.findings.is_empty() {
        out.line("  —");
    }
    for finding in &result.findings {
        out.line(format!(
            "  [{}] {} — заметка #{} ({})",
            finding.severity.as_str(),
            finding.code,
            finding.note_index,
            optional(finding.guid.as_deref())
        ));
        out.line(format!(
            "      колода: {}, модель: {}",
            finding.deck_path,
            optional(finding.note_model.as_deref())
        ));
        if let Some(field) = &finding.field {
            out.line(format!(
                "      поле: {} (ord {})",
                field,
                finding
                    .field_ord
                    .map_or_else(|| "?".to_string(), |ord| ord.to_string())
            ));
        }
        out.line(format!("      {}", finding.message));
        if !finding.addressable {
            out.line(
                "        адресуемость: нет (guid отсутствует, пуст или повторяется, \
                 либо модель заметки не разрешается) — предложение по этой заметке \
                 неисполнимо, структурные причины смотрите в validate",
            );
        }
        if finding.group_size.is_some() {
            out.line(format!(
                "        участники группы ({}): {}",
                finding
                    .group_size
                    .map_or_else(|| "?".to_string(), |size| size.to_string()),
                describe_related(&finding.related_note_indices, &finding.related_guids)
            ));
            if finding.related_truncated {
                out.line("        … показаны не все участники группы");
            }
        }
    }
    if result.truncated {
        out.line(format!(
            "  … показано не более {} findings на код: остальные не выводятся ни здесь, ни в --json; \
             увеличьте --max-per-code (или сузьте выборку через --code)",
            result.max_per_code
        ));
    }

    out.blank();
    out.line(format!("Правила ({}):", result.rules.len()));
    for rule in &result.rules {
        out.line(format!(
            "  {} [{}] применимо: {}, findings: {}",
            rule.code,
            rule.severity.as_str(),
            yes_no(rule.applicable),
            rule.findings
        ));
        out.line(format!("      {}", rule.description));
    }

    out.finish()
}

/// Печатает результат `review`.
pub fn review(result: &ReviewResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir));
    out.line(format!("Критерий: {}", describe_selection(result)));
    out.line(format!(
        "Выбрано заметок: {} из {}, страница: offset {}, limit {}, возвращено {}, усечено: {}",
        result.total_selected,
        result.notes_total,
        result.offset,
        result.limit,
        result.returned,
        yes_no(result.truncated)
    ));
    match result.next_offset {
        Some(next) => out.line(format!("Следующая страница: --offset {next}")),
        None => out.line("Следующая страница: нет"),
    };
    if result.excluded_unaddressable > 0 {
        out.line(format!(
            "Исключено неадресуемых заметок: {} (нет guid, guid не уникален или модель не \
             разрешается; предложение по ним неисполнимо, структурные причины — в validate)",
            result.excluded_unaddressable
        ));
    }

    for item in &result.items {
        out.blank();
        out.line(format!(
            "[#{}] guid: {}",
            item.note_index,
            optional(item.note.guid.as_deref())
        ));
        out.line(format!("    колода: {}", item.note.deck_path));
        out.line(format!(
            "    модель: {}",
            optional(item.note.note_model_name.as_deref())
        ));
        out.line(format!(
            "    теги: {}",
            if item.note.tags.is_empty() {
                "—".to_string()
            } else {
                item.note.tags.join(", ")
            }
        ));
        for field in &item.note.fields {
            out.line(format!("    {}: {}", field.name, render_field(field)));
        }
        for membership in &item.group_membership {
            out.line(format!(
                "    группа {}: участник группы заметки #{} ({}), всего участников {}",
                membership.code,
                membership.owner_note_index,
                optional(membership.owner_guid.as_deref()),
                membership.group_size
            ));
        }
        if item.qa_findings.is_empty() {
            out.line("    QA: —");
        } else {
            out.line(format!("    QA ({}):", item.qa_findings.len()));
            for finding in &item.qa_findings {
                let field = finding
                    .field
                    .as_deref()
                    .map_or(String::new(), |field| format!(", поле {field}"));
                out.line(format!(
                    "      [{}] {}{} — {}",
                    finding.severity.as_str(),
                    finding.code,
                    field,
                    finding.message
                ));
                if finding.group_size.is_some() {
                    out.line(format!(
                        "        группа: участников {}, кроме этой заметки: {}",
                        finding
                            .group_size
                            .map_or_else(|| "?".to_string(), |size| size.to_string()),
                        describe_related_notes(&finding.related)
                    ));
                    if finding.related_truncated {
                        out.line("        … показаны не все участники группы");
                    }
                }
            }
            if item.qa_findings_truncated {
                out.line("      … список findings обрезан");
            }
        }
    }

    out.finish()
}

/// Печатает результат `review-check`.
pub fn review_check(result: &ReviewCheckResult) -> String {
    let blockers: Vec<&str> = result
        .source_blockers
        .iter()
        .map(|blocker| blocker.code)
        .collect();
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir));
    out.line(format!("deck.json: {}", result.deck_json));
    out.line(format!("Итог: {}", result.outcome.as_str()));
    out.line(format!(
        "Предложений в документе: {}",
        result.proposals_total
    ));
    out.line(format!(
        "Статусы: valid {}, already_correct {}, already_applied {}, conflict {}, invalid {}",
        result.counts.valid,
        result.counts.already_correct,
        result.counts.already_applied,
        result.counts.conflict,
        result.counts.invalid
    ));
    out.line(format!(
        "Эффективных предложений: {}",
        result.effective_proposals
    ));
    out.line(format!(
        "Готовый запрос для Stage 2: {}",
        match &result.edit_request {
            Some(request) => format!("да, правок {}", request.edits.len()),
            None if !result.source_blockers.is_empty() => {
                format!("нет: исходник нельзя править ({})", blockers.join(", "))
            }
            None if result.outcome == ReviewOutcome::Ok => "нет: нечего применять".to_string(),
            None => "нет: отчёт не ok".to_string(),
        }
    ));
    if !result.source_blockers.is_empty() {
        out.line(format!(
            "Исходник: править нельзя ({}); отчёт по предложениям всё равно показан ниже",
            blockers.join(", ")
        ));
        for blocker in &result.source_blockers {
            out.line(format!("  {} — {}", blocker.code, blocker.message));
        }
    }

    out.blank();
    out.line(format!("Предложения ({}):", result.proposals.len()));
    if result.proposals.is_empty() {
        out.line("  —");
    }
    for proposal in &result.proposals {
        out.line(format!(
            "  #{} {} — {}",
            proposal.proposal_index,
            proposal.status.as_str(),
            format_args!("{:?}/{:?}", proposal.guid, proposal.field)
        ));
        if let (Some(current), Some(_)) = (proposal.current_sample.as_deref(), proposal.note_index)
        {
            out.line(format!("      сейчас: {}", compact_value(current)));
        }
        out.line(format!(
            "      ожидалось: {}",
            compact_value(&proposal.expected_sample)
        ));
        out.line(format!(
            "      замена: {}",
            compact_value(&proposal.replacement_sample)
        ));
        if let Some(reason) = &proposal.reason {
            out.line(format!("      пояснение агента: {}", compact_value(reason)));
        }
        if let Some(message) = &proposal.message {
            out.line(format!(
                "      проблема [{}]: {message}",
                proposal.problem.unwrap_or("problem")
            ));
        }
    }
    if result.proposals_truncated {
        out.line(format!(
            "  … в отчёт попало только {} предложений из {}; счётчики выше считают все, \
             а остальные предложения не показаны ни здесь, ни в --json",
            result.proposals.len(),
            result.proposals_total
        ));
    }

    out.finish()
}

/// Печатает участников группы в форме `note_index (guid)`.
fn describe_related(positions: &[usize], guids: &[String]) -> String {
    if positions.is_empty() {
        return "—".to_string();
    }
    positions
        .iter()
        .enumerate()
        .map(|(offset, position)| match guids.get(offset) {
            Some(guid) => format!("#{position} ({guid})"),
            None => format!("#{position} (без guid)"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Печатает участников группы batch'а в форме `note_index (guid)`.
fn describe_related_notes(related: &[crate::ops::review::RelatedNote]) -> String {
    if related.is_empty() {
        return "—".to_string();
    }
    related
        .iter()
        .map(|note| match &note.guid {
            Some(guid) => format!("#{} ({guid})", note.note_index),
            None => format!("#{} (без guid)", note.note_index),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Описывает критерий выбора `review`.
fn describe_selection(result: &ReviewResult) -> String {
    let selection = &result.selection;
    let mut parts = vec![selection.kind.to_string()];
    if let Some(guid) = &selection.guid {
        parts.push(format!("guid {guid:?}"));
    }
    if let Some(field) = &selection.field {
        parts.push(format!(
            "поле {field:?}, значение {:?}, режим {}",
            selection.value.as_deref().unwrap_or_default(),
            selection.match_mode.unwrap_or("contains")
        ));
    }
    if let Some(code) = &selection.qa_code {
        parts.push(format!("QA-код {code}"));
    }
    if let Some(deck) = &selection.deck {
        parts.push(format!("колода {deck:?}"));
    }
    parts.join("; ")
}
