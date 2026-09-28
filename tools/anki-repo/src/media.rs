//! Media-счётчики, безопасная проверка наличия файлов и распознавание
//! media-ссылок в значении поля.
//!
//! `media_files` — это недоверенный список имён, а не содержимое media.
//! Tool никогда не конструирует из него filesystem path: сравниваются только
//! множества имён, а физические имена берутся листингом каталога `media/`.
//!
//! Распознавание ссылок в HTML-значении поля тоже живёт здесь, и это
//! единственный владелец ответа на такой вопрос. Textual-поиск (`contains("src=")`)
//! отвечает на него неверно: HTML ASCII case-insensitive в именах элементов и
//! атрибутов, допускает пробелы вокруг `=`, unquoted-значения и одинарные
//! кавычки — то есть валидная разметка проходила бы мимо гейта
//! `media_forbidden`. Разбор разметки выполняет [`crate::htmlscan`], а политика
//! «какой атрибут какого элемента несёт media» объявлена здесь одной таблицей
//! [`MEDIA_ATTRIBUTES`], чтобы README, гейт `create` и подстановка ссылок в
//! отчёте не разъезжались.
//!
//! Две функции намеренно различаются, и различие документировано:
//!
//! - [`visible_media_references`] отвечает «что увидит браузер» — только полные
//!   элементы (незакрытый тег в документе элемента не создаёт);
//! - [`forbidden_media_references`] — это гейт `create`, и он **fail-closed**:
//!   значение поля не является документом, Anki склеивает его с шаблоном, и
//!   незакрытый тег может закрыться уже там. Поэтому незакрытая конструкция с
//!   media-атрибутом считается ссылкой, а адрес из CSS — такой же ссылкой, как
//!   `src`.
//!
//! Разбор самой разметки выполняет [`crate::htmlscan`], разбор CSS — владелец
//! политики адресов [`crate::report::css`]: гейт не заводит второй парсер, иначе
//! «что здесь адрес» решалось бы в двух местах и разъезжалось.
//!
//! Гейт используется только для отказа; ссылок он не читает и файлов не
//! открывает.

use std::collections::BTreeSet;
use std::fs;
use std::ops::Range;
use std::path::Path;

use crate::htmlscan::{self, Tag};
use crate::index::ExportIndex;
use crate::report::css;

/// Имя каталога media внутри экспорта.
pub const MEDIA_DIR: &str = "media";

/// Префикс звуковой ссылки Anki.
pub const SOUND_OPEN: &str = "[sound:";

/// Что означает атрибут с точки зрения запроса файла.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceKind {
    /// Атрибут несёт media: браузер запрашивает файл, чтобы показать его.
    ///
    /// Именно эти конструкции запрещены новым значениям полей у `create`: сам
    /// toolkit файлов не создаёт и не копирует.
    Media,
    /// Атрибут несёт адрес перехода: картинка не запрашивается, но переход уводит
    /// документ за его пределы.
    ///
    /// Гейт `create` такие ссылки не запрещает — это не media. Зато граница
    /// доверия отчёта обязана их видеть: внешний адрес в превью — это запрос,
    /// которого офлайн-отчёт не имеет права сделать.
    Navigation,
}

