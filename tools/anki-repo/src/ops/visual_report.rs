//! Визуальный отчёт об изменениях двух состояний одного экспорта.
//!
//! Задача отчёта — показать человеку, что именно изменилось и как это будет
//! выглядеть в Anki. Отсюда два решения, определяющие всё остальное.
//!
//! **Сопоставление только по `guid`.** CrowdAnki при импорте ищет заметку по
//! `guid` и создаёт новую, если не нашёл. Ровно так же сопоставляет состояния
//! отчёт: заметка появилась в «после» — создана; есть в обоих — сравниваются
//! поля и теги; была только в «до» — исчезла. Совпадения по позиции, по
//! значению поля или по «похожести» здесь нет, потому что такое совпадение
//! выдало бы догадку за факт ровно там, где ревьюер принимает решение.
//! Неоднозначный `guid` (повтор внутри состояния) не разрешается никак: он
//! попадает в диагностику и в отдельный список, а не в одну из категорий.
//!
//! **Никаких догадок о неподдержанном.** Превью собирается
//! [`crate::template::render_card`], который честно отказывается от того, что
//! нельзя вычислить статически (cloze, фильтры, `<script>`), и причина отказа
//! видна и в HTML карточки, и в разделе неподдержанных конструкций. Ревьюер
//! должен видеть, где превью не равно Anki, а не верить превью.
//!
//! Отчёт read-only: он ничего не пишет за пределами `--out`, не ходит в сеть и
//! не требует канонической формы исходников. Он также не решает, что делать с
//! найденным: физическое удаление заметки и смена идентичности модели
//! объявляются неподдержанными, а не «применяются» молча.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::{ExportIndex, NoteRef, model_fields_in_ord_order, resolve_named_fields};
use crate::media as media_index;
use crate::model::{DeckNode, FieldValue, NoteModel, TemplateDef};
use crate::ops::retire::validate_tag;
use crate::report::diff::{TokenKind, diff_tokens_lossy};
use crate::report::html::{
    CardFile, CardSide, DiffMark, FieldDiff, ReportCard, ReportCounts, ReportDiagnosticView,
    ReportDocument, ReportPreview, ReportSection, card_html, index_html,
};
use crate::report::media::{self, MediaPlan};
use crate::template::{
    ConstructKind, ModelKind, RenderedSide, TemplateContext, render_card, scan_constructs,
};
use crate::text::bounded_sample;

/// Предел числа заметок, попавших в отчёт подробно.
pub const DEFAULT_PREVIEW_LIMIT: usize = 200;

/// Максимальный предел подробных заметок.
pub const MAX_PREVIEW_LIMIT: usize = 1000;

/// Предел числа заметок в machine-readable списках.
pub const MAX_LISTED_NOTES: usize = 500;

/// Предел числа шаблонов одной модели, для которых строится превью.
pub const MAX_TEMPLATES_PER_MODEL: usize = 32;

/// Предел числа файлов превью.
pub const MAX_CARD_FILES: usize = 4000;

/// Каталог файлов превью внутри каталога отчёта.
pub const CARDS_SUBDIR: &str = "cards";

/// Имя файла-точки входа отчёта.
pub const INDEX_HTML: &str = "index.html";

/// Запрос на построение визуального отчёта.
#[derive(Debug, Clone)]
pub struct ReportRequest {
    /// Каталог экспорта в состоянии «до».
    pub before: PathBuf,
    /// Каталог экспорта в состоянии «после».
    pub after: PathBuf,
    /// Каталог, в который пишется отчёт.
    pub out: PathBuf,
    /// Тег вывода из обращения, если колода его использует.
    pub retire_tag: Option<String>,
    /// Сколько заметок показать подробно.
    pub preview_limit: usize,
}

impl ReportRequest {
    /// Запрос с пределом по умолчанию.
    #[must_use]
    pub fn new(
        before: impl Into<PathBuf>,
        after: impl Into<PathBuf>,
        out: impl Into<PathBuf>,
    ) -> Self {
        Self {
            before: before.into(),
            after: after.into(),
            out: out.into(),
            retire_tag: None,
            preview_limit: DEFAULT_PREVIEW_LIMIT,
        }
    }
}

/// Категория изменения заметки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteChangeKind {
    /// Заметки не было в «до».
    Created,
    /// Изменились значения полей или теги.
    Changed,
    /// Появился тег вывода из обращения, содержимое не тронуто.
    Retired,
    /// Заметка была в «до» и исчезла в «после».
    Removed,
    /// Ничего не изменилось.
    Unchanged,
    /// `guid` повторяется внутри одного из состояний.
    Ambiguous,
}

impl NoteChangeKind {
    /// Стабильное machine-readable имя.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Changed => "changed",
            Self::Retired => "retired",
            Self::Removed => "removed",
            Self::Unchanged => "unchanged",
            Self::Ambiguous => "ambiguous",
        }
    }

    /// Русская подпись для отчёта.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Created => "создана",
            Self::Changed => "изменена",
            Self::Retired => "выведена из обращения",
            Self::Removed => "исчезла из экспорта",
            Self::Unchanged => "без изменений",
            Self::Ambiguous => "неоднозначный guid",
        }
    }
}

/// Классифицированная заметка отчёта.
#[derive(Debug, Clone)]
pub struct NoteOutcome {
    /// `guid` заметки.
    pub guid: String,
    /// Категория.
    pub kind: NoteChangeKind,
    /// Путь колоды; для исчезнувших — из состояния «до».
    pub deck_path: String,
    /// Имя модели, если она разрешилась.
    pub model_name: Option<String>,
    /// Теги в «до».
    pub tags_before: Vec<String>,
    /// Теги в «после».
    pub tags_after: Vec<String>,
    /// Имена полей, значения которых изменились.
    pub changed_fields: Vec<String>,
    /// Сколько раз `guid` встретился в «до».
    pub before_occurrences: usize,
    /// Сколько раз `guid` встретился в «после».
    pub after_occurrences: usize,
}

/// Описание одного состояния экспорта.
#[derive(Debug, Clone)]
pub struct ReportSide {
    /// Каталог экспорта в том виде, в котором его получил CLI.
    pub export_dir: PathBuf,
    /// Полный путь к `deck.json`.
    pub deck_json: PathBuf,
    /// Имя корневой колоды.
    pub deck_name: String,
    /// `crowdanki_uuid` корневой колоды.
    pub deck_uuid: Option<String>,
    /// Сколько заметок в состоянии.
    pub notes: usize,
    /// Сколько узлов колод в состоянии.
    pub decks: usize,
    /// Сколько моделей объявлено.
    pub models: usize,
}

/// Диагностика отчёта.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Стабильный код.
    pub code: &'static str,
    /// `warning` или `info`.
    pub severity: &'static str,
    /// Объяснение.
    pub message: String,
    /// Заметка или модель, к которой относится диагностика.
    pub subject: Option<String>,
}

impl Diagnostic {
    fn warning(code: &'static str, message: String, subject: Option<String>) -> Self {
        Self {
            code,
            severity: "warning",
            message,
            subject,
        }
    }

    fn info(code: &'static str, message: String, subject: Option<String>) -> Self {
        Self {
            code,
            severity: "info",
            message,
            subject,
        }
    }
}

/// Неподдержанная конструкция шаблона, встреченная в отчёте.
#[derive(Debug, Clone)]
pub struct UnsupportedConstruct {
    /// Сырая конструкция.
    pub construct: String,
    /// Причина отказа.
    pub reason: String,
}

/// Сводка работы с media.
#[derive(Debug, Clone, Default)]
pub struct MediaSummary {
    /// Сколько файлов скопировано в отчёт.
    pub copied: usize,
    /// Имена без файла в экспорте.
    pub missing: Vec<String>,
    /// Ссылки, пытавшиеся выйти за пределы `media/`.
    pub traversal: Vec<String>,
    /// Внешние ссылки: не скачиваются.
    pub remote: Vec<String>,
    /// Символические ссылки: не читаются.
    pub symlinks: Vec<String>,
    /// Файлы, превысившие предел размера.
    pub oversized: Vec<String>,
    /// Сколько ссылок не обработано из-за предела.
    pub budget_skipped: usize,
}

/// Выполненные проверки отчёта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportChecks {
    /// Состояние «до» разобрано.
    pub before_parsed: bool,
    /// Состояние «после» разобрано.
    pub after_parsed: bool,
    /// Каждая заметка обоих состояний попала ровно в одну категорию.
    pub every_note_classified: bool,
    /// Каталог отчёта не находится внутри `decks/`.
    pub out_dir_outside_decks: bool,
    /// Все записанные файлы лежат внутри каталога отчёта.
    pub all_files_inside_out_dir: bool,
    /// Точка входа отчёта не ссылается на внешние ресурсы.
    ///
    /// Утверждение относится именно к `index.html`: он обязан открываться
    /// офлайн. Файлы превью могут содержать внешние ссылки из значений полей,
    /// и это отдельно объявлено ограничением отчёта.
    pub index_without_external_assets: bool,
    /// Media скопировано только внутрь каталога отчёта.
    pub media_confined_to_out_dir: bool,
}

