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
//! - **адрес**: значение адресного атрибута обязано быть локальным
//!   ([`Page::allows`]) — то есть вести к файлу, который в отчёте действительно
//!   есть, иначе атрибут удаляется;
//! - **CSS**: `@import` вырезается, а нелокальный адрес заменяется на инертную
//!   заглушку. Что считать адресом в CSS, решает лексический разбор
//!   ([`crate::report::css`]), а не поиск подстроки: `u\72l(…)` — это `url(…)`.
//!   Это же применяется к атрибуту `style` и к содержимому `<style>` — и к CSS
//!   модели, которую подставляет сборщик страницы карточки;
//! - **незакрытый тег**: `<` в его начале экранируется, потому что иначе
//!   недоверенный фрагмент склеивается с обёрткой отчёта, и границу между «данные
//!   кончились» и «начался код отчёта» перестаёт существовать.
//!
//! Что считать адресом в разметке, решает таблица `media::ADDRESS_ATTRIBUTES`:
//! `src` — адрес только у элементов, которые его несут, поэтому `<div src="…">`
//! остаётся обычной разметкой, а не запрещённой ссылкой.
//!
//! Runtime отчёта проходит здесь же: он не «доверенный по имени», а **опознанный
//! по содержимому**. Сборщик страницы помечает свои теги `data-report-runtime`, а
//! [`inspect`] принимает `<script>` только тогда, когда его тело совпадает с одной
//! из констант отчёта ([`crate::report::runtime`]). Поэтому проверка «в документе
//! нет исполняемого чужого кода» — это не поиск подстроки, а сравнение с
//! единственным разрешённым телом, независимо от регистра, кавычек и прочей записи.
//!
//! Проверка одна на все сгенерированные файлы ([`inspect`]): и `index.html`, и
//! `cards/*.html` проходят её перед записью, а тесты — после. Санитайз без
//! независимой проверки — это обещание; проверка превращает его в контракт.
//! Проверка при этом строже санитайза: адрес она сверяет с точным набором файлов
//! отчёта ([`Resources`]), поэтому ссылка на такой же по форме, но отсутствующий
//! файл становится отказом генерации, а не живым запросом у пользователя.
//!
//! Отвергнутое не выбрасывается молча: [`Sanitized::blocked`] перечисляет, что
//! именно не показано, и это уходит в превью и в диагностику отчёта.

use std::collections::BTreeSet;
use std::ops::Range;

use crate::htmlscan::{self, Tag};
use crate::media;
use crate::report::css;
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

/// Страница отчёта: у каждой свой документ и своя форма локального адреса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// `cards/*.html`: рядом с ним скопированные media.
    Card,
    /// `index.html`: рядом с ним документы превью.
    Index,
}

