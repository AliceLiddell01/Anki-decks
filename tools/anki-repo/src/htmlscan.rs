//! Минимальный, но грамматически честный сканер HTML-тегов.
//!
//! В toolkit'е есть два вопроса, которые звучат по-разному, а решаться обязаны
//! одним и тем же ответом: «есть ли в этом значении поля ссылка на media» (гейт
//! `media_forbidden` у `create`) и «какая именно ссылка стоит в этом атрибуте и
//! где её заменить» (подстановка media и санитайз превью у `visual-report`).
//! Textual-поиск вроде `contains("src=")` на них отвечает неверно: HTML
//! ASCII case-insensitive в именах элементов и атрибутов, допускает пробелы
//! вокруг `=`, unquoted-значения, одинарные и двойные кавычки. Поэтому владелец
//! ответа один — этот модуль, а оба потребителя спрашивают его.
//!
//! Что здесь сознательно поддержано, а что нет:
//!
//! - поддержаны открывающие теги, комментарии, `<!doctype …>`, `<?…?>` и
//!   raw-text содержимое `script`, `style`, `textarea`, `title`;
//! - не строятся дерево, каскад и исправление ошибок вложенности: вопрос о ссылке
//!   в атрибуте от этого не зависит. Символьные ссылки разбираются точечно —
//!   [`decoded_attribute_value`] и только для значения атрибута, потому что
//!   браузер раскодирует их именно там, а адрес обязан читаться так, как его
//!   увидит браузер;
//! - незакрытый тег честно возвращается как [`Tag::Unterminated`]. По HTML5
//!   такой тег не порождает элемента (в браузерной семантике ссылки нет), но
//!   сканируемый фрагмент — это ещё не документ: если следом идёт текст с `>`,
//!   незакрытый тег в документе закроется. Поэтому решение «что с ним делать»
//!   принимает потребитель: гейт `create` считает такую конструкцию ссылкой
//!   (fail-closed), а санитайз отчёта делает её безвредным текстом.

use std::borrow::Cow;
use std::ops::Range;

/// Пробельные символы, которые HTML считает разделителями атрибутов.
#[must_use]
pub fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// Один атрибут открывающего тега.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute<'a> {
    /// Имя атрибута ровно так, как оно записано в источнике.
    pub name: &'a str,
    /// Значение без кавычек; у атрибута без `=значение` — пустая строка.
    pub value: &'a str,
    /// Байтовый диапазон значения в источнике (без кавычек).
    ///
    /// `None`, если у атрибута вовсе нет `=`: значение такого атрибута пустое по
    /// семантике, но заменять в источнике нечего.
    pub value_range: Option<Range<usize>>,
    /// Байтовый диапазон всего атрибута: имя, `=` и значение.
    pub span: Range<usize>,
}

impl Attribute<'_> {
    /// Имя атрибута без учёта ASCII-регистра.
    #[must_use]
    pub fn name_is(&self, expected: &str) -> bool {
        self.name.eq_ignore_ascii_case(expected)
    }
}

/// Полный открывающий тег.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element<'a> {
    /// Имя элемента ровно так, как оно записано в источнике.
    pub name: &'a str,
    /// Индекс `<`.
    pub start: usize,
    /// Индекс закрывающей `>`.
    pub end: usize,
    /// Записан ли тег как самозакрывающийся (`/>`).
    pub self_closing: bool,
    /// Атрибуты в порядке записи.
    pub attributes: Vec<Attribute<'a>>,
}

impl<'a> Element<'a> {
    /// Имя элемента без учёта ASCII-регистра.
    #[must_use]
    pub fn name_is(&self, expected: &str) -> bool {
        self.name.eq_ignore_ascii_case(expected)
    }

    /// Диапазон всего тега вместе со скобками.
    #[must_use]
    pub fn span(&self) -> Range<usize> {
        self.start..self.end + 1
    }

    /// Первый атрибут с этим именем (без учёта ASCII-регистра).
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<&Attribute<'a>> {
        self.attributes
            .iter()
            .find(|attribute| attribute.name_is(name))
    }
}

