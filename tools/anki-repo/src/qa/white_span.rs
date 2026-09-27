//! Точечный поиск устаревших белых `<span>`-обёрток в значениях полей.
//!
//! Историческое оформление колоды делало часть текста невидимой для читателя
//! через белый цвет (`<span style="color: rgb(255, 255, 255);">`). Такая обёртка
//! — продуктовый дефект содержимого, а не структурная ошибка экспорта, поэтому
//! её ищет QA-правило, а не `validate`.
//!
//! Полноценный HTML/CSS-парсер здесь не нужен и намеренно не используется:
//! правило обязано быть точным и предсказуемым, поэтому разбирается ровно
//! синтаксис, который создаёт такой дефект — тег `<span>` и его атрибут `style`.
//! Всё остальное содержимое поля анализу не подвергается.

/// Результат сканирования одного значения поля.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WhiteSpanScan {
    /// Сколько раз в значении найден белый `color` внутри `<span>`.
    pub occurrences: usize,
    /// Текст первого найденного тега (ограничен по длине).
    pub first_tag: Option<String>,
}

/// Предел длины сохраняемого текста тега.
const TAG_SAMPLE_LIMIT: usize = 160;

/// Ищет `<span>`, у которого объявлен белый цвет текста.
#[must_use]
pub fn scan_white_spans(text: &str) -> WhiteSpanScan {
    let mut scan = WhiteSpanScan::default();
    let mut cursor = 0;
    while let Some(start) = find_span_tag(text, cursor) {
        let Some(end) = text[start..].find('>') else {
            break;
        };
        let tag_end = start + end + 1;
        let inner = &text[start..tag_end];
        // Внутренности тега после имени `<span`: имя уже проверено сканером
        // без учёта регистра, а его длина в байтах фиксирована.
        let attributes = &inner[5..];
        if style_is_white(attributes) {
            scan.occurrences += 1;
            if scan.first_tag.is_none() {
                scan.first_tag = Some(truncate_tag(inner));
            }
        }
        cursor = tag_end;
    }
    scan
}