/// Результат `visual-report`.
#[derive(Debug, Clone)]
pub struct VisualReportResult {
    /// Состояние «до».
    pub before: ReportSide,
    /// Состояние «после».
    pub after: ReportSide,
    /// Каталог отчёта.
    pub out_dir: PathBuf,
    /// Полный путь к точке входа отчёта.
    pub index_html: PathBuf,
    /// Относительные пути файлов превью (обрезано до [`MAX_LISTED_NOTES`]).
    pub card_files: Vec<String>,
    /// Сколько файлов превью записано всего.
    pub card_files_total: usize,
    /// Тег вывода из обращения.
    pub retire_tag: Option<String>,
    /// Счётчики отчёта.
    pub counts: ReportCounts,
    /// Подробности по заметкам (обрезано до [`MAX_LISTED_NOTES`]).
    pub outcomes: Vec<NoteOutcome>,
    /// Был ли список заметок обрезан.
    pub outcomes_truncated: bool,
    /// Диагностика.
    pub diagnostics: Vec<Diagnostic>,
    /// Неподдержанные конструкции затронутых моделей.
    pub unsupported_constructs: Vec<UnsupportedConstruct>,
    /// Сводка media.
    pub media: MediaSummary,
    /// Ограничения отчёта.
    pub limitations: Vec<String>,
    /// Выполненные проверки.
    pub checks: ReportChecks,
}

/// Строит визуальный отчёт.
///
/// # Errors
///
/// [`ErrorCode::InvalidRequest`], если каталог отчёта или тег недопустимы,
/// [`ErrorCode::InputUnreadable`]/[`ErrorCode::InvalidJson`] при чтении состояний
/// и [`ErrorCode::WriteFailed`] при записи отчёта.
pub fn report(request: &ReportRequest) -> Result<VisualReportResult, DomainError> {
    if let Some(tag) = &request.retire_tag {
        validate_tag(tag)?;
    }

    let before = load_side(&request.before)?;
    let after = load_side(&request.after)?;

    let canonical_before = canonical_ish(&before.summary.export_dir);
    let canonical_after = canonical_ish(&after.summary.export_dir);
    if canonical_before == canonical_after {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "состояния «до» и «после» указывают на один каталог {}",
                canonical_before.display()
            ),
            details! {
                "reason" => "before_equals_after",
                "export_dir" => canonical_before.display().to_string(),
            },
        ));
    }

    let out_dir = ensure_out_dir(
        &request.out,
        &[canonical_before.clone(), canonical_after.clone()],
    )?;
    let preview_limit = request.preview_limit.clamp(1, MAX_PREVIEW_LIMIT);

    let classification = classify(&before, &after, request.retire_tag.as_deref());
    let mut counts = classification.counts;
    let mut diagnostics = classification.diagnostics;
    let mut build = Build::new(&after);

    let mut sections: Vec<ReportSection> = Vec::new();
    for plan in &classification.plans {
        let selected: Vec<&NoteOutcome> = plan
            .outcomes
            .iter()
            .filter(|outcome| outcome.kind == plan.kind)
            .collect();

        if selected.is_empty() {
            continue;
        }

        let mut cards: Vec<ReportCard> = Vec::new();
        for outcome in selected.iter().take(preview_limit) {
            cards.push(build_card(
                outcome,
                &before,
                &after,
                request.retire_tag.as_deref(),
                &mut build,
            ));
        }

        let truncated = (selected.len() > preview_limit).then(|| {
            format!(
                "показано {preview_limit} заметок из {}: остальные перечислены только в JSON-результате",
                selected.len()
            )
        });

        sections.push(ReportSection {
            title: plan.title.to_string(),
            hint: plan.hint.to_string(),
            cards,
            truncated,
        });
    }

    if !classification.ambiguous.is_empty() {
        sections.push(ReportSection {
            title: "Неоднозначные guid".to_string(),
            hint: "guid повторяется внутри одного состояния: отчёт не выбирает, какая из заметок \
                   имелась в виду."
                .to_string(),
            cards: classification
                .ambiguous
                .iter()
                .take(preview_limit)
                .map(ambiguous_card)
                .collect(),
            truncated: (classification.ambiguous.len() > preview_limit).then(|| {
                format!(
                    "показано {preview_limit} записей из {}",
                    classification.ambiguous.len()
                )
            }),
        });
    }

    // Источники — каталоги экспорта, а не их `media`: подкаталог выбирает сам
    // план, и второй раз присоединять его здесь означало бы искать файлы в
    // `media/media/`.
    let source_exports = [
        after.summary.export_dir.clone(),
        before.summary.export_dir.clone(),
    ];
    let mut media_plan = media::plan(&source_exports, &build.media_references, &out_dir)?;
    media::note_symlinks(&mut media_plan, &source_exports, &build.media_references);
    push_media_diagnostics(&media_plan, &mut diagnostics);
    diagnostics.append(&mut build.preview_issues);

    let mut touched: Vec<String> = build.touched_models.iter().cloned().collect();
    touched.sort();
    for uuid in &touched {
        if let Some(scan) = build.model_scans.get(uuid) {
            emit_model_diagnostics(scan, uuid, &mut diagnostics);
        }
    }

    // Подстановка ссылок выполняется после планирования media: до него неизвестно,
    // какие файлы окажутся рядом с отчётом.
    let mut written: Vec<PathBuf> = Vec::new();
    for (file, document) in &build.card_documents {
        let mut document = document.clone();
        for side in &mut document.sides {
            side.html = rewrite_media_refs(&side.html, &media_plan, "../");
        }
        let path = out_dir.join(file);
        write_report_file(&path, card_html(&document).as_bytes())?;
        written.push(path);
    }

    let unsupported: Vec<UnsupportedConstruct> = touched
        .iter()
        .filter_map(|uuid| build.model_scans.get(uuid))
        .flat_map(|scan| {
            scan.unsupported
                .iter()
                .map(|(construct, reason)| UnsupportedConstruct {
                    construct: construct.clone(),
                    reason: reason.clone(),
                })
        })
        .collect();

    let card_files_total = build.card_files.len();
    counts.previews = card_files_total;

    let limitations = limitations();
    let document = ReportDocument {
        title: "Изменения экспорта CrowdAnki".to_string(),
        before_label: before.summary.export_dir.display().to_string(),
        after_label: after.summary.export_dir.display().to_string(),
        retire_tag: request.retire_tag.clone(),
        counts,
        sections,
        diagnostics: diagnostics
            .iter()
            .map(|diagnostic| ReportDiagnosticView {
                code: diagnostic.code.to_string(),
                severity: diagnostic.severity,
                message: diagnostic.message.clone(),
                subject: diagnostic.subject.clone(),
            })
            .collect(),
        limitations: limitations.clone(),
        unsupported_constructs: unsupported
            .iter()
            .map(|item| (item.construct.clone(), item.reason.clone()))
            .collect(),
    };

    let index_path = out_dir.join(INDEX_HTML);
    let index = index_html(&document);
    write_report_file(&index_path, index.as_bytes())?;
    written.push(index_path.clone());

    let all_inside = written.iter().all(|path| path.starts_with(&out_dir));
    let media_dir = out_dir.join(media::MEDIA_SUBDIR);
    let media_confined = media_plan.copied.iter().all(|name| {
        !name.contains('/')
            && !name.contains('\\')
            && media_dir.join(name).starts_with(&out_dir)
            && media_dir.join(name).is_file()
    });
    let out_dir_outside_decks = !canonical_ish(&out_dir)
        .components()
        .any(|component| matches!(component, Component::Normal(name) if name == "decks"));

    Ok(VisualReportResult {
        before: before.summary,
        after: after.summary,
        out_dir: out_dir.clone(),
        index_html: index_path,
        card_files: build
            .card_files
            .iter()
            .take(MAX_LISTED_NOTES)
            .cloned()
            .collect(),
        card_files_total,
        retire_tag: request.retire_tag.clone(),
        counts,
        outcomes_truncated: classification.outcomes.len() > MAX_LISTED_NOTES,
        outcomes: classification
            .outcomes
            .into_iter()
            .take(MAX_LISTED_NOTES)
            .collect(),
        diagnostics,
        unsupported_constructs: unsupported,
        media: MediaSummary {
            copied: media_plan.copied.len(),
            missing: media_plan.missing.clone(),
            traversal: media_plan.traversal.clone(),
            remote: media_plan.remote.clone(),
            symlinks: media_plan.symlinks.clone(),
            oversized: media_plan.oversized.clone(),
            budget_skipped: media_plan.budget_skipped,
        },
        limitations,
        checks: ReportChecks {
            before_parsed: true,
            after_parsed: true,
            every_note_classified: classification.every_note_classified,
            out_dir_outside_decks,
            all_files_inside_out_dir: all_inside,
            index_without_external_assets: !index_references_external(&index),
            media_confined_to_out_dir: media_confined,
        },
    })
}

