//! CSS как граница доверия: tokenization вместо поиска подстроки.
//!
//! Поиск `url(` по тексту отвечает на вопрос «есть ли здесь запрос» неверно, и
//! ошибается он только в одну сторону — в сторону пропуска. CSS не различает
//! регистр в именах функций и ключевых слов и разрешает escape-последовательности
//! в любом идентификаторе: `u\72l(https://…)`, `URL(…)` и `url(https://…)` — это
//! один и тот же `url()`, который браузер исполнит. Поиск подстроки `@import`
//! ломается так же: `@im\70 ort` — валидный at-keyword с именем `import`.
//!
//! Поэтому владелец ответа на вопрос «какие адреса и at-rule здесь есть» — этот
//! модуль, и он разбирает CSS лексически, а не ищет в нём текст:
//!
//! - идентификаторы и at-keywords читаются вместе с escape-последовательностями
//!   (`\72`, `\000072`, `\40 `) и сравниваются по декодированному имени;
//! - комментарии и пробелы пропускаются там, где их допускает грамматика CSS;
//! - поддерживаются обе формы адреса в `url()` — без кавычек и в кавычках, — и
//!   escape-последовательности внутри самого адреса декодируются: `https\3a //…`
//!   это `https://…`, а не путь;
//! - строка, которая является аргументом несущей адрес функции (`image-set`,
//!   `-webkit-image-set`, `image`) или значением дескриптора `src` в `@font-face`,
//!   тоже считается адресом: браузер запрашивает её так же, как `url()`;
//! - незакрытый `url(` поглощает остаток: разметка непонятна, и «не понял»
//!   означает «нейтрализовать», а не «пропустить».
//!
//! Модуль ничего не решает про локальность адреса: он отвечает только «вот
//! конструкция и вот её декодированный адрес». Разрешать или нейтрализовать —
//! дело вызывающего, у которого есть точный набор файлов отчёта.
//!
//! Модуль не строит дерево и не вычисляет каскад: ни то, ни другое не меняет
//! ответ на вопрос, куда браузер пойдёт за файлом. Он также не является полным
//! CSS-парсером и не пытается им быть: он лексический, и всё, что он не смог
//! разобрать однозначно, он называет адресом, а не пропускает.

use std::ops::Range;

/// Как записан адрес в CSS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressForm {
    /// `url(…)`, включая запись кавычками и escape-последовательности в имени.
    Url,
    /// Строка в позиции, где браузер запрашивает файл: аргумент `image-set()` или
    /// значение `src` в `@font-face`.
    String,
}

/// Адрес, найденный в CSS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    /// Диапазон конструкции, которую нужно нейтрализовать.
    pub span: Range<usize>,
    /// Декодированный адрес: escape-последовательности уже развёрнуты.
    pub target: String,
    /// Как адрес записан в источнике.
    pub form: AddressForm,
}

/// Найденный в CSS элемент политики.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `@import` в любой записи: правило целиком, включая точку с запятой.
    Import(Range<usize>),
    /// Адрес, за которым браузер пошёл бы.
    Address(Address),
}

impl Event {
    /// Диапазон, который событие занимает в источнике.
    #[must_use]
    pub fn span(&self) -> Range<usize> {
        match self {
            Self::Import(span) => span.clone(),
            Self::Address(address) => address.span.clone(),
        }
    }
}

/// Разбирает CSS и возвращает события политики по возрастанию позиции.
///
/// События не пересекаются: `@import` поглощает своё правило целиком, поэтому
/// адрес внутри него отдельным событием не становится — правило и так удаляется.
#[must_use]
pub fn scan(style: &str) -> Vec<Event> {
    let mut scanner = Scanner {
        style,
        pos: 0,
        events: Vec::new(),
        functions: Vec::new(),
        blocks: Vec::new(),
        pending_at_rule: None,
        previous: None,
        before_previous: None,
    };
    scanner.run();
    scanner.events.sort_by_key(|event| event.span().start);
    scanner.events
}