/// Позиция следующего открывающего тега `<span`.
///
/// Сравнение идёт по байтам, а не по срезам строки: иначе многобайтовый символ
/// сразу после `<` мог бы оборвать обход и скрыть настоящий тег.
fn find_span_tag(text: &str, from: usize) -> Option<usize> {
    const NEEDLE: &[u8; 5] = b"<span";
    let bytes = text.as_bytes();
    let mut index = from;
    while index + NEEDLE.len() <= bytes.len() {
        if bytes[index..index + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE) {
            let following = text[index + NEEDLE.len()..].chars().next();
            // Дальше обязан идти разделитель: `<span>`, `<span …`, `<span/`.
            if following.is_none_or(|character| {
                character.is_ascii_whitespace() || character == '>' || character == '/'
            }) {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

/// Проверяет атрибуты тега `span` на `style` с белым цветом текста.
fn style_is_white(attributes: &str) -> bool {
    let mut rest = attributes;
    loop {
        rest = rest.trim_start();
        // Закрывающая часть тега: атрибутов больше нет.
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('>') {
            return false;
        }

        let (name, after_name) = split_name(rest);
        let after_name = after_name.trim_start();
        let (value, after_value) = if let Some(unquoted) = after_name.strip_prefix('=') {
            read_value(unquoted.trim_start())
        } else {
            (None, after_name)
        };

        if name.eq_ignore_ascii_case("style")
            && let Some(value) = value
            && declares_white_color(value)
        {
            return true;
        }

        if after_value.is_empty() {
            return false;
        }
        rest = after_value;
    }
}

/// Отделяет имя атрибута от остатка тега.
fn split_name(text: &str) -> (&str, &str) {
    let end = text
        .find(|character: char| {
            character.is_ascii_whitespace() || character == '=' || character == '/'
        })
        .unwrap_or(text.len());
    (&text[..end], &text[end..])
}

/// Читает значение атрибута: в кавычках или без них.
fn read_value(text: &str) -> (Option<&str>, &str) {
    let Some(quote) = text.chars().next().filter(|c| *c == '"' || *c == '\'') else {
        let end = text
            .find(|character: char| character.is_ascii_whitespace() || character == '>')
            .unwrap_or(text.len());
        return (Some(&text[..end]), &text[end..]);
    };
    let inner = &text[quote.len_utf8()..];
    match inner.find(quote) {
        Some(end) => (Some(&inner[..end]), &inner[end + quote.len_utf8()..]),
        None => (Some(inner), ""),
    }
}

/// Ищет `color`, значение которого эквивалентно белому.
///
/// `background-color` и прочие свойства с `color` в имени намеренно не
/// считаются совпадением: проверяется именно имя свойства `color`.
fn declares_white_color(style: &str) -> bool {
    style.split(';').any(|declaration| {
        let Some((property, value)) = declaration.split_once(':') else {
            return false;
        };
        property.trim().eq_ignore_ascii_case("color") && is_white_value(value)
    })
}

/// Распознаёт белый цвет в формах, встречающихся в содержимом колоды.
fn is_white_value(value: &str) -> bool {
    let mut value = value.trim().to_ascii_lowercase();
    if let Some(stripped) = value.strip_suffix("!important") {
        value = stripped.trim_end().to_string();
    }
    let compact: String = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();

    if matches!(compact.as_str(), "white" | "#fff" | "#ffffff") {
        return true;
    }

    let Some(arguments) = compact
        .strip_prefix("rgb(")
        .or_else(|| compact.strip_prefix("rgba("))
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };

    let components: Vec<&str> = arguments.split(',').collect();
    if components.len() < 3 {
        return false;
    }

    components[..3]
        .iter()
        .all(|component| *component == "255" || *component == "100%")
}

/// Ограничивает текст тега для evidence.
fn truncate_tag(tag: &str) -> String {
    let mut result: String = tag.chars().take(TAG_SAMPLE_LIMIT).collect();
    if tag.chars().count() > TAG_SAMPLE_LIMIT {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(text: &str) -> bool {
        scan_white_spans(text).occurrences > 0
    }

    #[test]
    fn canonical_form_is_found() {
        let text = r#"<span style="color: rgb(255, 255, 255);">невидимый</span>"#;
        let scan = scan_white_spans(text);
        assert_eq!(scan.occurrences, 1);
        assert_eq!(scan.first_tag.as_deref(), Some(text_tag(text)));
    }

    fn text_tag(text: &str) -> &str {
        &text[..text.find('>').expect("тег") + 1]
    }

    #[test]
    fn equivalent_forms_are_found() {
        let forms = [
            r#"<span style="color: rgb(255,255,255)">x</span>"#,
            r#"<span style="color:rgb(255, 255, 255);">x</span>"#,
            r#"<span style="color: rgba(255, 255, 255, 1);">x</span>"#,
            r#"<span style="COLOR: #FFFFFF;">x</span>"#,
            r#"<span style='color: #fff'>x</span>"#,
            r#"<span style="color: white;">x</span>"#,
            r#"<span style="color: WHITE !important;">x</span>"#,
            r#"<span style="color: rgb(100%, 100%, 100%);">x</span>"#,
            r#"<span style="font-size: 12px; color: rgb(255, 255, 255);">x</span>"#,
            r#"<span class="word-focus" style="color: rgb(255, 255, 255)">x</span>"#,
            r#"<span style = "color: rgb(255, 255, 255)" >x</span>"#,
            r#"<span style="color:rgb(255,255,255)" class="y">x</span>"#,
            r#"<SPAN STYLE="color:white">x</SPAN>"#,
            r#"<span style="color: white">x</span>"#,
        ];
        for form in forms {
            assert!(matches(form), "форма должна находиться: {form}");
        }
    }

    #[test]
    fn real_deck_colors_are_not_matches() {
        // Реальные значения колоды: 255 только в первом компоненте.
        let forms = [
            r#"<span style="color: rgb(255, 165, 0);"><b>の</b></span>"#,
            r#"<span style="color: rgb(170, 170, 127);"><b>が</b></span>"#,
            r#"<span style="color: #FFA500;">х</span>"#,
            r#"<span style="color: rgb(255,255,254)">х</span>"#,
            r#"<span style="color: #ffff">х</span>"#,
            r#"<span style="color: rgb(254, 255, 255)">х</span>"#,
            r#"<span style="color: rgb(255, 165, 0, 1)">х</span>"#,
            r#"<span style="background-color: rgb(255, 255, 255);">видимый</span>"#,
            r#"<span style="border-color: white;">х</span>"#,
            "255 255 255",
            "белый цвет 255, 255, 255",
            r#"<span>color: rgb(255, 255, 255)</span>"#,
            r#"<span class="color: white">х</span>"#,
            r#"<span data-style="color: white">х</span>"#,
            r#"<div style="color: rgb(255, 255, 255)">х</div>"#,
            r#"<span style="color">х</span>"#,
            r#"<span style="color: ">х</span>"#,
        ];
        for form in forms {
            assert!(!matches(form), "форма не должна находиться: {form}");
        }
    }

    #[test]
    fn occurrences_are_counted_and_first_tag_is_kept() {
        let text = concat!(
            r#"<span style="color: rgb(255, 165, 0);">норма</span>"#,
            r#"<span style="color: rgb(255, 255, 255);">раз</span>"#,
            r#"<span style="color: #fff;">два</span>"#,
        );
        let scan = scan_white_spans(text);
        assert_eq!(scan.occurrences, 2);
        assert!(
            scan.first_tag
                .as_deref()
                .is_some_and(|tag| tag.contains("rgb(255, 255, 255)")),
            "первым должен остаться первый найденный тег: {:?}",
            scan.first_tag
        );
    }

    #[test]
    fn unquoted_style_value_is_parsed() {
        // Некорректный, но встречающийся в природе HTML: кавычек нет.
        assert!(matches(r#"<span style=color:white>x</span>"#));
    }

    #[test]
    fn unterminated_tag_does_not_panic() {
        assert_eq!(
            scan_white_spans(r#"<span style="color: white"#).occurrences,
            0
        );
        assert_eq!(scan_white_spans("<span").occurrences, 0);
        assert_eq!(scan_white_spans("<").occurrences, 0);
        assert_eq!(scan_white_spans("").occurrences, 0);
        assert_eq!(scan_white_spans("span style=color:white").occurrences, 0);
    }

    #[test]
    fn tag_sample_is_bounded() {
        let long_style = "a".repeat(500);
        let text = format!(r#"<span style="color: white; {long_style}">х</span>"#);
        let scan = scan_white_spans(&text);
        assert_eq!(scan.occurrences, 1);
        let tag = scan.first_tag.expect("тег");
        assert!(tag.chars().count() <= TAG_SAMPLE_LIMIT + 1);
        assert!(tag.ends_with('…'));
    }
}
