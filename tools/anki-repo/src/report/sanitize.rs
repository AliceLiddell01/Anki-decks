//! Граница доверия отчёта: превращение недоверенного HTML в безопасный.
//!
//! Отчёт открывается в браузере с диска, поэтому всё, что он показывает, — это
//! документ, который исполняется. Значения полей и шаблоны Anki недоверенны: они
//! приходят из экспорта, а экспорт — это данные репозитория, а не код отчёта.
//! Задача модуля — сделать так, чтобы HTML модели нельзя было исполнить или
//! заставить сходить в сеть, и при этом не отнять у превью то, ради чего оно
//! собрано: вид карточки, локальные media и звук.
//!
//! Что здесь является границей:
//!
//! - **элемент**, который исполняет или запрашивает: `script`, `iframe`, `object`,
//!   `embed`, `applet`, `frame`, `frameset`, `base`, `link`, `meta`, `form` — тег
//!   удаляется, содержимое (если это осмысленный фрагмент) остаётся текстом;
//! - **атрибут-обработчик** (`on*`) — удаляется целиком;
//! - **адрес**: значение URL-атрибута обязано быть локальным
//!   ([`Sanitizer::allows_reference`]), иначе атрибут удаляется;
//! - **CSS**: `@import` вырезается, а `url()` с нелокальным адресом заменяется на
//!   инертную заглушку. Это же применяется к атрибуту `style` и к содержимому
//!   `<style>` — и к CSS модели, которую подставляет сборщик страницы карточки;
//! - **незакрытый тег**: `<` в его начале экранируется, потому что иначе
//!   недоверенный фрагмент склеивается с обёрткой отчёта, и границу между «данные
//!   кончились» и «начался код отчёта» перестаёт существовать.
//!
//! Runtime отчёта проходит здесь же: он не «доверенный по имени», а **опознанный
//! по содержимому**. Сборщик страницы помечает свои теги `data-report-runtime`, а
//! [`GeneratedHtml::inspect`] принимает `<script>` только тогда, когда его тело
//! совпадает с одной из констант отчёта ([`crate::report::runtime`]). Поэтому
//! проверка «в документе нет исполняемого чужого кода» — это не поиск подстроки, а
//! сравнение с единственным разрешённым телом, независимо от регистра, кавычек и
//! прочей записи.
//!
//! Проверка одна на все сгенерированные файлы ([`GeneratedHtml::inspect`]): и
//! `index.html`, и `cards/*.html` проходят её перед записью, а тесты — после.
//! Санитайз без независимой проверки — это обещание; проверка превращает его в
//! контракт.
//!
//! Отвергнутое не выбрасывается молча: [`Sanitized::blocked`] перечисляет, что
//! именно не показано, и это уходит в превью и в диагностику отчёта.

use std::ops::Range;

use crate::htmlscan::{self, Tag};
use crate::media;
use crate::report::runtime;

/// Чем заменяется нелокальный адрес в CSS.
///
/// Именно `none`, а не инертная ссылка: любая ссылка — это запрос, и проверка
/// [`inspect`] справедливо считает `url(…)` с нелокальным адресом нарушением.
/// `none` оставляет объявление синтаксически целым (`background: none no-repeat`)
/// и не просит ничего.
const INERT_CSS_VALUE: &str = "none";

/// Элементы, тег которых удаляется: они исполняют код или запрашивают ресурс.
///
/// Содержимое `<object>` и `<iframe>` бывает осмысленным запасным текстом, поэтому
/// удаляется тег, а не всё поддерево. У `<script>` содержимое — код, и оно
/// экранируется отдельно.
const DROPPED_ELEMENTS: &[&str] = &[
    "applet", "base", "embed", "form", "frame", "frameset", "iframe", "link", "meta", "object",
];

/// Что именно не показано в превью.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// Короткое имя того, что отвергнуто: элемент, атрибут или конструкция CSS.
    pub what: String,
    /// Человеческое объяснение для превью и диагностики.
    pub reason: String,
}

impl Blocked {
    fn new(what: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            reason: reason.into(),
        }
    }
}

/// Результат очистки фрагмента HTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sanitized {
    /// Очищенный HTML.
    pub html: String,
    /// Что было отвергнуто.
    pub blocked: Vec<Blocked>,
}