impl Page {
    /// Каталог документа относительно корня отчёта.
    fn document_dir(self) -> &'static str {
        match self {
            Self::Card => "cards",
            Self::Index => "",
        }
    }

    /// Приводит адрес к пути относительно корня отчёта.
    ///
    /// Это единственное место, где адрес становится путём, и оно fail-closed:
    /// `None` означает «показать нельзя». Отказ получают схема (`data:`,
    /// `https:`), абсолютный путь, обратный слэш, управляющие символы, пустой или
    /// неоднозначный сегмент (`.`, пустой — то есть `//` и хвостовой `/`) и,
    /// главное, любой адрес, который после нормализации выходит за корень отчёта.
    ///
    /// Нормализация идёт по сегментам относительно **пути документа**, а не по
    /// префиксу строки: `../media/before/…` разрешено ровно столько раз, сколько
    /// компонентов в каталоге документа (из `cards/` — один), `..` допустим только
    /// в начале и только пока есть что выталкивать, а `cards/../…` — уже не
    /// нормализация, а попытка выйти из своего каталога раньше времени.
    ///
    /// Отдельно отвергаются символы, которые меняют смысл адреса в URL, а не в
    /// имени файла: `?`, `#` и `%`. Адрес в документе — это URL, и браузер читает
    /// путь из него уже процент-декодированным, поэтому имя вида `..%2f..%2f…`
    /// увело бы запрос туда, куда тот же текст как путь не ведёт. Имя файла отчёта
    /// таких символов не содержит, и «не могу доказать, что это то же имя» здесь
    /// означает отказ.
    #[must_use]
    pub fn resolve(self, value: &str) -> Option<String> {
        let value = value.trim();
        if value.is_empty()
            || value.contains('\\')
            || value.contains(':')
            || value.contains('?')
            || value.contains('#')
            || value.contains('%')
        {
            return None;
        }
        if value.starts_with('/') {
            return None;
        }
        if value.chars().any(char::is_control) {
            return None;
        }

        let mut segments: Vec<&str> = Vec::new();
        if !self.document_dir().is_empty() {
            segments.extend(self.document_dir().split('/'));
        }
        let mut descended = false;
        for segment in value.split('/') {
            match segment {
                "" | "." => return None,
                ".." => {
                    if descended {
                        return None;
                    }
                    segments.pop()?;
                }
                other => {
                    descended = true;
                    segments.push(other);
                }
            }
        }

        let resolved = segments.join("/");
        if !self.has_resource_shape(&resolved) {
            return None;
        }
        Some(resolved)
    }

    /// Форма ресурса, который эта страница имеет право запросить.
    fn has_resource_shape(self, path: &str) -> bool {
        let segments: Vec<&str> = path.split('/').collect();
        match self {
            Self::Card => {
                matches!(segments.as_slice(), ["media", state, name]
                    if matches!(*state, "before" | "after") && is_plain_name(name))
            }
            Self::Index => {
                matches!(segments.as_slice(), ["cards", name]
                    if is_plain_name(name) && name.ends_with(".html"))
            }
        }
    }

    /// Что эта страница имеет право запрашивать.
    ///
    /// Разрешены переходы внутри документа и ровно те ресурсы, которые текущий
    /// отчёт действительно содержит: `cards/*.html` для точки входа и
    /// `media/<состояние>/*` для превью. Всё остальное — включая `data:`,
    /// абсолютные пути, `..` за пределы отчёта и ссылку на файл, которого в
    /// отчёте нет, — считается внешним: отчёт обязан оставаться офлайн и
    /// переносимым.
    #[must_use]
    pub fn allows(self, resources: &Resources, value: &str) -> bool {
        if value.trim_start().starts_with('#') {
            return true;
        }
        let Some(path) = self.resolve(value) else {
            return false;
        };
        match self {
            Self::Card => resources.media_files.contains(&path),
            Self::Index => resources.card_files.contains(&path),
        }
    }
}

/// Может ли это имя быть последним сегментом адреса внутри отчёта.
///
/// Вопрос один и тот же у двух потребителей: у политики адресов (здесь) и у гейта
/// копирования media ([`crate::report::media`]). Второй не имеет права быть слабее
/// первого: скопированный файл, имя которого нельзя записать в адрес, — это файл,
/// на который не может сослаться ни одна страница отчёта.
///
/// Запрещено всё, что делает имя не одним сегментом пути (`/`) либо расходится при
/// записи в разметку и чтении из неё: разделитель пути (`\`), символы, меняющие
/// смысл адреса (`:`, `?`, `#`, `%`), символы, которые экранируются в атрибуте
/// (`"`, `&`, `<`, `>`), и служебные символы. Имя файла на диске и имя в адресе
/// обязаны совпадать: иначе адрес указывал бы не на тот файл, который скопирован.
pub(crate) fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.chars().any(|character| {
            matches!(
                character,
                '/' | '\\' | ':' | '?' | '#' | '%' | '"' | '&' | '<' | '>'
            ) || character.is_control()
        })
}

/// Точный набор ресурсов, которые есть в каталоге отчёта.
///
/// Политика адресов не может быть только префиксом: `cards/` и `media/` — это
/// каталоги, а не доказательство существования файла. Набор передаётся туда, где
/// документ проверяется, и заполняется тем, что отчёт действительно записал:
/// [`Page::allows`] отвечает не «похоже на свой каталог», а «этот файл здесь есть».
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resources {
    card_files: BTreeSet<String>,
    media_files: BTreeSet<String>,
}

