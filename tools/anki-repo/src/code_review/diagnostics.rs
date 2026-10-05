//! Машинный адаптер диагностик Cargo/rustc/Clippy для свидетельств code-review.
//!
//! Парсер принимает только JSON-сообщения Cargo и намеренно не разбирает
//! человекочитаемый stdout/stderr. Диагностики сохраняются как свидетельства: их
//! уровень сам по себе не превращает запись в подтверждённое замечание ревью.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Максимальная длина однострочной сводки stderr.
pub const STDERR_SUMMARY_MAX_CHARS: usize = 2_000;

/// Известный или будущий уровень rustc-диагностики.
///
/// Неизвестные значения сохраняются как строки, чтобы новая версия rustc не
/// делала старый сборщик свидетельств непригодным.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Note,
    Help,
    FailureNote,
    Unknown(String),
}

impl DiagnosticSeverity {
    fn from_level(level: String) -> Self {
        match level.as_str() {
            "error" => Self::Error,
            "warning" => Self::Warning,
            "note" => Self::Note,
            "help" => Self::Help,
            "failure-note" => Self::FailureNote,
            _ => Self::Unknown(level),
        }
    }

    fn as_level(&self) -> &str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Note => "note",
            Self::Help => "help",
            Self::FailureNote => "failure-note",
            Self::Unknown(level) => level,
        }
    }
}

impl Serialize for DiagnosticSeverity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_level())
    }
}

impl<'de> Deserialize<'de> for DiagnosticSeverity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SeverityVisitor;

        impl<'visitor> Visitor<'visitor> for SeverityVisitor {
            type Value = DiagnosticSeverity;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("строку с уровнем rustc-диагностики")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(DiagnosticSeverity::from_level(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(DiagnosticSeverity::from_level(value))
            }
        }

        deserializer.deserialize_string(SeverityVisitor)
    }
}

/// Источник нормализованной диагностики.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticTool {
    Cargo,
    Rustc,
    Clippy,
}

/// Код компилятора или lint-код, если он присутствует в сообщении.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticCode {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

/// Строка исходника и выделенный в ней диапазон из диагностики Cargo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticSourceLine {
    pub text: String,
    pub highlight_start: u64,
    pub highlight_end: u64,
}

/// Локатор и исходный фрагмент одного диапазона rustc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticSpan {
    pub file_name: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub line_start: u64,
    pub line_end: u64,
    pub column_start: u64,
    pub column_end: u64,
    pub is_primary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_replacement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applicability: Option<String>,
    pub source_lines: Vec<DiagnosticSourceLine>,
}

/// Происхождение сообщения в графе сборки Cargo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticSource {
    pub tool: DiagnosticTool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
}

/// Структурированная диагностика без заключения о корректности изменения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeReviewDiagnostic {
    pub severity: DiagnosticSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<DiagnosticCode>,
    pub message: String,
    pub spans: Vec<DiagnosticSpan>,
    /// Вложенные примечания и подсказки rustc, сохранённые вместе с исходной диагностикой.
    pub children: Vec<CodeReviewDiagnostic>,
    pub source: DiagnosticSource,
}

/// Результат разбора строк Cargo JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoDiagnosticParse {
    pub diagnostics: Vec<CodeReviewDiagnostic>,
    /// Непустые строки, которые не удалось разобрать как JSON или сообщение компилятора.
    pub malformed_lines: usize,
    /// Валидные JSON-записи Cargo, не относящиеся к `compiler-message`.
    pub ignored_records: usize,
}

/// Итог локального запуска анализатора.
///
/// `diagnostics` остаются доступны и при ненулевом коде выхода. Результат
/// определяется запуском процесса и целостностью JSON-потока, а не наличием
/// `warning`/`error` или их количеством.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRun {
    pub tool: DiagnosticTool,
    pub status: ToolRunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<ExitStatusSummary>,
    pub diagnostics: Vec<CodeReviewDiagnostic>,
    pub malformed_lines: usize,
    pub ignored_records: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Состояние запуска внешнего анализатора.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRunStatus {
    /// Cargo не удалось запустить, например, потому что его нет в `PATH`.
    Unavailable,
    /// Процесс завершился с ошибкой или JSON-поток нельзя было полностью прочитать.
    Failed,
    /// Процесс завершился успешно и структурированных диагностик нет.
    SuccessWithoutDiagnostics,
    /// Процесс завершился успешно и выдал одну или несколько диагностик.
    Diagnostics,
}

/// Сводка состояния завершения, включая завершение по сигналу без числового кода.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatusSummary {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
}