/// Результат очистки CSS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedCss {
    /// Очищенный CSS.
    pub css: String,
    /// Что было отвергнуто.
    pub blocked: Vec<Blocked>,
}

/// Нарушение границы доверия в готовом документе.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Что именно нарушено.
    pub what: String,
}

/// Страница отчёта: у каждой свой набор локальных адресов.
///
/// Набор объявлен здесь, а не у вызывающего: это часть границы доверия, и
/// страница не должна получать «свой» предикат в каждом месте вызова.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// `cards/*.html`: рядом с ним скопированные media.
    Card,
    /// `index.html`: рядом с ним документы превью.
    Index,
}

impl Page {
    /// Что эта страница имеет право запрашивать.
    ///
    /// Разрешены только относительные пути внутрь самого отчёта и переходы
    /// внутри документа. Всё остальное — включая `data:` и абсолютные пути —
    /// считается внешним: отчёт обязан оставаться офлайн и переносимым.
    #[must_use]
    pub fn allows(self, value: &str) -> bool {
        let value = value.trim();
        if value.starts_with('#') {
            return true;
        }
        if value.contains(':') {
            return false;
        }
        let prefix = match self {
            Self::Card => CARD_MEDIA_PREFIX,
            Self::Index => CARD_FILE_PREFIX,
        };
        value.starts_with(prefix) && !value.starts_with("//")
    }
}

/// Префикс скопированной media относительно документа превью.
pub const CARD_MEDIA_PREFIX: &str = "../media/";

/// Префикс документа превью относительно точки входа отчёта.
pub const CARD_FILE_PREFIX: &str = "cards/";

/// Очищает недоверенный фрагмент HTML.
#[must_use]
pub fn html(fragment: &str, page: Page) -> Sanitized {
    let mut sanitizer = Sanitizer {
        source: fragment,
        page,
        out: String::with_capacity(fragment.len()),
        blocked: Vec::new(),
        pos: 0,
    };
    sanitizer.run();
    Sanitized {
        html: sanitizer.out,
        blocked: sanitizer.blocked,
    }
}