/// Функции, строковый аргумент которых браузер запрашивает как файл.
const URL_CARRYING_FUNCTIONS: &[&str] = &["image-set", "-webkit-image-set", "image"];

/// Функция, аргумент которой адресом не является, даже если это строка.
const LOCAL_ONLY_FUNCTIONS: &[&str] = &["local"];

fn is_url_carrying_function(name: &str) -> bool {
    URL_CARRYING_FUNCTIONS.contains(&name)
}

fn is_local_only_function(name: &str) -> bool {
    LOCAL_ONLY_FUNCTIONS.contains(&name)
}

struct Scanner<'a> {
    style: &'a str,
    pos: usize,
    events: Vec<Event>,
    /// Стек имён открытых функций.
    functions: Vec<String>,
    /// Стек блоков `{…}`: имя at-rule, который блок открыл, или пустая строка.
    blocks: Vec<String>,
    /// Последний at-keyword перед `{`, `;` или `}`.
    pending_at_rule: Option<String>,
    /// Два последних значимых токена — для распознавания `src: "…"`.
    previous: Option<String>,
    before_previous: Option<String>,
}

impl Scanner<'_> {
    fn run(&mut self) {
        while self.pos < self.style.len() {
            let byte = self.byte(self.pos);
            match byte {
                b'/' if self.peek(1) == Some(b'*') => {
                    self.skip_comment();
                }
                byte if is_whitespace(byte) => {
                    self.pos += 1;
                }
                b'@' => self.read_at_keyword(),
                b'"' | b'\'' => self.read_string_token(),
                b'\\' if self.would_start_identifier(self.pos) => self.read_ident_like(),
                b'{' => {
                    self.blocks
                        .push(self.pending_at_rule.take().unwrap_or_default());
                    self.pos += 1;
                    self.note("block-start");
                }
                b'}' => {
                    self.blocks.pop();
                    self.pending_at_rule = None;
                    self.pos += 1;
                    self.note("block-end");
                }
                b';' => {
                    self.pending_at_rule = None;
                    self.pos += 1;
                    self.note("semicolon");
                }
                b')' => {
                    self.functions.pop();
                    self.pos += 1;
                    self.note("close-paren");
                }
                b':' => {
                    self.pos += 1;
                    self.note(":");
                }
                _ if self.would_start_identifier(self.pos) => self.read_ident_like(),
                _ => {
                    self.pos += self.char_len(self.pos);
                    self.note("other");
                }
            }
        }
    }

    /// `@` и имя at-keyword: `@import`, `@im\70 ort` — одно и то же.
    fn read_at_keyword(&mut self) {
        let start = self.pos;
        self.pos += 1;
        let (name, end) = read_identifier(self.style, self.pos);
        self.pos = end;
        let name = name.to_ascii_lowercase();
        if name == "import" {
            let end = self.end_of_at_rule();
            self.events.push(Event::Import(start..end));
            self.pos = end;
            self.pending_at_rule = Some(name.clone());
            self.note("at-rule");
            return;
        }
        self.pending_at_rule = Some(name.clone());
        self.note(&format!("at:{name}"));
    }

    /// Идентификатор или функция, включая запись через escape.
    fn read_ident_like(&mut self) {
        let start = self.pos;
        let (mut name, end) = read_identifier(self.style, self.pos);
        self.pos = end;

        // `\40 import` в CSS не является at-rule: escape разворачивается в
        // идентификатор с именем `@import`. Браузер такой правило не исполнит, но
        // разбирать его как `@import` безопаснее: неподдержанная запись не
        // становится пропуском.
        let escaped_at = name.starts_with('@');
        if escaped_at {
            name.remove(0);
        }

        self.skip_trivia();
        if self.byte_at(self.pos) == Some(b'(') {
            self.pos += 1;
            let lower = name.to_ascii_lowercase();
            if lower == "url" || (escaped_at && lower == "url") {
                self.read_url_argument(start);
                return;
            }
            self.functions.push(lower.clone());
            self.note(&format!("function:{lower}"));
            return;
        }

        let lower = name.to_ascii_lowercase();
        if escaped_at && lower == "import" {
            let end = self.end_of_at_rule();
            self.events.push(Event::Import(start..end));
            self.pos = end;
            self.pending_at_rule = Some(lower.clone());
            self.note("at-rule");
            return;
        }
        self.note(&format!("ident:{lower}"));
    }

    /// `url(` вместе со своим аргументом.
    ///
    /// Диапазон события — вся конструкция, поэтому нейтрализация заменяет её
    /// целиком и не оставляет `url` без скобок. Незакрытая скобка поглощает
    /// остаток стиля: это fail-closed, а не «не ссылка».
    fn read_url_argument(&mut self, start: usize) {
        let inner_start = self.pos;
        let mut depth = 0usize;
        let mut close: Option<usize> = None;
        while self.pos < self.style.len() {
            let byte = self.byte(self.pos);
            match byte {
                b'/' if self.peek(1) == Some(b'*') => self.skip_comment(),
                b'"' | b'\'' => {
                    self.read_string_body();
                }
                b'\\' => {
                    let (_, end) = read_escape(self.style, self.pos);
                    self.pos = end;
                }
                b'(' => {
                    depth += 1;
                    self.pos += 1;
                }
                b')' if depth == 0 => {
                    close = Some(self.pos);
                    break;
                }
                b')' => {
                    depth -= 1;
                    self.pos += 1;
                }
                _ => self.pos += self.char_len(self.pos),
            }
        }

        let end = close.unwrap_or(self.style.len());
        let target = decode_url_argument(&self.style[inner_start..end]);
        let span = if close.is_some() {
            start..end + 1
        } else {
            start..end
        };
        self.pos = if close.is_some() { end + 1 } else { end };
        self.events.push(Event::Address(Address {
            span,
            target,
            form: AddressForm::Url,
        }));
        self.note("url");
    }

    /// Строковый токен: адрес — только в позиции, где его запрашивает браузер.
    fn read_string_token(&mut self) {
        let start = self.pos;
        let value = self.read_string_body();
        let is_address = self.string_is_address();
        if is_address {
            self.events.push(Event::Address(Address {
                span: start..self.pos,
                target: value,
                form: AddressForm::String,
            }));
        }
        self.note("string");
    }

    /// Строка внутри несущей адрес функции или дескриптора `src` в `@font-face`.
    fn string_is_address(&self) -> bool {
        if let Some(innermost) = self.functions.last() {
            if is_local_only_function(innermost) {
                return false;
            }
            if is_url_carrying_function(innermost) {
                return true;
            }
        }
        self.blocks.last().is_some_and(|block| block == "font-face")
            && self.previous.as_deref() == Some(":")
            && self.before_previous.as_deref() == Some("ident:src")
    }

    /// Читает тело строки вместе с кавычками; возвращает декодированное значение.
    fn read_string_body(&mut self) -> String {
        let quote = self.byte(self.pos);
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.style.len() {
            let byte = self.byte(self.pos);
            if byte == quote {
                self.pos += 1;
                break;
            }
            // Незакрытая строка не продолжается за переводом строки: разметка
            // непонятна, и остаток читается как обычный текст.
            if byte == b'\n' || byte == b'\r' || byte == 0x0c {
                break;
            }
            if byte == b'\\' {
                let (character, end) = read_escape(self.style, self.pos);
                out.push(character);
                self.pos = end;
                continue;
            }
            let character = self.style[self.pos..]
                .chars()
                .next()
                .unwrap_or(char::REPLACEMENT_CHARACTER);
            out.push(character);
            self.pos += character.len_utf8();
        }
        out
    }

    /// Конец at-rule: `;` на нулевой глубине скобок, `}` или конец стиля.
    fn end_of_at_rule(&mut self) -> usize {
        let mut depth = 0usize;
        while self.pos < self.style.len() {
            let byte = self.byte(self.pos);
            match byte {
                b'/' if self.peek(1) == Some(b'*') => self.skip_comment(),
                b'"' | b'\'' => {
                    self.read_string_body();
                }
                b'\\' => {
                    let (_, end) = read_escape(self.style, self.pos);
                    self.pos = end;
                }
                b'(' => {
                    depth += 1;
                    self.pos += 1;
                }
                b')' => {
                    depth = depth.saturating_sub(1);
                    self.pos += 1;
                }
                b';' if depth == 0 => return self.pos + 1,
                b'}' if depth == 0 => return self.pos,
                _ => self.pos += self.char_len(self.pos),
            }
        }
        self.style.len()
    }

    fn skip_comment(&mut self) {
        self.pos += 2;
        while self.pos < self.style.len() {
            if self.byte(self.pos) == b'*' && self.peek(1) == Some(b'/') {
                self.pos += 2;
                return;
            }
            self.pos += self.char_len(self.pos);
        }
    }

    fn skip_trivia(&mut self) {
        loop {
            while self.pos < self.style.len() && is_whitespace(self.byte(self.pos)) {
                self.pos += 1;
            }
            if self.pos < self.style.len()
                && self.byte(self.pos) == b'/'
                && self.peek(1) == Some(b'*')
            {
                self.skip_comment();
                continue;
            }
            return;
        }
    }

    fn note(&mut self, token: &str) {
        self.before_previous = self.previous.take();
        self.previous = Some(token.to_string());
    }

    fn would_start_identifier(&self, position: usize) -> bool {
        let Some(byte) = self.byte_at(position) else {
            return false;
        };
        if byte == b'\\' {
            return self.byte_at(position + 1).is_some_and(|next| next != b'\n');
        }
        if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 {
            return true;
        }
        if byte == b'-' {
            return self.byte_at(position + 1).is_some_and(|next| {
                next.is_ascii_alphabetic() || next == b'_' || next == b'-' || next >= 0x80
            });
        }
        false
    }

    fn byte(&self, position: usize) -> u8 {
        self.style.as_bytes()[position]
    }

    fn byte_at(&self, position: usize) -> Option<u8> {
        self.style.as_bytes().get(position).copied()
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.byte_at(self.pos + offset)
    }

    /// Длина символа по границе UTF-8, начиная с позиции.
    fn char_len(&self, position: usize) -> usize {
        self.style[position..]
            .chars()
            .next()
            .map_or(1, char::len_utf8)
    }
}

