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
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::{ExportIndex, NoteRef, model_fields_in_ord_order, resolve_named_fields};
use crate::media as media_index;
use crate::model::{DeckNode, FieldValue, NoteModel, TemplateDef};
use crate::ops::retire::validate_tag;
use crate::report::SideState;
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
///
/// Общие списки (`copied`, `missing`, …) описывают отчёт целиком, а `states`
/// называет то же самое по состояниям: одинаковое имя файла в «до» и «после» —
/// это два разных факта, и по общей сводке их не различить.
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
    /// Работа с media по состояниям.
    pub states: Vec<(SideState, media::StateMedia)>,
}

/// Файл превью вместе с тем, что он доказывает: состояние, заметку и модель.
#[derive(Debug, Clone)]
pub struct PreviewFileFact {
    /// Относительный путь файла внутри каталога отчёта.
    pub file: String,
    /// Состояние, из которого построено превью.
    pub state: SideState,
    /// `guid` заметки.
    pub guid: String,
    /// Имя модели.
    pub model_name: String,
    /// Имя шаблона.
    pub template_name: String,
    /// Подпись шаблона в превью: `до`/`после` и ord.
    pub hint: String,
    /// Относительные пути media, которые превью использует.
    pub media: Vec<String>,
    /// Звуки без файла: показаны как отсутствующие.
    pub missing_sounds: Vec<String>,
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
    /// Что доказывает каждый файл превью (обрезано до [`MAX_LISTED_NOTES`]).
    pub preview_files: Vec<PreviewFileFact>,
    /// Был ли список превью обрезан.
    pub preview_files_truncated: bool,
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
    let mut build = Build::new(&before, &after);

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

    // Источники — каталоги экспорта вместе с их состоянием, а не их `media`:
    // подкаталог выбирает сам план, и второй раз присоединять его здесь означало
    // бы искать файлы в `media/media/`. Состояние обязательно: файл ищется только
    // в своём экспорте, иначе превью «до» показало бы файл из «после».
    let source_exports = [
        (SideState::Before, before.summary.export_dir.clone()),
        (SideState::After, after.summary.export_dir.clone()),
    ];
    let mut media_plan = media::plan(&source_exports, &build.media_references, &out_dir)?;
    media::note_symlinks(&mut media_plan, &source_exports, &build.media_references);
    push_media_diagnostics(&media_plan, &mut diagnostics);
    diagnostics.append(&mut build.preview_issues);

    let mut touched: Vec<(SideState, String)> = build
        .touched_models
        .iter()
        .flat_map(|(state, uuids)| uuids.iter().map(|uuid| (*state, uuid.clone())))
        .collect();
    touched.sort();
    for (state, uuid) in &touched {
        if let Some(scan) = build
            .model_scans
            .get(state)
            .and_then(|scans| scans.get(uuid))
        {
            emit_model_diagnostics(scan, uuid, &mut diagnostics);
        }
    }

    // Подстановка ссылок выполняется после планирования media: до него неизвестно,
    // какие файлы окажутся рядом с отчётом. Ссылка состояния подставляется только
    // на файл своего состояния, а звук без файла честно помечается отсутствующим.
    let mut written: Vec<PathBuf> = Vec::new();
    let mut preview_files: Vec<PreviewFileFact> = Vec::new();
    let documents = std::mem::take(&mut build.card_documents);
    for document in &documents {
        let mut card = document.document.clone();
        let mut resolved_media: BTreeSet<String> = BTreeSet::new();
        let mut missing_sounds: BTreeSet<String> = BTreeSet::new();
        for side in &mut card.sides {
            side.html = rewrite_preview_references(
                &side.html,
                document.state,
                &media_plan,
                &mut resolved_media,
                &mut missing_sounds,
            );
        }
        let path = out_dir.join(&document.file);
        write_report_file(&path, card_html(&card).as_bytes())?;
        written.push(path);
        preview_files.push(PreviewFileFact {
            file: document.file.clone(),
            state: document.state,
            guid: document.guid.clone(),
            model_name: document.model_name.clone(),
            template_name: document.template_name.clone(),
            hint: document.hint.clone(),
            media: resolved_media.into_iter().collect(),
            missing_sounds: missing_sounds.into_iter().collect(),
        });
    }