impl From<ExitStatus> for ExitStatusSummary {
    fn from(status: ExitStatus) -> Self {
        Self {
            success: status.success(),
            code: status.code(),
        }
    }
}

/// Разбирает Cargo `--message-format=json`; посторонние записи
/// игнорируются, неизвестные поля и уровни сохраняются/пропускаются безопасно.
#[must_use]
pub fn parse_cargo_json_output(output: &str, tool: DiagnosticTool) -> CargoDiagnosticParse {
    let mut parsed = CargoDiagnosticParse::default();
    for line in output.lines() {
        parse_cargo_json_line(line, tool, &mut parsed);
    }
    parsed
}

/// Запускает один локальный анализ Clippy из корня рабочего пространства.
///
/// `--offline` и `CARGO_NET_OFFLINE=true` запрещают Cargo обращаться в сеть;
/// `RUSTUP_AUTO_INSTALL=0` не позволяет rustup автоматически скачивать
/// недостающий toolchain. Расширения и компоненты не устанавливаются.
/// Эти параметры не изолируют проект: Cargo исполняет build.rs и proc-macro
/// с правами текущего пользователя. Вызывающий код обязан получить явное
/// разрешение на исполнение, прежде чем вызывать эту функцию.
#[must_use]
pub fn run_local_clippy(repo_root: &Path) -> ToolRun {
    if !repo_root.is_dir() {
        return failed_run(
            None,
            None,
            "Не удалось открыть каталог корня репозитория для Clippy.".to_owned(),
        );
    }

    let mut command = Command::new("cargo");
    command
        .arg("clippy")
        .arg("--workspace")
        .arg("--all-targets")
        .arg("--locked")
        .arg("--offline")
        .arg("--quiet")
        .arg("--message-format=json")
        .current_dir(repo_root)
        .env("CARGO_NET_OFFLINE", "true")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let status = status_for_spawn_error(&error);
            let message = match status {
                ToolRunStatus::Unavailable => {
                    "Исполняемый файл cargo не найден в PATH; Clippy не запускался.".to_owned()
                }
                _ => format!("Не удалось запустить Cargo для Clippy: {error}"),
            };
            return ToolRun {
                tool: DiagnosticTool::Clippy,
                status,
                exit_status: None,
                diagnostics: Vec::new(),
                malformed_lines: 0,
                ignored_records: 0,
                stderr_summary: None,
                message: Some(message),
            };
        }
    };

    let stdout = child.stdout.take().expect("stdout был настроен как pipe");
    let stderr = child.stderr.take().expect("stderr был настроен как pipe");
    let stderr_reader = thread::spawn(move || read_bounded_stderr_summary(stderr));

    let mut parsed = CargoDiagnosticParse::default();
    let mut stdout_error = None;
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) => parse_cargo_json_line(&line, DiagnosticTool::Clippy, &mut parsed),
            Err(error) => {
                stdout_error = Some(error.to_string());
                break;
            }
        }
    }

    let exit_status = match child.wait() {
        Ok(status) => Some(ExitStatusSummary::from(status)),
        Err(error) => {
            stdout_error.get_or_insert_with(|| {
                format!("не удалось получить состояние завершения: {error}")
            });
            None
        }
    };
    let stderr_summary = stderr_reader.join().ok().and_then(Result::ok).flatten();

    let status = classify_completion(
        exit_status.is_some_and(|status| status.success),
        &parsed,
        stdout_error.is_some(),
    );
    let message = match status {
        ToolRunStatus::Unavailable => None,
        ToolRunStatus::Failed if stdout_error.is_some() => Some(format!(
            "Не удалось полностью прочитать вывод Clippy: {}",
            stdout_error.as_deref().unwrap_or_default()
        )),
        ToolRunStatus::Failed if parsed.malformed_lines > 0 => Some(format!(
            "Вывод Clippy содержит {} непонятных JSON-строк; свидетельства могут быть неполными.",
            parsed.malformed_lines
        )),
        ToolRunStatus::Failed => Some("Clippy завершился с ненулевым кодом выхода.".to_owned()),
        ToolRunStatus::SuccessWithoutDiagnostics | ToolRunStatus::Diagnostics => None,
    };

    ToolRun {
        tool: DiagnosticTool::Clippy,
        status,
        exit_status,
        diagnostics: parsed.diagnostics,
        malformed_lines: parsed.malformed_lines,
        ignored_records: parsed.ignored_records,
        stderr_summary,
        message,
    }
}

#[derive(Debug, Deserialize)]
struct CargoRecord {
    package_id: Option<String>,
    target: Option<CargoTarget>,
    message: Option<CargoCompilerDiagnostic>,
}