/// Очищает CSS: `@import` вырезается, нелокальные `url()` становятся инертными.
///
/// Отдельная функция, потому что CSS приходит двумя путями: как содержимое
/// `<style>` и как CSS модели, который подставляет сборщик страницы. Один
/// владелец политики — одно поведение.
#[must_use]
pub fn css(style: &str, page: Page) -> SanitizedCss {
    let stripped = strip_imports(style);
    let mut blocked: Vec<Blocked> = Vec::new();
    if stripped.blocked {
        blocked.push(Blocked::new(
            "@import",
            "внешняя таблица стилей не подключается: отчёт обязан оставаться офлайн",
        ));
    }

    let mut out = String::with_capacity(stripped.css.len());
    let mut rest = stripped.css.as_str();
    while let Some(position) = find_ci(rest, "url(") {
        out.push_str(&rest[..position]);
        let after = &rest[position + 4..];
        let Some(end) = after.find(')') else {
            // Не закрытая скобка: остаток не является ссылкой.
            out.push_str(&rest[position..]);
            rest = "";
            break;
        };
        let target = after[..end].trim().trim_matches(|c| c == '"' || c == '\'');
        if target.is_empty() || page.allows(target) {
            out.push_str(&rest[position..position + 4 + end + 1]);
        } else {
            out.push_str(INERT_CSS_VALUE);
            blocked.push(Blocked::new(
                format!("url({target})"),
                "адрес CSS не ведёт к скопированному рядом файлу: запрос не выполняется",
            ));
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);

    SanitizedCss {
        css: escape_css_close(&out),
        blocked,
    }
}

/// Инспектирует готовый документ отчёта на нарушение границы доверия.
///
/// Одна проверка на все сгенерированные файлы: она не знает, кто именно собрал
/// документ, и ищет только конструкции, которые исполнили бы чужой код или пошли
/// бы по адресу. Разрешено ровно одно исключение — runtime отчёта, опознанный по
/// телу, а не по имени тега.
#[must_use]
pub fn inspect(document: &str, page: Page) -> Vec<Violation> {
    let mut violations: Vec<Violation> = Vec::new();

    for tag in htmlscan::scan_tags(document) {
        match tag {
            Tag::Element(element) => {
                let name = element.name.to_ascii_lowercase();
                let runtime_owned = element.attribute("data-report-runtime").is_some();

                if name == "script" && !runtime_owned {
                    violations.push(Violation {
                        what: "исполняемый <script> без пометки runtime отчёта".to_string(),
                    });
                }
                if DROPPED_ELEMENTS.contains(&name.as_str())
                    && !(name == "iframe" && element.attribute("data-report-state").is_some())
                    && !(name == "meta" && is_report_meta(&element))
                {
                    violations.push(Violation {
                        what: format!("элемент <{name}>"),
                    });
                }

                for attribute in &element.attributes {
                    let attribute_name = attribute.name.to_ascii_lowercase();
                    if attribute_name.starts_with("on") {
                        violations.push(Violation {
                            what: format!("обработчик {attribute_name} у <{name}>"),
                        });
                        continue;
                    }
                    if media::attribute_is_address(&name, &attribute_name)
                        && !attribute.value.is_empty()
                        && !page.allows(attribute.value)
                    {
                        violations.push(Violation {
                            what: format!("адрес {attribute_name}=\"{}\"", attribute.value),
                        });
                    }
                    if attribute_name == "style" {
                        violations.extend(css_violations(attribute.value, page, "style"));
                    }
                }
            }
            Tag::RawText {
                name,
                body,
                close_end,
                ..
            } => {
                let lower = name.to_ascii_lowercase();
                if lower == "script" {
                    let body_text = &document[body.clone()];
                    if !is_report_runtime(body_text) {
                        violations.push(Violation {
                            what: "тело <script> не совпадает с runtime отчёта".to_string(),
                        });
                    }
                }
                if lower == "style" {
                    violations.extend(css_violations(&document[body.clone()], page, "style"));
                }
                if close_end.is_none() {
                    violations.push(Violation {
                        what: format!("незакрытый <{lower}>"),
                    });
                }
            }
            Tag::Unterminated { name, .. } => violations.push(Violation {
                what: format!("незакрытый тег <{name}>"),
            }),
        }
    }

    violations
}

/// Проверяет CSS на нелокальные адреса и `@import`.
fn css_violations(style: &str, page: Page, where_: &str) -> Vec<Violation> {
    let mut violations: Vec<Violation> = Vec::new();
    if find_ci(style, "@import").is_some() {
        violations.push(Violation {
            what: format!("@import в {where_}"),
        });
    }

    let mut rest = style;
    while let Some(position) = find_ci(rest, "url(") {
        let after = &rest[position + 4..];
        let Some(end) = after.find(')') else {
            break;
        };
        let target = after[..end].trim().trim_matches(|c| c == '"' || c == '\'');
        if !target.is_empty() && !page.allows(target) {
            violations.push(Violation {
                what: format!("адрес url({target}) в {where_}"),
            });
        }
        rest = &after[end + 1..];
    }

    violations
}

/// Разрешены ли метаданные, которые ставит сам отчёт.
fn is_report_meta(element: &htmlscan::Element<'_>) -> bool {
    match element.attribute("charset") {
        Some(charset) => !charset.value.is_empty(),
        None => element
            .attribute("name")
            .is_some_and(|name| name.value.eq_ignore_ascii_case("viewport")),
    }
}

/// Тело runtime отчёта: сравнение точное и независимое от регистра тега.
fn is_report_runtime(body: &str) -> bool {
    let body = body.trim();
    body == runtime::INDEX_RUNTIME_JS.trim() || body == runtime::CARD_RUNTIME_JS.trim()
}

struct Sanitizer<'a> {
    source: &'a str,
    page: Page,
    out: String,
    blocked: Vec<Blocked>,
    pos: usize,
}

