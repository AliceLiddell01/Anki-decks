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
//! - не строятся дерево, каскад, entities и исправление ошибок вложенности:
//!   вопрос о ссылке в атрибуте от этого не зависит;
//! - незакрытый тег честно возвращается как [`Tag::Unterminated`]. По HTML5
//!   такой тег не порождает элемента (в браузерной семантике ссылки нет), но
//!   сканируемый фрагмент — это ещё не документ: если следом идёт текст с `>`,
//!   незакрытый тег в документе закроется. Поэтому решение «что с ним делать»
//!   принимает потребитель: гейт `create` считает такую конструкцию ссылкой
//!   (fail-closed), а санитайз отчёта делает её безвредным текстом.

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
                    let raw = is_raw_text(element.name) && !element.self_closing;
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
fn skip_declaration(html: &str, start: usize) -> usize {
    let after = start + 2;
    if html[after.min(html.len())..].starts_with("--") {
        if let Some(end) = html[after..].find("-->") {
            return after + end + 3;
        }
        return html.len();
    }
    skip_to_bracket(html, start + 1).map_or(html.len(), |end| end + 1)
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
            if name_end <= bytes.len()
                && html[name_start..name_end].eq_ignore_ascii_case(name)
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
}