/// Атрибуты, по которым браузер идёт по адресу.
///
/// Первый элемент пары — имя элемента или `*` для любого; второй — имя атрибута;
/// третий — что этот атрибут означает. Сравнение без учёта ASCII-регистра: HTML не
/// различает регистр в именах элементов и атрибутов. Таблица — единственное место,
/// где объявлено, что считать ссылкой, поэтому гейт `create` и граница доверия
/// отчёта отвечают на этот вопрос одинаково, а README перечисляет ровно эти
/// конструкции.
///
/// Область действия атрибута — часть семантики, а не удобство записи:
///
/// - **media-атрибуты перечислены поэлементно.** `src` запрашивает файл только у
///   элементов, которые его несут: у `img`, `audio`, `video`, `source`, `track`,
///   `input`, `embed`, `iframe`, `frame`, `script`. `<div src="…">` не запрашивает
///   ничего — такого атрибута у `div` нет, — поэтому считать его ссылкой значило
///   бы запрещать валидную разметку на основании имени атрибута, а не семантики
///   элемента. Так же поэлементны legacy `background`, `srcset` и `poster`;
/// - **навигационные атрибуты, наоборот, перечислены через `*`.** Переход не
///   запрашивает файл, но уводит документ за его пределы, а состав элементов,
///   которые его выполняют, шире и включает SVG (`xlink:href` у `use` и `image`).
///   Здесь неполный список означал бы живой внешний адрес в офлайн-отчёте, поэтому
///   область выбрана намеренно широкой: лишняя нейтрализация видна и безопасна.
pub const ADDRESS_ATTRIBUTES: &[(&str, &str, ReferenceKind)] = &[
    ("img", "src", ReferenceKind::Media),
    ("audio", "src", ReferenceKind::Media),
    ("video", "src", ReferenceKind::Media),
    ("source", "src", ReferenceKind::Media),
    ("track", "src", ReferenceKind::Media),
    ("input", "src", ReferenceKind::Media),
    ("embed", "src", ReferenceKind::Media),
    ("iframe", "src", ReferenceKind::Media),
    ("frame", "src", ReferenceKind::Media),
    ("script", "src", ReferenceKind::Media),
    ("body", "background", ReferenceKind::Media),
    ("table", "background", ReferenceKind::Media),
    ("thead", "background", ReferenceKind::Media),
    ("tbody", "background", ReferenceKind::Media),
    ("tfoot", "background", ReferenceKind::Media),
    ("tr", "background", ReferenceKind::Media),
    ("td", "background", ReferenceKind::Media),
    ("th", "background", ReferenceKind::Media),
    ("img", "srcset", ReferenceKind::Media),
    ("source", "srcset", ReferenceKind::Media),
    ("video", "poster", ReferenceKind::Media),
    ("object", "data", ReferenceKind::Media),
    ("*", "href", ReferenceKind::Navigation),
    ("*", "xlink:href", ReferenceKind::Navigation),
    ("*", "action", ReferenceKind::Navigation),
    ("*", "formaction", ReferenceKind::Navigation),
    ("*", "ping", ReferenceKind::Navigation),
    ("*", "manifest", ReferenceKind::Navigation),
    ("*", "cite", ReferenceKind::Navigation),
    ("*", "longdesc", ReferenceKind::Navigation),
    ("*", "usemap", ReferenceKind::Navigation),
    ("*", "archive", ReferenceKind::Navigation),
    ("*", "classid", ReferenceKind::Navigation),
    ("*", "codebase", ReferenceKind::Navigation),
];

/// Что означает этот атрибут этого элемента.
#[must_use]
pub fn attribute_kind(element: &str, attribute: &str) -> Option<ReferenceKind> {
    ADDRESS_ATTRIBUTES
        .iter()
        .find(|(scope, name, _)| {
            attribute.eq_ignore_ascii_case(name)
                && (*scope == "*" || element.eq_ignore_ascii_case(scope))
        })
        .map(|(_, _, kind)| *kind)
}

/// Несёт ли этот атрибут этого элемента media.
#[must_use]
pub fn attribute_carries_media(element: &str, attribute: &str) -> bool {
    attribute_kind(element, attribute) == Some(ReferenceKind::Media)
}

/// Ведёт ли этот атрибут этого элемента по адресу (media или переход).
#[must_use]
pub fn attribute_is_address(element: &str, attribute: &str) -> bool {
    attribute_kind(element, attribute).is_some()
}

/// Ссылка на media, найденная в HTML-значении поля.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaReference {
    /// Имя элемента в нижнем регистре.
    pub element: String,
    /// Имя атрибута в нижнем регистре.
    pub attribute: String,
    /// Значение атрибута без кавычек; у `srcset` — каждый кандидат отдельно.
    pub value: String,
}

/// Найденная в значении поля конструкция `[sound:NAME]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundReference<'a> {
    /// Имя файла без пробелов по краям.
    pub name: &'a str,
    /// Диапазон всей конструкции `[sound:NAME]` в источнике.
    pub span: Range<usize>,
}