/// Пробельный символ CSS.
fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// Читает имя идентификатора вместе с escape-последовательностями.
///
/// Возвращает декодированное имя и позицию после него. Чтение останавливается на
/// первом символе, который не может быть частью имени.
fn read_identifier(style: &str, start: usize) -> (String, usize) {
    let bytes = style.as_bytes();
    let mut out = String::new();
    let mut position = start;
    while position < bytes.len() {
        let byte = bytes[position];
        if byte == b'\\' {
            let (character, end) = read_escape(style, position);
            out.push(character);
            position = end;
            continue;
        }
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte >= 0x80 {
            let character = style[position..]
                .chars()
                .next()
                .unwrap_or(char::REPLACEMENT_CHARACTER);
            out.push(character);
            position += character.len_utf8();
            continue;
        }
        break;
    }
    (out, position)
}

/// Читает одну escape-последовательность CSS начиная с `\`.
///
/// Возвращает декодированный символ и позицию после последовательности. Шестнадца-
/// теричная запись поглощает до шести цифр и один пробельный символ после них —
/// именно так её разбирает браузер, поэтому `https\3a //…` даёт `https://…`.
fn read_escape(style: &str, start: usize) -> (char, usize) {
    let bytes = style.as_bytes();
    let mut position = start + 1;
    if position >= bytes.len() {
        return (char::REPLACEMENT_CHARACTER, position);
    }
    let byte = bytes[position];
    if byte == b'\n' || byte == b'\r' || byte == 0x0c {
        // Escape перед переводом строки не является escape-последовательностью.
        return (char::REPLACEMENT_CHARACTER, position);
    }
    if byte.is_ascii_hexdigit() {
        let mut value: u32 = 0;
        let mut digits = 0usize;
        while position < bytes.len() && digits < 6 && bytes[position].is_ascii_hexdigit() {
            value = value * 16 + u32::from(hex_value(bytes[position]));
            position += 1;
            digits += 1;
        }
        // Один пробельный символ после hex-записи принадлежит escape.
        if position < bytes.len() && is_whitespace(bytes[position]) {
            if bytes[position] == b'\r' && bytes.get(position + 1) == Some(&b'\n') {
                position += 1;
            }
            position += 1;
        }
        let character = match char::from_u32(value) {
            Some(character) if character != '\0' && !character.is_control() => character,
            _ => char::REPLACEMENT_CHARACTER,
        };
        return (character, position);
    }
    let character = style[position..]
        .chars()
        .next()
        .unwrap_or(char::REPLACEMENT_CHARACTER);
    (character, position + character.len_utf8())
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => byte - b'A' + 10,
    }
}