/// Найденная в HTML конструкция.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tag<'a> {
    /// Полный открывающий тег.
    Element(Element<'a>),
    /// Raw-text содержимое элемента `script`, `style`, `textarea`, `title`.
    ///
    /// Внутри такого тела `<` не начинает тег, поэтому байты тела не
    /// сканируются: их владелец — сам элемент. Именно так в HTML работает
    /// JavaScript и CSS, и именно поэтому `"<div>"` внутри `script` не является
    /// элементом, а `</style>` внутри CSS закрывает таблицу стилей.
    RawText {
        /// Имя открывающего элемента.
        name: &'a str,
        /// Индекс `<` открывающего тега.
        start: usize,
        /// Диапазон тела между `>` открывающего тега и закрывающим тегом.
        body: Range<usize>,
        /// Позиция `>` закрывающего тега, если он в источнике есть.
        close_end: Option<usize>,
    },
    /// Тег начался, но до конца строки не закрылся.
    ///
    /// По HTML5 такой тег не порождает элемента. Attributes содержит уже
    /// разобранную часть: потребителю, которому нужен fail-closed, она нужна.
    Unterminated {
        /// Имя элемента ровно так, как оно записано в источнике.
        name: &'a str,
        /// Индекс `<`.
        start: usize,
        /// Разобранные атрибуты незакрытого тега.
        attributes: Vec<Attribute<'a>>,
    },
}

impl Tag<'_> {
    /// Имя элемента без учёта ASCII-регистра.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Element(element) => element.name,
            Self::RawText { name, .. } | Self::Unterminated { name, .. } => name,
        }
    }

    /// Является ли конструкция элементом с этим именем.
    #[must_use]
    pub fn is_element(&self, expected: &str) -> bool {
        self.name().eq_ignore_ascii_case(expected)
    }
}

/// Элементы, содержимое которых HTML разбирает как raw text (или RCDATA).
#[must_use]
pub fn is_raw_text(name: &str) -> bool {
    ["script", "style", "textarea", "title"]
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// Символьные ссылки HTML, которые этот разбор понимает.
///
/// Только те, что встречаются в данных: `&amp;` в CSS вполне возможен, а таблица
/// из двух тысяч имён — уже не разбор адреса, а словарь. Остальные ссылки
/// попадают в [`Reference::Unknown`], а «не понял» здесь означает отказ.
const NAMED_REFERENCES: &[(&str, char)] = &[
    ("amp", '&'),
    ("lt", '<'),
    ("gt", '>'),
    ("quot", '"'),
    ("apos", '\''),
];

/// Предел длины имени ссылки: `&#x10FFFF;` и самое длинное имя с запасом.
const MAX_REFERENCE_BYTES: usize = 32;

/// Значение атрибута так, как его увидит браузер.
///
/// Браузер раскодирует символьные ссылки в значении атрибута, поэтому адрес может
/// быть записан как `url&#40;a.png&#41;`: в исходном тексте адреса не видно, а в
/// браузере он есть. Тот, кто ищет адрес, обязан смотреть на это значение, а не на
/// исходный текст.
///
/// `None` — ссылка, которой этот разбор не знает: она может значить что угодно, и
/// «не понял» здесь означает отказ, а не пропуск. Одиночный `&` ссылкой не
/// считается и остаётся собой: `content:'a & b'` — это данные, а не адрес. По той
/// же причине не дочитывается и `&amp` без `;` — запись, которую браузер понимает
/// только в устаревшей форме.
#[must_use]
pub fn decoded_attribute_value(value: &str) -> Option<Cow<'_, str>> {
    if !value.contains('&') {
        return Some(Cow::Borrowed(value));
    }

    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(offset) = rest.find('&') {
        out.push_str(&rest[..offset]);
        rest = &rest[offset..];
        match decode_reference(rest) {
            Reference::Decoded { symbol, len } => {
                out.push(symbol);
                rest = &rest[len..];
            }
            Reference::Literal => {
                out.push('&');
                rest = &rest[1..];
            }
            Reference::Unknown => return None,
        }
    }
    out.push_str(rest);

    Some(Cow::Owned(out))
}