/// Ссылается ли точка входа отчёта на внешний ресурс.
///
/// Проверяются именно ссылки, а не любое упоминание схемы: ограничения отчёта
/// перечисляют схемы словами, и запрет на подстроку `http://` запрещал бы
/// объяснять пользователю, что отчёт не ходит в сеть.
fn index_references_external(index: &str) -> bool {
    const EXTERNAL_PREFIXES: [&str; 7] = [
        "src=\"http",
        "src='http",
        "src=\"//",
        "src='//",
        "href=\"http",
        "href='http",
        "href=\"//",
    ];
    EXTERNAL_PREFIXES
        .iter()
        .any(|prefix| index.contains(prefix))
}

/// Ограничения отчёта — то, что он заведомо не показывает.
fn limitations() -> Vec<String> {
    vec![
        "Превью статическое: JavaScript шаблона не выполняется, cloze-разметка и фильтры \
         (cloze, type, tts, furigana, hint) не вычисляются. Неподдержанные конструкции видны \
         в самом превью сырым текстом и перечислены отдельным разделом."
            .to_string(),
        "Обёртка карточки собрана как `card card{N}`, где N — ord шаблона плюс один. Ночной \
         режим, масштаб шрифта и темы устройства не воспроизводятся: показывается только \
         дневное оформление."
            .to_string(),
        "Базовые стили страницы превью — приближение базовых стилей Anki, а не их копия. \
         Оформление карточки задаёт CSS модели, и он подключён после базовых правил."
            .to_string(),
        "Рядом с отчётом лежат только те media-файлы, которые нашлись в `media/` одного из \
         состояний. `media_files` — список ссылок, а не доказательство наличия файла, поэтому \
         отсутствующие файлы перечислены в диагностике, а не выдуманы."
            .to_string(),
        "Внешние ссылки (`http://`, `https://`, `//`, `data:`) не скачиваются: отчёт открывается \
         офлайн и не заменяет их ничем."
            .to_string(),
        "Отчёт сопоставляет два состояния по `guid` и не воспроизводит решения импорта \
         CrowdAnki: он не утверждает, что Anki создаст, обновит или не тронет что-либо."
            .to_string(),
        "Diff значений полей сравнивается по токенам (HTML-теги, сущности, слова, символы), \
         а не по отрендеренному тексту: правка одной разметки должна быть видна."
            .to_string(),
        "Приложение открывает отчёт как обычный файл: ссылки на превью и media относительные, \
         поэтому каталог отчёта нужно переносить целиком."
            .to_string(),
    ]
}

/// Загруженное состояние экспорта вместе со сводкой.
struct Side {
    index: SideIndex,
    summary: ReportSide,
}

/// Факты состояния, на которых стоит отчёт.
struct SideIndex {
    guids: BTreeMap<String, Vec<usize>>,
    notes: Vec<NoteFacts>,
    decks: BTreeMap<String, Option<String>>,
    models: BTreeMap<String, ModelFacts>,
}

/// Факты о заметке, нужные отчёту.
struct NoteFacts {
    deck_path: String,
    model_uuid: Option<String>,
    tags: Vec<String>,
    fields: BTreeMap<String, String>,
    field_order: Vec<(String, String)>,
}

/// Факты о модели, нужные отчёту.
struct ModelFacts {
    name: String,
    css: String,
    kind: ModelKind,
    field_names: BTreeSet<String>,
    templates: Vec<TemplateFacts>,
}

/// Факты о шаблоне, нужные отчёту.
struct TemplateFacts {
    name: String,
    ord: i64,
    qfmt: String,
    afmt: String,
}

/// Читает состояние экспорта и собирает факты, на которых стоит отчёт.
fn load_side(export_dir: &Path) -> Result<Side, DomainError> {
    let loaded = crate::loader::load_export(export_dir)?;
    let index = ExportIndex::build(&loaded.root);

    let mut guids: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut notes: Vec<NoteFacts> = Vec::with_capacity(index.notes.len());

    for entry in &index.notes {
        let model = entry
            .note
            .note_model_uuid
            .as_deref()
            .and_then(|uuid| index.model_by_uuid(uuid));
        let facts = note_facts(&index, entry, model);
        if let Some(guid) = entry.note.guid.as_deref()
            && !guid.is_empty()
        {
            guids.entry(guid.to_string()).or_default().push(notes.len());
        }
        notes.push(facts);
    }

    let mut models: BTreeMap<String, ModelFacts> = BTreeMap::new();
    for model in &index.models {
        if let Some(uuid) = &model.crowdanki_uuid {
            models.insert(uuid.clone(), model_facts(model));
        }
    }

    let mut decks: BTreeMap<String, Option<String>> = BTreeMap::new();
    collect_decks(&loaded.root, &mut decks);

    let notes_total = notes.len();

    Ok(Side {
        summary: ReportSide {
            export_dir: loaded.export_dir.clone(),
            deck_json: loaded.deck_json.clone(),
            deck_name: loaded.root.display_name().to_string(),
            deck_uuid: loaded.root.crowdanki_uuid.clone(),
            notes: notes_total,
            decks: decks.len(),
            models: models.len(),
        },
        index: SideIndex {
            guids,
            notes,
            decks,
            models,
        },
    })
}

/// Собирает факты о заметке.
fn note_facts(
    index: &ExportIndex<'_>,
    entry: &NoteRef<'_>,
    model: Option<&NoteModel>,
) -> NoteFacts {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut field_order: Vec<(String, String)> = Vec::new();

    if let Some(model) = model {
        for resolved in resolve_named_fields(entry.note, model) {
            let value = resolved.value.map(FieldValue::rendered).unwrap_or_default();
            fields.insert(resolved.name.to_string(), value.clone());
            field_order.push((resolved.name.to_string(), value));
        }
    } else {
        // Модель не разрешилась: показываем значения по позициям и не выдаём
        // позицию за имя поля.
        for (position, value) in entry.note.fields.iter().enumerate() {
            let name = format!("#{position}");
            let value = value.rendered();
            fields.insert(name.clone(), value.clone());
            field_order.push((name, value));
        }
    }

    NoteFacts {
        deck_path: index.note_deck_path(entry).to_string(),
        model_uuid: entry.note.note_model_uuid.clone(),
        tags: entry.note.tags.clone(),
        fields,
        field_order,
    }
}

/// Собирает факты о модели.
fn model_facts(model: &NoteModel) -> ModelFacts {
    let kind = match model.model_type.value() {
        Some(1) => ModelKind::Cloze,
        _ => ModelKind::Standard,
    };

    let mut field_names: BTreeSet<String> = BTreeSet::new();
    for field in model_fields_in_ord_order(model) {
        field_names.insert(field.name.clone());
    }

    // Порядок шаблонов задаёт `ord`; позиция в массиве — только разрешение
    // ничьей, чтобы результат не зависел от порядка ключей.
    let mut templates: Vec<(i64, usize, TemplateFacts)> = model
        .tmpls
        .iter()
        .enumerate()
        .map(|(position, template)| {
            (
                template.ord.value().unwrap_or(i64::MAX),
                position,
                TemplateFacts {
                    name: template_name(template, position),
                    ord: template.ord.value().unwrap_or(position as i64),
                    qfmt: template.qfmt.clone(),
                    afmt: template.afmt.clone(),
                },
            )
        })
        .collect();
    templates.sort_by_key(|item| (item.0, item.1));

    ModelFacts {
        name: model.name.clone().unwrap_or_default(),
        css: model.css.clone().unwrap_or_default(),
        kind,
        field_names,
        templates: templates.into_iter().map(|(_, _, item)| item).collect(),
    }
}

/// Имя шаблона так, как его видит Anki: безымянный шаблон называется по позиции.
fn template_name(template: &TemplateDef, position: usize) -> String {
    match template.name.as_deref() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => format!("Карточка {}", position + 1),
    }
}

/// Собирает узлы колод по имени вместе с их `crowdanki_uuid`.
fn collect_decks(node: &DeckNode, decks: &mut BTreeMap<String, Option<String>>) {
    decks.insert(node.display_name().to_string(), node.crowdanki_uuid.clone());
    for child in &node.children {
        collect_decks(child, decks);
    }
}

/// Итог классификации заметок.
struct Classification {
    counts: ReportCounts,
    outcomes: Vec<NoteOutcome>,
    ambiguous: Vec<NoteOutcome>,
    diagnostics: Vec<Diagnostic>,
    plans: Vec<SectionPlan>,
    every_note_classified: bool,
}

/// Описание раздела отчёта.
struct SectionPlan {
    kind: NoteChangeKind,
    title: &'static str,
    hint: &'static str,
    outcomes: Vec<NoteOutcome>,
}