/// Разворачивает аргумент `url()`: снимает кавычки и декодирует escape.
fn decode_url_argument(argument: &str) -> String {
    let trimmed = argument
        .trim_matches(|character: char| matches!(character, ' ' | '\t' | '\n' | '\r' | '\u{c}'));
    let unquoted = if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        let first = bytes[0];
        if (first == b'"' || first == b'\'')
            && bytes[bytes.len() - 1] == first
            && !trimmed[1..trimmed.len() - 1].contains(char::from(first))
        {
            &trimmed[1..trimmed.len() - 1]
        } else {
            trimmed
        }
    } else {
        trimmed
    };
    unescape(unquoted)
}

/// Декодирует escape-последовательности в произвольном тексте.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut position = 0usize;
    while position < text.len() {
        if text.as_bytes()[position] == b'\\' {
            let (character, end) = read_escape(text, position);
            out.push(character);
            position = end;
            continue;
        }
        let character = text[position..]
            .chars()
            .next()
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        out.push(character);
        position += character.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addresses(style: &str) -> Vec<String> {
        scan(style)
            .into_iter()
            .filter_map(|event| match event {
                Event::Address(address) => {
                    assert_eq!(
                        &style[address.span.clone()],
                        &style[address.span.clone()],
                        "диапазон обязан лежать в источнике"
                    );
                    Some(address.target)
                }
                Event::Import(_) => None,
            })
            .collect()
    }

    fn imports(style: &str) -> Vec<String> {
        scan(style)
            .into_iter()
            .filter_map(|event| match event {
                Event::Import(span) => Some(style[span].to_string()),
                Event::Address(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_plain_url_is_an_address() {
        assert_eq!(
            addresses("a { background: url(https://evil.example/a.png) }"),
            vec!["https://evil.example/a.png"]
        );
        assert_eq!(
            addresses("a { background: url('https://evil.example/a.png') }"),
            vec!["https://evil.example/a.png"]
        );
        assert_eq!(
            addresses("a { background: url( \"https://evil.example/a.png\" ) }"),
            vec!["https://evil.example/a.png"]
        );
    }

    #[test]
    fn the_function_name_may_be_escaped_or_recased() {
        for style in [
            "u\\72l(https://evil.example/a.png)",
            "\\75 rl(https://evil.example/a.png)",
            "URL(https://evil.example/a.png)",
            "Url(https://evil.example/a.png)",
            "/* x */ u\\72 l (https://evil.example/a.png)",
        ] {
            assert_eq!(
                addresses(style),
                vec!["https://evil.example/a.png"],
                "{style:?}"
            );
        }
    }

    #[test]
    fn escapes_inside_the_address_are_decoded_before_the_check() {
        assert_eq!(
            addresses("a{background:url(https\\3a //evil.example/a.png)}"),
            vec!["https://evil.example/a.png"]
        );
        assert_eq!(
            addresses("a{background:url(\"https\\3a //evil.example/a.png\")}"),
            vec!["https://evil.example/a.png"]
        );
    }

    #[test]
    fn imports_are_recognised_in_every_spelling() {
        assert_eq!(
            imports("@import url(https://evil.example/a.css);"),
            vec!["@import url(https://evil.example/a.css);"]
        );
        assert_eq!(
            imports("@IMPORT \"https://evil.example/a.css\";"),
            vec!["@IMPORT \"https://evil.example/a.css\";"]
        );
        assert_eq!(
            imports("@im\\70 ort \"https://evil.example/a.css\";"),
            vec!["@im\\70 ort \"https://evil.example/a.css\";"]
        );
        assert_eq!(
            imports("\\40 import \"https://evil.example/a.css\";"),
            vec!["\\40 import \"https://evil.example/a.css\";"]
        );
        // `@import` через escape-запись имени at-keyword и с концом блока.
        assert_eq!(imports("@import \"a.css\""), vec!["@import \"a.css\""]);
    }

    #[test]
    fn an_import_rule_is_one_event_and_carries_no_nested_address() {
        let events = scan("@import url(https://evil.example/a.css); a { color: red }");
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(events[0], Event::Import(_)));
    }

    #[test]
    fn a_string_argument_of_a_url_carrying_function_is_an_address() {
        assert_eq!(
            addresses("a{background:image-set(\"https://evil.example/a.png\" 1x)}"),
            vec!["https://evil.example/a.png"]
        );
        assert_eq!(
            addresses("a{background:-webkit-image-set('https://evil.example/a.png' 2x)}"),
            vec!["https://evil.example/a.png"]
        );
        // `local()` в `src` — не адрес, даже внутри `@font-face`.
        let style = "@font-face { src: local(\"My Font\"), url(https://evil.example/a.woff2) }";
        assert_eq!(addresses(style), vec!["https://evil.example/a.woff2"]);
        assert_eq!(
            addresses("@font-face { src: \"https://evil.example/a.woff2\" }"),
            vec!["https://evil.example/a.woff2"]
        );
        assert_eq!(
            addresses("a{content:\"https://evil.example\"}"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_local_fragment_is_still_an_address_to_be_judged() {
        assert_eq!(addresses("a{filter:url(#f)}"), vec!["#f"]);
        assert_eq!(
            addresses("a{filter:url( ../media/before/a.png )}"),
            vec![" ../media/before/a.png ".trim().to_string()]
        );
    }

    #[test]
    fn an_unterminated_url_swallows_the_rest() {
        let style = "a{background:url(https://evil.example/a.png";
        let events = scan(style);
        assert_eq!(events.len(), 1);
        let span = events[0].span();
        assert_eq!(span, 13..style.len());
        assert_eq!(
            events[0],
            Event::Address(Address {
                span: 13..style.len(),
                target: "https://evil.example/a.png".to_string(),
                form: AddressForm::Url,
            })
        );
    }

    #[test]
    fn comments_do_not_join_two_identifiers() {
        // `ur/**/l(` — это идентификатор `ur`, а не функция `url`.
        assert_eq!(
            addresses("a{background:ur/**/l(https://evil.example/a.png)}"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_comment_does_not_hide_an_import() {
        assert_eq!(imports("@im/* x */port \"a.css\";"), Vec::<String>::new());
        assert_eq!(
            imports("@import/* x */ \"a.css\";"),
            vec!["@import/* x */ \"a.css\";"]
        );
    }

    #[test]
    fn scanning_always_terminates_on_hostile_input() {
        for style in [
            "url(",
            "url('",
            "url(\"",
            "@import",
            "@import url(",
            "\\",
            "\\40 ",
            "/*",
            "a{",
            "url(\\",
            "url(a b)",
            "@font-face{src:\"",
        ] {
            let _ = scan(style);
        }
    }
}
