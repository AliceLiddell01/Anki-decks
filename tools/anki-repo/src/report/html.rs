//! Сборка статического HTML визуального отчёта.
//!
//! Отчёт состоит из двух видов страниц:
//!
//! - `index.html` — сводка, диагностика и карточки отчёта;
//! - `cards/*.html` — по одному документу на превью карточки.
//!
//! Превью вынесено в отдельный документ и показывается через `<iframe>`, а не
//! вставлено в страницу отчёта. Причина содержательная: карточку оформляет CSS
//! модели, и этот CSS описывает `.card`, `body` и произвольные селекторы. Если
//! подставить его в общую страницу, стиль одной модели начнёт менять вид
//! карточек другой модели, а Chrome и CSS отчёта начнут влиять друг на друга.
//! Отдельный документ даёт ровно то, что нужно ревью: карточка выглядит так, как
//! её оформит модель, и ничего вокруг не портит.
//!
//! Никакого JavaScript в отчёте нет: раскрытие — это `<details>`, а ссылки на
//! карточки работают как обычные ссылки на файл. Никаких внешних ресурсов —
//! ни CDN, ни шрифтов, ни картинок по сети: отчёт обязан открываться офлайн, из
//! каталога на диске.

use std::fmt::Write as _;

use crate::report::style::{CARD_BASE_CSS, REPORT_CSS};

/// Высота превью карточки в отчёте.
pub const PREVIEW_HEIGHT_PX: u32 = 420;

/// Экранирует текст для вставки в HTML-текст.
#[must_use]
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Счётчики отчёта.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportCounts {
    /// Заметки, которых не было в `before`.
    pub created: usize,
    /// Заметки, у которых изменились поля или теги.
    pub changed: usize,
    /// Заметки, у которых появился тег вывода из обращения.
    pub retired: usize,
    /// Заметки, которые были в `before` и исчезли в `after`.
    pub removed: usize,
    /// Заметки без изменений.
    pub unchanged: usize,
    /// `guid`, встретившиеся более одного раза.
    pub ambiguous: usize,
    /// Сколько превью карточек собрано.
    pub previews: usize,
    /// Заметок в `before`.
    pub notes_before: usize,
    /// Заметок в `after`.
    pub notes_after: usize,
}

/// Один фрагмент diff значения поля.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMark {
    /// Фрагмент есть в обеих версиях.
    Equal,
    /// Фрагмент добавлен.
    Inserted,
    /// Фрагмент удалён.
    Deleted,
}

/// Diff одного поля.
#[derive(Debug, Clone)]
pub struct FieldDiff {
    /// Имя поля.
    pub field: String,
    /// Фрагменты значения и их вид.
    pub tokens: Vec<(DiffMark, String)>,
}

/// Ссылка на превью карточки.
#[derive(Debug, Clone)]
pub struct ReportPreview {
    /// Подпись превью: имя шаблона.
    pub label: String,
    /// Относительный путь к документу превью.
    pub file: String,
    /// Подсказка о том, что именно показывает превью.
    pub hint: String,
}

/// Карточка отчёта.
#[derive(Debug, Clone)]
pub struct ReportCard {
    /// Заголовок: колода и краткая подпись заметки.
    pub heading: String,
    /// Путь колоды.
    pub deck_path: String,
    /// `guid` заметки.
    pub guid: String,
    /// Имя модели заметки.
    pub model_name: String,
    /// Теги до изменения.
    pub tags_before: Vec<String>,
    /// Теги после изменения.
    pub tags_after: Vec<String>,
    /// Значения полей (для заметок, у которых превью не строится).
    pub fields: Vec<(String, String)>,
    /// Diff значений полей.
    pub diffs: Vec<FieldDiff>,
    /// Собранные превью.
    pub previews: Vec<ReportPreview>,
    /// Пояснения по конкретной заметке.
    pub notes: Vec<String>,
}

/// Раздел отчёта.
#[derive(Debug, Clone)]
pub struct ReportSection {
    /// Заголовок раздела.
    pub title: String,
    /// Пояснение к разделу.
    pub hint: String,
    /// Карточки раздела.
    pub cards: Vec<ReportCard>,
    /// Пояснение об усечении списка.
    pub truncated: Option<String>,
}