/// Сопоставляет два состояния по `guid` и раскладывает заметки по категориям.
fn classify(before: &Side, after: &Side, retire_tag: Option<&str>) -> Classification {
    let mut counts = ReportCounts {
        notes_before: before.summary.notes,
        notes_after: after.summary.notes,
        ..ReportCounts::default()
    };
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut outcomes: Vec<NoteOutcome> = Vec::new();
    let mut ambiguous: Vec<NoteOutcome> = Vec::new();

    let mut created: Vec<NoteOutcome> = Vec::new();
    let mut changed: Vec<NoteOutcome> = Vec::new();
    let mut retired: Vec<NoteOutcome> = Vec::new();
    let mut removed: Vec<NoteOutcome> = Vec::new();
    let mut unchanged: Vec<NoteOutcome> = Vec::new();

    let mut guids: BTreeSet<&str> = BTreeSet::new();
    guids.extend(before.index.guids.keys().map(String::as_str));
    guids.extend(after.index.guids.keys().map(String::as_str));

    let mut accounted = 0usize;

    for guid in guids {
        let before_positions: &[usize] =
            before.index.guids.get(guid).map_or(&[][..], Vec::as_slice);
        let after_positions: &[usize] = after.index.guids.get(guid).map_or(&[][..], Vec::as_slice);

        if before_positions.is_empty() {
            if after_positions.len() > 1 {
                counts.ambiguous += 1;
                accounted += after_positions.len();
                ambiguous.push(ambiguous_outcome(
                    guid,
                    before,
                    after,
                    0,
                    after_positions.len(),
                ));
                continue;
            }
            for position in after_positions {
                counts.created += 1;
                accounted += 1;
                created.push(single_outcome(
                    guid,
                    before,
                    after,
                    None,
                    Some(*position),
                    NoteChangeKind::Created,
                ));
            }
            continue;
        }

        if after_positions.is_empty() {
            if before_positions.len() > 1 {
                counts.ambiguous += 1;
                accounted += before_positions.len();
                ambiguous.push(ambiguous_outcome(
                    guid,
                    before,
                    after,
                    before_positions.len(),
                    0,
                ));
                continue;
            }
            for position in before_positions {
                counts.removed += 1;
                accounted += 1;
                removed.push(single_outcome(
                    guid,
                    before,
                    after,
                    Some(*position),
                    None,
                    NoteChangeKind::Removed,
                ));
                diagnostics.push(Diagnostic::warning(
                    "physical_removal_detected",
                    format!(
                        "заметка с guid {guid:?} есть в состоянии «до» и отсутствует в «после»: \
                         CrowdAnki не удаляет заметки, исчезнувшие из JSON, поэтому это не \
                         удаление в Anki"
                    ),
                    Some(guid.to_string()),
                ));
            }
            continue;
        }

        if before_positions.len() > 1 || after_positions.len() > 1 {
            counts.ambiguous += 1;
            accounted += before_positions.len() + after_positions.len();
            ambiguous.push(ambiguous_outcome(
                guid,
                before,
                after,
                before_positions.len(),
                after_positions.len(),
            ));
            diagnostics.push(Diagnostic::warning(
                "ambiguous_guid",
                format!(
                    "guid {guid:?} встречается {} раз в «до» и {} раз в «после»: отчёт не \
                     выбирает заметку",
                    before_positions.len(),
                    after_positions.len()
                ),
                Some(guid.to_string()),
            ));
            continue;
        }

        let before_facts = &before.index.notes[before_positions[0]];
        let after_facts = &after.index.notes[after_positions[0]];
        accounted += 2;

        let kind = classify_pair(
            guid,
            before_facts,
            after_facts,
            retire_tag,
            &mut diagnostics,
            &mut counts,
        );
        let outcome = single_outcome(
            guid,
            before,
            after,
            Some(before_positions[0]),
            Some(after_positions[0]),
            kind,
        );

        match kind {
            NoteChangeKind::Unchanged => unchanged.push(outcome),
            NoteChangeKind::Retired => retired.push(outcome),
            _ => changed.push(outcome),
        }
    }

    let every_note_classified = accounted == before.summary.notes + after.summary.notes;

    diagnostics.extend(deck_diagnostics(before, after));
    outcomes.extend(created.iter().cloned());
    outcomes.extend(changed.iter().cloned());
    outcomes.extend(retired.iter().cloned());
    outcomes.extend(removed.iter().cloned());
    outcomes.extend(unchanged.iter().cloned());

    let plans = vec![
        SectionPlan {
            kind: NoteChangeKind::Created,
            title: "Созданные заметки",
            hint: "Заметок не было в состоянии «до». Превью собрано из фактических значений полей \
                   состояния «после».",
            outcomes: created,
        },
        SectionPlan {
            kind: NoteChangeKind::Changed,
            title: "Изменённые заметки",
            hint: "Diff сравнивает значения полей по токенам; превью показывает состояние «после».",
            outcomes: changed,
        },
        SectionPlan {
            kind: NoteChangeKind::Retired,
            title: "Выведенные из обращения",
            hint: "Изменился только массив тегов: содержимое заметок не тронуто, поэтому превью \
                   для них не строится.",
            outcomes: retired,
        },
        SectionPlan {
            kind: NoteChangeKind::Removed,
            title: "Исчезли из экспорта",
            hint: "Заметка была в состоянии «до» и отсутствует в «после». CrowdAnki не удаляет \
                   такие заметки: пропажа из JSON — не удаление в Anki.",
            outcomes: removed,
        },
    ];

    Classification {
        counts,
        outcomes,
        ambiguous,
        diagnostics,
        plans,
        every_note_classified,
    }
}

/// Классифицирует пару «одна заметка до, одна после».
fn classify_pair(
    guid: &str,
    before: &NoteFacts,
    after: &NoteFacts,
    retire_tag: Option<&str>,
    diagnostics: &mut Vec<Diagnostic>,
    counts: &mut ReportCounts,
) -> NoteChangeKind {
    if before.model_uuid != after.model_uuid {
        diagnostics.push(Diagnostic::warning(
            "structural_identity_change",
            format!(
                "у заметки {guid:?} изменился note_model_uuid: {:?} → {:?}; значения полей \
                 сравниваются, но их позиции принадлежат разным моделям",
                before.model_uuid, after.model_uuid
            ),
            Some(guid.to_string()),
        ));
        counts.changed += 1;
        return NoteChangeKind::Changed;
    }

    if before.deck_path != after.deck_path {
        diagnostics.push(Diagnostic::warning(
            "structural_identity_change",
            format!(
                "заметка {guid:?} сменила колоду: «{}» → «{}»",
                before.deck_path, after.deck_path
            ),
            Some(guid.to_string()),
        ));
    }

    let fields = changed_fields(before, after);
    let added: Vec<&String> = after
        .tags
        .iter()
        .filter(|tag| !before.tags.contains(tag))
        .collect();
    let dropped: Vec<&String> = before
        .tags
        .iter()
        .filter(|tag| !after.tags.contains(tag))
        .collect();
    let retirement = retire_tag.is_some_and(|tag| added.iter().any(|added| added.as_str() == tag));

    let kind = if !fields.is_empty() {
        NoteChangeKind::Changed
    } else if added.is_empty() && dropped.is_empty() {
        NoteChangeKind::Unchanged
    } else if retirement && dropped.is_empty() {
        NoteChangeKind::Retired
    } else {
        NoteChangeKind::Changed
    };

    match kind {
        NoteChangeKind::Unchanged => counts.unchanged += 1,
        NoteChangeKind::Retired => counts.retired += 1,
        _ => counts.changed += 1,
    }

    kind
}

/// Диагностика по идентичностям колод.
fn deck_diagnostics(before: &Side, after: &Side) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    if before.summary.deck_uuid != after.summary.deck_uuid {
        diagnostics.push(Diagnostic::warning(
            "root_deck_identity_changed",
            format!(
                "у корневой колоды изменился crowdanki_uuid: {:?} → {:?}",
                before.summary.deck_uuid, after.summary.deck_uuid
            ),
            Some(before.summary.deck_name.clone()),
        ));
    }

    for (path, uuid) in &before.index.decks {
        let Some(after_uuid) = after.index.decks.get(path) else {
            continue;
        };
        if uuid != after_uuid {
            diagnostics.push(Diagnostic::warning(
                "deck_identity_changed",
                format!("у колоды «{path}» изменился crowdanki_uuid: {uuid:?} → {after_uuid:?}"),
                Some(path.clone()),
            ));
        }
    }

    diagnostics
}

/// Имена полей, значения которых различаются между состояниями.
fn changed_fields(before: &NoteFacts, after: &NoteFacts) -> Vec<String> {
    let mut names: BTreeSet<&String> = BTreeSet::new();
    names.extend(before.fields.keys());
    names.extend(after.fields.keys());

    names
        .into_iter()
        .filter(|name| before.fields.get(*name) != after.fields.get(*name))
        .cloned()
        .collect()
}

