//! Dispatch и рендеринг: единственное место, где CLI встречается с доменной
//! логикой.
//!
//! [`execute`] полностью готовит stdout и exit code команды и не пишет в
//! process stdio, поэтому контракт проверяем без обязательного subprocess.
//! Единственное чтение из process stdio — `edit --request -`.

use std::io::Read;
use std::path::Path;

use crate::cli::{Cli, Command, MatchArg};
use crate::error::{DomainError, ErrorCode};
use crate::index::ExportIndex;
use crate::loader::load_export;
use crate::ops::edit as edit_op;
use crate::ops::edit::{EditRequest, EditSpec, STDIN_REQUEST_SOURCE};
use crate::ops::find as find_op;
use crate::ops::find::{FindCriteria, FindQuery, MatchMode, WORD_SHORTCUT_FIELD};
use crate::ops::inspect as inspect_op;
use crate::ops::stats::{StatsQuery, stats as stats_op};
use crate::ops::validate::validate as validate_op;
use crate::render::{human, json};

/// Полностью подготовленный к печати результат команды.
#[derive(Debug)]
pub struct Rendered {
    /// Имя команды для JSON envelope.
    pub command: &'static str,
    /// Текст, который должен попасть в stdout.
    pub stdout: String,
    /// Process exit code успешного выполнения.
    pub exit: u8,
}

/// Выполняет команду и готовит её вывод.
///
/// # Errors
///
/// Возвращает [`DomainError`] для любой доменной проблемы.
pub fn execute(cli: &Cli) -> Result<Rendered, DomainError> {
    match &cli.command {
        Command::Inspect {
            export_dir,
            verbose,
        } => {
            let loaded = load_export(export_dir)?;
            let result = inspect_op::inspect(
                &loaded.export_dir,
                &loaded.deck_json,
                &loaded.root,
                *verbose,
            );
            Ok(Rendered {
                command: "inspect",
                stdout: if cli.json {
                    json::inspect_json(&result)
                } else {
                    human::inspect(&result)
                },
                exit: 0,
            })
        }

        Command::Find {
            export_dir,
            guid,
            word,
            field,
            value,
            match_mode,
            deck,
            limit,
        } => {
            let loaded = load_export(export_dir)?;
            let index = ExportIndex::build(&loaded.root);
            let query = FindQuery {
                criteria: build_criteria(
                    guid.as_deref(),
                    word.as_deref(),
                    field.as_deref(),
                    value.as_deref(),
                    *match_mode,
                )?,
                deck: deck.clone(),
                limit: usize::try_from(*limit).unwrap_or(usize::MAX),
            };
            let result = find_op::find(&loaded.export_dir, &index, &query)?;
            Ok(Rendered {
                command: "find",
                stdout: if cli.json {
                    json::find_json(&result)
                } else {
                    human::find(&result)
                },
                exit: 0,
            })
        }

        Command::Stats {
            export_dir,
            group_by,
            top,
        } => {
            let loaded = load_export(export_dir)?;
            let index = ExportIndex::build(&loaded.root);
            let query = StatsQuery {
                group_by: group_by.clone(),
                top: usize::try_from(top.unwrap_or(crate::cli::DEFAULT_TOP)).unwrap_or(usize::MAX),
            };
            let result = stats_op(&loaded.export_dir, &index, &query)?;
            Ok(Rendered {
                command: "stats",
                stdout: if cli.json {
                    json::stats_json(&result)
                } else {
                    human::stats(&result)
                },
                exit: 0,
            })
        }

        Command::Validate { export_dir } => {
            let result = validate_op(export_dir)?;
            Ok(Rendered {
                command: "validate",
                stdout: if cli.json {
                    json::validate_json(&result)
                } else {
                    human::validate(&result)
                },
                exit: if result.valid { 0 } else { 6 },
            })
        }

        Command::Edit {
            export_dir,
            request_file,
            guid,
            field,
            set,
            expect,
            apply,
        } => {
            let request = build_edit_request(
                request_file.as_deref(),
                guid.as_deref(),
                field.as_deref(),
                set.as_deref(),
                expect.as_deref(),
            )?;
            let result = edit_op::edit(export_dir, &request, *apply)?;
            Ok(Rendered {
                command: "edit",
                stdout: if cli.json {
                    json::edit_json(&result)
                } else {
                    human::edit(&result)
                },
                exit: 0,
            })
        }
    }
}