impl Resources {
    /// Набор из фактических относительных путей файлов отчёта.
    ///
    /// Пути, не относящиеся ни к `cards/`, ни к `media/`, отбрасываются: они не
    /// могут быть целью адреса ни на одной странице.
    #[must_use]
    pub fn from_report_files<'a>(files: impl IntoIterator<Item = &'a String>) -> Self {
        let mut resources = Self::default();
        for file in files {
            if file.starts_with("cards/") {
                resources.card_files.insert(file.clone());
            } else if file.starts_with("media/") {
                resources.media_files.insert(file.clone());
            }
        }
        resources
    }

    /// Документы превью, которые есть в отчёте.
    #[must_use]
    pub fn card_files(&self) -> &BTreeSet<String> {
        &self.card_files
    }

    /// Скопированные media, которые есть в отчёте.
    #[must_use]
    pub fn media_files(&self) -> &BTreeSet<String> {
        &self.media_files
    }
}

/// Разрешено ли значение атрибута-адреса целиком.
///
/// У `srcset` в одном значении перечислено несколько адресов, и проверять их
/// нужно по отдельности: список — это несколько запросов, а не один адрес, и
/// префиксная проверка «начинается с `../media/`» пропустила бы всё, что стоит в
/// списке после первого кандидата. Предикат один на санитайз и на проверку
/// готового документа: две реализации одной политики разошлись бы.
///
/// Пустое значение разрешено: адреса в нём нет, а запрос текущего документа
/// остаётся внутри отчёта.
#[must_use]
pub fn attribute_value_is_allowed(
    page: Page,
    resources: &Resources,
    attribute: &str,
    value: &str,
) -> bool {
    let candidates = media::split_attribute_values(attribute, value);
    candidates
        .iter()
        .all(|candidate| page.allows(resources, &candidate.text))
}