/// Ищет факты заметки в состоянии по `guid`.
fn single_fact<'a>(side: &'a Side, guid: &str) -> Option<&'a NoteFacts> {
    side.index
        .guids
        .get(guid)
        .and_then(|positions| positions.first())
        .map(|position| &side.index.notes[*position])
}

/// Строит outcome для однозначной заметки.
fn single_outcome(
    guid: &str,
    before: &Side,
    after: &Side,
    before_position: Option<usize>,
    after_position: Option<usize>,
    kind: NoteChangeKind,
) -> NoteOutcome {
    let before_facts = before_position.map(|position| &before.index.notes[position]);
    let after_facts = after_position.map(|position| &after.index.notes[position]);

    let changed = match (before_facts, after_facts) {
        (Some(before_facts), Some(after_facts)) => changed_fields(before_facts, after_facts),
        _ => Vec::new(),
    };

    let model_uuid = after_facts
        .and_then(|facts| facts.model_uuid.clone())
        .or_else(|| before_facts.and_then(|facts| facts.model_uuid.clone()));
    let model_name = model_uuid
        .as_deref()
        .and_then(|uuid| {
            after
                .index
                .models
                .get(uuid)
                .or_else(|| before.index.models.get(uuid))
        })
        .map(|model| model.name.clone())
        .filter(|name| !name.is_empty());

    NoteOutcome {
        guid: guid.to_string(),
        kind,
        deck_path: after_facts
            .map(|facts| facts.deck_path.clone())
            .or_else(|| before_facts.map(|facts| facts.deck_path.clone()))
            .unwrap_or_default(),
        model_name,
        tags_before: before_facts
            .map(|facts| facts.tags.clone())
            .unwrap_or_default(),
        tags_after: after_facts
            .map(|facts| facts.tags.clone())
            .unwrap_or_default(),
        changed_fields: changed,
        before_occurrences: usize::from(before_facts.is_some()),
        after_occurrences: usize::from(after_facts.is_some()),
    }
}

/// Строит outcome для неоднозначного `guid`.
fn ambiguous_outcome(
    guid: &str,
    before: &Side,
    after: &Side,
    before_occurrences: usize,
    after_occurrences: usize,
) -> NoteOutcome {
    let deck_path = before
        .index
        .guids
        .get(guid)
        .and_then(|positions| positions.first())
        .map(|position| before.index.notes[*position].deck_path.clone())
        .or_else(|| {
            after
                .index
                .guids
                .get(guid)
                .and_then(|positions| positions.first())
                .map(|position| after.index.notes[*position].deck_path.clone())
        })
        .unwrap_or_default();

    NoteOutcome {
        guid: guid.to_string(),
        kind: NoteChangeKind::Ambiguous,
        deck_path,
        model_name: None,
        tags_before: Vec::new(),
        tags_after: Vec::new(),
        changed_fields: Vec::new(),
        before_occurrences,
        after_occurrences,
    }
}

/// Карточка отчёта для неоднозначного `guid` — без превью и без догадок.
fn ambiguous_card(outcome: &NoteOutcome) -> ReportCard {
    ReportCard {
        heading: format!("guid {}", bounded_sample(&outcome.guid)),
        deck_path: outcome.deck_path.clone(),
        guid: outcome.guid.clone(),
        model_name: "не определена".to_string(),
        tags_before: Vec::new(),
        tags_after: Vec::new(),
        fields: Vec::new(),
        diffs: Vec::new(),
        previews: Vec::new(),
        notes: vec![format!(
            "guid встречается {} раз в «до» и {} раз в «после»: заметка не отнесена ни к одной \
             категории, превью не строится",
            outcome.before_occurrences, outcome.after_occurrences
        )],
    }
}

/// Накопитель отчёта: файлы превью, media-ссылки и наблюдения по моделям.
struct Build {
    card_files: Vec<String>,
    card_documents: Vec<(String, CardFile)>,
    media_references: BTreeSet<String>,
    model_scans: BTreeMap<String, ModelScan>,
    touched_models: BTreeSet<String>,
    /// Наблюдения о неполных превью: собираются при сборке карточек.
    preview_issues: Vec<Diagnostic>,
}

impl Build {
    /// Готовит наблюдения по всем моделям состояния «после».
    fn new(after: &Side) -> Self {
        let mut model_scans: BTreeMap<String, ModelScan> = BTreeMap::new();
        for (uuid, model) in &after.index.models {
            model_scans.insert(uuid.clone(), scan_model(uuid, model));
        }
        Self {
            card_files: Vec::new(),
            card_documents: Vec::new(),
            media_references: BTreeSet::new(),
            model_scans,
            touched_models: BTreeSet::new(),
            preview_issues: Vec::new(),
        }
    }
}

/// Наблюдения по одной модели: что в её шаблонах не поддержано превью.
struct ModelScan {
    model_name: String,
    unsupported: BTreeSet<(String, String)>,
    unknown_fields: BTreeSet<String>,
    css_media_reference: bool,
}

/// Сканирует шаблоны модели.
fn scan_model(uuid: &str, model: &ModelFacts) -> ModelScan {
    let mut unsupported: BTreeSet<(String, String)> = BTreeSet::new();
    let mut unknown_fields: BTreeSet<String> = BTreeSet::new();

    for template in &model.templates {
        for text in [&template.qfmt, &template.afmt] {
            for construct in scan_constructs(text, model.kind) {
                match construct.kind {
                    ConstructKind::Unsupported => {
                        unsupported.insert((
                            construct.raw.clone(),
                            construct
                                .reason
                                .clone()
                                .unwrap_or_else(|| "причина не указана".to_string()),
                        ));
                    }
                    ConstructKind::Field
                    | ConstructKind::SectionOpen
                    | ConstructKind::SectionNegated => {
                        if !construct.name.is_empty()
                            && !model.field_names.contains(&construct.name)
                        {
                            unknown_fields.insert(construct.name.clone());
                        }
                    }
                    ConstructKind::SectionClose | ConstructKind::Special => {}
                }
            }
        }
    }

    let _ = uuid;
    ModelScan {
        model_name: model.name.clone(),
        unsupported,
        unknown_fields,
        css_media_reference: model.css.contains("url("),
    }
}

/// Диагностика по наблюдениям модели.
fn emit_model_diagnostics(scan: &ModelScan, uuid: &str, diagnostics: &mut Vec<Diagnostic>) {
    for name in &scan.unknown_fields {
        diagnostics.push(Diagnostic::info(
            "unknown_field_in_template",
            format!(
                "шаблон модели «{}» ссылается на поле {name:?}, которого нет в модели",
                scan.model_name
            ),
            Some(uuid.to_string()),
        ));
    }
    if scan.css_media_reference {
        diagnostics.push(Diagnostic::info(
            "unsupported_css_media_reference",
            format!(
                "CSS модели «{}» содержит url(): файлы, на которые ссылается CSS, в отчёт не \
                 копируются, и относительные ссылки в превью могут не открыться",
                scan.model_name
            ),
            Some(uuid.to_string()),
        ));
    }
}