impl Sanitizer<'_> {
    fn run(&mut self) {
        let tags = htmlscan::scan_tags(self.source);
        for tag in tags {
            match tag {
                Tag::Element(element) => self.element(&element),
                Tag::RawText {
                    name,
                    start,
                    body,
                    close_end,
                    ..
                } => self.raw_text(name, start, body, close_end),
                Tag::Unterminated { start, .. } => self.escape_tag_start(start),
            }
        }
        self.flush(self.source.len());
    }

    /// Элемент с атрибутами: адреса проверяются, обработчики удаляются.
    fn element(&mut self, element: &htmlscan::Element<'_>) {
        let name = element.name.to_ascii_lowercase();
        self.flush(element.start);

        if DROPPED_ELEMENTS.contains(&name.as_str()) {
            self.blocked.push(Blocked::new(
                format!("<{name}>"),
                "элемент исполняет код или запрашивает ресурс: тег удалён, содержимое осталось текстом",
            ));
            self.pos = element.end;
            return;
        }

        // Пометки сборщика страницы ставит сам отчёт: они нужны runtime и не
        // являются данными.
        let report_owned = element.attribute("data-report-runtime").is_some()
            || element.attribute("data-report-state").is_some()
            || element.attribute("data-report-theme-value").is_some();

        let mut kept: Vec<usize> = Vec::new();
        let mut dropped: Vec<Blocked> = Vec::new();
        for (index, attribute) in element.attributes.iter().enumerate() {
            let attribute_name = attribute.name.to_ascii_lowercase();
            if attribute_name.starts_with("on") && !report_owned {
                dropped.push(Blocked::new(
                    attribute_name,
                    "обработчик события исполнил бы чужой код: атрибут удалён",
                ));
                continue;
            }
            if media::attribute_is_address(&name, &attribute_name)
                && !attribute.value.is_empty()
                && !self.page.allows(attribute.value)
            {
                dropped.push(Blocked::new(
                    format!("{attribute_name}=\"{}\"", attribute.value),
                    Self::address_reason(attribute.value),
                ));
                continue;
            }
            kept.push(index);
        }

        let sanitized_style = element
            .attribute("style")
            .map(|attribute| css(attribute.value, self.page));
        if let Some(style) = &sanitized_style {
            self.blocked.extend(style.blocked.iter().cloned());
        }

        self.out.push('<');
        self.out.push_str(element.name);
        // Порядок атрибутов и их запись сохраняются: перестановка или перезапись
        // сделали бы превью непохожим на то, что видит Anki. Единственное
        // исключение — `style`, значение которого берётся из очищенного CSS.
        for (index, attribute) in element.attributes.iter().enumerate() {
            if !kept.contains(&index) {
                continue;
            }
            let value = match sanitized_style.as_ref() {
                Some(style) if attribute.name_is("style") => style.css.clone(),
                _ => escape_attr(attribute.value),
            };
            self.out.push(' ');
            self.out.push_str(attribute.name);
            self.out.push_str("=\"");
            self.out.push_str(&value);
            self.out.push('"');
        }
        if element.self_closing {
            self.out.push_str(" /");
        }
        self.out.push('>');

        self.blocked.extend(dropped);
        // `>` уже записан выше: источник продолжается после него.
        self.pos = element.end + 1;
    }

    /// Объясняет, почему адрес не показан.
    ///
    /// «Ушло бы в сеть» и «не ведёт к скопированному файлу» — разные вещи, и вторая
    /// встречается чаще: пользователь должен видеть причину, а не общий запрет.
    fn address_reason(value: &str) -> String {
        let external = value.contains(':') || value.starts_with("//");
        if external {
            "внешний адрес не запрашивается: отчёт обязан оставаться офлайн, поэтому атрибут \
             удалён"
                .to_string()
        } else {
            "адрес не ведёт к файлу, скопированному рядом с отчётом: атрибут удалён, чтобы \
             превью не запрашивало то, чего в отчёте нет"
                .to_string()
        }
    }

    /// Элемент с содержимым, которое не является разметкой.
    fn raw_text(&mut self, name: &str, start: usize, body: Range<usize>, close_end: Option<usize>) {
        let lower = name.to_ascii_lowercase();
        if close_end.is_none() {
            // Незакрытое содержимое поглотило бы остаток документа, включая
            // обёртку отчёта: `<` экранируется, и конструкция остаётся текстом.
            self.blocked.push(Blocked::new(
                format!("<{lower}> без закрывающего тега"),
                "незакрытая конструкция поглотила бы остаток документа: показана текстом",
            ));
            self.escape_tag_start(start);
            return;
        }

        if lower == "script" || lower == "textarea" || lower == "title" {
            self.blocked.push(Blocked::new(
                format!("<{lower}>"),
                "содержимое показано текстом: в превью исполняется только код самого отчёта",
            ));
            self.escape_whole_element(start, close_end.map_or(body.end, |end| end + 1));
            return;
        }

        // `<style>`: оформление сохраняется, потому что именно оно и делает
        // превью похожим на карточку, но CSS проходит ту же проверку адресов.
        self.flush(start);
        let sanitized = css(&self.source[body.clone()], self.page);
        self.blocked.extend(sanitized.blocked);
        self.out.push_str("<style>");
        self.out.push_str(&sanitized.css);
        self.out.push_str("</style>");
        self.pos = close_end.map_or(body.end, |end| end + 1);
    }

    /// Показывает элемент текстом целиком, включая закрывающий тег.
    fn escape_whole_element(&mut self, start: usize, end: usize) {
        self.flush(start);
        let fragment = &self.source[start..end];
        self.out.push_str(&fragment.replace('<', "&lt;"));
        self.pos = end;
    }

    /// Экранирует `<` незакрытого тега.
    fn escape_tag_start(&mut self, start: usize) {
        self.flush(start);
        self.out.push_str("&lt;");
        self.pos = start + 1;
    }

    /// Копирует источник до `end`, не пропуская открытый тег.
    fn flush(&mut self, end: usize) {
        if end <= self.pos {
            return;
        }
        // Текст перед тегом копируется как есть: `<`, который сканер не признал
        // началом тега, и в документе остаётся текстом.
        self.out.push_str(&self.source[self.pos..end]);
        self.pos = end;
    }
}