/// Очищает недоверенный фрагмент HTML.
#[must_use]
pub fn html(fragment: &str, page: Page, resources: &Resources) -> Sanitized {
    let mut sanitizer = Sanitizer {
        source: fragment,
        page,
        resources,
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

/// Очищает CSS: `@import` вырезается, нелокальные адреса становятся инертными.
///
/// Отдельная функция, потому что CSS приходит двумя путями: как содержимое
/// `<style>` и как CSS модели, который подставляет сборщик страницы. Один
/// владелец политики — одно поведение. Разбор, который решает, что здесь адрес,
/// тоже один: [`crate::report::css`], а не поиск подстроки.
#[must_use]
pub fn css(style: &str, page: Page, resources: &Resources) -> SanitizedCss {
    let mut out = String::with_capacity(style.len());
    let mut blocked: Vec<Blocked> = Vec::new();
    let mut cursor = 0usize;

    for event in css::scan(style) {
        let span = event.span();
        // События не пересекаются, но защита нужна: перекрывающаяся замена
        // испортила бы вторичную проверку независимо от того, как её собрали.
        if span.start < cursor || span.end > style.len() {
            continue;
        }
        out.push_str(&style[cursor..span.start]);
        match event {
            css::Event::Import(_) => {
                blocked.push(Blocked::new(
                    "@import",
                    "внешняя таблица стилей не подключается: отчёт обязан оставаться офлайн",
                ));
            }
            css::Event::Address(address) => {
                if address.target.is_empty() || page.allows(resources, &address.target) {
                    out.push_str(&style[span.clone()]);
                } else {
                    out.push_str(INERT_CSS_VALUE);
                    blocked.push(Blocked::new(
                        format!("url({})", address.target),
                        "адрес CSS не ведёт к файлу, скопированному рядом с отчётом: запрос не \
                         выполняется",
                    ));
                }
            }
        }
        cursor = span.end;
    }
    out.push_str(&style[cursor..]);

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
///
/// Проверка намеренно строже санитайза: адреса она сверяет с точным набором
/// ресурсов отчёта, а не только с формой пути. Если санитайз что-то пропустил,
/// именно здесь это становится отказом генерации, а не живым запросом в браузере
/// пользователя.
#[must_use]
pub fn inspect(document: &str, page: Page, resources: &Resources) -> Vec<Violation> {
    let mut violations: Vec<Violation> = Vec::new();

    for tag in htmlscan::scan_tags(document) {
        match tag {
            Tag::Element(element) => {
                let name = element.name.to_ascii_lowercase();

                // Исключения нет: runtime отчёта — это raw-text `script`, который
                // разобран как [`Tag::RawText`] и сверен по телу. Элемент `script`
                // в готовом документе — всегда чужая разметка, и пометка
                // `data-report-runtime` в значении поля её не оправдывает.
                if name == "script" {
                    violations.push(Violation {
                        what: "исполняемый <script> без тела runtime отчёта".to_string(),
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
                        && !attribute_value_is_allowed(
                            page,
                            resources,
                            &attribute_name,
                            attribute.value,
                        )
                    {
                        violations.push(Violation {
                            what: format!("адрес {attribute_name}=\"{}\"", attribute.value),
                        });
                    }
                    if attribute_name == "style" {
                        match htmlscan::decoded_attribute_value(attribute.value) {
                            Some(decoded) => violations
                                .extend(css_violations(&decoded, page, resources, "style")),
                            None => violations.push(Violation {
                                what: "значение style записано символьной ссылкой".to_string(),
                            }),
                        }
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
                    violations.extend(css_violations(
                        &document[body.clone()],
                        page,
                        resources,
                        "style",
                    ));
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
///
/// Разбор тот же, что у [`css`]: проверка не повторяет слабую эвристику, а
/// спрашивает у того же владельца политики, только с точным набором ресурсов.
fn css_violations(style: &str, page: Page, resources: &Resources, where_: &str) -> Vec<Violation> {
    let mut violations: Vec<Violation> = Vec::new();
    for event in css::scan(style) {
        match event {
            css::Event::Import(_) => violations.push(Violation {
                what: format!("@import в {where_}"),
            }),
            css::Event::Address(address) => {
                if !address.target.is_empty() && !page.allows(resources, &address.target) {
                    violations.push(Violation {
                        what: format!("адрес url({}) в {where_}", address.target),
                    });
                }
            }
        }
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
    resources: &'a Resources,
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
            // Удаляется тег целиком, вместе с его `>`: иначе закрывающая скобка
            // осталась бы в тексте превью как обычный символ.
            self.pos = element.end + 1;
            return;
        }

        // Пометки сборщика страницы ставит сам отчёт, но внутри значения поля их
        // может написать кто угодно: данные не отличаются от разметки отчёта.
        // Поэтому «свой» здесь не значит «доверенный» — исключений для обработчиков
        // нет, и `report_owned` больше не ослабляет правило.
        let mut kept: Vec<usize> = Vec::new();
        let mut dropped: Vec<Blocked> = Vec::new();
        for (index, attribute) in element.attributes.iter().enumerate() {
            let attribute_name = attribute.name.to_ascii_lowercase();
            if attribute_name.starts_with("on") {
                dropped.push(Blocked::new(
                    attribute_name,
                    "обработчик события исполнил бы чужой код: атрибут удалён",
                ));
                continue;
            }
            if media::attribute_is_address(&name, &attribute_name)
                && !attribute_value_is_allowed(
                    self.page,
                    self.resources,
                    &attribute_name,
                    attribute.value,
                )
            {
                dropped.push(Blocked::new(
                    format!("{attribute_name}=\"{}\"", attribute.value),
                    Self::address_reason(attribute.value),
                ));
                continue;
            }
            kept.push(index);
        }

        // Ссылки в значении атрибута раскодируются до разбора CSS: браузер увидит
        // адрес там, где в исходном тексте его не видно, и очистка обязана решать
        // по тому же тексту, что и браузер. Незнакомая ссылка значения не имеет:
        // тогда весь `style` становится инертным, а причина называется.
        let sanitized_style = element.attribute("style").map(|attribute| {
            match htmlscan::decoded_attribute_value(attribute.value) {
                Some(decoded) => css(&decoded, self.page, self.resources),
                None => SanitizedCss {
                    css: INERT_CSS_VALUE.to_string(),
                    blocked: vec![Blocked::new(
                        "style".to_string(),
                        "значение style записано символьной ссылкой: адрес в нём разбору не виден, \
                         поэтому значение не показано"
                            .to_string(),
                    )],
                },
            }
        });
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
                Some(style) if attribute.name_is("style") => escape_attr(&style.css),
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
        let sanitized = css(&self.source[body.clone()], self.page, self.resources);
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

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: Page = Page::Card;

    /// Набор отчёта для тестов: ровно те файлы, которые страница вправе запросить.
    fn resources() -> Resources {
        Resources::from_report_files(&[
            "index.html".to_string(),
            "cards/card-0001.html".to_string(),
            "media/before/a.png".to_string(),
            "media/before/b.png".to_string(),
            "media/before/f.woff2".to_string(),
            "media/after/a.png".to_string(),
        ])
    }

    fn sanitize(fragment: &str) -> Sanitized {
        html(fragment, PAGE, &resources())
    }

    fn clean(document: &str) -> bool {
        inspect(document, PAGE, &resources()).is_empty()
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
            assert!(clean(&result.html), "{result:?}");
        }
    }

    #[test]
    fn a_dropped_element_leaves_no_stray_bracket_behind() {
        // Удаляется тег, который действует, а не запись о нём: содержимое
        // остаётся текстом, и закрывающий тег тоже (сканер разметки закрывающие
        // теги не сообщает, а второго разбора здесь нет). Обрезать тег посередине
        // нельзя: `<object>` не имеет права превратиться в голую `>`.
        for (fragment, text) in [
            ("<object data=\"a.png\">текст</object>", "текст</object>"),
            (
                "<iframe src=\"https://evil.example\"></iframe>",
                "</iframe>",
            ),
            ("<OBJECT data=\"a.png\"/>слово", "слово"),
        ] {
            let result = sanitize(fragment);
            assert_eq!(result.html, text, "{fragment:?} → {result:?}");
            assert_eq!(result.blocked.len(), 1, "{fragment:?}: {result:?}");
            assert!(clean(&result.html), "{result:?}");
        }
    }

    #[test]
    fn a_dropped_element_does_not_hide_what_it_contained() {
        // Отбрасывается только сам элемент: вложенная в него разметка проходит ту
        // же очистку, что и любая другая, и адрес внутри неё не выживает.
        let result = sanitize(
            "<object><style>a{background:url(https://evil.example/a.png)}</style></object>",
        );

        assert!(!result.html.contains("https://evil.example"), "{result:?}");
        assert!(!result.html.contains("<object"), "{result:?}");
        assert!(result.html.contains("background"), "{result:?}");
        assert!(clean(&result.html), "{result:?}");
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
            assert!(clean(&result.html), "{result:?}");
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
            "<iframe src=\"https://evil.example\"></iframe>",
        ] {
            let result = sanitize(fragment);
            assert!(
                !result.html.contains("evil.example") || result.html.contains("&quot;"),
                "{result:?}"
            );
            assert!(
                clean(&result.html),
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
    fn a_src_outside_a_resource_bearing_element_is_not_an_address() {
        // `src` у `div` такого атрибута не имеет, и браузер по нему ничего не
        // запрашивает: запрещать эту разметку значило бы судить по имени
        // атрибута, а не по семантике элемента.
        let result = sanitize("<div src=\"не-адрес\">x</div>");
        assert!(result.html.contains("не-адрес"), "{result:?}");
        assert!(result.blocked.is_empty(), "{result:?}");
        assert!(clean(&result.html), "{result:?}");
    }

    #[test]
    fn the_address_policy_is_structural_and_never_leaves_the_report_root() {
        let resources = resources();
        for bad in [
            "../../outside.png",
            "../media/before/../../../outside.png",
            "cards/../../media/before/a.png",
            "../media/../media/before/a.png",
            "/media/before/a.png",
            "//evil.example/a.png",
            "media/before/a.png",
            "cards/card-0001.html",
            "..\\media\\before\\a.png",
            "../media/before/./a.png",
            "../media/before//a.png",
            "../media/before/",
            "../media/before/a.png?x=1",
            "../media/before/a.png#фрагмент",
            "../media/before/%2e%2e%2f%2e%2e%2foutside.png",
            "../media/before/..%2f..%2foutside.png",
            "data:image/png;base64,AAAA",
            "https://evil.example/a.png",
            "javascript:alert(1)",
            "index.html",
            "../media/период/../../../outside.png",
        ] {
            assert!(
                !PAGE.allows(&resources, bad),
                "{bad:?} не может быть разрешён"
            );
            assert_eq!(
                PAGE.resolve(bad),
                None,
                "{bad:?} не имеет разрешённого пути"
            );
        }
    }

    #[test]
    fn only_the_exact_set_of_report_files_is_reachable() {
        let resources = resources();
        assert!(PAGE.allows(&resources, "../media/before/a.png"));
        assert!(PAGE.allows(&resources, "../media/after/a.png"));
        assert!(PAGE.allows(&resources, " #локальный-фрагмент"));
        // Форма та же, файла в отчёте нет: набор — это доказательство
        // существования, а не форма пути.
        assert!(!PAGE.allows(&resources, "../media/before/нет.png"));
        assert!(!PAGE.allows(&resources, "../media/после/a.png"));
        // Точка входа адресуется документами превью, а не ими же.
        assert!(Page::Index.allows(&resources, "cards/card-0001.html"));
        assert!(Page::Index.allows(&resources, "#якорь"));
        assert!(!Page::Index.allows(&resources, "cards/card-0002.html"));
        assert!(!Page::Index.allows(&resources, "../media/before/a.png"));
    }

    #[test]
    fn an_address_of_the_right_shape_to_an_absent_file_is_dropped() {
        let result = sanitize("<img src=\"../media/before/нет.png\">");
        assert!(!result.html.contains("нет.png"), "{result:?}");
        assert!(!result.blocked.is_empty(), "{result:?}");
        assert!(clean(&result.html), "{result:?}");
    }

    #[test]
    fn css_imports_and_remote_urls_are_neutralised() {
        let style = "a { background: url(https://evil.example/a.png); font: url(../media/before/f.woff2); }@import url(\"https://evil.example/c.css\");";
        let result = css(style, PAGE, &resources());
        assert!(!result.css.contains("evil.example/a.png"), "{result:?}");
        assert!(!result.css.contains("@import"), "{result:?}");
        assert!(result.css.contains("background: none"), "{result:?}");
        assert!(result.css.contains("../media/before/f.woff2"), "{result:?}");
        assert!(clean(&format!("<style>{}</style>", result.css)));
    }

    #[test]
    fn an_escaped_or_unterminated_css_address_never_survives() {
        for style in [
            "a{background:url(https://evil.example/a.png)}",
            "a{background:URL(https://evil.example/a.png)}",
            "a{background:u\\72l(https://evil.example/a.png)}",
            "a{background:\\75 rl(https://evil.example/a.png)}",
            "a{background:url(https\\3a //evil.example/a.png)}",
            "a{background:url('https\\3a //evil.example/a.png')}",
            "a{background:url( \"https://evil.example/a.png\" )}",
            "a{background:image-set(\"https://evil.example/a.png\" 1x)}",
            "a{background:-webkit-image-set('https://evil.example/a.png' 2x)}",
            "@import url(https://evil.example/c.css);",
            "@IMPORT \"https://evil.example/c.css\";",
            "@im\\70 ort \"https://evil.example/c.css\";",
            "a{background:url(https://evil.example/a.png",
            "@font-face{src:\"https://evil.example/a.woff2\"}",
            "a{background:url(https\\3a //evil.example/a.png)}",
            "a{background:url(\"https\\3a //evil.example/a.png\")}",
            "a{background:url(https\\3a //evil.example/a.png",
        ] {
            let result = css(style, PAGE, &resources());
            assert!(!result.css.contains("evil"), "{style:?} → {:?}", result.css);
            assert!(
                clean(&format!("<style>{}</style>", result.css)),
                "{style:?} → {:?}",
                result.css
            );
        }
    }

    #[test]
    fn safe_css_keeps_its_meaning() {
        let style = "a { color: red; content: \"это не адрес\"; background: url(../media/before/a.png); filter: url(#локальный); }";
        let result = css(style, PAGE, &resources());
        assert!(result.css.contains("color: red"), "{result:?}");
        assert!(result.css.contains("это не адрес"), "{result:?}");
        assert!(result.css.contains("../media/before/a.png"), "{result:?}");
        assert!(result.css.contains("url(#локальный)"), "{result:?}");
        assert!(result.blocked.is_empty(), "{result:?}");
    }

    #[test]
    fn css_cannot_leave_the_style_element() {
        let result = css(
            "a { color: red; }</style><script>alert(1)</script>",
            PAGE,
            &resources(),
        );
        assert!(!result.css.contains("</style>"), "{result:?}");
        assert!(!result.css.contains('<'), "{result:?}");
        assert!(clean(&format!("<style>{}</style>", result.css)));
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
                clean(&result.html),
                "незакрытая конструкция остаётся текстом: {result:?}"
            );
        }
    }

    #[test]
    fn every_generated_document_passes_the_same_check() {
        let resources = resources();
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
            inspect(&index, Page::Index, &resources).is_empty(),
            "{:?}",
            inspect(&index, Page::Index, &resources)
        );

        let card = crate::report::html::card_html(
            &crate::report::html::CardFile {
                title: "Карточка".to_string(),
                model_css: "a { color: red }".to_string(),
                sides: vec![crate::report::html::CardSide {
                    label: "Лицевая сторона".to_string(),
                    classes: "card card1".to_string(),
                    html: sanitize("<b>слово</b>").html,
                    issues: Vec::new(),
                }],
            },
            &resources,
        )
        .html;
        assert!(
            inspect(&card, PAGE, &resources).is_empty(),
            "{:?}",
            inspect(&card, PAGE, &resources)
        );
    }

    #[test]
    fn a_document_that_carried_css_the_sanitizer_missed_is_refused() {
        // Проверка не повторяет политику санитайза, а отвечает на тот же вопрос
        // строже: документ, в котором адрес всё-таки остался, отвергается.
        let resources = resources();
        for bad in [
            "<script>alert(1)</script>",
            "<iframe src=\"https://evil.example\"></iframe>",
            "<img src=\"https://evil.example/a.png\">",
            "<style>@import url(https://evil.example);</style>",
            "<style>a{background:u\\72l(https://evil.example/a.png)}</style>",
            "<img src=../media/before/a.png onerror=x>",
            "<img src=\"../media/before/нет.png\">",
            "<style>a{background:url(https://evil.example/a.png)</style>",
            "<style>@im\\70 ort \"https://evil.example/c.css\";</style>",
        ] {
            assert!(
                !inspect(bad, PAGE, &resources).is_empty(),
                "{bad:?} обязано отвергаться"
            );
        }
    }

    #[test]
    fn the_check_rejects_what_the_sanitizer_should_have_removed() {
        // Тот же вопрос, что и выше, но от лица санитайза: после очистки документ
        // обязан проходить проверку, а до неё — нет.
        for fragment in [
            "<script>alert(1)</script>",
            "<img src=\"https://evil.example/a.png\">",
            "<style>@import url(https://evil.example);</style>",
        ] {
            let sanitized = sanitize(fragment);
            assert!(clean(&sanitized.html), "{fragment:?} → {sanitized:?}");
        }
    }

    /// Тег, записанный как самозакрытый, всё равно открывает raw-text элемент.
    ///
    /// HTML5 игнорирует `/` у непустого элемента, поэтому `<script/>` — это
    /// открывающий тег, а тело после него читается браузером как код. Разбор
    /// обязан увидеть то же: иначе чужой код в поле остался бы в документе тегом.
    #[test]
    fn a_self_closing_script_is_shown_as_text() {
        let result = sanitize("<script/>alert(1)</script>");
        assert!(!result.html.contains("<script"), "{result:?}");
        assert!(result.html.contains("alert(1)"), "{result:?}");
        assert_eq!(result.blocked.len(), 1, "{result:?}");
        assert!(clean(&result.html), "{result:?}");
    }

    /// Пометка сборщика отчёта не делает обработчик своим.
    ///
    /// `data-report-runtime` в значении поля может написать кто угодно, а
    /// обработчик события — это исполнение чужого кода в отчёте.
    #[test]
    fn a_handler_behind_a_report_marker_is_dropped() {
        let result = sanitize("<div data-report-runtime=\"card\" onmouseover=\"alert(3)\">т</div>");
        assert_eq!(result.html, "<div data-report-runtime=\"card\">т</div>");
        assert_eq!(result.blocked.len(), 1, "{result:?}");
        assert!(clean(&result.html), "{result:?}");
    }

    /// Кавычка внутри `style` не имеет права выйти за пределы атрибута.
    ///
    /// Значение `style` пишется в двойных кавычках, поэтому кавычка в очищенном
    /// CSS — это конец атрибута и начало чужой разметки.
    #[test]
    fn a_quote_in_a_style_value_cannot_leave_the_attribute() {
        let result =
            sanitize("<div style='content:\"x\";background:url(https://evil/a.png)'>т</div>");
        assert_eq!(
            result.html,
            "<div style=\"content:&quot;x&quot;;background:none\">т</div>"
        );
        assert!(clean(&result.html), "{result:?}");
    }

    /// Адрес, записанный символьной ссылкой, остаётся адресом.
    ///
    /// Браузер раскодирует ссылки в значении атрибута: `url&#40;a.png&#41;` — это
    /// для него запрос файла, которого в отчёте нет.
    #[test]
    fn an_address_written_as_a_character_reference_is_still_an_address() {
        let result = sanitize("<div style=\"background:url&#40;a.png&#41;\">т</div>");
        assert_eq!(result.html, "<div style=\"background:none\">т</div>");
        assert_eq!(result.blocked.len(), 1, "{result:?}");
        assert!(clean(&result.html), "{result:?}");
    }

    /// Незнакомая символьная ссылка делает значение инертным.
    ///
    /// Что именно она значит, разбор не знает, а адрес в таком значении мог бы
    /// остаться незамеченным: поэтому значение не показывается, а причина
    /// называется.
    #[test]
    fn an_unknown_character_reference_makes_the_style_inert() {
        let result = sanitize("<div style=\"x:&lpar;\">т</div>");
        assert_eq!(result.html, "<div style=\"none\">т</div>");
        assert_eq!(result.blocked.len(), 1, "{result:?}");
        assert!(clean(&result.html), "{result:?}");
    }

    /// Комментарий кончается там, где его закрывает браузер.
    ///
    /// `--!>` закрывает комментарий для браузера: разметка после него действует.
    /// Если разбор считает её комментарием, очистка не видит ни тега, ни
    /// обработчика, и оба доходят до документа.
    #[test]
    fn a_comment_that_ends_early_does_not_hide_markup() {
        let result = sanitize("<!--a--!><img src=no.png onerror=\"alert(2)\">");
        assert_eq!(result.html, "<!--a--!><img>");
        assert_eq!(result.blocked.len(), 2, "{result:?}");
        assert!(clean(&result.html), "{result:?}");
    }

    /// Документ с тегом `script` отвергается и с пометкой runtime.
    ///
    /// Runtime отчёта — это raw-text `script`, сверенный по телу, а элемент
    /// `script` в готовом документе всегда чужая разметка.
    #[test]
    fn a_script_element_is_refused_even_with_a_report_marker() {
        for document in [
            "<body><script data-report-runtime=\"card\">alert(1)</script></body>",
            "<body><script data-report-runtime=\"card\"/></body>",
        ] {
            assert!(!clean(document), "{document:?}");
        }
    }
}