/// Строит карточку отчёта, включая превью и diff.
fn build_card(
    outcome: &NoteOutcome,
    before: &Side,
    after: &Side,
    retire_tag: Option<&str>,
    build: &mut Build,
) -> ReportCard {
    let after_facts = single_fact(after, &outcome.guid);
    let before_facts = single_fact(before, &outcome.guid);

    let Some(after_facts) = after_facts else {
        // Заметка исчезла или её `guid` неоднозначен: превью строится по
        // состоянию «до», а не выдумывается.
        return ReportCard {
            heading: heading(outcome),
            deck_path: outcome.deck_path.clone(),
            guid: outcome.guid.clone(),
            model_name: outcome.model_name.clone().unwrap_or_default(),
            tags_before: outcome.tags_before.clone(),
            tags_after: outcome.tags_after.clone(),
            fields: before_facts
                .map(|facts| bounded_fields(&facts.field_order))
                .unwrap_or_default(),
            diffs: Vec::new(),
            previews: Vec::new(),
            notes: vec![
                "заметка отсутствует в состоянии «после»: превью показывает только значения «до»"
                    .to_string(),
            ],
        };
    };

    let model = after_facts
        .model_uuid
        .as_deref()
        .and_then(|uuid| after.index.models.get(uuid));

    let Some(model) = model else {
        return ReportCard {
            heading: heading(outcome),
            deck_path: outcome.deck_path.clone(),
            guid: outcome.guid.clone(),
            model_name: outcome.model_name.clone().unwrap_or_default(),
            tags_before: outcome.tags_before.clone(),
            tags_after: outcome.tags_after.clone(),
            fields: bounded_fields(&after_facts.field_order),
            diffs: Vec::new(),
            previews: Vec::new(),
            notes: vec![
                "модель заметки не разрешилась в состоянии «после»: превью не строится".to_string(),
            ],
        };
    };

    if let Some(uuid) = &after_facts.model_uuid {
        build.touched_models.insert(uuid.clone());
    }

    let diffs = field_diffs(outcome, before_facts, after_facts);
    let mut notes: Vec<String> = Vec::new();
    let mut previews: Vec<ReportPreview> = Vec::new();

    let fields = if outcome.kind == NoteChangeKind::Changed {
        Vec::new()
    } else {
        bounded_fields(&after_facts.field_order)
    };

    match outcome.kind {
        NoteChangeKind::Created | NoteChangeKind::Changed => {
            if model.templates.len() > MAX_TEMPLATES_PER_MODEL {
                notes.push(format!(
                    "у модели {} шаблонов: превью построено для первых {MAX_TEMPLATES_PER_MODEL}",
                    model.templates.len()
                ));
            }

            for template in model.templates.iter().take(MAX_TEMPLATES_PER_MODEL) {
                if build.card_files.len() >= MAX_CARD_FILES {
                    notes.push(format!(
                        "достигнут предел {MAX_CARD_FILES} файлов превью: остальные не записаны"
                    ));
                    break;
                }

                let document = match render_template(
                    template,
                    after_facts,
                    model,
                    &mut build.media_references,
                ) {
                    Ok(document) => document,
                    Err(reason) => {
                        notes.push(reason);
                        continue;
                    }
                };

                if document.sides.iter().any(|side| !side.issues.is_empty()) {
                    notes.push(format!(
                        "превью шаблона «{}» неполное: часть конструкций не вычисляется статически",
                        template.name
                    ));
                }

                // То же наблюдение обязано попасть и в диагностику, а не только в
                // HTML карточки: иначе машинный потребитель `--json` не узнает,
                // что превью неполное.
                let issues: Vec<String> = document
                    .sides
                    .iter()
                    .flat_map(|side| side.issues.iter().cloned())
                    .collect();
                if !issues.is_empty() {
                    build.preview_issues.push(Diagnostic::warning(
                        "preview_incomplete",
                        format!(
                            "превью шаблона «{}» заметки {} неполное: {}",
                            template.name,
                            outcome.guid,
                            issues.join("; ")
                        ),
                        Some(outcome.guid.clone()),
                    ));
                }

                let relative =
                    format!("{CARDS_SUBDIR}/card-{:04}.html", build.card_files.len() + 1);
                previews.push(ReportPreview {
                    label: template.name.clone(),
                    file: relative.clone(),
                    hint: format!("шаблон ord {}", template.ord),
                });
                build.card_files.push(relative.clone());
                build.card_documents.push((relative, document));
            }
        }
        NoteChangeKind::Retired => {
            notes.push(format!(
                "тег вывода из обращения: {}. Содержимое заметки не менялось, поэтому превью не \
                 строится.",
                retire_tag.unwrap_or("(не задан)")
            ));
        }
        _ => {}
    }

    ReportCard {
        heading: heading(outcome),
        deck_path: outcome.deck_path.clone(),
        guid: outcome.guid.clone(),
        model_name: model.name.clone(),
        tags_before: outcome.tags_before.clone(),
        tags_after: outcome.tags_after.clone(),
        fields,
        diffs,
        previews,
        notes,
    }
}

/// Обрезает значения полей для показа в отчёте.
fn bounded_fields(fields: &[(String, String)]) -> Vec<(String, String)> {
    fields
        .iter()
        .map(|(name, value)| (name.clone(), bounded_sample(value)))
        .collect()
}

/// Считает token-level diff по изменившимся полям.
fn field_diffs(
    outcome: &NoteOutcome,
    before: Option<&NoteFacts>,
    after: &NoteFacts,
) -> Vec<FieldDiff> {
    let mut names = outcome.changed_fields.clone();
    names.sort();

    names
        .into_iter()
        .map(|name| {
            let old = before
                .and_then(|facts| facts.fields.get(&name))
                .cloned()
                .unwrap_or_default();
            let new = after.fields.get(&name).cloned().unwrap_or_default();
            FieldDiff {
                field: name,
                tokens: diff_tokens_lossy(&old, &new)
                    .into_iter()
                    .map(|token| {
                        let mark = match token.kind {
                            TokenKind::Equal => DiffMark::Equal,
                            TokenKind::Inserted => DiffMark::Inserted,
                            TokenKind::Deleted => DiffMark::Deleted,
                        };
                        (mark, token.text)
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Заголовок карточки отчёта.
fn heading(outcome: &NoteOutcome) -> String {
    let sample = outcome
        .deck_path
        .rsplit("::")
        .next()
        .unwrap_or(&outcome.deck_path);
    format!("{} — {sample}", outcome.kind.label())
}

/// Рендерит один шаблон модели для заметки.
fn render_template(
    template: &TemplateFacts,
    facts: &NoteFacts,
    model: &ModelFacts,
    media_references: &mut BTreeSet<String>,
) -> Result<CardFile, String> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in &facts.field_order {
        media_references.extend(media_index::extract_media_references(value));
        fields.insert(name.clone(), value.clone());
    }

    let base = TemplateContext {
        fields: &fields,
        tags: &facts.tags,
        deck_path: &facts.deck_path,
        notetype_name: &model.name,
        template_name: &template.name,
        model_kind: model.kind,
        front_side: "",
    };

    // Два прохода: `{{FrontSide}}` на обратной стороне — это уже отрендеренная
    // лицевая сторона, а её значение известно только после первого прохода.
    // Рендер детерминирован, поэтому лицевая сторона второго прохода совпадает
    // с первой, и повторный вызов ничего не меняет.
    let first = render_card(&template.qfmt, &template.afmt, &base);
    let preview = render_card(
        &template.qfmt,
        &template.afmt,
        &TemplateContext {
            front_side: &first.front.html,
            ..base
        },
    );

    if !preview.generated && model.kind == ModelKind::Standard {
        return Err(format!(
            "шаблон «{}» не порождает карточку: лицевая сторона пуста после подстановки полей",
            template.name
        ));
    }

    let side = |label: &str, rendered: &RenderedSide| CardSide {
        label: label.to_string(),
        classes: format!("card card{}", template.ord + 1),
        html: rendered.html.clone(),
        issues: rendered
            .issues
            .iter()
            .map(|issue| format!("{}: {}", issue.construct, issue.reason))
            .collect(),
    };

    Ok(CardFile {
        title: format!("{} — {}", model.name, template.name),
        model_css: model.css.clone(),
        sides: vec![
            side("Лицевая сторона", &preview.front),
            side("Обратная сторона", &preview.back),
        ],
    })
}

/// Заменяет ссылки на media относительными путями внутри отчёта.
fn rewrite_media_refs(html: &str, plan: &MediaPlan, prefix: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(position) = rest.find("src=") {
        let (head, tail) = rest.split_at(position + "src=".len());
        out.push_str(head);

        let Some(quote) = tail.chars().next() else {
            break;
        };
        if quote != '"' && quote != '\'' {
            out.push_str(tail.split_at(quote.len_utf8()).0);
            rest = &tail[quote.len_utf8()..];
            continue;
        }

        let Some(end) = tail[1..].find(quote) else {
            out.push_str(tail);
            return out;
        };

        let reference = &tail[1..1 + end];
        match plan.resolved_name(reference) {
            Some(name) => {
                out.push_str(&format!(
                    "{quote}{prefix}{}/{name}{quote}",
                    media::MEDIA_SUBDIR
                ));
            }
            None => out.push_str(&format!("{quote}{reference}{quote}")),
        }
        rest = &tail[1 + end + 1..];
    }

    out.push_str(rest);
    out
}

/// Добавляет диагностику по работе с media.
fn push_media_diagnostics(plan: &MediaPlan, diagnostics: &mut Vec<Diagnostic>) {
    for name in &plan.missing {
        diagnostics.push(Diagnostic::warning(
            "missing_media",
            format!("файл media {name:?} не найден в экспорте: превью останется без него"),
            Some(name.clone()),
        ));
    }
    for reference in &plan.traversal {
        diagnostics.push(Diagnostic::warning(
            "media_path_traversal",
            format!(
                "ссылка {reference:?} содержит путь: в отчёт попадает только базовое имя файла"
            ),
            Some(reference.clone()),
        ));
    }
    for reference in &plan.remote {
        diagnostics.push(Diagnostic::info(
            "remote_media_reference",
            format!("ссылка {reference:?} ведёт вне экспорта и не копируется в отчёт"),
            Some(reference.clone()),
        ));
    }
    for name in &plan.symlinks {
        diagnostics.push(Diagnostic::warning(
            "media_symlink_skipped",
            format!("файл media {name:?} — символическая ссылка: он не читается и не копируется"),
            Some(name.clone()),
        ));
    }
    for name in &plan.oversized {
        diagnostics.push(Diagnostic::warning(
            "media_too_large",
            format!("файл media {name:?} превысил предел размера и не скопирован"),
            Some(name.clone()),
        ));
    }
    if plan.budget_skipped > 0 {
        diagnostics.push(Diagnostic::warning(
            "media_budget_exhausted",
            format!(
                "{} ссылок не обработано: достигнут предел числа копируемых файлов",
                plan.budget_skipped
            ),
            None,
        ));
    }
}

/// Проверяет и при необходимости создаёт каталог отчёта.
fn ensure_out_dir(out: &Path, export_dirs: &[PathBuf]) -> Result<PathBuf, DomainError> {
    let canonical = canonical_ish(out);

    if canonical
        .components()
        .any(|component| matches!(component, Component::Normal(name) if name == "decks"))
    {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "каталог отчёта {} находится внутри decks/: отчёт туда не пишется",
                canonical.display()
            ),
            details! {
                "reason" => "out_dir_inside_decks",
                "out_dir" => canonical.display().to_string(),
            },
        ));
    }

    for export_dir in export_dirs {
        if canonical == *export_dir
            || export_dir.starts_with(&canonical)
            || canonical.starts_with(export_dir)
        {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!(
                    "каталог отчёта {} пересекается с каталогом экспорта {}",
                    canonical.display(),
                    export_dir.display()
                ),
                details! {
                    "reason" => "out_dir_overlaps_export",
                    "out_dir" => canonical.display().to_string(),
                    "export_dir" => export_dir.display().to_string(),
                },
            ));
        }
    }

    if canonical.exists() && !canonical.is_dir() {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!("{} существует и не является каталогом", canonical.display()),
            details! {
                "reason" => "out_dir_not_a_directory",
                "out_dir" => canonical.display().to_string(),
            },
        ));
    }

    if canonical.is_dir() {
        let mut entries = 0usize;
        for entry in fs::read_dir(&canonical).map_err(|error| out_dir_error(&canonical, &error))? {
            entry.map_err(|error| out_dir_error(&canonical, &error))?;
            entries += 1;
        }

        // Перезапись собственного отчёта разрешена: в каталоге уже лежит
        // `index.html`. Любой другой непустой каталог — чужой, и стирать его
        // содержимое инструмент не имеет права.
        if entries > 0 && !canonical.join(INDEX_HTML).is_file() {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!(
                    "каталог отчёта {} не пуст и не похож на каталог отчёта: {entries} записей",
                    canonical.display()
                ),
                details! {
                    "reason" => "out_dir_not_empty",
                    "out_dir" => canonical.display().to_string(),
                    "entries" => entries,
                },
            ));
        }
    }

    fs::create_dir_all(&canonical).map_err(|error| out_dir_error(&canonical, &error))?;
    Ok(canonical)
}