#[derive(Debug, Deserialize)]
struct CargoTarget {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CargoCompilerDiagnostic {
    message: String,
    level: DiagnosticSeverity,
    #[serde(default)]
    code: Option<CargoCode>,
    #[serde(default)]
    spans: Vec<CargoSpan>,
    #[serde(default)]
    children: Vec<CargoCompilerDiagnostic>,
}

#[derive(Debug, Deserialize)]
struct CargoCode {
    code: String,
    #[serde(default)]
    explanation: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CargoSpan {
    #[serde(default)]
    file_name: String,
    #[serde(default)]
    byte_start: u64,
    #[serde(default)]
    byte_end: u64,
    #[serde(default)]
    line_start: u64,
    #[serde(default)]
    line_end: u64,
    #[serde(default)]
    column_start: u64,
    #[serde(default)]
    column_end: u64,
    #[serde(default)]
    is_primary: bool,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    suggested_replacement: Option<String>,
    #[serde(default)]
    applicability: Option<String>,
    #[serde(default, rename = "text")]
    source_lines: Vec<CargoSourceLine>,
}

#[derive(Debug, Deserialize)]
struct CargoSourceLine {
    #[serde(default)]
    text: String,
    #[serde(default)]
    highlight_start: u64,
    #[serde(default)]
    highlight_end: u64,
}

fn parse_cargo_json_line(line: &str, tool: DiagnosticTool, parsed: &mut CargoDiagnosticParse) {
    if line.trim().is_empty() {
        return;
    }

    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => {
            parsed.malformed_lines += 1;
            return;
        }
    };
    if value.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-message") {
        parsed.ignored_records += 1;
        return;
    }
    let record: CargoRecord = match serde_json::from_value(value) {
        Ok(record) => record,
        Err(_) => {
            parsed.malformed_lines += 1;
            return;
        }
    };

    let Some(message) = record.message else {
        parsed.malformed_lines += 1;
        return;
    };
    let source = DiagnosticSource {
        tool,
        package_id: record.package_id,
        target_name: record.target.and_then(|target| target.name),
    };
    parsed
        .diagnostics
        .push(normalize_compiler_diagnostic(message, &source));
}

fn normalize_compiler_diagnostic(
    diagnostic: CargoCompilerDiagnostic,
    source: &DiagnosticSource,
) -> CodeReviewDiagnostic {
    CodeReviewDiagnostic {
        severity: diagnostic.level,
        code: diagnostic.code.map(|code| DiagnosticCode {
            code: code.code,
            explanation: code.explanation,
        }),
        message: diagnostic.message,
        spans: diagnostic
            .spans
            .into_iter()
            .map(|span| DiagnosticSpan {
                file_name: span.file_name,
                byte_start: span.byte_start,
                byte_end: span.byte_end,
                line_start: span.line_start,
                line_end: span.line_end,
                column_start: span.column_start,
                column_end: span.column_end,
                is_primary: span.is_primary,
                label: span.label,
                suggested_replacement: span.suggested_replacement,
                applicability: span.applicability,
                source_lines: span
                    .source_lines
                    .into_iter()
                    .map(|line| DiagnosticSourceLine {
                        text: line.text,
                        highlight_start: line.highlight_start,
                        highlight_end: line.highlight_end,
                    })
                    .collect(),
            })
            .collect(),
        children: diagnostic
            .children
            .into_iter()
            .map(|child| normalize_compiler_diagnostic(child, source))
            .collect(),
        source: source.clone(),
    }
}

fn status_for_spawn_error(error: &io::Error) -> ToolRunStatus {
    if error.kind() == io::ErrorKind::NotFound {
        ToolRunStatus::Unavailable
    } else {
        ToolRunStatus::Failed
    }
}

fn classify_completion(
    process_succeeded: bool,
    parsed: &CargoDiagnosticParse,
    stdout_failed: bool,
) -> ToolRunStatus {
    if !process_succeeded || stdout_failed || parsed.malformed_lines > 0 {
        return ToolRunStatus::Failed;
    }
    if parsed.diagnostics.is_empty() {
        ToolRunStatus::SuccessWithoutDiagnostics
    } else {
        ToolRunStatus::Diagnostics
    }
}

fn failed_run(
    exit_status: Option<ExitStatusSummary>,
    stderr_summary: Option<String>,
    message: String,
) -> ToolRun {
    ToolRun {
        tool: DiagnosticTool::Clippy,
        status: ToolRunStatus::Failed,
        exit_status,
        diagnostics: Vec::new(),
        malformed_lines: 0,
        ignored_records: 0,
        stderr_summary,
        message: Some(message),
    }
}