/// Что оказалось на месте `&`.
enum Reference {
    /// Ссылка, значение которой известно.
    Decoded { symbol: char, len: usize },
    /// `&` без ссылки: остаётся собой.
    Literal,
    /// Ссылка, значение которой разбор не знает.
    Unknown,
}

/// Разбирает символьную ссылку в начале `text`.
fn decode_reference(text: &str) -> Reference {
    debug_assert!(text.starts_with('&'));
    let bytes = text.as_bytes();

    let mut end = 1;
    while end < bytes.len()
        && end - 1 < MAX_REFERENCE_BYTES
        && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'#')
    {
        end += 1;
    }
    if bytes.get(end) != Some(&b';') {
        return if end == 1 {
            Reference::Literal
        } else {
            Reference::Unknown
        };
    }

    let len = end + 1;
    let body = &text[1..end];
    if body.is_empty() || matches!(body, "#" | "#x" | "#X") {
        // `&;`: ссылки здесь нет, `&` остаётся собой.
        return Reference::Literal;
    }
    let symbol = match body.strip_prefix('#') {
        Some(digits) => decode_numeric(digits),
        None => NAMED_REFERENCES
            .iter()
            .find(|(name, _)| *name == body)
            .map(|(_, symbol)| *symbol),
    };

    match symbol {
        Some(symbol) => Reference::Decoded { symbol, len },
        None => Reference::Unknown,
    }
}

/// Числовая ссылка `&#NNN;` или `&#xHHH;` как символ.
fn decode_numeric(digits: &str) -> Option<char> {
    let (radix, digits) = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => (16, hex),
        None => (10, digits),
    };
    if digits.is_empty() {
        return None;
    }
    // Символ подстановки, которым HTML5 заменяет негодную ссылку, здесь не
    // выдумывается: негодная ссылка — это не понятая ссылка, то есть отказ.
    let code = u32::from_str_radix(digits, radix).ok()?;
    if code == 0 {
        return None;
    }
    char::from_u32(code)
}

/// Разбирает HTML на последовательность найденных конструкций.
#[must_use]
pub fn scan_tags(html: &str) -> Vec<Tag<'_>> {
    let bytes = html.as_bytes();
    let mut tags: Vec<Tag<'_>> = Vec::new();
    let mut position = 0usize;

    while position < bytes.len() {
        let Some(offset) = html[position..].find('<') else {
            break;
        };
        let start = position + offset;
        let next = start + 1;

        match bytes.get(next) {
            // Комментарий или декларация: тег внутри них не начинается.
            Some(b'!') => {
                position = skip_declaration(html, start);
            }
            Some(b'?') => {
                position = skip_to_bracket(html, next).map_or(bytes.len(), |end| end + 1);
            }
            // Закрывающий тег: сам по себе интереса не представляет.
            Some(b'/') => {
                position = skip_to_bracket(html, next).map_or(bytes.len(), |end| end + 1);
            }
            Some(byte) if byte.is_ascii_alphabetic() => match parse_start_tag(html, start) {
                ParsedTag::Complete(element) => {
                    // Raw text — свойство имени элемента, а не записи тега: HTML5
                    // игнорирует `/` у непустых элементов, поэтому `<script/>`
                    // открывает `script`, а не закрывает его. Признать такой тег
                    // обычным элементом значило бы разбирать его тело как разметку:
                    // браузер увидел бы в ней теги, а разбор — текст.
                    let raw = is_raw_text(element.name);
                    let body_start = element.end + 1;
                    if raw {
                        let (body_end, close_end) = raw_text_range(html, element.name, body_start);
                        tags.push(Tag::RawText {
                            name: element.name,
                            start: element.start,
                            body: body_start..body_end,
                            close_end,
                        });
                        position = close_end.map_or(bytes.len(), |end| end + 1);
                    } else {
                        tags.push(Tag::Element(element));
                        position = body_start;
                    }
                }
                ParsedTag::Unterminated { name, attributes } => {
                    tags.push(Tag::Unterminated {
                        name,
                        start,
                        attributes,
                    });
                    break;
                }
            },
            // Одиночный `<` в тексте тегом не является.
            _ => {
                position = next;
            }
        }
    }

    tags
}