/// Находит конструкции `[sound:NAME]`.
///
/// Незакрытая конструкция не угадывается: она не считается ссылкой, потому что
/// её не видит и Anki. Сканирование при этом останавливается, а не продолжается
/// после выдуманной границы.
#[must_use]
pub fn sound_references(text: &str) -> Vec<SoundReference<'_>> {
    let mut found: Vec<SoundReference<'_>> = Vec::new();
    let mut position = 0usize;

    while let Some(offset) = text[position..].find(SOUND_OPEN) {
        let start = position + offset;
        let content_start = start + SOUND_OPEN.len();
        let Some(end) = text[content_start..].find(']') else {
            break;
        };
        let name = text[content_start..content_start + end].trim();
        if !name.is_empty() {
            found.push(SoundReference {
                name,
                span: start..content_start + end + 1,
            });
        }
        position = content_start + end + 1;
    }

    found
}

/// Все media-ссылки в HTML-значении поля по HTML-грамматике.
///
/// Учитываются только полные элементы: незакрытый тег элемента не создаёт, и
/// браузер по его атрибуту ничего не запрашивает.
#[must_use]
pub fn html_media_references(html: &str) -> Vec<MediaReference> {
    let mut found: Vec<MediaReference> = Vec::new();

    for tag in htmlscan::scan_tags(html) {
        let Tag::Element(element) = tag else {
            continue;
        };
        for attribute in &element.attributes {
            if !attribute_carries_media(element.name, attribute.name) {
                continue;
            }
            for value in split_attribute_values(attribute.name, attribute.value) {
                let value = value.text;
                if value.is_empty() {
                    continue;
                }
                found.push(MediaReference {
                    element: element.name.to_ascii_lowercase(),
                    attribute: attribute.name.to_ascii_lowercase(),
                    value,
                });
            }
        }
    }

    found
}

/// Ссылки на media, которые действительно видны браузеру: полные элементы и
/// `[sound:NAME]`.
///
/// Именно этот набор имеет смысл копировать и подставлять в отчёт: он отвечает
/// на вопрос «что запросит браузер», а не «что похоже на ссылку».
#[must_use]
pub fn visible_media_references(html: &str) -> Vec<String> {
    let mut found: Vec<String> = sound_references(html)
        .iter()
        .map(|reference| reference.name.to_string())
        .collect();
    found.extend(
        html_media_references(html)
            .into_iter()
            .map(|reference| reference.value),
    );
    found
}

/// Извлекает ссылки на media из HTML-значения поля.
///
/// Это гейт `media_forbidden` у `create`, поэтому политика **fail-closed**:
/// кроме того, что видит браузер ([`visible_media_references`]), ссылкой
/// считается незакрытая конструкция с media-атрибутом. Значение поля — не
/// документ: Anki склеивает его с шаблоном, и незакрытый тег может закрыться
/// уже там, превратившись в настоящий запрос файла.
///
/// Сам гейт спрашивает не её, а [`forbidden_media_references`]: та добавляет к
/// этому набору адреса из CSS.
#[must_use]
pub fn extract_media_references(text: &str) -> Vec<String> {
    let mut found = visible_media_references(text);

    for tag in htmlscan::scan_tags(text) {
        let Tag::Unterminated {
            name, attributes, ..
        } = tag
        else {
            continue;
        };
        for attribute in &attributes {
            if !attribute_carries_media(name, attribute.name) {
                continue;
            }
            for value in split_attribute_values(attribute.name, attribute.value) {
                let value = value.text;
                if value.is_empty() {
                    continue;
                }
                found.push(value);
            }
        }
    }

    found
}

/// Все ссылки на media, которые гейт `create` обязан отвергнуть.
///
/// Кроме ссылок в разметке ([`extract_media_references`]) сюда входят адреса из
/// CSS внутри значения поля: `url(…)` в `style`-атрибуте и в `<style>`, а также
/// `@import`. Причина та же, что у незакрытого тега: браузер пойдёт по этому
/// адресу за файлом, а значение поля Anki склеит с шаблоном и покажет в карточке.
/// Отдельного признака «это CSS, здесь можно» у ссылки нет: адрес есть адрес.
#[must_use]
pub fn forbidden_media_references(text: &str) -> Vec<String> {
    let mut found = extract_media_references(text);
    found.extend(css_media_references(text));
    found
}