/// Экранирует `&`, `<`, `>`, `"` в значении атрибута.
fn escape_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

/// Экранирует `<` в CSS, чтобы он не закрыл `<style>` и не начал разметку.
///
/// `\3c ` — это CSS-escape для `<`, поэтому смысл значения сохраняется, а тег
/// остаётся тегом: недоверенный CSS не может «выйти» из `<style>`.
fn escape_css_close(style: &str) -> String {
    style.replace('<', "\\3c ")
}

/// Ищет подстроку без учёта ASCII-регистра.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
}

/// Вырезает `@import` вместе с его правилом.
struct StrippedImports {
    css: String,
    blocked: bool,
}

fn strip_imports(style: &str) -> StrippedImports {
    let mut out = String::with_capacity(style.len());
    let mut rest = style;
    let mut blocked = false;

    while let Some(position) = find_ci(rest, "@import") {
        blocked = true;
        out.push_str(&rest[..position]);
        let after = &rest[position + "@import".len()..];
        let end = after.find(';').or_else(|| after.find('}'));
        match end {
            Some(end) => rest = &after[end + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);

    StrippedImports { css: out, blocked }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: Page = Page::Card;

    fn sanitize(fragment: &str) -> Sanitized {
        html(fragment, PAGE)
    }

    #[test]
    fn scripts_are_shown_as_text_in_any_register() {
        for fragment in [
            "<script>alert(1)</script>",
            "<SCRIPT>alert(1)</SCRIPT>",
            "<sCrIpT src=\"https://evil\"></sCrIpT>",
        ] {
            let result = sanitize(fragment);
            assert!(!result.html.contains("<script"), "{result:?}");
            assert!(!result.html.contains("<SCRIPT"), "{result:?}");
            assert!(result.html.contains("alert(1)") || result.html.contains("evil"));
            assert!(inspect(&result.html, PAGE).is_empty(), "{result:?}");
        }
    }

    #[test]
    fn event_handlers_are_dropped_in_any_register() {
        for fragment in [
            "<img src=\"../media/before/a.png\" onerror=\"alert(1)\">",
            "<div ONMOUSEOVER=alert(1)>x</div>",
            "<body onload = \"x\">",
        ] {
            let result = sanitize(fragment);
            assert!(
                !result.html.to_ascii_lowercase().contains("onerror")
                    && !result.html.to_ascii_lowercase().contains("onmouseover")
                    && !result.html.to_ascii_lowercase().contains("onload"),
                "{result:?}"
            );
            assert!(inspect(&result.html, PAGE).is_empty(), "{result:?}");
        }
    }

    #[test]
    fn remote_addresses_are_dropped_in_any_register_and_spelling() {
        for fragment in [
            "<img src=\"https://evil.example/a.png\">",
            "<img SRC='//evil.example/a.png'>",
            "<img src = http://evil.example/a.png>",
            "<a href=\"https://evil.example\">x</a>",
            "<video poster=\"https://evil.example/a.png\"></video>",
            "<object data=\"https://evil.example\"></object>",
            "<img srcset=\"https://evil.example/a.png 1x\">",
        ] {
            let result = sanitize(fragment);
            assert!(
                !result.html.contains("evil.example") || result.html.contains("&quot;"),
                "{result:?}"
            );
            assert!(
                inspect(&result.html, PAGE).is_empty(),
                "после очистки внешних адресов не остаётся: {result:?}"
            );
            assert!(!result.blocked.is_empty(), "отвергнутое обязано называться");
        }
    }

    #[test]
    fn local_media_survives_and_stays_reachable() {
        let result = sanitize(
            "<img src=\"../media/before/a.png\"><source srcset=\"../media/before/b.png\">",
        );
        assert!(result.html.contains("../media/before/a.png"), "{result:?}");
        assert!(result.html.contains("../media/before/b.png"), "{result:?}");
        assert!(result.blocked.is_empty(), "{result:?}");
    }

    #[test]
    fn css_imports_and_remote_urls_are_neutralised() {
        let style = "a { background: url(https://evil.example/a.png); font: url(../media/before/f.woff2); }@import url(\"https://evil.example/c.css\");";
        let result = css(style, PAGE);
        assert!(!result.css.contains("evil.example/a.png"), "{result:?}");
        assert!(!result.css.contains("@import"), "{result:?}");
        assert!(result.css.contains("background: none"), "{result:?}");
        assert!(result.css.contains("../media/before/f.woff2"), "{result:?}");
        assert!(inspect(&format!("<style>{}</style>", result.css), PAGE).is_empty());
    }

    #[test]
    fn css_cannot_leave_the_style_element() {
        let result = css("a { color: red; }</style><script>alert(1)</script>", PAGE);
        assert!(!result.css.contains("</style>"), "{result:?}");
        assert!(!result.css.contains('<'), "{result:?}");
        assert!(inspect(&format!("<style>{}</style>", result.css), PAGE).is_empty());
    }

    #[test]
    fn unterminated_tags_are_escaped_not_merged_with_the_wrapper() {
        for fragment in [
            "<img src=\"../media/before/a.png\"",
            "<a href=\"https://evil.example\"",
            "текст <b",
            "<title>поглотить всё",
        ] {
            let result = sanitize(fragment);
            assert!(
                inspect(&result.html, PAGE).is_empty(),
                "незакрытая конструкция остаётся текстом: {result:?}"
            );
        }
    }

    #[test]
    fn every_generated_document_passes_the_same_check() {
        let index = crate::report::html::index_html(&crate::report::html::ReportDocument {
            title: "Отчёт".to_string(),
            before_label: "до".to_string(),
            after_label: "после".to_string(),
            retire_tag: None,
            counts: crate::report::html::ReportCounts::default(),
            sections: Vec::new(),
            diagnostics: Vec::new(),
            limitations: vec!["ограничение".to_string()],
            unsupported_constructs: Vec::new(),
        });
        assert!(
            inspect(&index, PAGE).is_empty(),
            "{:?}",
            inspect(&index, PAGE)
        );

        let card = crate::report::html::card_html(&crate::report::html::CardFile {
            title: "Карточка".to_string(),
            model_css: "a { color: red }".to_string(),
            sides: vec![crate::report::html::CardSide {
                label: "Лицевая сторона".to_string(),
                classes: "card card1".to_string(),
                html: sanitize("<b>слово</b>").html,
                issues: Vec::new(),
            }],
        })
        .html;
        assert!(
            inspect(&card, PAGE).is_empty(),
            "{:?}",
            inspect(&card, PAGE)
        );
    }

    #[test]
    fn the_check_rejects_what_the_sanitizer_should_have_removed() {
        for bad in [
            "<script>alert(1)</script>",
            "<iframe src=\"https://evil.example\"></iframe>",
            "<img src=\"https://evil.example/a.png\">",
            "<style>@import url(https://evil.example);</style>",
            "<img src=../media/before/a.png onerror=x>",
        ] {
            assert!(
                !inspect(bad, PAGE).is_empty(),
                "{bad:?} обязано отвергаться"
            );
        }
    }
}