    // Одна и та же модель может быть затронута в обоих состояниях: в списке
    // неподдержанных конструкций она обязана появиться один раз.
    let mut unsupported: Vec<UnsupportedConstruct> = Vec::new();
    for item in touched
        .iter()
        .filter_map(|(state, uuid)| {
            build
                .model_scans
                .get(state)
                .and_then(|scans| scans.get(uuid))
        })
        .flat_map(|scan| {
            scan.unsupported
                .iter()
                .map(|(construct, reason)| UnsupportedConstruct {
                    construct: construct.clone(),
                    reason: reason.clone(),
                })
        })
    {
        if !unsupported
            .iter()
            .any(|known| known.construct == item.construct && known.reason == item.reason)
        {
            unsupported.push(item);
        }
    }

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
    // Скопированный файл обязан лежать в подкаталоге media своего состояния:
    // проверка повторяет границу каталога независимо от того, как планировщик
    // сложил пути.
    let media_confined = SideState::ALL.iter().all(|state| {
        let dir = out_dir.join(state.media_dir());
        media_plan.state(*state).copied.iter().all(|name| {
            !name.contains('/')
                && !name.contains('\\')
                && dir.join(name).starts_with(&out_dir)
                && dir.join(name).is_file()
        })
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
        preview_files_truncated: preview_files.len() > MAX_LISTED_NOTES,
        preview_files: preview_files.into_iter().take(MAX_LISTED_NOTES).collect(),
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
            copied: media_plan.copied_total(),
            missing: media_plan.missing_union(),
            traversal: media_plan.traversal_union(),
            remote: media_plan.remote_union(),
            symlinks: media_plan.symlinks_union(),
            oversized: media_plan.oversized_union(),
            budget_skipped: media_plan.budget_skipped,
            states: SideState::ALL
                .iter()
                .map(|state| (*state, media_plan.state(*state)))
                .collect(),
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
        "Превью разметки статическое: cloze-разметка и фильтры (cloze, type, tts, furigana, \
         hint) не вычисляются. Неподдержанные конструкции видны в самом превью сырым текстом и \
         перечислены отдельным разделом."
            .to_string(),
        "Свой код отчёта выполняется: он переключает светлую и ночную тему, подгоняет высоту \
         кадра по содержимому и показывает звук проигрывателем. Он детерминированно записан в \
         сами файлы отчёта и не ходит в сеть. Код шаблона Anki при этом не выполняется: \
         `<script>` из шаблона или значения поля остаётся неподдержанной конструкцией и показан \
         в превью текстом, а не тегом."
            .to_string(),
        "Обёртка карточки собрана как `card card{N}`, где N — ord шаблона плюс один. Ночная тема \
         переключается классом `nightMode` на корне документа и на самой карточке, как в Anki, \
         поэтому селекторы модели `.card.nightMode` и `.nightMode .…` применяются; тёмные \
         значения по умолчанию — разумное приближение, а не копия темы Anki. Масштаб шрифта \
         интерфейса и темы устройства не воспроизводятся."
            .to_string(),
        "Базовые стили страницы превью — приближение базовых стилей Anki, а не их копия. \
         Оформление карточки задаёт CSS модели, и он подключён после базовых правил."
            .to_string(),
        "Media копируются по состояниям, в `media/before` и `media/after`: одинаковое имя файла \
         в двух экспортах — два разных файла. Рядом с отчётом лежат только те файлы, которые \
         нашлись в `media/` своего состояния. `media_files` — список ссылок, а не доказательство \
         наличия файла, поэтому отсутствующие файлы перечислены в диагностике, а не выдуманы."
            .to_string(),
        "Звук `[sound:имя]` показывается локальным проигрывателем. Если файла нет, ссылка \
         остаётся видимой и помеченной отсутствующей: подставлять чужой звук отчёт не будет."
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
         поэтому каталог отчёта нужно переносить целиком. Без JavaScript отчёт остаётся \
         читаемым: превью видны полностью, но высота кадра не подстраивается под содержимое."
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
    card_documents: Vec<CardDocument>,
    /// Media-ссылки по состояниям: файл состояния ищется только в своём экспорте.
    media_references: BTreeMap<SideState, BTreeSet<String>>,
    /// Наблюдения по моделям, отдельно для каждого состояния.
    model_scans: BTreeMap<SideState, BTreeMap<String, ModelScan>>,
    /// Модели, для которых строилось превью, отдельно для каждого состояния.
    touched_models: BTreeMap<SideState, BTreeSet<String>>,
    /// Наблюдения о неполных превью: собираются при сборке карточек.
    preview_issues: Vec<Diagnostic>,
}

impl Build {
    /// Готовит наблюдения по моделям обоих состояний.
    ///
    /// Модели сканируются в обоих состояниях, потому что превью «до» строится по
    /// модели «до»: правка могла быть именно в шаблоне, и тогда шаблон «после» к
    /// превью «до» отношения не имеет.
    fn new(before: &Side, after: &Side) -> Self {
        let mut model_scans: BTreeMap<SideState, BTreeMap<String, ModelScan>> = BTreeMap::new();
        for (state, side) in [(SideState::Before, before), (SideState::After, after)] {
            let mut scans: BTreeMap<String, ModelScan> = BTreeMap::new();
            for (uuid, model) in &side.index.models {
                scans.insert(uuid.clone(), scan_model(uuid, model));
            }
            model_scans.insert(state, scans);
        }
        Self {
            card_files: Vec::new(),
            card_documents: Vec::new(),
            media_references: BTreeMap::new(),
            model_scans,
            touched_models: BTreeMap::new(),
            preview_issues: Vec::new(),
        }
    }

    /// Запоминает, что модель участвовала в превью этого состояния.
    fn touch_model(&mut self, state: SideState, uuid: &str) {
        self.touched_models
            .entry(state)
            .or_default()
            .insert(uuid.to_string());
    }
}

/// Документ превью вместе с тем, из чего он построен.
///
/// Состояние — не подпись, а источник: по нему выбираются media-файлы, поэтому
/// документ обязан помнить его до самого момента записи.
struct CardDocument {
    file: String,
    state: SideState,
    guid: String,
    model_name: String,
    template_name: String,
    hint: String,
    document: CardFile,
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
        // Заметка исчезла или её `guid` неоднозначен: состояние «до» остаётся
        // единственным источником правды, и превью строится по нему. Состояние
        // «после» не выдумывается: пустой кадр выглядел бы как «стало пусто».
        let mut notes =
            vec!["заметка отсутствует в состоянии «после»: показано состояние «до»".to_string()];
        let before_model = before_facts
            .and_then(|facts| facts.model_uuid.as_deref())
            .and_then(|uuid| before.index.models.get(uuid));
        let previews = match (before_facts, before_model) {
            (Some(facts), Some(model)) => {
                if let Some(uuid) = &facts.model_uuid {
                    build.touch_model(SideState::Before, uuid);
                }
                build_state_previews(outcome, SideState::Before, facts, model, &mut notes, build)
            }
            _ => {
                notes.push(
                    "модель заметки не разрешилась в состоянии «до»: превью не строится"
                        .to_string(),
                );
                Vec::new()
            }
        };
        return ReportCard {
            heading: heading(outcome),
            deck_path: outcome.deck_path.clone(),
            guid: outcome.guid.clone(),
            model_name: before_model
                .map(|model| model.name.clone())
                .unwrap_or_else(|| outcome.model_name.clone().unwrap_or_default()),
            tags_before: outcome.tags_before.clone(),
            tags_after: outcome.tags_after.clone(),
            fields: before_facts
                .map(|facts| bounded_fields(&facts.field_order))
                .unwrap_or_default(),
            diffs: Vec::new(),
            previews,
            notes,
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
        build.touch_model(SideState::After, uuid);
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

            // Изменённая заметка показывается обеими сторонами: ревьюер должен
            // видеть, что было, а не только что осталось. Кадр «до» строится по
            // заметке и модели состояния «до», а не украшается diff'ом поверх
            // «после». Порядок сборки задаёт нумерацию файлов, поэтому «до» идёт
            // первым — так номер файла совпадает с порядком колонок.
            if outcome.kind == NoteChangeKind::Changed
                && let Some(before_facts) = before_facts
                && let Some(before_model) = before_facts
                    .model_uuid
                    .as_deref()
                    .and_then(|uuid| before.index.models.get(uuid))
            {
                if let Some(uuid) = &before_facts.model_uuid {
                    build.touch_model(SideState::Before, uuid);
                }
                previews = build_state_previews(
                    outcome,
                    SideState::Before,
                    before_facts,
                    before_model,
                    &mut notes,
                    build,
                );
            }

            previews.extend(build_state_previews(
                outcome,
                SideState::After,
                after_facts,
                model,
                &mut notes,
                build,
            ));
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

/// Строит превью одной заметки для одного состояния экспорта.
///
/// Состояние передаётся явно и не выводится из модели: этим определяется и
/// набор media-файлов, и то, какой документ куда попал.
fn build_state_previews(
    outcome: &NoteOutcome,
    state: SideState,
    facts: &NoteFacts,
    model: &ModelFacts,
    notes: &mut Vec<String>,
    build: &mut Build,
) -> Vec<ReportPreview> {
    let mut previews: Vec<ReportPreview> = Vec::new();

    for template in model.templates.iter().take(MAX_TEMPLATES_PER_MODEL) {
        if build.card_files.len() >= MAX_CARD_FILES {
            notes.push(format!(
                "достигнут предел {MAX_CARD_FILES} файлов превью: остальные не записаны"
            ));
            break;
        }

        let references = build.media_references.entry(state).or_default();
        let document = match render_template(template, facts, model, references) {
            Ok(document) => document,
            Err(reason) => {
                notes.push(reason);
                continue;
            }
        };

        if document.sides.iter().any(|side| !side.issues.is_empty()) {
            notes.push(format!(
                "превью шаблона «{}» ({}) неполное: часть конструкций не вычисляется статически",
                template.name,
                state.label()
            ));
        }

        // То же наблюдение обязано попасть и в диагностику, а не только в HTML
        // карточки: иначе машинный потребитель `--json` не узнает, что превью
        // неполное.
        let issues: Vec<String> = document
            .sides
            .iter()
            .flat_map(|side| side.issues.iter().cloned())
            .collect();
        if !issues.is_empty() {
            build.preview_issues.push(Diagnostic::warning(
                "preview_incomplete",
                format!(
                    "превью шаблона «{}» ({}) заметки {} неполное: {}",
                    template.name,
                    state.label(),
                    outcome.guid,
                    issues.join("; ")
                ),
                Some(outcome.guid.clone()),
            ));
        }

        let relative = format!("{CARDS_SUBDIR}/card-{:04}.html", build.card_files.len() + 1);
        // Состояние уже названо подписью кадра и полем `state` в JSON: повторять
        // его в подсказке — шум для читателя отчёта.
        let hint = format!("шаблон ord {}", template.ord);
        previews.push(ReportPreview {
            label: template.name.clone(),
            state,
            file: relative.clone(),
            hint: hint.clone(),
            resolved_media: Vec::new(),
            missing_sounds: Vec::new(),
        });
        build.card_files.push(relative.clone());
        build.card_documents.push(CardDocument {
            file: relative,
            state,
            guid: outcome.guid.clone(),
            model_name: model.name.clone(),
            template_name: template.name.clone(),
            hint,
            document,
        });
    }

    previews
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

/// Подставляет в готовый HTML превью ссылки своего состояния и звук.
///
/// Ссылка состояния разрешается только на файл этого состояния: план хранит
/// разрешение с ключом `(состояние, ссылка)`, поэтому превью «до» не может
/// показать файл из «после».
///
/// Локальная картинка, которой в этом состоянии нет, заменяется видимым
/// плейсхолдером вместе со всем элементом `<img>`. Оставить исходный `src` нельзя:
/// браузер запросил бы несуществующий файл и нарисовал свою иконку сломанного
/// изображения, которая молчит о том, чего именно не хватает, и сама участвует в
/// inline-раскладке. Отсутствие при этом устанавливается по данным плана этого
/// состояния, а не по имени файла: внешние ссылки, ссылки с путём и прочие
/// media-классы плейсхолдером не подменяются.
///
/// Звук (`[sound:имя]`) превращается в локальный проигрыватель с классом
/// `replay-button` — тем же хуком, которым его оформляет модель в Anki.
/// Отсутствующий файл не выдумывается и не заменяется: ссылка остаётся видимой и
/// помеченной, а её имя попадает в диагностику и в JSON-результат.
fn rewrite_preview_references(
    html: &str,
    state: SideState,
    plan: &MediaPlan,
    resolved_media: &mut BTreeSet<String>,
    missing_sounds: &mut BTreeSet<String>,
) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    loop {
        let src_position = rest.find("src=");
        let sound_position = rest.find("[sound:");
        let position = match (src_position, sound_position) {
            (Some(src), Some(sound)) => src.min(sound),
            (Some(src), None) => src,
            (None, Some(sound)) => sound,
            (None, None) => break,
        };

        if sound_position == Some(position) {
            let after = &rest[position + "[sound:".len()..];
            let Some(end) = after.find(']') else {
                // Незакрытая ссылка: остаток копируется как есть, гадать не о чем.
                break;
            };
            let reference = &after[..end];
            out.push_str(&rest[..position]);
            match plan.resolved_path(state, reference) {
                Some(path) => {
                    resolved_media.insert(path.to_string());
                    let _ = write!(
                        out,
                        "<span class=\"replay-button\"><audio class=\"report-audio\" controls \
                         preload=\"none\" src=\"../{path}\"></audio></span>"
                    );
                }
                None => {
                    if !media::is_remote(reference) {
                        missing_sounds.insert(media_index::normalize_media_name(reference));
                    }
                    let _ = write!(
                        out,
                        "<span class=\"replay-button report-audio-missing\" title=\"файл не \
                         найден в экспорте\">[sound:{}]</span>",
                        escape_attr(reference)
                    );
                }
            }
            rest = &after[end + 1..];
            continue;
        }

        let (head, tail) = rest.split_at(position + "src=".len());
        let Some(quote) = tail.chars().next() else {
            out.push_str(head);
            break;
        };
        if quote != '"' && quote != '\'' {
            out.push_str(head);
            out.push_str(tail.split_at(quote.len_utf8()).0);
            rest = &tail[quote.len_utf8()..];
            continue;
        }

        let Some(end) = tail[1..].find(quote) else {
            out.push_str(head);
            out.push_str(tail);
            return out;
        };

        let reference = &tail[1..1 + end];
        let mut replaced_element = false;
        match plan.preview_reference(state, reference) {
            media::PreviewReference::Copied(path) => {
                out.push_str(head);
                resolved_media.insert(path.to_string());
                let _ = write!(out, "{quote}../{path}{quote}");
            }
            media::PreviewReference::MissingLocal => {
                match missing_image_element(rest, position, reference, state) {
                    Some((start, tag_end, placeholder)) => {
                        // Элемент отдаётся целиком: он целиком же и заменяется,
                        // вместе с атрибутами, которые к отсутствующей картинке
                        // уже не относятся.
                        out.push_str(&rest[..start]);
                        out.push_str(&placeholder);
                        rest = &rest[tag_end + 1..];
                        replaced_element = true;
                    }
                    None => {
                        out.push_str(head);
                        out.push_str(&format!("{quote}{reference}{quote}"));
                    }
                }
            }
            media::PreviewReference::NotAPreviewFile => {
                out.push_str(head);
                out.push_str(&format!("{quote}{reference}{quote}"));
            }
        }
        if replaced_element {
            continue;
        }
        rest = &tail[1 + end + 1..];
    }

    out.push_str(rest);
    out
}

/// Готовит замену элемента `<img>`, если ссылка подтверждённо отсутствует.
///
/// Возвращает начало тега, индекс его `>` и сам плейсхолдер. Плейсхолдер ставится
/// только на месте картинки: у прочих элементов `src` означает не картинку, и
/// отчёт не подменяет их текстом.
fn missing_image_element(
    html: &str,
    position: usize,
    reference: &str,
    state: SideState,
) -> Option<(usize, usize, String)> {
    let (tag_start, tag_name) = enclosing_open_tag(html, position)?;
    if tag_name != "img" {
        return None;
    }
    let tag_end = closing_bracket(html, position)?;
    Some((
        tag_start,
        tag_end,
        missing_image_placeholder(reference, state),
    ))
}

/// Плейсхолдер отсутствующей картинки: говорит basename и то, что файла нет.
fn missing_image_placeholder(reference: &str, state: SideState) -> String {
    let name = media_index::normalize_media_name(reference);
    let title = format!(
        "файл {} отсутствует в состоянии «{}»: превью осталось без него",
        name,
        state.as_str()
    );
    format!(
        "<span class=\"report-media-missing\" title=\"{}\">missing: {}</span>",
        escape_attr(&title),
        escape_attr(&name)
    )
}

/// Открывающий тег, внутри атрибутов которого находится позиция `position`.
///
/// Возвращает индекс `<` и имя тега в нижнем регистре. Если между `<` и позицией
/// уже встретился `>`, то `src=` стоит не в атрибутах тега, и трогать нечего.
fn enclosing_open_tag(html: &str, position: usize) -> Option<(usize, String)> {
    let start = html[..position].rfind('<')?;
    let head = &html[start + 1..position];
    if head.contains('>') {
        return None;
    }
    let name: String = head
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric())
        .collect();
    Some((start, name.to_ascii_lowercase()))
}

/// Индекс `>`, закрывающего тег, начиная с `from`, с учётом кавычек в атрибутах.
fn closing_bracket(html: &str, from: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (offset, character) in html[from..].char_indices() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None if character == '"' || character == '\'' => quote = Some(character),
            None if character == '>' => return Some(from + offset),
            None => {}
        }
    }
    None
}

/// Экранирует значение атрибута.
fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Добавляет диагностику по работе с media.
///
/// Диагностика называется вместе с состоянием: «файла нет» — это утверждение о
/// конкретном экспорте, и для изменённой заметки оно должно быть отнесено к «до»
/// или к «после», иначе читатель сделает неверный вывод о причине правки.
fn push_media_diagnostics(plan: &MediaPlan, diagnostics: &mut Vec<Diagnostic>) {
    for (state, state_media) in &plan.states {
        for name in &state_media.missing {
            diagnostics.push(Diagnostic::warning(
                "missing_media",
                format!(
                    "файл media {name:?} не найден в состоянии «{}»: превью останется без него",
                    state.as_str()
                ),
                Some(format!("{}:{name}", state.as_str())),
            ));
        }
        for reference in &state_media.traversal {
            diagnostics.push(Diagnostic::warning(
                "media_path_traversal",
                format!(
                    "ссылка {reference:?} содержит путь: в отчёт попадает только базовое имя файла"
                ),
                Some(reference.clone()),
            ));
        }
        for reference in &state_media.remote {
            diagnostics.push(Diagnostic::info(
                "remote_media_reference",
                format!("ссылка {reference:?} ведёт вне экспорта и не копируется в отчёт"),
                Some(reference.clone()),
            ));
        }
        for name in &state_media.symlinks {
            diagnostics.push(Diagnostic::warning(
                "media_symlink_skipped",
                format!(
                    "файл media {name:?} — символическая ссылка: он не читается и не копируется"
                ),
                Some(format!("{}:{name}", state.as_str())),
            ));
        }
        for name in &state_media.oversized {
            diagnostics.push(Diagnostic::warning(
                "media_too_large",
                format!("файл media {name:?} превысил предел размера и не скопирован"),
                Some(format!("{}:{name}", state.as_str())),
            ));
        }
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
        // Созданная заметка показывается одним состоянием, изменённая — двумя:
        // «до» и «после» строятся из разных экспортов независимо друг от друга.
        assert_eq!(
            result.card_files_total, 3,
            "превью для created и changed: одно состояние у created, два у changed"
        );
        let states_of = |guid: &str| -> Vec<&str> {
            result
                .preview_files
                .iter()
                .filter(|fact| fact.guid == guid)
                .map(|fact| fact.state.as_str())
                .collect()
        };
        assert_eq!(
            states_of("guid-3"),
            vec!["after"],
            "созданная: только «после»"
        );
        assert_eq!(
            states_of("guid-1"),
            vec!["before", "after"],
            "изменённая: обе стороны, «до» первым"
        );

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

    fn state_references(state: SideState, names: &[&str]) -> BTreeMap<SideState, BTreeSet<String>> {
        let mut map: BTreeMap<SideState, BTreeSet<String>> = BTreeMap::new();
        map.insert(
            state,
            names.iter().map(|name| (*name).to_string()).collect(),
        );
        map
    }

    #[test]
    fn media_refs_are_rewritten_only_for_copied_files() {
        let dir = TempDir::new("visual-report-rewrite");
        let source = dir.path().join("source");
        fs::create_dir_all(source.join("media")).expect("media");
        fs::write(source.join("media/есть.png"), b"x").expect("файл");

        let references = state_references(SideState::After, &["есть.png", "нет.png"]);
        let plan = media::plan(
            &[(SideState::After, source)],
            &references,
            &dir.path().join("out"),
        )
        .expect("план media");

        let html = "<img src=\"есть.png\"><img src=\"нет.png\"><img src=\"https://x/у.png\">";
        let mut resolved: BTreeSet<String> = BTreeSet::new();
        let mut missing: BTreeSet<String> = BTreeSet::new();
        let rewritten =
            rewrite_preview_references(html, SideState::After, &plan, &mut resolved, &mut missing);

        assert!(rewritten.contains("src=\"../media/after/есть.png\""));
        // Отсутствующий локальный файл не остаётся живой ссылкой: браузер показал бы
        // на её месте значок битой картинки и 404 в консоли, то есть превью молча
        // соврало бы о содержимом карточки.
        assert!(
            !rewritten.contains("src=\"нет.png\""),
            "битая ссылка не остаётся в превью: {rewritten}"
        );
        assert!(rewritten.contains("class=\"report-media-missing\""));
        assert!(rewritten.contains("missing: нет.png"));
        // Внешняя ссылка локальным файлом отчёта не является: она не скачивается и
        // не выдаётся за «файла нет в экспорте».
        assert!(rewritten.contains("src=\"https://x/у.png\""));
        assert!(!rewritten.contains("missing: у.png"));
        assert_eq!(resolved, ["media/after/есть.png".to_string()].into());
        assert!(
            missing.is_empty(),
            "картинка — не звук: в missing_sounds не попадает"
        );
    }

    /// Ссылка, которая не является локальным файлом отчёта, не подменяется чипом
    /// «файла нет»: у неё своя причина, и она названа диагностикой.
    #[test]
    fn a_reference_that_is_not_a_local_file_is_not_shown_as_missing() {
        let dir = TempDir::new("visual-report-not-local");
        let source = dir.path().join("source");
        fs::create_dir_all(source.join("media")).expect("media");
        fs::write(source.join("media/есть.png"), b"x").expect("файл");
        std::os::unix::fs::symlink(
            source.join("media/есть.png"),
            source.join("media/ссылка.png"),
        )
        .expect("символическая ссылка");

        let names = [
            "https://x/вне.png",
            "../уход.png",
            "ссылка.png",
            "нет.png",
            "есть.png",
        ];
        let references = state_references(SideState::Before, &names);
        let mut plan = media::plan(
            &[(SideState::Before, source.clone())],
            &references,
            &dir.path().join("out"),
        )
        .expect("план media");
        // Порядок как в прогоне отчёта: символические ссылки известны плану до
        // того, как по нему собирается HTML превью.
        media::note_symlinks(&mut plan, &[(SideState::Before, source)], &references);

        let html = names
            .iter()
            .map(|name| format!("<img src=\"{name}\">"))
            .collect::<String>();
        let mut resolved: BTreeSet<String> = BTreeSet::new();
        let mut missing: BTreeSet<String> = BTreeSet::new();
        let rewritten = rewrite_preview_references(
            &html,
            SideState::Before,
            &plan,
            &mut resolved,
            &mut missing,
        );

        assert_eq!(
            rewritten.matches("class=\"report-media-missing\"").count(),
            1,
            "чип получает только настоящий локальный пропуск: {rewritten}"
        );
        assert!(rewritten.contains("missing: нет.png"));
        assert!(rewritten.contains("src=\"https://x/вне.png\""));
        assert!(rewritten.contains("src=\"../уход.png\""));
        assert!(rewritten.contains("src=\"ссылка.png\""));
        assert!(rewritten.contains("src=\"../media/before/есть.png\""));
        // Ссылка с путём осталась диагностикой, а не источником для чтения.
        assert_eq!(plan.traversal_union(), vec!["../уход.png".to_string()]);
        assert_eq!(plan.symlinks_union(), vec!["ссылка.png".to_string()]);
    }

    /// Чип — утверждение о конкретном состоянии, а не о заметке целиком.
    #[test]
    fn a_missing_local_image_is_marked_on_its_own_side_only() {
        let dir = TempDir::new("visual-report-placeholder-state");
        let before = dir.path().join("before");
        let after = dir.path().join("after");
        fs::create_dir_all(before.join("media")).expect("media before");
        fs::create_dir_all(after.join("media")).expect("media after");
        fs::write(after.join("media/竹.gif"), b"after").expect("файл after");

        let mut references: BTreeMap<SideState, BTreeSet<String>> = BTreeMap::new();
        for state in SideState::ALL {
            references.insert(state, ["竹.gif".to_string()].into_iter().collect());
        }
        let plan = media::plan(
            &[(SideState::Before, before), (SideState::After, after)],
            &references,
            &dir.path().join("out"),
        )
        .expect("план media");

        let mut resolved: BTreeSet<String> = BTreeSet::new();
        let mut missing: BTreeSet<String> = BTreeSet::new();
        let before_html = rewrite_preview_references(
            "<img src=\"竹.gif\">",
            SideState::Before,
            &plan,
            &mut resolved,
            &mut missing,
        );
        let after_html = rewrite_preview_references(
            "<img src=\"竹.gif\">",
            SideState::After,
            &plan,
            &mut resolved,
            &mut missing,
        );

        assert!(before_html.contains("class=\"report-media-missing\""));
        assert!(before_html.contains("missing: 竹.gif"));
        assert!(!before_html.contains("src="), "ссылки в чипе нет");
        assert!(before_html.contains("before"), "состояние названо в чипе");
        assert!(after_html.contains("src=\"../media/after/竹.gif\""));
        assert!(!after_html.contains("report-media-missing"));
        assert_eq!(resolved, ["media/after/竹.gif".to_string()].into());
    }

    /// Чип встаёт только вместо картинки: ссылка на media у другого элемента
    /// остаётся его собственной ссылкой.
    #[test]
    fn only_an_image_element_is_replaced_by_the_missing_placeholder() {
        let dir = TempDir::new("visual-report-placeholder-tag");
        let source = dir.path().join("source");
        fs::create_dir_all(source.join("media")).expect("media");

        let plan = media::plan(
            &[(SideState::After, source)],
            &state_references(SideState::After, &["нет.png"]),
            &dir.path().join("out"),
        )
        .expect("план media");

        let html = "<audio src=\"нет.png\"></audio><img src=\"нет.png\">";
        let mut resolved: BTreeSet<String> = BTreeSet::new();
        let mut missing: BTreeSet<String> = BTreeSet::new();
        let rewritten =
            rewrite_preview_references(html, SideState::After, &plan, &mut resolved, &mut missing);

        assert!(rewritten.contains("<audio src=\"нет.png\"></audio>"));
        assert!(rewritten.contains("class=\"report-media-missing\""));
    }

    #[test]
    fn sound_becomes_an_offline_player_and_a_missing_sound_stays_marked() {
        let dir = TempDir::new("visual-report-sound");
        let source = dir.path().join("source");
        fs::create_dir_all(source.join("media")).expect("media");
        fs::write(source.join("media/есть.mp3"), b"x").expect("файл");

        let references = state_references(SideState::Before, &["есть.mp3", "нет.mp3"]);
        let plan = media::plan(
            &[(SideState::Before, source)],
            &references,
            &dir.path().join("out"),
        )
        .expect("план media");

        let html = "слово [sound:есть.mp3] и [sound:нет.mp3]";
        let mut resolved: BTreeSet<String> = BTreeSet::new();
        let mut missing: BTreeSet<String> = BTreeSet::new();
        let rewritten =
            rewrite_preview_references(html, SideState::Before, &plan, &mut resolved, &mut missing);

        assert!(rewritten.contains("class=\"replay-button\""));
        assert!(rewritten.contains("class=\"report-audio\""));
        assert!(rewritten.contains("src=\"../media/before/есть.mp3\""));
        // Отсутствующий звук остаётся видимым и помеченным: подставлять чужой
        // файл отчёт не имеет права.
        assert!(rewritten.contains("report-audio-missing"));
        assert!(rewritten.contains("[sound:нет.mp3]"));
        assert!(!rewritten.contains("media/before/нет.mp3"));
        assert_eq!(
            resolved,
            ["media/before/есть.mp3".to_string()].into_iter().collect()
        );
        assert_eq!(missing, ["нет.mp3".to_string()].into_iter().collect());
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