/// Адреса, найденные в CSS внутри значения поля.
///
/// CSS встречается здесь ровно в двух записях: значение `style`-атрибута и тело
/// `<style>`. Находит их [`htmlscan`], а сами адреса называет
/// [`crate::report::css`] — тот же владелец политики адресов, что и у
/// `visual-report`, поэтому расхождение между гейтом и отчётом невозможно.
/// Незакрытый тег обрабатывается как та же запись атрибута: конструкция неполная,
/// но значение прочитано, и браузер прочитает его так же.
fn css_media_references(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for tag in htmlscan::scan_tags(text) {
        match tag {
            Tag::Element(element) => {
                for attribute in &element.attributes {
                    if !attribute.name.eq_ignore_ascii_case("style") {
                        continue;
                    }
                    found.extend(css_addresses(attribute.value));
                }
            }
            Tag::RawText { name, body, .. } => {
                if name.eq_ignore_ascii_case("style") {
                    found.extend(css_addresses(&text[body.clone()]));
                }
            }
            // CSS в незакрытом теге тоже читает браузер: значение поля склеится с
            // шаблоном, и атрибут закроется уже там.
            Tag::Unterminated { attributes, .. } => {
                for attribute in &attributes {
                    if !attribute.name.eq_ignore_ascii_case("style") {
                        continue;
                    }
                    found.extend(css_addresses(attribute.value));
                }
            }
        }
    }
    found
}

/// Адреса одного фрагмента CSS в записи владельца политики.
///
/// `@import` называется правилом целиком: адрес внутри него отдельным событием
/// не становится, а гейту важно назвать нарушение так, чтобы его можно было
/// найти в значении поля.
fn css_addresses(style: &str) -> Vec<String> {
    css::scan(style)
        .into_iter()
        .filter_map(|event| match event {
            css::Event::Address(address) => {
                let target = address.target.trim();
                (!target.is_empty()).then(|| target.to_string())
            }
            css::Event::Import(span) => Some(style[span].trim().to_string()),
        })
        .collect()
}

/// Отдельная ссылка внутри значения атрибута и её место в этом значении.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeValue {
    /// Текст ссылки.
    pub text: String,
    /// Байтовый диапазон ссылки внутри значения атрибута.
    ///
    /// Нужен тому, кто подменяет ссылку: замена на месте сохраняет остальную
    /// запись значения, включая дескрипторы `srcset`.
    pub range: Range<usize>,
    /// Байтовый диапазон всего кандидата внутри значения атрибута — вместе с
    /// дескрипторами `srcset` (`200w`, `2x`) и без запятой-разделителя.
    ///
    /// Нужен тому, кто убирает кандидата целиком: список пересобирается из
    /// кандидатов, и «выкинуть ссылку, оставив её дескриптор» — не то же самое,
    /// что выкинуть кандидата.
    pub segment: Range<usize>,
}