/// Результат разбора одного открывающего тега.
enum ParsedTag<'a> {
    /// Тег закрылся.
    Complete(Element<'a>),
    /// Тег не закрылся до конца строки.
    Unterminated {
        /// Имя элемента.
        name: &'a str,
        /// Успевшие разобраться атрибуты.
        attributes: Vec<Attribute<'a>>,
    },
}

/// Разбирает открывающий тег начиная с `<`.
fn parse_start_tag<'a>(html: &'a str, start: usize) -> ParsedTag<'a> {
    let bytes = html.as_bytes();
    let mut position = start + 1;

    let name_start = position;
    while position < bytes.len() && is_name_byte(bytes[position]) {
        position += 1;
    }
    let name = &html[name_start..position];

    let mut attributes: Vec<Attribute<'a>> = Vec::new();

    loop {
        let step_start = position;

        while position < bytes.len() && is_whitespace(bytes[position]) {
            position += 1;
        }

        let Some(byte) = bytes.get(position).copied() else {
            return ParsedTag::Unterminated { name, attributes };
        };

        if byte == b'>' {
            return ParsedTag::Complete(Element {
                name,
                start,
                end: position,
                self_closing: false,
                attributes,
            });
        }

        if byte == b'/' {
            if bytes.get(position + 1) == Some(&b'>') {
                return ParsedTag::Complete(Element {
                    name,
                    start,
                    end: position + 1,
                    self_closing: true,
                    attributes,
                });
            }
            position += 1;
            continue;
        }

        let attribute_start = position;
        let attribute_name_start = position;
        while position < bytes.len()
            && !is_whitespace(bytes[position])
            && bytes[position] != b'='
            && bytes[position] != b'>'
            && bytes[position] != b'/'
        {
            position += 1;
        }
        let attribute_name = &html[attribute_name_start..position];

        let mut value: &str = "";
        let mut value_range: Option<Range<usize>> = None;
        // Незакрытая кавычка значения: тег не порождает элемента, но уже
        // собранное значение важно потребителю с fail-closed.
        let mut unterminated_value = false;

        let mut after_name = position;
        while after_name < bytes.len() && is_whitespace(bytes[after_name]) {
            after_name += 1;
        }

        if bytes.get(after_name) == Some(&b'=') {
            let mut value_start = after_name + 1;
            while value_start < bytes.len() && is_whitespace(bytes[value_start]) {
                value_start += 1;
            }

            let Some(byte) = bytes.get(value_start).copied() else {
                return ParsedTag::Unterminated { name, attributes };
            };

            match byte {
                b'"' | b'\'' => {
                    let quote = byte;
                    let content_start = value_start + 1;
                    let mut end = content_start;
                    // Незакрытая кавычка — это `eof-in-tag`: тега в документе не
                    // будет, и это честно называется, а не чинится догадкой.
                    while end < bytes.len() && bytes[end] != quote {
                        end += 1;
                    }
                    value = &html[content_start..end];
                    value_range = Some(content_start..end);
                    if end >= bytes.len() {
                        unterminated_value = true;
                        position = end;
                    } else {
                        position = end + 1;
                    }
                }
                b'>' => {
                    // `attr=>`: значение пустое, а тег на этом заканчивается.
                    value_range = Some(value_start..value_start);
                    position = value_start;
                }
                _ => {
                    let mut end = value_start;
                    while end < bytes.len() && !is_whitespace(bytes[end]) && bytes[end] != b'>' {
                        end += 1;
                    }
                    value = &html[value_start..end];
                    value_range = Some(value_start..end);
                    position = end;
                }
            }
        }

        if !attribute_name.is_empty() {
            attributes.push(Attribute {
                name: attribute_name,
                value,
                value_range,
                span: attribute_start..position,
            });
        }

        if unterminated_value {
            return ParsedTag::Unterminated { name, attributes };
        }

        // Гарантия прогресса: нераспознанный байт не должен зациклить разбор.
        if position == step_start {
            position += 1;
        }
    }
}

