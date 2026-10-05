//! Извлечение человеческого текста и применение явных решений внешнего агента.
//!
//! Лексический сканер возвращает кандидатов, а не нарушения политики. Смещения
//! относятся к исходным UTF-8 байтам, включая escapes в строковых литералах.
//! Публикация использует общий владелец записи `crate::write`; до первой записи
//! проверяются все решения. Несогласованный внешний writer не участвует в
//! advisory-блокировках, поэтому ошибки публикации явно содержат список уже
//! изменённых файлов.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const LANGUAGE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextContext {
    Comment,
    DocComment,
    StringLiteral,
    MarkdownProse,
    ConfigurationValue,
    ScriptOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageCandidate {
    pub id: String,
    pub path: String,
    pub source_sha256: String,
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub column: usize,
    pub context: TextContext,
    pub text: String,
    pub signals: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScannedFile {
    pub path: String,
    pub source_sha256: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageScan {
    pub schema_version: u32,
    pub files: Vec<ScannedFile>,
    pub candidates: Vec<LanguageCandidate>,
    pub skipped: Vec<SkippedFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguageAction {
    Replace,
    Allow,
    Ignore,
    NeedsReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageDecision {
    pub candidate: LanguageCandidate,
    pub action: LanguageAction,
    pub replacement: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageDecisions {
    pub schema_version: u32,
    pub decisions: Vec<LanguageDecision>,
}

impl LanguageDecisions {
    /// Заготовка требует явной классификации каждого кандидата агентом.
    #[must_use]
    pub fn pending(scan: &LanguageScan) -> Self {
        Self {
            schema_version: LANGUAGE_SCHEMA_VERSION,
            decisions: scan
                .candidates
                .iter()
                .map(|candidate| LanguageDecision {
                    candidate: candidate.clone(),
                    action: LanguageAction::NeedsReview,
                    replacement: None,
                    reason: String::new(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedFile {
    pub path: String,
    pub before_sha256: String,
    pub after_sha256: String,
    pub replacements: usize,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyResult {
    pub schema_version: u32,
    pub applied: bool,
    pub files: Vec<AppliedFile>,
}

#[derive(Debug, thiserror::Error)]
pub enum LanguageError {
    #[error("решения языка отклонены: {0}")]
    Preconditions(String),
    #[error("не удалось прочитать {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("публикация языка завершилась ошибкой: {message}; уже записаны: {written:?}")]
    Publication {
        message: String,
        written: Vec<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Rust,
    Markdown,
    Toml,
    Yaml,
    Json,
    Shell,
    PowerShell,
}

fn format(path: &str) -> Option<Format> {
    match Path::new(path).extension()?.to_str()? {
        "rs" => Some(Format::Rust),
        "md" | "markdown" => Some(Format::Markdown),
        "toml" => Some(Format::Toml),
        "yaml" | "yml" => Some(Format::Yaml),
        "json" => Some(Format::Json),
        "sh" | "bash" => Some(Format::Shell),
        "ps1" | "psm1" | "psd1" => Some(Format::PowerShell),
        _ => None,
    }
}

pub fn is_eligible_path(path: &str) -> bool {
    let components: Vec<_> = Path::new(path).components().collect();
    !components.is_empty()
        && !path.contains('\\')
        && !path.contains(':')
        && components
            .iter()
            .all(|part| matches!(part, Component::Normal(_)))
        && !components.iter().any(|part| {
            matches!(part, Component::Normal(name)
            if *name == ".git" || *name == "target")
        })
        && path.split('/').next().is_none_or(|part| part != "decks")
        && !path.starts_with(".anki-repo/review/")
        && format(path).is_some()
}

#[must_use]
pub fn source_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn candidate_id(path: &str, context: TextContext, text: &str, occurrence: usize) -> String {
    let key = serde_json::to_vec(&(path, context, text, occurrence))
        .expect("сериализация строкового локатора не должна завершаться ошибкой");
    format!("language:{}", source_sha256(&key))
}

/// Полные исходники предоставляются вызывающим владельцем Git snapshot.
/// `decks/**`, медиа и неподдерживаемые форматы исключаются явно.
#[must_use]
pub fn scan(files: &[SourceFile]) -> LanguageScan {
    let mut result = LanguageScan {
        schema_version: LANGUAGE_SCHEMA_VERSION,
        files: Vec::new(),
        candidates: Vec::new(),
        skipped: Vec::new(),
    };
    let mut ordered: Vec<_> = files.iter().collect();
    ordered.sort_by(|a, b| (&a.path, &a.content).cmp(&(&b.path, &b.content)));
    ordered.dedup_by(|a, b| a.path == b.path && a.content == b.content);
    for file in ordered {
        if !is_eligible_path(&file.path) {
            result.skipped.push(SkippedFile {
                path: file.path.clone(),
                reason: "предметные данные, служебный путь или неподдерживаемый формат".into(),
            });
            continue;
        }
        let digest = source_sha256(file.content.as_bytes());
        result.files.push(ScannedFile {
            path: file.path.clone(),
            source_sha256: digest.clone(),
            state: "scanned".into(),
        });
        let mut occurrences = BTreeMap::new();
        for (start, end, context) in extract(&file.path, &file.content) {
            let text = &file.content[start..end];
            if !foreign_text(text) {
                continue;
            }
            let key = (serde_json::to_string(&context).unwrap_or_default(), text);
            let occurrence = occurrences.entry(key).or_insert(0_usize);
            let id = candidate_id(&file.path, context, text, *occurrence);
            *occurrence += 1;
            let preceding = &file.content[..start];
            result.candidates.push(LanguageCandidate {
                id,
                path: file.path.clone(),
                source_sha256: digest.clone(),
                start,
                end,
                line: preceding.bytes().filter(|byte| *byte == b'\n').count() + 1,
                column: preceding.rsplit('\n').next().unwrap_or("").chars().count() + 1,
                context,
                text: text.into(),
                signals: vec!["foreign_human_text_requires_semantic_review".into()],
            });
        }
    }
    result
        .candidates
        .sort_by(|a, b| (&a.path, a.start, &a.id).cmp(&(&b.path, b.start, &b.id)));
    result
        .skipped
        .sort_by(|a, b| (&a.path, &a.reason).cmp(&(&b.path, &b.reason)));
    result
}

type Span = (usize, usize, TextContext);

fn extract(path: &str, text: &str) -> Vec<Span> {
    match format(path) {
        Some(Format::Rust) => rust_spans(text),
        Some(Format::Markdown) => markdown_spans(text),
        Some(Format::Json) => json_spans(text),
        Some(Format::Toml) => config_spans(text, false),
        Some(Format::Yaml) => config_spans(text, true),
        Some(Format::Shell) => script_spans(text, false),
        Some(Format::PowerShell) => script_spans(text, true),
        None => Vec::new(),
    }
}

/// Малый список точных терминов; регистр сохраняется для различения обычных слов.
fn allowed_term(word: &str) -> bool {
    matches!(
        word,
        "Rust"
            | "Git"
            | "GitHub"
            | "HTTP"
            | "HTTPS"
            | "JSON"
            | "TOML"
            | "YAML"
            | "SQL"
            | "CLI"
            | "API"
            | "URL"
            | "URI"
            | "UUID"
            | "SHA"
            | "UTF"
            | "HTML"
            | "CSS"
            | "Anki"
            | "CrowdAnki"
    )
}

fn machine_token(word: &str) -> bool {
    word.starts_with("--")
        || word.contains("::")
        || word.contains('_')
        || word.contains('/')
        || word.contains('\\')
        || word.contains("${")
        || word.contains("://")
        || word.contains('=')
        || word.contains('@')
        || word.starts_with('$')
        || word.contains(".rs")
        || word.contains(".json")
        || word.chars().any(|ch| ch.is_ascii_digit())
        || (word.starts_with('{') && word.ends_with('}'))
}

fn foreign_text(text: &str) -> bool {
    text.split_whitespace().any(|token| {
        if machine_token(token) {
            return false;
        }
        let token = token.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '_');
        if token.chars().any(|ch| {
            ch.is_alphabetic() && !ch.is_ascii() && !('\u{0400}'..='\u{052f}').contains(&ch)
        }) {
            return true;
        }
        token
            .split(|ch: char| !ch.is_ascii_alphabetic())
            .any(|word| word.len() >= 2 && !allowed_term(word))
    })
}

fn push_trimmed(spans: &mut Vec<Span>, text: &str, start: usize, end: usize, context: TextContext) {
    if start >= end {
        return;
    }
    let original = &text[start..end];
    let trimmed = original.trim();
    if trimmed.is_empty() {
        return;
    }
    let lead = original.len() - original.trim_start().len();
    spans.push((start + lead, start + lead + trimmed.len(), context));
}

/// Возвращает внутренний диапазон и байт после закрывающей кавычки.
fn quoted(text: &str, start: usize, quote: u8, powershell: bool) -> (usize, usize, usize) {
    let bytes = text.as_bytes();
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == quote {
            if powershell && bytes.get(index + 1) == Some(&quote) {
                index += 2;
                continue;
            }
            return (start + 1, index, index + 1);
        }
        if (bytes[index] == b'\\' && !powershell && quote != b'\'')
            || (bytes[index] == b'`' && powershell && quote == b'"')
        {
            index = (index + 2).min(bytes.len());
        } else {
            index += 1;
        }
    }
    // Незакрытый литерал не получает пригодного для правки диапазона.
    (start + 1, start + 1, bytes.len())
}

fn rust_spans(text: &str) -> Vec<Span> {
    let bytes = text.as_bytes();
    let mut result = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"//") {
            let end = text[index..].find('\n').map_or(bytes.len(), |n| index + n);
            let doc = bytes
                .get(index + 2)
                .is_some_and(|ch| *ch == b'/' || *ch == b'!');
            push_trimmed(
                &mut result,
                text,
                index + if doc { 3 } else { 2 },
                end,
                if doc {
                    TextContext::DocComment
                } else {
                    TextContext::Comment
                },
            );
            index = end;
        } else if bytes[index..].starts_with(b"/*") {
            let start = index;
            let doc = bytes
                .get(index + 2)
                .is_some_and(|ch| *ch == b'*' || *ch == b'!');
            index += 2;
            let mut depth = 1;
            while index < bytes.len() && depth > 0 {
                if bytes[index..].starts_with(b"/*") {
                    depth += 1;
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            if depth == 0 {
                push_trimmed(
                    &mut result,
                    text,
                    start + if doc { 3 } else { 2 },
                    index - 2,
                    if doc {
                        TextContext::DocComment
                    } else {
                        TextContext::Comment
                    },
                );
            }
        } else if bytes[index] == b'r'
            || bytes[index..].starts_with(b"br")
            || bytes[index..].starts_with(b"cr")
        {
            let external = bytes[index] != b'r';
            let prefix = index + if external { 2 } else { 1 };
            let mut opener = prefix;
            while bytes.get(opener) == Some(&b'#') {
                opener += 1;
            }
            if bytes.get(opener) == Some(&b'"') {
                let closer = format!("\"{}", "#".repeat(opener - prefix));
                if let Some(offset) = text[opener + 1..].find(&closer) {
                    let end = opener + 1 + offset;
                    if !external {
                        push_trimmed(
                            &mut result,
                            text,
                            opener + 1,
                            end,
                            TextContext::StringLiteral,
                        );
                    }
                    index = end + closer.len();
                } else {
                    index = bytes.len();
                }
            } else {
                index = skip_identifier(bytes, index);
            }
        } else if bytes[index] == b'"'
            || bytes[index..].starts_with(b"b\"")
            || bytes[index..].starts_with(b"c\"")
        {
            let external = bytes[index] != b'"';
            let (start, end, next) = quoted(text, index + usize::from(external), b'"', false);
            if !external {
                push_trimmed(&mut result, text, start, end, TextContext::StringLiteral);
            }
            index = next;
        } else if bytes[index] == b'\'' {
            // Символы и lifetimes не являются человеческим текстом.
            let mut end = index + 1;
            if bytes.get(end) == Some(&b'\\') {
                end = (end + 2).min(bytes.len());
            } else if end < bytes.len() {
                end += text[end..].chars().next().map_or(0, char::len_utf8);
            }
            index = if bytes.get(end) == Some(&b'\'') {
                end + 1
            } else {
                index + 1
            };
        } else if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' || bytes[index] >= 128
        {
            index = skip_identifier(bytes, index);
        } else {
            index += 1;
        }
    }
    result
}

fn skip_identifier(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len()
        && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_' || bytes[index] >= 128)
    {
        index += 1;
    }
    index
}

fn markdown_spans(text: &str) -> Vec<Span> {
    let mut result = Vec::new();
    let mut offset = 0;
    let mut fence: Option<(u8, usize)> = None;
    let mut inline_ticks = 0;
    for line in text.split_inclusive('\n') {
        let left = line.trim_start_matches(' ');
        let indentation = line.len() - left.len();
        let marker = left.as_bytes().first().copied().unwrap_or(0);
        let count = left.bytes().take_while(|byte| *byte == marker).count();
        if let Some((opening, opening_count)) = fence {
            if marker == opening && count >= opening_count && left[count..].trim().is_empty() {
                fence = None;
            }
            offset += line.len();
            continue;
        }
        if indentation >= 4 || line.starts_with('\t') {
            offset += line.len();
            continue;
        }
        if matches!(marker, b'`' | b'~') && count >= 3 {
            fence = Some((marker, count));
            offset += line.len();
            continue;
        }
        let bytes = line.as_bytes();
        let mut start = 0;
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if bytes[index] == b'`' {
                let count = bytes[index..]
                    .iter()
                    .take_while(|byte| **byte == b'`')
                    .count();
                if inline_ticks == 0 {
                    push_trimmed(
                        &mut result,
                        text,
                        offset + start,
                        offset + index,
                        TextContext::MarkdownProse,
                    );
                    inline_ticks = count;
                } else if inline_ticks == count {
                    inline_ticks = 0;
                }
                index += count;
                start = index;
            } else if inline_ticks == 0
                && (bytes[index] == b'<' || bytes[index..].starts_with(b"]("))
            {
                push_trimmed(
                    &mut result,
                    text,
                    offset + start,
                    offset + index,
                    TextContext::MarkdownProse,
                );
                let end = if bytes[index] == b'<' { b'>' } else { b')' };
                index += if end == b')' { 2 } else { 1 };
                while index < bytes.len() && bytes[index] != end {
                    index += 1;
                }
                index = (index + 1).min(bytes.len());
                start = index;
            } else {
                index += 1;
            }
        }
        if inline_ticks == 0 {
            push_trimmed(
                &mut result,
                text,
                offset + start,
                offset + line.len(),
                TextContext::MarkdownProse,
            );
        }
        offset += line.len();
    }
    result
}

fn json_spans(text: &str) -> Vec<Span> {
    // Валидность проверяется существующим serde, смещения сохраняет лексер.
    if serde_json::from_str::<serde_json::Value>(text).is_err() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut result = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let (start, end, next) = quoted(text, index, b'"', false);
            let mut following = next;
            while bytes.get(following).is_some_and(u8::is_ascii_whitespace) {
                following += 1;
            }
            if bytes.get(following) != Some(&b':') {
                push_trimmed(
                    &mut result,
                    text,
                    start,
                    end,
                    TextContext::ConfigurationValue,
                );
            }
            index = next;
        } else {
            index += 1;
        }
    }
    result
}

fn config_spans(text: &str, yaml: bool) -> Vec<Span> {
    if yaml
        && text.trim_start().starts_with('{')
        && serde_json::from_str::<serde_json::Value>(text).is_ok()
    {
        return json_spans(text);
    }
    let mut result = Vec::new();
    let mut offset = 0;
    let mut continuation = false;
    let mut block_indent: Option<usize> = None;
    let mut multiline: Option<(u8, usize)> = None;
    for line in text.split_inclusive('\n') {
        let bytes = line.as_bytes();
        let indentation = line.len() - line.trim_start().len();
        if let Some(indent) = block_indent {
            if indentation > indent || line.trim().is_empty() {
                push_trimmed(
                    &mut result,
                    text,
                    offset,
                    offset + line.len(),
                    TextContext::ConfigurationValue,
                );
                offset += line.len();
                continue;
            }
            block_indent = None;
        }
        let mut index = 0;
        let mut value = continuation || multiline.is_some();
        let mut plain_start = None;
        while index < bytes.len() {
            if let Some((quote, start)) = multiline {
                let delimiter = [quote; 3];
                if bytes[index..].starts_with(&delimiter) {
                    push_trimmed(
                        &mut result,
                        text,
                        start,
                        offset + index,
                        TextContext::ConfigurationValue,
                    );
                    multiline = None;
                    index += 3;
                } else {
                    index += 1;
                }
                continue;
            }
            if bytes[index] == b'#' {
                if let Some(start) = plain_start.take() {
                    let value_text = line[start..index].trim();
                    if yaml && (value_text.starts_with('|') || value_text.starts_with('>')) {
                        block_indent = Some(indentation);
                    } else {
                        push_trimmed(
                            &mut result,
                            text,
                            offset + start,
                            offset + index,
                            TextContext::ConfigurationValue,
                        );
                    }
                }
                push_trimmed(
                    &mut result,
                    text,
                    offset + index + 1,
                    offset + line.len(),
                    TextContext::Comment,
                );
                break;
            }
            if bytes[index] == b'"' || bytes[index] == b'\'' {
                if let Some(start) = plain_start.take() {
                    push_trimmed(
                        &mut result,
                        text,
                        offset + start,
                        offset + index,
                        TextContext::ConfigurationValue,
                    );
                }
                let quote = bytes[index];
                if value && !yaml && bytes[index..].starts_with(&[quote; 3]) {
                    multiline = Some((quote, offset + index + 3));
                    index += 3;
                } else {
                    let (start, end, next) = quoted(line, index, quote, quote == b'\'');
                    let mut following = next;
                    while bytes.get(following).is_some_and(u8::is_ascii_whitespace) {
                        following += 1;
                    }
                    let key = bytes.get(following) == Some(&if yaml { b':' } else { b'=' });
                    if value && !key {
                        push_trimmed(
                            &mut result,
                            text,
                            offset + start,
                            offset + end,
                            TextContext::ConfigurationValue,
                        );
                    }
                    index = next;
                }
                continue;
            }
            if (!yaml && bytes[index] == b'=')
                || (yaml
                    && bytes[index] == b':'
                    && bytes
                        .get(index + 1)
                        .is_none_or(|byte| byte.is_ascii_whitespace()))
            {
                plain_start = None;
                value = true;
                index += 1;
                continue;
            }
            if yaml
                && !value
                && bytes[index] == b'-'
                && bytes.get(index + 1).is_some_and(u8::is_ascii_whitespace)
            {
                value = true;
                index += 1;
                continue;
            }
            if value && yaml && !bytes[index].is_ascii_whitespace() && plain_start.is_none() {
                plain_start = Some(index);
            }
            index += 1;
        }
        if yaml && let Some(start) = plain_start {
            let remainder = line[start..].trim();
            if remainder.starts_with('|') || remainder.starts_with('>') {
                block_indent = Some(indentation);
            } else {
                push_trimmed(
                    &mut result,
                    text,
                    offset + start,
                    offset + line.len(),
                    TextContext::ConfigurationValue,
                );
            }
        }
        continuation = !yaml && value && line.trim_end().ends_with(['[', ',']);
        offset += line.len();
    }
    result
}

fn script_spans(text: &str, powershell: bool) -> Vec<Span> {
    let mut result = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if powershell && bytes[index..].starts_with(b"<#") {
            if let Some(end) = text[index + 2..].find("#>") {
                push_trimmed(
                    &mut result,
                    text,
                    index + 2,
                    index + 2 + end,
                    TextContext::Comment,
                );
                index += end + 4;
            } else {
                break;
            }
        } else if bytes[index] == b'#'
            && (index == 0
                || bytes[index - 1].is_ascii_whitespace()
                || (powershell && bytes[index - 1] != b'`'))
        {
            let end = text[index..]
                .find('\n')
                .map_or(bytes.len(), |offset| index + offset);
            if !bytes[index..].starts_with(b"#!") {
                push_trimmed(&mut result, text, index + 1, end, TextContext::Comment);
            }
            index = end;
        } else if bytes[index] == b'"' || bytes[index] == b'\'' {
            let (start, end, next) = quoted(text, index, bytes[index], powershell);
            push_trimmed(&mut result, text, start, end, TextContext::StringLiteral);
            index = next;
        } else if bytes[index] == b'\\' && !powershell || bytes[index] == b'`' && powershell {
            index = (index + 2).min(bytes.len());
        } else {
            index += 1;
        }
    }
    // Незаключённый в кавычки текст извлекается только у известных команд
    // вывода; имена прочих команд и аргументов остаются кодом.
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let left = line.trim_start();
        let command_end = left.find(char::is_whitespace).unwrap_or(left.len());
        let command = &left[..command_end];
        let output_command = if powershell {
            ["Write-Host", "Write-Output", "Write-Warning", "Write-Error"]
                .iter()
                .any(|known| command.eq_ignore_ascii_case(known))
        } else {
            command == "echo" || command == "printf"
        };
        if output_command {
            let arguments = left[command_end..].trim();
            if !arguments.is_empty()
                && !arguments.starts_with('-')
                && !arguments.contains(['\'', '"', '$', '`', ';', '|', '&', '<', '>', '\\'])
            {
                let end = arguments.find('#').unwrap_or(arguments.len());
                let lead = line.len() - left.len() + command_end + left[command_end..].len()
                    - left[command_end..].trim_start().len();
                push_trimmed(
                    &mut result,
                    text,
                    offset + lead,
                    offset + lead + end,
                    TextContext::ScriptOutput,
                );
            }
        }
        offset += line.len();
    }
    result.sort_by_key(|span| (span.0, span.1));
    result
}

fn reject(message: impl Into<String>) -> LanguageError {
    LanguageError::Preconditions(message.into())
}

fn safe_path(root: &Path, relative: &str) -> Result<PathBuf, LanguageError> {
    if !is_eligible_path(relative) {
        return Err(reject(format!("запрещённый путь: {relative}")));
    }
    let mut target = root.to_path_buf();
    for component in Path::new(relative).components() {
        target.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&target).map_err(|source| LanguageError::Read {
            path: relative.into(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(reject(format!("символическая ссылка: {relative}")));
        }
    }
    if !fs::metadata(&target)
        .map_err(|source| LanguageError::Read {
            path: relative.into(),
            source,
        })?
        .is_file()
    {
        return Err(reject(format!(
            "цель не является обычным файлом: {relative}"
        )));
    }
    Ok(target)
}

fn validate_replacement(
    candidate: &LanguageCandidate,
    replacement: &str,
) -> Result<(), LanguageError> {
    if replacement.contains('\0') {
        return Err(reject("замена содержит нулевой байт"));
    }
    let before_newlines: Vec<_> = candidate
        .text
        .match_indices('\n')
        .map(|(at, _)| candidate.text.as_bytes().get(at.wrapping_sub(1)) == Some(&b'\r'))
        .collect();
    let after_newlines: Vec<_> = replacement
        .match_indices('\n')
        .map(|(at, _)| replacement.as_bytes().get(at.wrapping_sub(1)) == Some(&b'\r'))
        .collect();
    if before_newlines != after_newlines
        || replacement.matches('\r').count() != after_newlines.iter().filter(|crlf| **crlf).count()
    {
        return Err(reject(
            "замена должна сохранить число и вид переводов строк",
        ));
    }
    Ok(())
}

/// Проверяет все файлы до первой публикации. По умолчанию вызывающий CLI
/// передаёт `false`; отчёт содержит точные состояния до и после без записи.
pub fn apply(
    root: &Path,
    decisions: &LanguageDecisions,
    apply_changes: bool,
) -> Result<ApplyResult, LanguageError> {
    if decisions.schema_version != LANGUAGE_SCHEMA_VERSION {
        return Err(reject("неподдерживаемая версия решений"));
    }
    let root = fs::canonicalize(root).map_err(|source| LanguageError::Read {
        path: root.display().to_string(),
        source,
    })?;
    let mut groups: BTreeMap<&str, Vec<&LanguageDecision>> = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for decision in &decisions.decisions {
        if decision.action != LanguageAction::Replace {
            continue;
        }
        if decision.reason.trim().is_empty() {
            return Err(reject("для замены требуется объяснение решения"));
        }
        if !ids.insert(&decision.candidate.id) {
            return Err(reject("повторное решение для одного кандидата"));
        }
        if decision.replacement.is_none() {
            return Err(reject("для replace требуется replacement"));
        }
        groups
            .entry(&decision.candidate.path)
            .or_default()
            .push(decision);
    }
    let mut paths = BTreeMap::new();
    let mut parents = BTreeSet::new();
    for relative in groups.keys() {
        let path = safe_path(&root, relative)?;
        parents.insert(
            path.parent()
                .ok_or_else(|| reject("отсутствует родитель цели"))?
                .to_path_buf(),
        );
        paths.insert(*relative, path);
    }
    // Упорядоченные блокировки не создают файлов и предотвращают гонки между
    // writer-процессами toolkit, использующими общий владелец публикации.
    let mut guards = BTreeMap::new();
    if apply_changes {
        for parent in parents {
            let guard = crate::write::ExportLock::acquire(&parent).map_err(|error| {
                LanguageError::Publication {
                    message: error.to_string(),
                    written: Vec::new(),
                }
            })?;
            guards.insert(parent, guard);
        }
    }
    let mut result = ApplyResult {
        schema_version: LANGUAGE_SCHEMA_VERSION,
        applied: apply_changes,
        files: Vec::new(),
    };
    for (relative, mut replacements) in groups {
        let path = safe_path(&root, relative)?;
        let bytes = fs::read(&path).map_err(|source| LanguageError::Read {
            path: relative.into(),
            source,
        })?;
        let before = String::from_utf8(bytes)
            .map_err(|_| reject(format!("неподдерживаемая кодировка: {relative}")))?;
        let digest = source_sha256(before.as_bytes());
        let scanned = scan(&[SourceFile {
            path: relative.into(),
            content: before.clone(),
        }]);
        replacements.sort_by_key(|decision| (decision.candidate.start, decision.candidate.end));
        let mut previous_end = 0;
        for decision in &replacements {
            let candidate = &decision.candidate;
            if candidate.source_sha256 != digest {
                return Err(reject(format!("устаревший SHA-256: {relative}")));
            }
            if candidate.start < previous_end || candidate.start >= candidate.end {
                return Err(reject(format!("перекрывающиеся диапазоны: {relative}")));
            }
            if before.get(candidate.start..candidate.end) != Some(candidate.text.as_str()) {
                return Err(reject(format!("исходный текст изменился: {relative}")));
            }
            if !scanned.candidates.contains(candidate) {
                return Err(reject(format!(
                    "локатор кандидата не подтверждён сканером: {relative}"
                )));
            }
            validate_replacement(candidate, decision.replacement.as_deref().unwrap_or(""))?;
            previous_end = candidate.end;
        }
        let mut after = before.clone();
        for decision in replacements.iter().rev() {
            after.replace_range(
                decision.candidate.start..decision.candidate.end,
                decision.replacement.as_deref().unwrap_or(""),
            );
        }
        // Замена текста не должна открывать/закрывать литералы или комментарии.
        // Точные границы всех текстовых поверхностей должны сдвигаться лишь на
        // длину подтверждённых замен, включая полностью русские поверхности.
        let original_spans = extract(relative, &before);
        let mut expected_spans = Vec::new();
        for (start, end, context) in original_spans {
            let shift_at = |position: usize| -> usize {
                let mut shifted = position as i128;
                for decision in &replacements {
                    if decision.candidate.end <= position {
                        shifted += decision.replacement.as_ref().map_or(0, String::len) as i128
                            - (decision.candidate.end - decision.candidate.start) as i128;
                    }
                }
                shifted as usize
            };
            expected_spans.push((shift_at(start), shift_at(end), context));
        }
        if extract(relative, &after) != expected_spans {
            return Err(reject(format!(
                "замена меняет лексическую структуру: {relative}"
            )));
        }
        result.files.push(AppliedFile {
            path: relative.into(),
            before_sha256: digest,
            after_sha256: source_sha256(after.as_bytes()),
            replacements: replacements.len(),
            before,
            after,
        });
    }
    if apply_changes {
        // Повторный общий контроль перед записью; ошибка конкретного rename
        // далее возвращает точный список опубликованных файлов.
        for file in &result.files {
            let path = safe_path(&root, &file.path)?;
            let guard = &guards[path
                .parent()
                .ok_or_else(|| reject("отсутствует родитель цели"))?];
            guard
                .check_source(&path, file.before.as_bytes())
                .map_err(|error| LanguageError::Publication {
                    message: error.to_string(),
                    written: Vec::new(),
                })?;
        }
        let mut written = Vec::new();
        for file in &result.files {
            let path = paths[file.path.as_str()].clone();
            let guard = &guards[path
                .parent()
                .ok_or_else(|| reject("отсутствует родитель цели"))?];
            guard
                .replace(&path, file.before.as_bytes(), file.after.as_bytes())
                .map_err(|error| LanguageError::Publication {
                    message: error.to_string(),
                    written: written.clone(),
                })?;
            written.push(file.path.clone());
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn candidates(path: &str, content: &str) -> Vec<LanguageCandidate> {
        scan(&[SourceFile {
            path: path.into(),
            content: content.into(),
        }])
        .candidates
    }

    fn replacement(path: &str, content: &str, replacement: &str) -> LanguageDecision {
        LanguageDecision {
            candidate: candidates(path, content).remove(0),
            action: LanguageAction::Replace,
            replacement: Some(replacement.into()),
            reason: "подтверждён человеческий текст".into(),
        }
    }

    fn decisions(items: Vec<LanguageDecision>) -> LanguageDecisions {
        LanguageDecisions {
            schema_version: LANGUAGE_SCHEMA_VERSION,
            decisions: items,
        }
    }

    #[test]
    fn rust_extracts_comments_and_literals_but_not_identifiers_or_external_bytes() {
        let source = "fn english_identifier() { let value = \"human message\"; let raw = r##\"raw message\"##; let bytes = b\"external bytes\"; }\n// comment text\n/// documentation text\n/* outer /* inner */ comment */\n";
        let found = candidates("src/a.rs", source);
        assert_eq!(found.len(), 5);
        assert!(
            found
                .iter()
                .all(|candidate| !candidate.text.contains("english_identifier")
                    && !candidate.text.contains("external bytes"))
        );
        assert!(
            found
                .iter()
                .any(|candidate| candidate.context == TextContext::DocComment)
        );
    }

    #[test]
    fn technical_tokens_do_not_hide_surrounding_prose() {
        assert!(candidates("a.rs", "// Rust JSON API --apply src/file.rs\n").is_empty());
        assert_eq!(candidates("a.rs", "// Rust processing failed\n").len(), 1);
        assert!(candidates("a.rs", "let x = \"https://example.invalid/api\";").is_empty());
    }

    #[test]
    fn markdown_excludes_fences_inline_code_and_destinations() {
        let source = "Human prose `technical_identifier`\n```rust\nforeign code\n```\n[Русский](https://example.invalid/english)\n    indented code\n";
        let found = candidates("docs/a.md", source);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "Human prose");
    }

    #[test]
    fn configurations_distinguish_keys_values_and_machine_tokens() {
        assert_eq!(candidates("a.json", r#"{"foreign key":"Human message","url":"https://example.invalid","technical":"API"}"#).len(), 1);
        assert_eq!(
            candidates(
                "a.toml",
                "\"foreign key\" = \"Human message\"\nurl = \"https://example.invalid\"\n"
            )
            .len(),
            1
        );
        assert_eq!(
            candidates(
                "a.yaml",
                "foreign key: Human message\nurl: https://example.invalid\n"
            )
            .len(),
            1
        );
        assert_eq!(
            candidates(
                "a.yaml",
                "description: |\n  Human message\n  Another message\nkey: API\n"
            )
            .len(),
            2
        );
    }

    #[test]
    fn scripts_scan_comments_and_strings() {
        assert_eq!(
            candidates(
                "a.sh",
                "#!/bin/bash\n# Human comment\necho 'Human output'\n"
            )
            .len(),
            2
        );
        assert_eq!(
            candidates(
                "a.ps1",
                "<# Human comment #>\nWrite-Host \"Human output\"\n"
            )
            .len(),
            2
        );
    }

    #[test]
    fn data_is_excluded_and_order_is_stable() {
        assert!(candidates("decks/任意/deck.json", r#"{"fields":["Human message"]}"#).is_empty());
        let a = SourceFile {
            path: "a.rs".into(),
            content: "// Human comment\n".into(),
        };
        let b = SourceFile {
            path: "b.md".into(),
            content: "Human prose\n".into(),
        };
        assert_eq!(scan(&[a.clone(), b.clone()]), scan(&[b, a]));
        let old = candidates("a.rs", "// Human comment\n").remove(0);
        let shifted = candidates("a.rs", "\n// Human comment\n").remove(0);
        assert_eq!(old.id, shifted.id);
        assert_ne!(old.source_sha256, shifted.source_sha256);
    }

    #[test]
    fn apply_preserves_crlf_and_is_dry_by_default() {
        let temp = TempDir::new("language-apply");
        let source = "// Human comment\r\nfn main() {}\r\n";
        fs::write(temp.path().join("a.rs"), source).unwrap();
        let approved = decisions(vec![replacement("a.rs", source, "Русский комментарий")]);
        let dry = apply(temp.path(), &approved, false).unwrap();
        assert!(!dry.applied);
        assert_eq!(
            fs::read_to_string(temp.path().join("a.rs")).unwrap(),
            source
        );
        assert_eq!(
            dry.files[0].after,
            "// Русский комментарий\r\nfn main() {}\r\n"
        );
        let actual = apply(temp.path(), &approved, true).unwrap();
        assert_eq!(actual.files, dry.files);
        assert_eq!(
            fs::read_to_string(temp.path().join("a.rs")).unwrap(),
            dry.files[0].after
        );
    }

    #[test]
    fn allow_and_ignore_are_not_written() {
        let temp = TempDir::new("language-allow");
        let mut decision = replacement("absent.rs", "// Human comment", "Комментарий");
        decision.action = LanguageAction::Allow;
        assert!(
            apply(temp.path(), &decisions(vec![decision]), true)
                .unwrap()
                .files
                .is_empty()
        );
    }

    #[test]
    fn all_preconditions_run_before_first_write() {
        let temp = TempDir::new("language-preflight");
        let source = "// Human comment\n";
        fs::write(temp.path().join("a.rs"), source).unwrap();
        fs::write(temp.path().join("z.rs"), "// Changed comment\n").unwrap();
        let approved = decisions(vec![
            replacement("a.rs", source, "Комментарий"),
            replacement("z.rs", source, "Комментарий"),
        ]);
        assert!(apply(temp.path(), &approved, true).is_err());
        assert_eq!(
            fs::read_to_string(temp.path().join("a.rs")).unwrap(),
            source
        );
    }

    #[test]
    fn rejects_stale_anchor_overlap_traversal_and_syntax_changes() {
        let temp = TempDir::new("language-refusal");
        let source = "let value = \"Human message\";\n";
        fs::write(temp.path().join("a.rs"), source).unwrap();
        let valid = replacement("a.rs", source, "Сообщение");
        let mut stale = valid.clone();
        stale.candidate.source_sha256 = "0".repeat(64);
        assert!(apply(temp.path(), &decisions(vec![stale]), true).is_err());
        let mut anchor = valid.clone();
        anchor.candidate.text = "Changed message".into();
        assert!(apply(temp.path(), &decisions(vec![anchor]), true).is_err());
        assert!(
            apply(
                temp.path(),
                &decisions(vec![valid.clone(), valid.clone()]),
                true
            )
            .is_err()
        );
        for path in [
            "../a.rs",
            "/a.rs",
            "decks/a.rs",
            ".git/a.rs",
            "media/a.rs",
            "a\\b.rs",
        ] {
            let mut escape = valid.clone();
            escape.candidate.path = path.into();
            assert!(apply(temp.path(), &decisions(vec![escape]), true).is_err());
        }
        let mut injection = valid;
        injection.replacement = Some("\"; panic!(\"Сообщение".into());
        assert!(apply(temp.path(), &decisions(vec![injection]), true).is_err());
        assert_eq!(
            fs::read_to_string(temp.path().join("a.rs")).unwrap(),
            source
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_in_leaf_or_parent() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new("language-symlink");
        let source = "// Human comment\n";
        fs::write(temp.path().join("real.rs"), source).unwrap();
        symlink(temp.path().join("real.rs"), temp.path().join("a.rs")).unwrap();
        assert!(
            apply(
                temp.path(),
                &decisions(vec![replacement("a.rs", source, "Комментарий")]),
                true
            )
            .is_err()
        );
        symlink(temp.path(), temp.path().join("nested")).unwrap();
        assert!(
            apply(
                temp.path(),
                &decisions(vec![replacement("nested/real.rs", source, "Комментарий")]),
                true
            )
            .is_err()
        );
    }
}