/// Диагностика отчёта.
#[derive(Debug, Clone)]
pub struct ReportDiagnosticView {
    /// Стабильный код.
    pub code: String,
    /// `warning` или `info`.
    pub severity: &'static str,
    /// Объяснение.
    pub message: String,
    /// К чему относится диагностика.
    pub subject: Option<String>,
}

/// Полный документ отчёта.
#[derive(Debug, Clone)]
pub struct ReportDocument {
    /// Заголовок страницы.
    pub title: String,
    /// Подпись состояния «до».
    pub before_label: String,
    /// Подпись состояния «после».
    pub after_label: String,
    /// Тег вывода из обращения, если он был задан.
    pub retire_tag: Option<String>,
    /// Счётчики.
    pub counts: ReportCounts,
    /// Разделы с карточками.
    pub sections: Vec<ReportSection>,
    /// Диагностика.
    pub diagnostics: Vec<ReportDiagnosticView>,
    /// Ограничения этого отчёта.
    pub limitations: Vec<String>,
    /// Неподдержанные конструкции шаблонов, встреченные в изменениях.
    pub unsupported_constructs: Vec<(String, String)>,
}

/// Один бок карточки: лицевая или обратная сторона.
#[derive(Debug, Clone)]
pub struct CardSide {
    /// Подпись.
    pub label: String,
    /// Классы обёртки: Anki различает карточки по `card{N}`.
    pub classes: String,
    /// HTML, отрендеренный шаблоном.
    pub html: String,
    /// Проблемы рендера этого бока.
    pub issues: Vec<String>,
}

/// Документ превью одной карточки.
#[derive(Debug, Clone)]
pub struct CardFile {
    /// Заголовок документа.
    pub title: String,
    /// CSS модели.
    pub model_css: String,
    /// Стороны карточки.
    pub sides: Vec<CardSide>,
}