/// Байты, допустимые в имени элемента или атрибута для наших целей.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':')
}

/// Пропускает `<!-- … -->`, `<!doctype …>` и любую другую декларацию.
///
/// Границы комментария берутся из состояний комментария HTML5, а не из поиска
/// `-->`: браузер закрывает комментарий и на `--!>`, и сразу на `<!-->`, и на
/// `<!--->`. Расхождение здесь — это разметка, которую браузер видит, а разбор
/// нет: текст после раннего закрытия он считал бы комментарием, а браузер
/// исполнял бы как теги.
fn skip_declaration(html: &str, start: usize) -> usize {
    let after = start + 2;
    if !html[after.min(html.len())..].starts_with("--") {
        return skip_to_bracket(html, start + 1).map_or(html.len(), |end| end + 1);
    }

    // Пустой комментарий закрывается сразу: `<!-->` — первым `>`, `<!--->` — им же
    // после `-`.
    let body = (start + 4).min(html.len());
    if html[body..].starts_with('>') {
        return body + 1;
    }
    if html[body..].starts_with("->") {
        return body + 2;
    }

    match comment_end(html, body) {
        Some(end) => end,
        None => html.len(),
    }
}

/// Позиция сразу после `-->` или `--!>`, если они есть в остатке.
fn comment_end(html: &str, from: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut index = from;
    while index + 1 < bytes.len() {
        if bytes[index] == b'-' && bytes[index + 1] == b'-' {
            match bytes.get(index + 2) {
                Some(b'>') => return Some(index + 3),
                Some(b'!') if bytes.get(index + 3) == Some(&b'>') => return Some(index + 4),
                // `--!` без `>` комментарий не закрывает: HTML5 возвращается в
                // состояние комментария, и разбор обязан искать дальше.
                _ => {}
            }
        }
        index += 1;
    }
    None
}

/// Индекс ближайшей `>` начиная с `from`.
fn skip_to_bracket(html: &str, from: usize) -> Option<usize> {
    if from > html.len() {
        return None;
    }
    html[from..].find('>').map(|offset| from + offset)
}