fn read_bounded_stderr_summary(mut reader: impl Read) -> io::Result<Option<String>> {
    let mut bytes = [0_u8; 4_096];
    let mut summary = String::new();
    let mut line = String::new();
    let mut line_chars = 0_usize;
    let mut line_truncated = false;
    let mut truncated = false;

    loop {
        let count = reader.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&bytes[..count]);
        for character in text.chars() {
            if character == '\n' {
                append_stable_stderr_line(&mut summary, &line, line_truncated, &mut truncated);
                line.clear();
                line_chars = 0;
                line_truncated = false;
                continue;
            }
            if line_chars <= STDERR_SUMMARY_MAX_CHARS {
                line.push(character);
                line_chars += 1;
            } else {
                line_truncated = true;
            }
        }
    }
    append_stable_stderr_line(&mut summary, &line, line_truncated, &mut truncated);

    if truncated {
        summary.push('…');
    }
    if summary.is_empty() {
        Ok(None)
    } else {
        Ok(Some(summary))
    }
}

fn append_stable_stderr_line(
    summary: &mut String,
    line: &str,
    line_truncated: bool,
    truncated: &mut bool,
) {
    // Даже при --quiet не сохраняем известные строки прогресса Cargo:
    // время сборки и ожидания блокировок не являются фактами о Git-снимке.
    let trimmed = line.trim();
    if [
        "Compiling ",
        "Checking ",
        "Finished ",
        "Fresh ",
        "Blocking ",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
    {
        return;
    }
    *truncated |= line_truncated;
    let mut chars = summary.chars().count();
    let mut pending_space = !summary.is_empty();
    for character in trimmed.chars() {
        if character.is_whitespace() || character.is_control() {
            pending_space = !summary.is_empty();
            continue;
        }
        let additional = if pending_space { 2 } else { 1 };
        if chars + additional > STDERR_SUMMARY_MAX_CHARS {
            *truncated = true;
            break;
        }
        if pending_space {
            summary.push(' ');
            chars += 1;
            pending_space = false;
        }
        summary.push(character);
        chars += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn parses_code_level_spans_and_source() {
        let output = r#"{"reason":"compiler-message","package_id":"path+file:///repo#anki-repo@0.1.0","target":{"name":"anki_repo","kind":["lib"]},"message":{"message":"unnecessary borrow","code":{"code":"clippy::needless_borrow","explanation":"A borrow can be removed."},"level":"warning","spans":[{"file_name":"tools/anki-repo/src/lib.rs","byte_start":12,"byte_end":20,"line_start":2,"line_end":2,"column_start":3,"column_end":11,"is_primary":true,"label":"remove this borrow","suggested_replacement":"value","applicability":"MachineApplicable","text":[{"text":"let x = &value;","highlight_start":9,"highlight_end":15}]}],"children":[{"message":"help child","level":"help","spans":[]}],"rendered":"warning: unnecessary borrow"}}"#;

        let parsed = parse_cargo_json_output(output, DiagnosticTool::Clippy);
        assert_eq!(parsed.malformed_lines, 0);
        assert_eq!(parsed.diagnostics.len(), 1);
        let diagnostic = &parsed.diagnostics[0];
        assert_eq!(diagnostic.severity, DiagnosticSeverity::Warning);
        assert_eq!(
            diagnostic.code.as_ref().map(|code| code.code.as_str()),
            Some("clippy::needless_borrow")
        );
        assert_eq!(diagnostic.message, "unnecessary borrow");
        assert_eq!(diagnostic.source.tool, DiagnosticTool::Clippy);
        assert_eq!(diagnostic.source.target_name.as_deref(), Some("anki_repo"));
        assert_eq!(diagnostic.spans.len(), 1);
        assert_eq!(diagnostic.spans[0].file_name, "tools/anki-repo/src/lib.rs");
        assert_eq!(diagnostic.spans[0].line_start, 2);
        assert!(diagnostic.spans[0].is_primary);
        assert_eq!(diagnostic.spans[0].source_lines[0].text, "let x = &value;");
        assert_eq!(
            diagnostic.spans[0].suggested_replacement.as_deref(),
            Some("value")
        );
        assert_eq!(diagnostic.children.len(), 1);
        assert_eq!(diagnostic.children[0].message, "help child");
        assert_eq!(diagnostic.children[0].severity, DiagnosticSeverity::Help);
    }

    #[test]
    fn ignores_unknown_fields_and_preserves_unknown_levels() {
        let output = r#"{"reason":"compiler-message","future_record_field":{"any":[1,true]},"package_id":"pkg","future_target_field":"ignored","target":{"name":"future_target","crate_types":["bin"],"future":"ignored"},"message":{"message":"future diagnostic","level":"catastrophic-future-level","future_message_field":42,"spans":[{"file_name":"src/main.rs","is_primary":false,"future_span_field":{} }]}}"#;

        let parsed = parse_cargo_json_output(output, DiagnosticTool::Rustc);
        assert_eq!(parsed.malformed_lines, 0);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(
            parsed.diagnostics[0].severity,
            DiagnosticSeverity::Unknown("catastrophic-future-level".to_owned())
        );
        assert_eq!(
            serde_json::to_string(&parsed.diagnostics[0].severity)
                .expect("уровень должен сериализоваться"),
            "\"catastrophic-future-level\""
        );
        assert_eq!(parsed.diagnostics[0].spans[0].line_start, 0);
    }

    #[test]
    fn malformed_and_unrelated_records_do_not_abort_parsing() {
        let output = concat!(
            "not json\n",
            "{\"reason\":\"build-finished\",\"success\":true}\n",
            "{\"reason\":\"future-event\",\"message\":\"not a compiler diagnostic\"}\n",
            "{\"reason\":\"compiler-message\",\"package_id\":\"pkg\"}\n",
            "{\"reason\":\"compiler-message\",\"message\":{\"message\":\"valid\",\"level\":\"note\"}}"
        );

        let parsed = parse_cargo_json_output(output, DiagnosticTool::Cargo);
        assert_eq!(parsed.malformed_lines, 2);
        assert_eq!(parsed.ignored_records, 2);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].severity, DiagnosticSeverity::Note);
    }

    #[test]
    fn analyzer_outcomes_distinguish_unavailable_failure_and_empty_success() {
        let missing = io::Error::from(io::ErrorKind::NotFound);
        let other = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(status_for_spawn_error(&missing), ToolRunStatus::Unavailable);
        assert_eq!(status_for_spawn_error(&other), ToolRunStatus::Failed);

        let empty = CargoDiagnosticParse::default();
        assert_eq!(
            classify_completion(true, &empty, false),
            ToolRunStatus::SuccessWithoutDiagnostics
        );
        assert_eq!(
            classify_completion(false, &empty, false),
            ToolRunStatus::Failed
        );
        assert_eq!(
            classify_completion(true, &empty, true),
            ToolRunStatus::Failed
        );

        let with_diagnostics = parse_cargo_json_output(
            r#"{"reason":"compiler-message","message":{"message":"lint","level":"warning"}}"#,
            DiagnosticTool::Clippy,
        );
        assert_eq!(
            classify_completion(true, &with_diagnostics, false),
            ToolRunStatus::Diagnostics
        );
        assert_eq!(
            classify_completion(false, &with_diagnostics, false),
            ToolRunStatus::Failed
        );
    }

    #[test]
    fn successful_cargo_output_can_have_zero_diagnostics() {
        let parsed = parse_cargo_json_output(
            concat!(
                "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"lib\"}}\n",
                "{\"reason\":\"build-finished\",\"success\":true}"
            ),
            DiagnosticTool::Clippy,
        );
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(parsed.ignored_records, 2);
        assert_eq!(
            classify_completion(true, &parsed, false),
            ToolRunStatus::SuccessWithoutDiagnostics
        );
    }

    #[test]
    fn stderr_summary_is_single_line_and_bounded() {
        let stderr = format!(
            "первое\nвторое\t{}",
            "x".repeat(STDERR_SUMMARY_MAX_CHARS + 20)
        );
        let summary = read_bounded_stderr_summary(Cursor::new(stderr.into_bytes()))
            .expect("stderr должен читаться без ошибки");
        let summary = summary.expect("сводка не должна быть пустой");
        assert_eq!(summary.chars().count(), STDERR_SUMMARY_MAX_CHARS + 1);
        assert!(summary.starts_with("первое второе "));
        assert!(summary.ends_with('…'));
        assert!(!summary.contains('\n'));
    }

    #[test]
    fn stderr_progress_is_excluded_but_useful_warnings_and_errors_remain() {
        let first = "    Checking fixture v0.1.0\nwarning: полезное сообщение\n    Finished `dev` profile in 0.04s\nerror: полезная ошибка\n";
        let second = "    Blocking waiting for file lock\nwarning: полезное сообщение\n    Finished `dev` profile in 12.90s\nerror: полезная ошибка\n";
        let first = read_bounded_stderr_summary(Cursor::new(first)).unwrap();
        let second = read_bounded_stderr_summary(Cursor::new(second)).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.as_deref(),
            Some("warning: полезное сообщение error: полезная ошибка")
        );
    }
}