/// Собирает запрос на правку из аргументов CLI.
///
/// `--request` читается целиком и разбирается; `-` означает stdin.
fn build_edit_request(
    request_file: Option<&Path>,
    guid: Option<&str>,
    field: Option<&str>,
    set: Option<&str>,
    expect: Option<&str>,
) -> Result<EditRequest, DomainError> {
    if let Some(path) = request_file {
        let label = path.display().to_string();
        let raw = if label == STDIN_REQUEST_SOURCE {
            read_stdin_request()?
        } else {
            std::fs::read(path).map_err(|error| {
                DomainError::with_details(
                    ErrorCode::InputUnreadable,
                    format!("не удалось прочитать запрос {label}: {error}"),
                    crate::details! {
                        "path" => label.as_str(),
                        "io_error" => error.to_string(),
                    },
                )
            })?
        };

        return edit_op::parse_request_bytes(&raw, &label);
    }

    let (Some(guid), Some(field), Some(set), Some(expect)) = (guid, field, set, expect) else {
        return Err(DomainError::new(
            ErrorCode::Usage,
            "для правки нужен либо --request, либо --guid с --field, --set и --expect",
        ));
    };

    let request = EditRequest {
        edits: vec![EditSpec {
            edit_id: None,
            guid: guid.to_string(),
            field: field.to_string(),
            expected: expect.to_string(),
            replacement: set.to_string(),
        }],
    };
    edit_op::validate_request(&request)?;
    Ok(request)
}

/// Читает JSON-запрос целиком со stdin.
fn read_stdin_request() -> Result<Vec<u8>, DomainError> {
    let mut raw = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut raw)
        .map_err(|error| {
            DomainError::with_details(
                ErrorCode::InputUnreadable,
                format!("не удалось прочитать запрос со stdin: {error}"),
                crate::details! {
                    "path" => STDIN_REQUEST_SOURCE,
                    "io_error" => error.to_string(),
                },
            )
        })?;
    Ok(raw)
}

fn build_criteria(
    guid: Option<&str>,
    word: Option<&str>,
    field: Option<&str>,
    value: Option<&str>,
    match_mode: Option<MatchArg>,
) -> Result<FindCriteria, DomainError> {
    // Проверяется здесь, а не только в clap: --match осмысленен исключительно
    // вместе с --field.
    if match_mode.is_some() && field.is_none() {
        return Err(DomainError::new(
            ErrorCode::Usage,
            "аргумент --match применяется только вместе с --field",
        ));
    }
    if let Some(guid) = guid {
        return Ok(FindCriteria::Guid {
            guid: guid.to_string(),
        });
    }
    if let Some(word) = word {
        return Ok(FindCriteria::Field {
            field: WORD_SHORTCUT_FIELD.to_string(),
            value: word.to_string(),
            mode: MatchMode::Contains,
        });
    }
    if let Some(field) = field {
        return Ok(FindCriteria::Field {
            field: field.to_string(),
            value: value.unwrap_or_default().to_string(),
            mode: match match_mode {
                Some(MatchArg::Exact) => MatchMode::Exact,
                _ => MatchMode::Contains,
            },
        });
    }
    Err(DomainError::new(
        ErrorCode::Usage,
        "не задан критерий поиска: нужен один из --guid, --word или --field",
    ))
}

/// Готовит stdout/stderr для доменной ошибки.
///
/// В JSON mode stdout содержит только один валидный JSON document, а stderr
/// остаётся пустым. В human mode сообщение уходит в stderr, а stdout пуст.
pub fn render_error(command: &str, json_mode: bool, error: &DomainError) -> (String, String) {
    if json_mode {
        (json::error_json(command, error), String::new())
    } else {
        (
            String::new(),
            format!(
                "anki-repo: ошибка [{code}]: {message}\n",
                code = error.code.as_str(),
                message = error.message
            ),
        )
    }
}