/// Разбирает значение атрибута на отдельные ссылки вместе с их местом.
///
/// У `srcset` их несколько («URL [дескриптор], …»), у остальных атрибутов —
/// одна. Значение без пробелов по краям обрезается: пробел вокруг ссылки не
/// делает её другим файлом. Место ссылки возвращается, чтобы подмена не была
/// отдельным разбором того же значения.
#[must_use]
pub fn split_attribute_values(attribute: &str, value: &str) -> Vec<AttributeValue> {
    if !attribute.eq_ignore_ascii_case("srcset") {
        let trimmed_start = value.len() - value.trim_start().len();
        let trimmed_end = value.trim_end().len();
        let text = value.trim();
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![AttributeValue {
                text: text.to_string(),
                range: trimmed_start..trimmed_end,
                segment: trimmed_start..trimmed_end,
            }]
        };
    }

    let mut found: Vec<AttributeValue> = Vec::new();
    let mut offset = 0usize;
    for candidate in value.split(',') {
        let start = offset;
        offset += candidate.len() + 1;
        let piece = candidate.trim();
        let Some(url) = piece.split_whitespace().next() else {
            continue;
        };
        let segment_start = start + (candidate.len() - candidate.trim_start().len());
        found.push(AttributeValue {
            text: url.to_string(),
            range: segment_start..segment_start + url.len(),
            segment: segment_start..segment_start + piece.len(),
        });
    }
    found
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn values(text: &str) -> Vec<String> {
        let mut found: Vec<String> = extract_media_references(text);
        found.sort();
        found
    }

    #[test]
    fn extracts_sound_and_src_references() {
        let text = r#"[sound:a.mp3]<img src="b.png">и <img src='c.gif'>"#;
        assert_eq!(values(text), vec!["a.mp3", "b.png", "c.gif"]);
    }

    /// Валидная HTML-грамматика не обходит гейт: регистр, пробелы вокруг `=`,
    /// unquoted-значения и одинарные кавычки — всё это те же ссылки.
    #[test]
    fn every_valid_attribute_spelling_is_a_reference() {
        for text in [
            r#"<img src="a.png">"#,
            r#"<img SRC="a.png">"#,
            r#"<IMG SRC="a.png">"#,
            r#"<img src = "a.png">"#,
            "<img\nsrc\t=\n\"a.png\">",
            "<img src= a.png>",
            "<img src=a.png>",
            r#"<img src='a.png'>"#,
            r#"<audio src="a.png">"#,
            r#"<video src="a.png">"#,
            r#"<source src="a.png">"#,
            r#"<embed src="a.png">"#,
            r#"<object data="a.png"></object>"#,
            r#"<source srcset="a.png">"#,
            r#"<video poster="a.png"></video>"#,
            r#"<table background="a.png">"#,
        ] {
            assert_eq!(values(text), vec!["a.png"], "разметка {text:?}");
        }

        let mut many = values(r#"<img srcset="a.png 1x, b.png 2x">"#);
        many.sort();
        assert_eq!(many, vec!["a.png", "b.png"]);
    }

    /// Значение поля — фрагмент, который Anki склеивает с шаблоном, поэтому
    /// незакрытая конструкция с media-атрибутом считается ссылкой (fail-closed):
    /// в документе её может закрыть следующий за ней текст.
    #[test]
    fn unterminated_media_construct_is_rejected_not_ignored() {
        assert_eq!(values(r#"<img src="a.png"#), vec!["a.png"]);
        assert_eq!(values(r#"<img src=a.png"#), vec!["a.png"]);
        assert_eq!(values(r#"<img src="a.png">"#), vec!["a.png"]);
        // Кавычка не закрылась: значение собрано до конца строки, и это ссылка.
        assert_eq!(values(r#"<img src="незакрытый>"#), vec!["незакрытый>"]);
    }

    /// Незакрытая конструкция не создаёт элемента в документе, поэтому
    /// browser-семантика её ссылкой не считает.
    #[test]
    fn visible_references_ignore_unterminated_constructs() {
        assert!(visible_media_references(r#"<img src="a.png"#).is_empty());
        assert!(visible_media_references(r#"<img src="незакрытый>"#).is_empty());
        assert_eq!(
            visible_media_references("[sound:без конца]"),
            vec!["без конца"]
        );
    }

    #[test]
    fn ignores_unrelated_src_and_unterminated_constructs() {
        assert!(extract_media_references("src=noquotes").is_empty());
        assert!(extract_media_references("[sound:без конца").is_empty());
        assert!(extract_media_references("обычный текст").is_empty());
        // `<img` без атрибутов — не ссылка, даже если тег не закрыт.
        assert!(extract_media_references("используй <img тег").is_empty());
        assert!(extract_media_references(r#"<img src="">"#).is_empty());
        // Комментарий не создаёт элемента и ссылкой не является.
        assert!(extract_media_references(r#"<!-- <img src="a.png"> -->"#).is_empty());
        // Регистр в имени атрибута, которого нет в таблице, значения не имеет.
        assert!(extract_media_references(r#"<img alt="a.png">"#).is_empty());
        // `src` вне разметки — обычный текст.
        assert!(extract_media_references("функция src=x в коде").is_empty());
    }

    #[test]
    fn media_reference_reports_where_it_was_found() {
        let found = html_media_references(r#"<IMG SRC = 'a.png'><object data="b.swf">"#);
        assert_eq!(
            found,
            vec![
                MediaReference {
                    element: "img".to_string(),
                    attribute: "src".to_string(),
                    value: "a.png".to_string(),
                },
                MediaReference {
                    element: "object".to_string(),
                    attribute: "data".to_string(),
                    value: "b.swf".to_string(),
                },
            ]
        );
    }

    #[test]
    fn sound_references_carry_their_span() {
        let text = "начало [sound: a.mp3 ] конец";
        let found = sound_references(text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "a.mp3");
        assert_eq!(&text[found[0].span.clone()], "[sound: a.mp3 ]");
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