/// Записывает файл отчёта.
fn write_report_file(path: &Path, bytes: &[u8]) -> Result<(), DomainError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| out_dir_error(parent, &error))?;
    }
    fs::write(path, bytes).map_err(|error| {
        DomainError::with_details(
            ErrorCode::WriteFailed,
            format!("не удалось записать {}: {error}", path.display()),
            details! {
                "reason" => "report_write_failed",
                "path" => path.display().to_string(),
            },
        )
    })
}

fn out_dir_error(path: &Path, error: &std::io::Error) -> DomainError {
    DomainError::with_details(
        ErrorCode::WriteFailed,
        format!(
            "не удалось подготовить каталог отчёта {}: {error}",
            path.display()
        ),
        details! {
            "reason" => "out_dir_prepare_failed",
            "path" => path.display().to_string(),
        },
    )
}

/// Канонизирует настолько, насколько это возможно для ещё не созданного пути.
fn canonical_ish(path: &Path) -> PathBuf {
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
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }

    let mut result = fs::canonicalize(&existing).unwrap_or(existing);
    for name in tail.iter().rev() {
        result.push(name);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MINIMAL_EXPORT, TempDir, export_with};

    fn write_export(dir: &Path, json: &str) {
        fs::create_dir_all(dir).expect("каталог экспорта");
        fs::write(dir.join("deck.json"), json).expect("deck.json");
    }

    #[test]
    fn report_classifies_created_changed_and_retired_notes() {
        let dir = TempDir::new("visual-report-diff");
        let before_dir = dir.path().join("before");
        let after_dir = dir.path().join("after");
        let out = dir.path().join("out");

        let after = export_with(MINIMAL_EXPORT, |value| {
            let notes = value["notes"].as_array_mut().expect("notes");
            // guid-1: изменено значение поля и добавлен тег вывода из обращения
            notes[0]["fields"][1] = serde_json::json!("случайность (перевод)");
            notes[0]["tags"] = serde_json::json!(["тэг", "retired::auto"]);
            // guid-2: не тронута
            // новая заметка
            notes.push(serde_json::json!({
                "__type__": "Note",
                "guid": "guid-3",
                "note_model_uuid": "model-1",
                "tags": [],
                "fields": ["新", "новое"]
            }));
        });

        write_export(&before_dir, MINIMAL_EXPORT);
        write_export(&after_dir, &after);

        let mut request = ReportRequest::new(&before_dir, &after_dir, &out);
        request.retire_tag = Some("retired::auto".to_string());
        let result = report(&request).expect("отчёт");

        assert_eq!(result.counts.created, 1);
        assert_eq!(result.counts.changed, 1);
        assert_eq!(result.counts.retired, 0);
        assert_eq!(result.counts.unchanged, 1);
        assert_eq!(result.counts.removed, 0);
        assert!(result.checks.every_note_classified);
        assert!(result.checks.all_files_inside_out_dir);
        assert!(result.checks.index_without_external_assets);
        assert!(result.checks.out_dir_outside_decks);
        assert_eq!(result.card_files_total, 2, "превью для created и changed");

        let index = fs::read_to_string(&result.index_html).expect("index.html");
        assert!(index.contains("Изменённые заметки"));
        assert!(index.contains("Созданные заметки"));

        // Заметка с изменённым полем осталась в категории «изменена», потому что
        // изменилось не только поле: тег вывода из обращения разбирается отдельно.
        let changed = result
            .outcomes
            .iter()
            .find(|outcome| outcome.guid == "guid-1")
            .expect("guid-1");
        assert_eq!(changed.kind, NoteChangeKind::Changed);
        assert_eq!(changed.changed_fields, vec!["Толкование".to_string()]);
    }

    #[test]
    fn retired_is_recognised_when_only_the_tag_changed() {
        let dir = TempDir::new("visual-report-retired");
        let before_dir = dir.path().join("before");
        let after_dir = dir.path().join("after");
        let out = dir.path().join("out");

        let after = export_with(MINIMAL_EXPORT, |value| {
            let notes = value["notes"].as_array_mut().expect("notes");
            notes[1]["tags"] = serde_json::json!(["retired::auto"]);
        });

        write_export(&before_dir, MINIMAL_EXPORT);
        write_export(&after_dir, &after);

        let mut request = ReportRequest::new(&before_dir, &after_dir, &out);
        request.retire_tag = Some("retired::auto".to_string());
        let result = report(&request).expect("отчёт");

        assert_eq!(result.counts.retired, 1);
        assert_eq!(result.counts.changed, 0);
        assert_eq!(result.card_files_total, 0, "для retired превью не строится");

        let index = fs::read_to_string(&result.index_html).expect("index.html");
        assert!(index.contains("Выведенные из обращения"));
        assert!(index.contains("report-tag-added"));
    }

    #[test]
    fn removed_note_is_reported_as_not_a_deletion() {
        let dir = TempDir::new("visual-report-removed");
        let before_dir = dir.path().join("before");
        let after_dir = dir.path().join("after");
        let out = dir.path().join("out");

        let after = export_with(MINIMAL_EXPORT, |value| {
            let notes = value["notes"].as_array_mut().expect("notes");
            notes.remove(1);
        });

        write_export(&before_dir, MINIMAL_EXPORT);
        write_export(&after_dir, &after);

        let result = report(&ReportRequest::new(&before_dir, &after_dir, &out)).expect("отчёт");

        assert_eq!(result.counts.removed, 1);
        assert!(result.checks.every_note_classified);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "physical_removal_detected")
        );
    }

    #[test]
    fn duplicated_guid_is_diagnosed_and_not_classified() {
        let dir = TempDir::new("visual-report-ambiguous");
        let before_dir = dir.path().join("before");
        let after_dir = dir.path().join("after");
        let out = dir.path().join("out");

        let after = export_with(MINIMAL_EXPORT, |value| {
            let notes = value["notes"].as_array_mut().expect("notes");
            let clone = notes[1].clone();
            notes.push(clone);
        });

        write_export(&before_dir, MINIMAL_EXPORT);
        write_export(&after_dir, &after);

        let result = report(&ReportRequest::new(&before_dir, &after_dir, &out)).expect("отчёт");

        assert_eq!(result.counts.ambiguous, 1);
        assert_eq!(result.counts.created, 0);
        assert_eq!(result.counts.unchanged, 1);
        assert!(result.checks.every_note_classified);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "ambiguous_guid")
        );
        assert_eq!(
            result.card_files_total, 0,
            "неоднозначный guid превью не получает"
        );
    }

    #[test]
    fn missing_media_is_diagnosed_not_invented() {
        let dir = TempDir::new("visual-report-media");
        let before_dir = dir.path().join("before");
        let after_dir = dir.path().join("after");
        let out = dir.path().join("out");

        write_export(&before_dir, MINIMAL_EXPORT);
        write_export(&after_dir, MINIMAL_EXPORT);

        let mut request = ReportRequest::new(&before_dir, &after_dir, &out);
        request.preview_limit = 10;
        let _ = report(&request).expect("отчёт");

        write_export(&before_dir, MINIMAL_EXPORT);
        let changed = export_with(MINIMAL_EXPORT, |value| {
            let notes = value["notes"].as_array_mut().expect("notes");
            notes[0]["fields"][1] = serde_json::json!("другое значение");
        });
        write_export(&after_dir, &changed);

        let result = report(&request).expect("отчёт");
        assert_eq!(result.media.missing, vec!["a.mp3".to_string()]);
        assert!(result.media.copied == 0);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "missing_media")
        );
    }

    /// Колонки «было» и «стало» — это значения поля, а не одна и та же смесь
    /// токенов: удалённый фрагмент принадлежит только состоянию «до».
    #[test]
    fn changed_note_diff_keeps_each_side_apart() {
        let dir = TempDir::new("visual-report-diff-sides");
        let before_dir = dir.path().join("before");
        let after_dir = dir.path().join("after");
        let out = dir.path().join("out");

        // Токен «然» удалён из значения поля; остальное значение не менялось.
        let after = export_with(MINIMAL_EXPORT, |value| {
            let notes = value["notes"].as_array_mut().expect("notes");
            notes[0]["fields"][0] = serde_json::json!("[sound:a.mp3]偶");
        });

        write_export(&before_dir, MINIMAL_EXPORT);
        write_export(&after_dir, &after);

        let result = report(&ReportRequest::new(&before_dir, &after_dir, &out)).expect("отчёт");
        assert_eq!(result.counts.changed, 1);

        let index = fs::read_to_string(&result.index_html).expect("index.html");
        let (before_cell, after_cell) = diff_row(&index, "Заголовок");

        assert_eq!(strip_markup(&before_cell), "[sound:a.mp3]偶然");
        assert_eq!(strip_markup(&after_cell), "[sound:a.mp3]偶");
        assert!(
            before_cell.contains("report-token-del"),
            "удалённый фрагмент помечен в колонке «было»"
        );
        assert!(
            !after_cell.contains("report-token-del"),
            "колонка «стало» не воспроизводит удалённый фрагмент"
        );
    }

    /// Возвращает html двух колонок diff для указанного поля.
    fn diff_row(index: &str, field: &str) -> (String, String) {
        let marker = format!("<td>{field}</td>");
        let start = index.find(&marker).expect("строка diff изменённого поля");
        let rest = &index[start + marker.len()..];
        let row = &rest[..rest.find("</tr>").expect("конец строки diff")];
        let mut cells = row
            .split("<td>")
            .skip(1)
            .map(|cell| cell[..cell.find("</td>").expect("закрытие ячейки")].to_string());
        (
            cells.next().expect("колонка «было»"),
            cells.next().expect("колонка «стало»"),
        )
    }

    /// Текст ячейки так, как его увидит читатель.
    fn strip_markup(cell: &str) -> String {
        let mut out = String::new();
        let mut rest = cell;
        while let Some(start) = rest.find('<') {
            out.push_str(&rest[..start]);
            let end = rest[start..].find('>').expect("закрытая скобка тега");
            rest = &rest[start + end + 1..];
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn identical_states_produce_no_changes() {
        let dir = TempDir::new("visual-report-identical");
        let before_dir = dir.path().join("before");
        let after_dir = dir.path().join("after");
        let out = dir.path().join("out");

        write_export(&before_dir, MINIMAL_EXPORT);
        write_export(&after_dir, MINIMAL_EXPORT);

        let result = report(&ReportRequest::new(&before_dir, &after_dir, &out)).expect("отчёт");

        assert_eq!(result.counts.unchanged, 2);
        assert_eq!(result.counts.created, 0);
        assert_eq!(result.card_files_total, 0);
        assert!(result.checks.every_note_classified);
    }

    #[test]
    fn out_dir_inside_decks_is_refused() {
        let error =
            ensure_out_dir(Path::new("decks/japanese/words/report"), &[]).expect_err("отказ");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.details["reason"], "out_dir_inside_decks");
    }

    #[test]
    fn out_dir_overlapping_export_is_refused() {
        let dir = TempDir::new("visual-report-out-overlap");
        let export = dir.path().join("export");
        fs::create_dir_all(&export).expect("каталог");
        let export = canonical_ish(&export);

        let error = ensure_out_dir(&export.join("report"), std::slice::from_ref(&export))
            .expect_err("отказ");
        assert_eq!(error.details["reason"], "out_dir_overlaps_export");
    }

    #[test]
    fn non_empty_foreign_out_dir_is_refused() {
        let dir = TempDir::new("visual-report-out-foreign");
        let out = dir.path().join("out");
        fs::create_dir_all(&out).expect("каталог");
        fs::write(out.join("чужое.txt"), b"x").expect("файл");

        let error = ensure_out_dir(&out, &[]).expect_err("отказ");
        assert_eq!(error.details["reason"], "out_dir_not_empty");
    }

    #[test]
    fn previous_report_dir_is_reusable() {
        let dir = TempDir::new("visual-report-out-reuse");
        let out = dir.path().join("out");
        fs::create_dir_all(&out).expect("каталог");
        fs::write(out.join(INDEX_HTML), b"<html></html>").expect("файл");

        assert!(ensure_out_dir(&out, &[]).is_ok());
    }

    #[test]
    fn media_refs_are_rewritten_only_for_copied_files() {
        let dir = TempDir::new("visual-report-rewrite");
        let source = dir.path().join("source");
        fs::create_dir_all(source.join("media")).expect("media");
        fs::write(source.join("media/есть.png"), b"x").expect("файл");

        let references: BTreeSet<String> = ["есть.png".to_string()].into_iter().collect();
        let plan =
            media::plan(&[source], &references, &dir.path().join("out")).expect("план media");

        let html = "<img src=\"есть.png\"><img src=\"нет.png\"><img src=\"https://x/у.png\">";
        let rewritten = rewrite_media_refs(html, &plan, "../");

        assert!(rewritten.contains("src=\"../media/есть.png\""));
        assert!(rewritten.contains("src=\"нет.png\""));
        assert!(rewritten.contains("src=\"https://x/у.png\""));
    }

    #[test]
    fn report_is_deterministic() {
        let dir = TempDir::new("visual-report-deterministic");
        let before_dir = dir.path().join("before");
        let after_dir = dir.path().join("after");
        let first = dir.path().join("out-1");
        let second = dir.path().join("out-2");

        let after = export_with(MINIMAL_EXPORT, |value| {
            let notes = value["notes"].as_array_mut().expect("notes");
            notes[0]["fields"][0] = serde_json::json!("изменено");
        });

        write_export(&before_dir, MINIMAL_EXPORT);
        write_export(&after_dir, &after);

        report(&ReportRequest::new(&before_dir, &after_dir, &first)).expect("первый отчёт");
        report(&ReportRequest::new(&before_dir, &after_dir, &second)).expect("второй отчёт");

        let first_index = fs::read(first.join(INDEX_HTML)).expect("index-1");
        let second_index = fs::read(second.join(INDEX_HTML)).expect("index-2");
        assert_eq!(first_index, second_index, "HTML отчёта обязан совпадать");

        let first_card = fs::read(first.join(CARDS_SUBDIR).join("card-0001.html")).expect("card-1");
        let second_card =
            fs::read(second.join(CARDS_SUBDIR).join("card-0001.html")).expect("card-2");
        assert_eq!(first_card, second_card, "файлы превью обязаны совпадать");
    }

    #[test]
    fn retire_tag_is_validated() {
        let dir = TempDir::new("visual-report-tag");
        let before_dir = dir.path().join("before");
        let after_dir = dir.path().join("after");
        write_export(&before_dir, MINIMAL_EXPORT);
        write_export(&after_dir, MINIMAL_EXPORT);

        let mut request = ReportRequest::new(&before_dir, &after_dir, dir.path().join("out"));
        request.retire_tag = Some("плохой тег".to_string());

        let error = report(&request).expect_err("отказ");
        assert_eq!(error.details["reason"], "invalid_tag");
    }

    #[test]
    fn same_directory_pair_is_refused() {
        let dir = TempDir::new("visual-report-same");
        let export = dir.path().join("export");
        write_export(&export, MINIMAL_EXPORT);

        let request = ReportRequest::new(&export, &export, dir.path().join("out"));
        let error = report(&request).expect_err("отказ");
        assert_eq!(error.details["reason"], "before_equals_after");
    }
}