/// Находит конец raw-text тела и позицию `>` закрывающего тега.
fn raw_text_range(html: &str, name: &str, body_start: usize) -> (usize, Option<usize>) {
    let bytes = html.as_bytes();
    let mut position = body_start;
    while position < bytes.len() {
        let Some(offset) = html[position..].find('<') else {
            break;
        };
        let candidate = position + offset;
        let after = candidate + 1;
        if bytes.get(after) == Some(&b'/') {
            let name_start = after + 1;
            let name_end = name_start + name.len();
            // Сравниваются байты, а не срез строки: `name_end` мог оказаться
            // внутри многобайтного символа, и срез строки по такому индексу
            // паникует — на значении поля вроде `</ыыы` разбор обрывался бы не
            // отказом, а паникой всего процесса.
            if name_end <= bytes.len()
                && bytes[name_start..name_end].eq_ignore_ascii_case(name.as_bytes())
                && bytes
                    .get(name_end)
                    .is_none_or(|byte| is_whitespace(*byte) || *byte == b'>' || *byte == b'/')
            {
                let close_end = skip_to_bracket(html, name_end);
                return (candidate, close_end);
            }
        }
        position = candidate + 1;
    }
    (html.len(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elements(html: &str) -> Vec<Element<'_>> {
        scan_tags(html)
            .into_iter()
            .filter_map(|tag| match tag {
                Tag::Element(element) => Some(element),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn attribute_names_and_values_are_read_regardless_of_spelling() {
        for html in [
            "<img src=\"a.png\">",
            "<img SRC=\"a.png\">",
            "<IMG SRC = \"a.png\">",
            "<img src = 'a.png'>",
            "<img\nsrc\t=\n\"a.png\">",
            "<img src=a.png>",
            "<img src= a.png>",
        ] {
            let found = elements(html);
            assert_eq!(found.len(), 1, "тег в {html:?}");
            let attribute = found[0].attribute("src").expect("src");
            assert_eq!(attribute.value, "a.png", "значение в {html:?}");
            assert!(found[0].name_is("img"));
        }
    }

    #[test]
    fn quoted_values_may_contain_brackets() {
        let found = elements(r#"<img alt="a > b" src="c.png">"#);
        assert_eq!(found[0].attribute("src").expect("src").value, "c.png");
    }

    #[test]
    fn comments_and_declarations_are_skipped() {
        let found = elements("<!doctype html><!-- <img src=\"a.png\"> --><p>x</p>");
        assert_eq!(found.len(), 1, "внутри комментария разметки нет: {found:?}");
        assert!(found[0].name_is("p"));
    }

    #[test]
    fn a_lone_angle_bracket_is_not_a_tag() {
        assert!(elements("a < b и 1 < 2").is_empty());
    }

    #[test]
    fn unterminated_tag_is_named_not_guessed() {
        let tags = scan_tags("<img src=\"a.png\"");
        assert!(
            matches!(tags.as_slice(), [Tag::Unterminated { .. }]),
            "{tags:?}"
        );
        let tags = scan_tags("<img src=\"незакрытый>");
        assert!(matches!(tags.as_slice(), [Tag::Unterminated { .. }]));
    }

    /// У незакрытого тега частично собранное значение обязано сохраниться:
    /// потребитель с fail-closed решает по нему, а не по пустоте.
    #[test]
    fn unterminated_tag_keeps_the_partial_value() {
        let tags = scan_tags("<img src=\"a.png");
        let Tag::Unterminated { attributes, .. } = &tags[0] else {
            panic!("{tags:?}");
        };
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].value, "a.png");

        let tags = scan_tags("<img src=a.png");
        let Tag::Unterminated { attributes, .. } = &tags[0] else {
            panic!("{tags:?}");
        };
        assert_eq!(attributes[0].value, "a.png");
    }

    #[test]
    fn raw_text_body_is_not_scanned_as_markup() {
        let tags = scan_tags("<style>.a{}</style><img src=\"a.png\">");
        assert!(
            matches!(&tags[0], Tag::RawText { name, .. } if name.eq_ignore_ascii_case("style"))
        );
        assert!(matches!(tags[1], Tag::Element(ref element) if element.name_is("img")));

        let tags = scan_tags("<script>var x = \"<img src='a.png'>\";</script>");
        assert_eq!(tags.len(), 1, "внутри script разметки нет: {tags:?}");
        assert!(matches!(tags[0], Tag::RawText { .. }));
    }

    #[test]
    fn raw_text_element_without_close_swallows_the_rest() {
        let tags = scan_tags("<style>body{color:red}");
        assert_eq!(tags.len(), 1);
        assert!(matches!(
            tags[0],
            Tag::RawText {
                close_end: None,
                ..
            }
        ));
    }

    #[test]
    fn self_closing_and_valueless_attributes_are_kept() {
        let found = elements("<img src=\"a.png\" hidden />");
        let element = &found[0];
        assert!(element.self_closing);
        let hidden = element.attribute("hidden").expect("hidden");
        assert_eq!(hidden.value, "");
        assert!(hidden.value_range.is_none());
        assert_eq!(element.attribute("src").expect("src").value, "a.png");
    }

    #[test]
    fn value_ranges_point_into_the_source() {
        let html = "<img src=\"a.png\">";
        let found = elements(html);
        let range = found[0].attribute("src").expect("src").value_range.clone();
        let range = range.expect("диапазон");
        assert_eq!(&html[range], "a.png");
    }

    #[test]
    fn scanner_always_terminates_on_hostile_input() {
        for html in [
            "<", "<a", "<a ", "<a =", "<a = ", "<a ===", "<a/", "<a/ ", "<a href", "<!--", "<!",
            "<?", "</", "<a b='",
        ] {
            let _ = scan_tags(html);
        }
    }

    /// Многобайтный символ сразу после `</` не роняет разбор.
    ///
    /// Граница имени закрывающего тега попадает внутрь символа: срез строки по
    /// такому индексу паникует, а сравнение байтов — нет. Значение поля приходит
    /// из экспорта, поэтому паника здесь — отказ всего процесса на чужих данных.
    #[test]
    fn a_multibyte_character_after_a_close_marker_is_not_a_panic() {
        for html in [
            "<style></ыыы",
            "<style>a</ыыыыы",
            "<script></日本語日本語",
            "<style>a</ы>",
        ] {
            let _ = scan_tags(html);
        }
    }

    /// `/` у непустого элемента HTML5 игнорирует: `<script/>` открывает `script`.
    ///
    /// Если такой тег счесть обычным элементом, тело разберётся как разметка, а
    /// браузер прочитает его как данные скрипта. Расхождение направлено ровно в
    /// одну сторону: разбор видит теги там, где браузер видит код.
    #[test]
    fn a_self_closing_script_is_still_raw_text() {
        let html = "<script/>alert(1)</script><p>x</p>";
        let tags = scan_tags(html);
        assert_eq!(tags.len(), 2, "тело script — не разметка: {tags:?}");

        let Tag::RawText { name, body, .. } = &tags[0] else {
            panic!("первый тег — raw-text script, а не {tags:?}");
        };
        assert!(name.eq_ignore_ascii_case("script"));
        assert_eq!(&html[body.clone()], "alert(1)");
        assert!(matches!(&tags[1], Tag::Element(element) if element.name_is("p")));
    }

    /// Комментарий кончается там, где его закрывает браузер, а не там, где рядом `-->`.
    #[test]
    fn a_comment_ends_where_the_browser_ends_it() {
        for (html, expected) in [
            // `--!>` закрывает комментарий.
            ("<!--a--!><img src=\"a.png\">", "img"),
            // Пустой комментарий закрывается сразу.
            ("<!--><img src=\"a.png\">", "img"),
            ("<!---><img src=\"a.png\">", "img"),
            // `--!` без `>` комментария не закрывает: разметки здесь ещё нет.
            ("<!--a--!b--><img src=\"a.png\">", "img"),
            // Вложенная запись комментарием не становится.
            ("<!-- <!-- --><img src=\"a.png\">", "img"),
        ] {
            let found = elements(html);
            assert_eq!(found.len(), 1, "разметка в {html:?}: {found:?}");
            assert!(found[0].name_is(expected), "тег в {html:?}: {found:?}");
        }
    }

    /// Незакрытый комментарий не порождает разметки: браузер тоже её не увидит.
    #[test]
    fn an_unterminated_comment_swallows_the_rest() {
        assert!(elements("<!-- <img src=\"a.png\">").is_empty());
        assert!(elements("<!--a--! <img src=\"a.png\">").is_empty());
    }

    /// Символьные ссылки в значении атрибута читаются так, как их видит браузер.
    #[test]
    fn a_reference_in_an_attribute_value_is_decoded() {
        let decoded = |value: &str| decoded_attribute_value(value).map(|value| value.into_owned());

        assert_eq!(decoded("url(a.png)").as_deref(), Some("url(a.png)"));
        assert_eq!(decoded("url&#40;a.png&#41;").as_deref(), Some("url(a.png)"));
        assert_eq!(
            decoded("url&#x28;a.png&#x29;").as_deref(),
            Some("url(a.png)")
        );
        assert_eq!(decoded("a&amp;b").as_deref(), Some("a&b"));
        assert_eq!(decoded("&lt;&gt;&quot;&apos;").as_deref(), Some("<>\"'"));
        // `&` в данных ссылкой не считается.
        assert_eq!(
            decoded("content:'a & b'").as_deref(),
            Some("content:'a & b'")
        );
        assert_eq!(decoded("&").as_deref(), Some("&"));
        assert_eq!(decoded("a &; b").as_deref(), Some("a &; b"));

        // Ссылка, которой разбор не знает, — отказ, а не пропуск: `&lpar;` вполне
        // может быть `(`, а `&#x0;` браузер заменяет подстановкой.
        assert_eq!(decoded("&lpar;"), None);
        assert_eq!(decoded("&amp"), None);
        assert_eq!(decoded("&#x0;"), None);
        assert_eq!(decoded("&#xD800;"), None);
        assert_eq!(decoded("&#1114112;"), None);
    }
}