/// Собирает `cards/*.html`.
#[must_use]
pub fn card_html(card: &CardFile) -> String {
    let mut out = String::with_capacity(card.model_css.len() + 1024);
    out.push_str("<!doctype html>\n<html lang=\"ru\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(out, "<title>{}</title>", escape_html(&card.title));
    out.push_str("<style>\n");
    out.push_str(CARD_BASE_CSS);
    out.push_str("</style>\n<style>\n");
    // CSS модели идёт после базовых правил: в Anki модель перекрывает базовые
    // стили, и превью обязано вести себя так же.
    out.push_str(&card.model_css);
    out.push_str("\n</style>\n</head>\n<body>\n");

    for side in &card.sides {
        let _ = writeln!(
            out,
            "<section class=\"report-side\"><h2 class=\"report-side-label\">{}</h2>",
            escape_html(&side.label)
        );
        let _ = writeln!(
            out,
            "<div class=\"{}\">{}</div>",
            escape_html(&side.classes),
            side.html
        );
        if !side.issues.is_empty() {
            out.push_str("<ul class=\"report-side-issues\">\n");
            for issue in &side.issues {
                let _ = writeln!(out, "<li>{}</li>", escape_html(issue));
            }
            out.push_str("</ul>\n");
        }
        out.push_str("</section>\n");
    }

    out.push_str(
        "<style>.report-side { border-bottom: 1px dashed #d7dbe0; }\n\
         .report-side-label { font: 12px/1.4 sans-serif; color: #5c6470; margin: .4em 0 0 .6em; }\n\
         .report-side-issues { font: 12px/1.4 sans-serif; color: #a32323; margin: .3em .6em; }\n\
         .report-side:last-of-type { border-bottom: 0; }</style>\n",
    );
    out.push_str("</body>\n</html>\n");
    out
}

/// Собирает `index.html`.
#[must_use]
pub fn index_html(document: &ReportDocument) -> String {
    let mut out = String::with_capacity(16 * 1024);
    out.push_str("<!doctype html>\n<html lang=\"ru\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(out, "<title>{}</title>", escape_html(&document.title));
    out.push_str("<style>\n");
    out.push_str(REPORT_CSS);
    out.push_str("</style>\n</head>\n<body class=\"report\">\n");

    out.push_str("<header class=\"report-header\">\n");
    let _ = writeln!(
        out,
        "<h1 class=\"report-title\">{}</h1>",
        escape_html(&document.title)
    );
    let _ = writeln!(
        out,
        "<p class=\"report-subtitle\">до: <code>{}</code> → после: <code>{}</code></p>",
        escape_html(&document.before_label),
        escape_html(&document.after_label)
    );
    if let Some(tag) = &document.retire_tag {
        let _ = writeln!(
            out,
            "<p class=\"report-subtitle\">тег вывода из обращения: <code>{}</code></p>",
            escape_html(tag)
        );
    }
    out.push_str("<ul class=\"report-counts\">\n");
    for (class, label, value) in [
        ("report-count-created", "создано", document.counts.created),
        ("report-count-changed", "изменено", document.counts.changed),
        (
            "report-count-retired",
            "выведено из обращения",
            document.counts.retired,
        ),
        (
            "report-count-removed",
            "исчезло из экспорта",
            document.counts.removed,
        ),
        ("", "без изменений", document.counts.unchanged),
        ("", "неоднозначных guid", document.counts.ambiguous),
        ("", "превью карточек", document.counts.previews),
    ] {
        let _ = writeln!(
            out,
            "<li class=\"report-count {class}\">{label}: {value}</li>"
        );
    }
    let _ = writeln!(
        out,
        "<li class=\"report-count\">заметок: {} → {}</li>",
        document.counts.notes_before, document.counts.notes_after
    );
    out.push_str("</ul>\n</header>\n");
    out.push_str("<main class=\"report-main\">\n");

    out.push_str("<section class=\"report-section\">\n<h2>Ограничения этого отчёта</h2>\n");
    out.push_str("<ul class=\"report-limits\">\n");
    for limitation in &document.limitations {
        let _ = writeln!(out, "<li>{}</li>", escape_html(limitation));
    }
    out.push_str("</ul>\n</section>\n");

    if !document.unsupported_constructs.is_empty() {
        out.push_str(
            "<section class=\"report-section\">\n<h2>Неподдержанные конструкции шаблонов</h2>\n\
             <p class=\"report-hint\">Эти конструкции встретились в шаблонах затронутых моделей. \
             Превью помечено как неподдержанное там, где оно могло бы выглядеть иначе, чем в Anki.</p>\n",
        );
        out.push_str(
            "<table class=\"report-fields\">\n<tr><th>конструкция</th><th>причина</th></tr>\n",
        );
        for (construct, reason) in &document.unsupported_constructs {
            let _ = writeln!(
                out,
                "<tr><td><code>{}</code></td><td>{}</td></tr>",
                escape_html(construct),
                escape_html(reason)
            );
        }
        out.push_str("</table>\n</section>\n");
    }

    if !document.diagnostics.is_empty() {
        out.push_str("<section class=\"report-section\">\n<h2>Диагностика</h2>\n");
        out.push_str(
            "<p class=\"report-hint\">Диагностика не отменяет отчёт: она называет то, \
             что инструмент не стал угадывать.</p>\n<div class=\"report-diagnostics\">\n",
        );
        for diagnostic in &document.diagnostics {
            out.push_str("<div class=\"report-diagnostic\">");
            let _ = write!(
                out,
                "<span class=\"report-severity-{}\"><code>{}</code></span>",
                escape_html(diagnostic.severity),
                escape_html(&diagnostic.code)
            );
            let _ = write!(out, " {}", escape_html(&diagnostic.message));
            if let Some(subject) = &diagnostic.subject {
                let _ = write!(out, " <code>{}</code>", escape_html(subject));
            }
            out.push_str("</div>\n");
        }
        out.push_str("</div>\n</section>\n");
    }

    for section in &document.sections {
        let _ = writeln!(out, "<section class=\"report-section\">");
        let _ = writeln!(out, "<h2>{}</h2>", escape_html(&section.title));
        let _ = writeln!(
            out,
            "<p class=\"report-hint\">{}</p>",
            escape_html(&section.hint)
        );
        if let Some(truncated) = &section.truncated {
            let _ = writeln!(
                out,
                "<p class=\"report-hint\">{}</p>",
                escape_html(truncated)
            );
        }
        for card in &section.cards {
            render_card(&mut out, card);
        }
        out.push_str("</section>\n");
    }

    out.push_str("</main>\n</body>\n</html>\n");
    out
}

fn render_card(out: &mut String, card: &ReportCard) {
    out.push_str("<article class=\"report-card\">\n");
    let _ = writeln!(out, "<h3>{}</h3>", escape_html(&card.heading));
    out.push_str("<p class=\"report-meta\">");
    let _ = write!(out, "колода <code>{}</code>", escape_html(&card.deck_path));
    let _ = write!(
        out,
        "<span>модель <code>{}</code></span>",
        escape_html(&card.model_name)
    );
    let _ = write!(
        out,
        "<span>guid <code>{}</code></span>",
        escape_html(&card.guid)
    );
    out.push_str("</p>\n");

    if card.tags_before != card.tags_after {
        out.push_str("<p class=\"report-tags\">теги:");
        for tag in &card.tags_after {
            let class = if card.tags_before.contains(tag) {
                "report-tag"
            } else {
                "report-tag report-tag-added"
            };
            let _ = write!(out, " <span class=\"{class}\">{}</span>", escape_html(tag));
        }
        for tag in &card.tags_before {
            if !card.tags_after.contains(tag) {
                let _ = write!(
                    out,
                    " <span class=\"report-tag report-tag-removed\">{}</span>",
                    escape_html(tag)
                );
            }
        }
        out.push_str("</p>\n");
    }

    if !card.diffs.is_empty() {
        out.push_str(
            "<table class=\"report-diff\">\n<tr><th>поле</th><th>было</th><th>стало</th></tr>\n",
        );
        for diff in &card.diffs {
            let _ = writeln!(out, "<tr><td>{}</td>", escape_html(&diff.field));
            let _ = writeln!(out, "<td>{}</td>", tokens_html(&diff.tokens, true));
            let _ = writeln!(out, "<td>{}</td>", tokens_html(&diff.tokens, false));
            out.push_str("</tr>\n");
        }
        out.push_str("</table>\n");
    }

    if !card.fields.is_empty() {
        out.push_str("<table class=\"report-fields\">\n");
        for (name, value) in &card.fields {
            let _ = writeln!(
                out,
                "<tr><th>{}</th><td>{}</td></tr>",
                escape_html(name),
                escape_html(value)
            );
        }
        out.push_str("</table>\n");
    }

    for preview in &card.previews {
        out.push_str("<div class=\"report-preview-host\">");
        let _ = writeln!(
            out,
            "<p class=\"report-preview-label\">превью: {} — {} \
             <a class=\"report-preview-link\" href=\"{}\">открыть отдельно</a></p>",
            escape_html(&preview.label),
            escape_html(&preview.hint),
            escape_html(&preview.file)
        );
        let _ = writeln!(
            out,
            "<iframe class=\"report-preview\" src=\"{}\" loading=\"lazy\" \
             title=\"{}\" height=\"{PREVIEW_HEIGHT_PX}\"></iframe>",
            escape_html(&preview.file),
            escape_html(&preview.label)
        );
        out.push_str("</div>\n");
    }

    if !card.notes.is_empty() {
        out.push_str("<ul class=\"report-issues\">\n");
        for note in &card.notes {
            let _ = writeln!(out, "<li>{}</li>", escape_html(note));
        }
        out.push_str("</ul>\n");
    }

    out.push_str("</article>\n");
}

/// Рендерит фрагменты diff для одной колонки: «было» показывает общие и
/// удалённые фрагменты, «стало» — общие и добавленные.
///
/// Колонка обязана читаться как фактическое значение поля, поэтому чужие для неё
/// фрагменты не просто помечаются, а не выводятся вовсе: удалённый фрагмент в
/// колонке «стало» означал бы, что текст, которого в новом значении нет, всё ещё
/// там есть, — то есть противоположное тому, что изменилось.
fn tokens_html(tokens: &[(DiffMark, String)], before_side: bool) -> String {
    let mut out = String::new();
    for (mark, text) in tokens {
        let class = match (mark, before_side) {
            (DiffMark::Deleted, false) | (DiffMark::Inserted, true) => continue,
            (DiffMark::Equal, _) => "",
            (DiffMark::Deleted, true) => " class=\"report-token-del\"",
            (DiffMark::Inserted, false) => " class=\"report-token-ins\"",
        };
        let _ = write!(out, "<span{class}>{}</span>", escape_html(text));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_covers_html_metacharacters() {
        assert_eq!(
            escape_html("<a href=\"x\">&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn index_html_is_offline_and_has_no_scripts_or_timestamps() {
        let document = ReportDocument {
            title: "Отчёт".to_string(),
            before_label: "before".to_string(),
            after_label: "after".to_string(),
            retire_tag: Some("retired::auto".to_string()),
            counts: ReportCounts {
                created: 1,
                ..ReportCounts::default()
            },
            sections: vec![ReportSection {
                title: "Созданные заметки".to_string(),
                hint: "нет в before".to_string(),
                cards: vec![ReportCard {
                    heading: "Слово — тест".to_string(),
                    deck_path: "Words::N1".to_string(),
                    guid: "abcdefghij".to_string(),
                    model_name: "Слова".to_string(),
                    tags_before: Vec::new(),
                    tags_after: vec!["тэг".to_string()],
                    fields: Vec::new(),
                    diffs: Vec::new(),
                    previews: vec![ReportPreview {
                        label: "Карточка 1".to_string(),
                        file: "cards/card-0001.html".to_string(),
                        hint: "шаблон модели".to_string(),
                    }],
                    notes: Vec::new(),
                }],
                truncated: None,
            }],
            diagnostics: Vec::new(),
            limitations: vec!["ночной режим не воспроизводится".to_string()],
            unsupported_constructs: Vec::new(),
        };

        let html = index_html(&document);
        assert!(html.starts_with("<!doctype html>"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("http://"));
        assert!(!html.contains("https://"));
        assert!(!html.contains("202"));
        assert!(html.contains("cards/card-0001.html"));
        assert!(html.contains("retired::auto"));
    }

    #[test]
    fn card_html_places_model_css_after_base_css() {
        let card = CardFile {
            title: "Карточка".to_string(),
            model_css: ".card { color: red; }".to_string(),
            sides: vec![CardSide {
                label: "Лицевая сторона".to_string(),
                classes: "card card1".to_string(),
                html: "<div>слово</div>".to_string(),
                issues: Vec::new(),
            }],
        };

        let html = card_html(&card);
        let base = html.find("font-family: arial").expect("базовые стили");
        let model = html.find(".card { color: red; }").expect("CSS модели");
        assert!(base < model, "CSS модели обязан идти после базовых правил");
        assert!(html.contains("<div class=\"card card1\"><div>слово</div></div>"));
    }

    #[test]
    fn diff_tokens_render_on_the_expected_side() {
        let tokens = vec![
            (DiffMark::Equal, "a".to_string()),
            (DiffMark::Deleted, "b".to_string()),
            (DiffMark::Inserted, "c".to_string()),
        ];
        let before = tokens_html(&tokens, true);
        let after = tokens_html(&tokens, false);
        assert!(before.contains("report-token-del"));
        assert!(!before.contains("report-token-ins"));
        assert!(after.contains("report-token-ins"));
        assert!(!after.contains("report-token-del"));
    }

    /// Колонки diff обязаны показывать свои значения, а не смесь обеих сторон:
    /// удалённый токен не может появиться в колонке «стало», а добавленный — в
    /// колонке «было». Маркировка классом этого не заменяет: текст колонки — это
    /// и есть значение поля, и читатель отчёта сверяется именно с ним.
    #[test]
    fn diff_columns_reconstruct_their_own_side() {
        let tokens = vec![
            (DiffMark::Equal, "a".to_string()),
            (DiffMark::Deleted, "b".to_string()),
            (DiffMark::Inserted, "c".to_string()),
            (DiffMark::Equal, "d".to_string()),
        ];
        assert_eq!(plain_text(&tokens_html(&tokens, true)), "abd");
        assert_eq!(plain_text(&tokens_html(&tokens, false)), "acd");
    }

    /// Удаление — самый частый исход правки: колонка «стало» не должна
    /// воспроизводить удалённый фрагмент.
    #[test]
    fn deleted_only_token_is_absent_from_the_after_column() {
        let tokens = vec![
            (DiffMark::Equal, "осталось".to_string()),
            (DiffMark::Deleted, "удалено".to_string()),
        ];
        assert_eq!(plain_text(&tokens_html(&tokens, true)), "осталосьудалено");
        assert_eq!(plain_text(&tokens_html(&tokens, false)), "осталось");
    }

    /// Склеивает текст колонки так, как его увидит читатель: разметка фрагментов
    /// не должна добавлять или убирать содержимое.
    fn plain_text(html: &str) -> String {
        let mut out = String::new();
        let mut rest = html;
        while let Some(start) = rest.find('<') {
            out.push_str(&rest[..start]);
            let end = rest[start..].find('>').expect("закрытая скобка тега");
            rest = &rest[start + end + 1..];
        }
        out.push_str(rest);
        out
    }
}
