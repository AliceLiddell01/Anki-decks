//! Выбор операции и подготовка вывода: единственное место, где CLI встречается
//! с предметной логикой.
//!
//! [`execute`] полностью готовит stdout и код возврата команды, не обращаясь
//! напрямую к стандартным потокам процесса, поэтому контракт можно проверять
//! без отдельного процесса. Единственное чтение из стандартного ввода —
//! `edit --request -`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde::Serialize;

use crate::cli::{
    Cli, CodeReviewCommand, Command, LanguageCommand, MatchArg, ReviewExecutionCommand,
    ReviewQueueCommand, SemanticTriageCommand,
};
use crate::code_review::model::CandidateStatus;
use crate::code_review::workflow::{LanguageSummary, SnapshotSummary, display_counts};
use crate::error::{DomainError, ErrorCode};
use crate::index::ExportIndex;
use crate::loader::load_export;
use crate::ops::create as create_op;
use crate::ops::deck_select::DeckSelector;
use crate::ops::edit as edit_op;
use crate::ops::edit::{EditRequest, EditSpec, STDIN_REQUEST_SOURCE};
use crate::ops::find as find_op;
use crate::ops::find::{FindCriteria, FindQuery, MatchMode};
use crate::ops::inspect as inspect_op;
use crate::ops::migrate_media as migrate_media_op;
use crate::ops::models as models_op;
use crate::ops::models::ModelsQuery;
use crate::ops::qa as qa_op;
use crate::ops::qa::QaQuery;
use crate::ops::retire as retire_op;
use crate::ops::retire::{RetireRequest, RetireSpec};
use crate::ops::review as review_op;
use crate::ops::review::{ReviewCriteria, ReviewQuery};
use crate::ops::review_check as review_check_op;
use crate::ops::source as source_op;
use crate::ops::stats::{StatsQuery, stats as stats_op};
use crate::ops::validate::validate as validate_op;
use crate::ops::visual_report as visual_report_op;
use crate::ops::visual_report::ReportRequest;
use crate::proposal;
use crate::render::{human, json};

/// Полностью подготовленный к печати результат команды.
#[derive(Debug)]
pub struct Rendered {
    /// Имя команды для оболочки JSON.
    pub command: &'static str,
    /// Текст, который должен попасть в stdout.
    pub stdout: String,
    /// Код возврата при успешном выполнении.
    pub exit: u8,
}

const LANGUAGE_CHECK_SAMPLE_LIMIT: usize = 20;

#[derive(Serialize)]
struct LanguageCheckOutput {
    summary: Option<LanguageSummary>,
    files_total: usize,
    candidates_total: usize,
    candidates_truncated: bool,
    skipped_total: usize,
    candidates: Vec<LanguageCandidateSummary>,
}

#[derive(Serialize)]
struct LanguageCandidateSummary {
    id: String,
    path: String,
    line: usize,
    column: usize,
    context: crate::code_review::language::TextContext,
    text: String,
    text_truncated: bool,
}

#[derive(Serialize)]
struct SnapshotOutput {
    artifact_dir: String,
    review_queue_artifact: String,
    target: crate::code_review::scope::GitTarget,
    files: usize,
    candidates: usize,
    review_queue: crate::code_review::review_queue::QueueSummary,
    diagnostics: usize,
    tool_runs: Vec<crate::code_review::model::ToolRunEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<DeltaSummary>,
}

#[derive(Serialize)]
struct DeltaSummary {
    before: crate::code_review::model::SnapshotIdentity,
    after: crate::code_review::model::SnapshotIdentity,
    candidates_total: usize,
    candidate_status_counts: BTreeMap<&'static str, usize>,
    diagnostics_total: usize,
    diagnostic_status_counts: BTreeMap<&'static str, usize>,
    tool_runs: Vec<ToolRunDeltaSummary>,
}

#[derive(Serialize)]
struct ToolRunDeltaSummary {
    tool: String,
    before_status: String,
    after_status: String,
    status_changed: bool,
    before_diagnostics: usize,
    after_diagnostics: usize,
}

fn snapshot_output(summary: &SnapshotSummary) -> SnapshotOutput {
    let delta = summary.delta.as_ref().map(|delta| DeltaSummary {
        before: delta.before.clone(),
        after: delta.after.clone(),
        candidates_total: delta.candidates.len(),
        candidate_status_counts: status_counts(delta.candidates.iter().map(|change| change.status)),
        diagnostics_total: delta.diagnostics.len(),
        diagnostic_status_counts: status_counts(
            delta.diagnostics.iter().map(|change| change.status),
        ),
        tool_runs: delta
            .tool_runs
            .iter()
            .map(|run| ToolRunDeltaSummary {
                tool: run.tool.clone(),
                before_status: run.before_status.clone(),
                after_status: run.after_status.clone(),
                status_changed: run.status_changed,
                before_diagnostics: run.before_diagnostics,
                after_diagnostics: run.after_diagnostics,
            })
            .collect(),
    });
    SnapshotOutput {
        artifact_dir: summary.artifact_dir.clone(),
        review_queue_artifact: summary.review_queue_artifact.clone(),
        target: summary.target.clone(),
        files: summary.files,
        candidates: summary.candidates,
        review_queue: summary.review_queue.clone(),
        diagnostics: summary.diagnostics,
        tool_runs: summary.tool_runs.clone(),
        delta,
    }
}

fn status_counts(statuses: impl Iterator<Item = CandidateStatus>) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for status in statuses {
        let name = match status {
            CandidateStatus::StillPresent => "still_present",
            CandidateStatus::Gone => "gone",
            CandidateStatus::Changed => "changed",
            CandidateStatus::New => "new",
        };
        *counts.entry(name).or_insert(0) += 1;
    }
    counts
}

fn language_check_output(
    scan: crate::code_review::language::LanguageScan,
    summary: Option<LanguageSummary>,
) -> LanguageCheckOutput {
    let candidates_total = scan.candidates.len();
    let candidates = scan
        .candidates
        .iter()
        .take(LANGUAGE_CHECK_SAMPLE_LIMIT)
        .map(|candidate| LanguageCandidateSummary {
            id: candidate.id.clone(),
            path: candidate.path.clone(),
            line: candidate.line,
            column: candidate.column,
            context: candidate.context,
            text: crate::text::bounded_sample(&candidate.text),
            text_truncated: candidate
                .text
                .chars()
                .nth(crate::text::VALUE_SAMPLE_CHARS)
                .is_some(),
        })
        .collect();
    LanguageCheckOutput {
        summary,
        files_total: scan.files.len(),
        candidates_total,
        candidates_truncated: candidates_total > LANGUAGE_CHECK_SAMPLE_LIMIT,
        skipped_total: scan.skipped.len(),
        candidates,
    }
}

/// Выполняет команду и готовит её вывод.
///
/// # Ошибки
///
/// Возвращает [`DomainError`] для любой доменной проблемы.
pub fn execute(cli: &Cli) -> Result<Rendered, DomainError> {
    execute_with_cancellation(cli, &AtomicBool::new(false))
}

/// Выполняет CLI, устанавливая обработчики отмены только для `code-review execution run`.
pub fn execute_cli(cli: &Cli) -> Result<Rendered, DomainError> {
    let cancellation = Arc::new(AtomicBool::new(false));
    if cli.command_name() == "code-review execution run" {
        #[cfg(unix)]
        for (signal, name) in [
            (signal_hook::consts::SIGINT, "SIGINT"),
            (signal_hook::consts::SIGTERM, "SIGTERM"),
        ] {
            signal_hook::flag::register(signal, Arc::clone(&cancellation)).map_err(|error| {
                DomainError::new(
                    ErrorCode::Internal,
                    format!("не удалось установить обработчик {name}: {error}"),
                )
            })?;
        }
    }
    execute_with_cancellation(cli, &cancellation)
}

fn execute_with_cancellation(
    cli: &Cli,
    cancellation: &AtomicBool,
) -> Result<Rendered, DomainError> {
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

        Command::Qa {
            export_dir,
            codes,
            max_per_code,
        } => {
            let loaded = load_export(export_dir)?;
            let index = ExportIndex::build(&loaded.root);
            let query = QaQuery {
                codes: codes.clone(),
                max_per_code: to_usize(*max_per_code),
            };
            let result = qa_op::qa(&loaded.export_dir, &index, &query)?;
            Ok(Rendered {
                command: "qa",
                stdout: if cli.json {
                    json::qa_json(&result)
                } else {
                    human::qa(&result)
                },
                exit: 0,
            })
        }

        Command::Review {
            export_dir,
            all,
            guid,
            field,
            value,
            match_mode,
            qa_code,
            deck,
            offset,
            limit,
        } => {
            let loaded = load_export(export_dir)?;
            let index = ExportIndex::build(&loaded.root);
            let query = ReviewQuery {
                criteria: build_review_criteria(
                    *all,
                    guid.as_deref(),
                    field.as_deref(),
                    value.as_deref(),
                    *match_mode,
                    qa_code.as_deref(),
                )?,
                deck: deck.clone(),
                offset: to_usize(*offset),
                limit: to_usize(*limit),
            };
            let result = review_op::review(&loaded.export_dir, &index, &query)?;
            Ok(Rendered {
                command: "review",
                stdout: if cli.json {
                    json::review_json(&result)
                } else {
                    human::review(&result)
                },
                exit: 0,
            })
        }

        Command::ReviewCheck {
            export_dir,
            proposals_file,
        } => {
            let label = proposals_file.display().to_string();
            let raw = read_document(
                proposals_file,
                proposal::MAX_PROPOSAL_BYTES,
                "документ предложений",
            )?;
            let document = proposal::parse_proposal_bytes(&raw, &label)?;

            // Тот же набор проверок исходника, что и в мутирующем `edit`:
            // запрос выпускается только там, где граница записи приняла бы этот
            // экспорт. Иначе `review-check` обещал бы запрос, который `edit`
            // отклонит из-за неканонического или невалидного источника.
            let source = source_op::read_source(export_dir)?;
            let blockers = source_op::source_blockers(&source);
            let index = ExportIndex::build(&source.root);
            let result = review_check_op::review_check(export_dir, &index, &document, &blockers)?;
            Ok(Rendered {
                command: "review-check",
                stdout: if cli.json {
                    json::review_check_json(&result)
                } else {
                    human::review_check(&result)
                },
                exit: result.exit_code,
            })
        }

        Command::Models {
            export_dir,
            deck,
            deck_uuid,
            deck_preorder,
            sample_limit,
        } => {
            let query = ModelsQuery {
                deck: deck_selector(deck.clone(), deck_uuid.clone(), *deck_preorder),
                sample_limit: to_usize(*sample_limit),
            };
            let result = models_op::inspect(export_dir, &query)?;
            Ok(Rendered {
                command: "models",
                stdout: if cli.json {
                    json::models_json(&result)
                } else {
                    human::models(&result)
                },
                exit: 0,
            })
        }

        Command::Create {
            export_dir,
            request_file,
            apply,
            emit_resolved,
            create_config,
            asset_store,
            pitch_asset_store,
        } => {
            let raw = read_document(request_file, create_op::MAX_REQUEST_BYTES, "запрос")?;
            let label = request_file.display().to_string();
            let request = create_op::parse_request_bytes(&raw, &label)?;
            // Разрешённый запрос публикуется внутри самой операции и до
            // мутации экспорта: см. `create_op::create`.
            let result = create_op::create_with_options(
                export_dir,
                &request,
                *apply,
                emit_resolved.as_deref(),
                &crate::ops::create_media::MediaOptions {
                    config: create_config.clone(),
                    asset_store: asset_store.clone(),
                    pitch_asset_store: pitch_asset_store.clone(),
                },
            )?;
            Ok(Rendered {
                command: "create",
                stdout: if cli.json {
                    json::create_json(&result)
                } else {
                    human::create(&result)
                },
                exit: 0,
            })
        }

        Command::MigrateMedia {
            export_dir,
            namespace,
            key,
            from,
            apply,
            create_config,
            asset_store,
            pitch_asset_store,
        } => {
            let request = migrate_media_op::MigrationRequest {
                namespace: namespace.clone(),
                key: key.clone(),
                legacy_filename: from.clone(),
            };
            let result = migrate_media_op::migrate(
                export_dir,
                &request,
                &crate::ops::create_media::MediaOptions {
                    config: create_config.clone(),
                    asset_store: asset_store.clone(),
                    pitch_asset_store: pitch_asset_store.clone(),
                },
                *apply,
            )?;
            Ok(Rendered {
                command: "migrate-media",
                stdout: if cli.json {
                    json::migrate_media_json(&result)
                } else {
                    human::migrate_media(&result)
                },
                exit: 0,
            })
        }

        Command::Retire {
            export_dir,
            request_file,
            guid,
            tag,
            apply,
        } => {
            let request = build_retire_request(request_file.as_deref(), guid, tag.as_deref())?;
            let result = retire_op::retire(export_dir, &request, *apply)?;
            Ok(Rendered {
                command: "retire",
                stdout: if cli.json {
                    json::retire_json(&result)
                } else {
                    human::retire(&result)
                },
                exit: 0,
            })
        }

        Command::VisualReport {
            before,
            after,
            out,
            retire_tag,
            preview_limit,
        } => {
            let request = ReportRequest {
                before: before.clone(),
                after: after.clone(),
                out: out.clone(),
                retire_tag: retire_tag.clone(),
                preview_limit: to_usize(*preview_limit),
            };
            let result = visual_report_op::report(&request)?;
            Ok(Rendered {
                command: "visual-report",
                stdout: if cli.json {
                    json::visual_report_json(&result)
                } else {
                    human::visual_report(&result)
                },
                exit: 0,
            })
        }

        Command::CodeReview { command } => match command {
            CodeReviewCommand::Collect {
                base,
                head,
                out_dir,
                run_clippy,
                pr_number,
            } => {
                let result = crate::code_review::workflow::collect(
                    base,
                    head,
                    out_dir.as_deref(),
                    *run_clippy,
                    pr_number.as_deref(),
                )?;
                Ok(Rendered {
                    command: "code-review collect",
                    stdout: if cli.json {
                        json::generic_json("code-review collect", snapshot_output(&result))
                    } else {
                        human_snapshot(&result)
                    },
                    exit: 0,
                })
            }
            CodeReviewCommand::Verify {
                baseline,
                head,
                out_dir,
                run_clippy,
                pr_number,
            } => {
                let result = crate::code_review::workflow::verify(
                    baseline,
                    head,
                    out_dir.as_deref(),
                    *run_clippy,
                    pr_number.as_deref(),
                )?;
                Ok(Rendered {
                    command: "code-review verify",
                    stdout: if cli.json {
                        json::generic_json("code-review verify", snapshot_output(&result))
                    } else {
                        human_snapshot(&result)
                    },
                    exit: 0,
                })
            }
            CodeReviewCommand::Delta { before, after, out } => {
                let result =
                    crate::code_review::workflow::compare_files(before, after, out.as_deref())?;
                Ok(Rendered {
                    command: "code-review delta",
                    stdout: if cli.json {
                        json::generic_json("code-review delta", result)
                    } else {
                        human_delta(&result)
                    },
                    exit: 0,
                })
            }
            CodeReviewCommand::Execution { command } => match command {
                ReviewExecutionCommand::Prepare {
                    pack,
                    mode,
                    pr_number,
                    scope,
                } => {
                    let result = crate::code_review::workflow::prepare_execution_job(
                        pack,
                        *mode,
                        pr_number.as_deref(),
                        scope,
                    )?;
                    Ok(Rendered {
                        command: "code-review execution prepare",
                        stdout: if cli.json {
                            json::generic_json("code-review execution prepare", result)
                        } else {
                            format!(
                                "Задание подготовлено без запуска кода.\nID: {}\nКаталог: {}\nWorktree: {}\nHEAD снимка: {}\nНаправление: {}\n",
                                result.job_id,
                                result.job_directory,
                                result.worktree,
                                result.source.snapshot.head_sha,
                                result.scope,
                            )
                        },
                        exit: 0,
                    })
                }
                ReviewExecutionCommand::Run {
                    job,
                    timeout_seconds,
                    cwd,
                    max_parallel_jobs,
                    environment,
                    argv,
                } => {
                    let cwd = cwd.to_str().ok_or_else(|| {
                        DomainError::new(
                            ErrorCode::InvalidRequest,
                            "--cwd должен быть Unicode-путём для переносимого структурированного результата",
                        )
                    })?;
                    let timeout_ms = timeout_seconds.checked_mul(1_000).ok_or_else(|| {
                        DomainError::new(ErrorCode::InvalidRequest, "срок выполнения слишком велик")
                    })?;
                    let mut environment_values = BTreeMap::new();
                    for assignment in environment {
                        let (key, value) = assignment.split_once('=').ok_or_else(|| {
                            DomainError::new(
                                ErrorCode::InvalidRequest,
                                "--env должен иметь формат KEY=VALUE",
                            )
                        })?;
                        if environment_values
                            .insert(key.to_owned(), value.to_owned())
                            .is_some()
                        {
                            return Err(DomainError::new(
                                ErrorCode::InvalidRequest,
                                format!("--env передан несколько раз для ключа {key}"),
                            ));
                        }
                    }
                    let environment = if environment_values.is_empty() {
                        crate::code_review::execution::EnvironmentPolicy::Minimal
                    } else {
                        crate::code_review::execution::EnvironmentPolicy::Explicit {
                            values: environment_values,
                        }
                    };
                    let request = crate::code_review::execution::CommandRequest {
                        argv: argv.clone(),
                        cwd: cwd.to_owned(),
                        options: crate::code_review::execution::RunOptions {
                            timeout_ms: Some(timeout_ms),
                            output_limit_bytes: 64 * 1024,
                            environment,
                            max_parallel_jobs: *max_parallel_jobs as usize,
                        },
                    };
                    let result = crate::code_review::workflow::run_execution_job(
                        job,
                        &request,
                        cancellation,
                    )?;
                    let exit = execution_exit(result.status);
                    Ok(Rendered {
                        command: "code-review execution run",
                        stdout: if cli.json {
                            json::generic_json("code-review execution run", &result)
                        } else {
                            human_execution_result(&result)
                        },
                        exit,
                    })
                }
                ReviewExecutionCommand::Inspect { job } => {
                    let result = crate::code_review::workflow::inspect_execution_job(job)?;
                    Ok(Rendered {
                        command: "code-review execution inspect",
                        stdout: if cli.json {
                            json::generic_json("code-review execution inspect", &result)
                        } else {
                            human_execution_inspection(&result)
                        },
                        exit: 0,
                    })
                }
                ReviewExecutionCommand::Cancel { job } => {
                    let result = crate::code_review::workflow::cancel_execution_job(job)?;
                    Ok(Rendered {
                        command: "code-review execution cancel",
                        stdout: if cli.json {
                            json::generic_json("code-review execution cancel", &result)
                        } else {
                            human_execution_inspection(&result)
                        },
                        exit: 0,
                    })
                }
                ReviewExecutionCommand::Cleanup {
                    job,
                    confirm_no_live_descendants,
                } => {
                    let options = crate::code_review::execution::CleanupOptions {
                        confirm_no_live_descendants: *confirm_no_live_descendants,
                    };
                    let result = crate::code_review::workflow::cleanup_execution_job_with_options(
                        job, &options,
                    )?;
                    Ok(Rendered {
                        command: "code-review execution cleanup",
                        stdout: if cli.json {
                            json::generic_json("code-review execution cleanup", &result)
                        } else {
                            human_execution_cleanup(&result)
                        },
                        exit: 0,
                    })
                }
            },
            CodeReviewCommand::Queue { command } => match command {
                ReviewQueueCommand::List {
                    pack,
                    queue,
                    limit,
                    offset,
                    priority,
                    unknown,
                    detector,
                    surface,
                    execution,
                    role,
                    text_role,
                    code_role,
                    structure_only,
                } => {
                    let options = crate::code_review::workflow::ReviewQueueListOptions {
                        priority: *priority,
                        unknown: *unknown,
                        detector: detector.clone(),
                        surface: *surface,
                        execution: *execution,
                        role: *role,
                        text_role: *text_role,
                        code_role: *code_role,
                        offset: *offset,
                        limit: *limit,
                    };
                    let result = crate::code_review::workflow::list_review_queue(
                        pack,
                        queue,
                        &options,
                        *structure_only,
                    )?;
                    Ok(Rendered {
                        command: "code-review queue list",
                        stdout: if cli.json {
                            json::generic_json("code-review queue list", result)
                        } else {
                            format!(
                                "{}Проверка подлинности: {}.\n",
                                human_review_queue_list(&result.value),
                                queue_authenticity_label(result.syntax_authenticity),
                            )
                        },
                        exit: 0,
                    })
                }
                ReviewQueueCommand::Validate {
                    pack,
                    queue,
                    structure_only,
                } => {
                    let result = crate::code_review::workflow::validate_review_queue(
                        pack,
                        queue,
                        *structure_only,
                    )?;
                    Ok(Rendered {
                        command: "code-review queue validate",
                        stdout: if cli.json {
                            json::generic_json("code-review queue validate", result)
                        } else {
                            human_review_queue_validation(&result)
                        },
                        exit: 0,
                    })
                }
                ReviewQueueCommand::Summary {
                    pack,
                    queue,
                    structure_only,
                } => {
                    let result = crate::code_review::workflow::summarize_review_queue(
                        pack,
                        queue,
                        *structure_only,
                    )?;
                    Ok(Rendered {
                        command: "code-review queue summary",
                        stdout: if cli.json {
                            json::generic_json("code-review queue summary", result)
                        } else {
                            format!(
                                "{}Проверка подлинности: {}.\n",
                                human_review_queue_summary(&result.value),
                                queue_authenticity_label(result.syntax_authenticity),
                            )
                        },
                        exit: 0,
                    })
                }
                ReviewQueueCommand::Group {
                    pack,
                    queue,
                    id,
                    structure_only,
                } => {
                    let result = crate::code_review::workflow::expand_review_queue_group(
                        pack,
                        queue,
                        id,
                        *structure_only,
                    )?;
                    Ok(Rendered {
                        command: "code-review queue group",
                        stdout: if cli.json {
                            json::generic_json("code-review queue group", result)
                        } else {
                            format!(
                                "{}Проверка подлинности: {}.\n",
                                human_review_queue_group(&result.value),
                                queue_authenticity_label(result.syntax_authenticity),
                            )
                        },
                        exit: 0,
                    })
                }
                ReviewQueueCommand::Candidate {
                    pack,
                    queue,
                    id,
                    structure_only,
                } => {
                    let result = crate::code_review::workflow::inspect_review_queue_candidate(
                        pack,
                        queue,
                        id,
                        *structure_only,
                    )?;
                    Ok(Rendered {
                        command: "code-review queue candidate",
                        stdout: if cli.json {
                            json::generic_json("code-review queue candidate", result)
                        } else {
                            format!(
                                "{}Проверка подлинности: {}.\n",
                                human_review_queue_candidate(&result.value),
                                queue_authenticity_label(result.syntax_authenticity),
                            )
                        },
                        exit: 0,
                    })
                }
            },
            CodeReviewCommand::Triage { command } => match command {
                SemanticTriageCommand::Init { pack, out } => {
                    let result = crate::code_review::workflow::init_semantic_triage(pack, out)?;
                    Ok(Rendered {
                        command: "code-review triage init",
                        stdout: if cli.json {
                            json::generic_json("code-review triage init", result)
                        } else {
                            human_semantic_triage_init(&result)
                        },
                        exit: 0,
                    })
                }
                SemanticTriageCommand::Validate {
                    pack,
                    triage,
                    canonical_out,
                } => {
                    let result = crate::code_review::workflow::validate_semantic_triage(
                        pack,
                        triage,
                        canonical_out.as_deref(),
                    )?;
                    Ok(Rendered {
                        command: "code-review triage validate",
                        stdout: if cli.json {
                            json::generic_json("code-review triage validate", result)
                        } else {
                            human_semantic_triage_validation(&result)
                        },
                        exit: 0,
                    })
                }
                SemanticTriageCommand::Summary { pack, triage } => {
                    let result =
                        crate::code_review::workflow::summarize_semantic_triage(pack, triage)?;
                    Ok(Rendered {
                        command: "code-review triage summary",
                        stdout: if cli.json {
                            json::generic_json("code-review triage summary", &result)
                        } else {
                            human_semantic_triage_summary(&result)
                        },
                        exit: 0,
                    })
                }
                SemanticTriageCommand::Report { pack, triage, out } => {
                    let result =
                        crate::code_review::workflow::report_semantic_triage(pack, triage, out)?;
                    Ok(Rendered {
                        command: "code-review triage report",
                        stdout: if cli.json {
                            json::generic_json("code-review triage report", result)
                        } else {
                            human_semantic_triage_report(&result)
                        },
                        exit: 0,
                    })
                }
            },
            CodeReviewCommand::Learning { db, command } => {
                learning_cli::execute(db.as_deref(), command, cli.json)
            }
        },

        Command::Language { command } => match command {
            LanguageCommand::Scan {
                root,
                paths,
                pack,
                out,
            } => {
                let result =
                    crate::code_review::workflow::scan_language(root, paths, pack.as_deref(), out)?;
                Ok(Rendered {
                    command: "language scan",
                    stdout: if cli.json {
                        json::generic_json("language scan", result)
                    } else {
                        human_language_scan(&result)
                    },
                    exit: 0,
                })
            }
            LanguageCommand::Check { scan, root, out } => {
                let (result, summary) =
                    crate::code_review::workflow::check_language(root, scan, out.as_deref())?;
                let output = language_check_output(result, summary);
                Ok(Rendered {
                    command: "language check",
                    stdout: if cli.json {
                        json::generic_json("language check", output)
                    } else {
                        human_language_check(&output)
                    },
                    exit: 0,
                })
            }
            LanguageCommand::Apply {
                decisions,
                root,
                apply,
            } => {
                let result = crate::code_review::workflow::apply_language(root, decisions, *apply)?;
                Ok(Rendered {
                    command: "language apply",
                    stdout: if cli.json {
                        json::generic_json("language apply", result)
                    } else {
                        human_language_apply(&result)
                    },
                    exit: 0,
                })
            }
        },

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

fn human_snapshot(result: &crate::code_review::workflow::SnapshotSummary) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = writeln!(text, "Пакет свидетельств сохранён: {}", result.artifact_dir);
    let _ = writeln!(text, "База: {}", result.target.base_sha);
    let _ = writeln!(text, "HEAD: {}", result.target.head_sha);
    let _ = writeln!(text, "Файлов: {}", result.files);
    let _ = writeln!(
        text,
        "Структурная очередь: {}",
        result.review_queue_artifact
    );
    let _ = writeln!(
        text,
        "Кандидатов: {}; единиц ревью: {} (отдельных: {}, групп: {}, кандидатов в группах: {}, с неизвестной классификацией: {})",
        result.review_queue.raw_candidates,
        result.review_queue.review_units,
        result.review_queue.individual_units,
        result.review_queue.group_units,
        result.review_queue.grouped_candidates,
        result.review_queue.unknown_candidates,
    );
    let _ = writeln!(
        text,
        "Полные свидетельства: review.json; навигационная сводка: review.txt"
    );
    let _ = writeln!(
        text,
        "Диагностик: {} (сами по себе не подтверждают дефект)",
        result.diagnostics
    );
    for run in &result.tool_runs {
        let _ = writeln!(text, "Анализатор {}: {}", run.tool, run.status);
        if let Some(message) = &run.message {
            let _ = writeln!(text, "  {message}");
        }
    }
    if let Some(delta) = &result.delta {
        let mut counts = std::collections::BTreeMap::new();
        for change in &delta.candidates {
            *counts.entry(change.status).or_insert(0usize) += 1;
        }
        let _ = writeln!(text, "Изменения кандидатов:");
        for (status, count) in counts {
            let _ = writeln!(text, "  {}: {count}", candidate_status_label(status));
        }
        let _ = writeln!(
            text,
            "Сравнение сигналов детекторов не подтверждает исправление дефекта."
        );
    }
    text
}

fn human_review_queue_validation(
    result: &crate::code_review::workflow::ReviewQueueValidationSummary,
) -> String {
    format!(
        "Структурная очередь проверена: {}.\nSHA-256 источника верен: {}; подлинность синтаксической классификации: {}.\nСырых кандидатов: {}; единиц ревью: {}; отдельных: {}; групп: {}.\n",
        if result.valid {
            "валидна"
        } else {
            "невалидна"
        },
        if result.source_digest_valid {
            "да"
        } else {
            "нет"
        },
        queue_authenticity_label(result.syntax_authenticity),
        result.summary.raw_candidates,
        result.summary.review_units,
        result.summary.individual_units,
        result.summary.group_units,
    )
}

fn execution_exit(status: crate::code_review::execution::ExecutionStatus) -> u8 {
    use crate::code_review::execution::ExecutionStatus;
    match status {
        ExecutionStatus::Passed => 0,
        ExecutionStatus::Failed => 9,
        ExecutionStatus::Incomplete => 10,
        ExecutionStatus::TimedOut => 11,
        ExecutionStatus::Cancelled => 12,
        ExecutionStatus::Unavailable => 127,
    }
}

fn execution_status_label(status: crate::code_review::execution::ExecutionStatus) -> &'static str {
    use crate::code_review::execution::ExecutionStatus;
    match status {
        ExecutionStatus::Passed => "успешно",
        ExecutionStatus::Failed => "ошибка проверки",
        ExecutionStatus::TimedOut => "истёк срок выполнения",
        ExecutionStatus::Cancelled => "отменено",
        ExecutionStatus::Unavailable => "исполнение недоступно",
        ExecutionStatus::Incomplete => "результат неполон",
    }
}

fn execution_lifecycle_label(
    status: crate::code_review::execution::LifecycleStatus,
) -> &'static str {
    use crate::code_review::execution::LifecycleStatus;
    match status {
        LifecycleStatus::Preparing => "идёт подготовка",
        LifecycleStatus::Prepared => "подготовлено",
        LifecycleStatus::Running => "выполняется",
        LifecycleStatus::Completed => "завершено",
        LifecycleStatus::PreparationFailed => "ошибка подготовки",
        LifecycleStatus::Interrupted => "прервано",
    }
}

fn execution_mode_label(mode: crate::code_review::execution::ExecutionMode) -> &'static str {
    use crate::code_review::execution::ExecutionMode;
    match mode {
        ExecutionMode::IsolatedChecks => "изолированные проверки",
        ExecutionMode::DisposableSourceExperiment => "эксперимент с отдельной копией исходников",
    }
}

fn process_cleanup_label(value: &str) -> String {
    let label = match value {
        "not_started" => "процесс не запускался",
        "process_group_killed_partial" => {
            "процессная группа завершена; вышедшие из неё потомки не проверены"
        }
        "process_group_kill_failed" => "не удалось завершить процессную группу",
        "direct_child_only" => "завершён только непосредственный дочерний процесс",
        "direct_child_reaped_descendants_unverified" => {
            "непосредственный дочерний процесс собран; потомки не проверены"
        }
        _ => "состояние очистки процессов",
    };
    label.to_owned()
}

fn human_execution_result(result: &crate::code_review::execution::ExecutionResult) -> String {
    use std::fmt::Write as _;
    let argv = serde_json::to_string(&crate::code_review::execution::safe_argv(
        &result.request.argv,
    ))
    .expect("массив строк всегда представим в JSON");
    let code = result
        .exit
        .as_ref()
        .and_then(|exit| exit.code)
        .map_or_else(|| "отсутствует".to_owned(), |value| value.to_string());
    let signal = result
        .exit
        .as_ref()
        .and_then(|exit| exit.signal)
        .map_or_else(|| "отсутствует".to_owned(), |value| value.to_string());
    let sandbox = if result.enforcement.security_sandbox == "absent" {
        "отсутствует"
    } else {
        &result.enforcement.security_sandbox
    };
    let mut text = format!(
        "Задание {}: результат {}; сохранённое состояние {}.\nHEAD снимка: {}; SHA-256 пакета: {}\nРежим: {}; направление: {}; защитная песочница: {}; завершение процессов: {}.\nКоманда argv (чувствительные значения скрыты): {}; SHA-256 argv: {}\nРабочий каталог: {}\nКод завершения: {}; сигнал: {}; время: {} мс.\n",
        result.job_id,
        execution_status_label(result.status),
        execution_lifecycle_label(result.lifecycle),
        result.source.snapshot.head_sha,
        result.source.review_pack_sha256,
        execution_mode_label(result.mode),
        result.scope,
        sandbox,
        process_cleanup_label(&result.enforcement.process_cleanup),
        argv,
        result.argv_sha256,
        result.request.cwd,
        code,
        signal,
        result.duration_ms,
    );
    if let Some(failure) = &result.failure {
        let _ = writeln!(text, "Причина: {failure}");
    }
    for (name, output) in [("stdout", &result.stdout), ("stderr", &result.stderr)] {
        let _ = writeln!(
            text,
            "{name}: {} байт{}; полный лог: {}",
            output.total_bytes,
            if output.truncated {
                " (вывод усечён)"
            } else {
                ""
            },
            output.log,
        );
        if !output.text.is_empty() {
            let _ = writeln!(text, "{}", output.text);
        }
    }
    for limitation in &result.enforcement.limitations {
        let _ = writeln!(text, "Ограничение: {limitation}");
    }
    let cleanup = match result.cleanup.as_str() {
        "evidence_retained; workspace_cleanup_allowed" => {
            "свидетельства сохранены; очистка рабочей области разрешена"
        }
        "evidence_and_workspace_retained; descendant_confirmation_required" => {
            "свидетельства и рабочая область сохранены; для очистки требуется подтверждение отсутствия живых потомков"
        }
        "evidence_and_workspace_retained; descendants_may_still_exist" => {
            "свидетельства и рабочая область сохранены; потомки могут продолжать работу"
        }
        _ => "политика сохранённого результата",
    };
    let _ = writeln!(text, "Очистка: {cleanup}.");
    text
}

fn human_execution_inspection(inspection: &crate::code_review::execution::JobInspection) -> String {
    use std::fmt::Write as _;
    let mut text = format!(
        "Задание {}: состояние {}; режим {}; направление {}.\nРабочая область удалена: {}.\n",
        inspection.job.job_id,
        execution_lifecycle_label(inspection.lifecycle),
        execution_mode_label(inspection.job.mode),
        inspection.job.scope,
        yes_no(inspection.workspace_removed),
    );
    for limitation in &inspection.limitations {
        let _ = writeln!(text, "Ограничение: {limitation}");
    }
    if let Some(result) = &inspection.result {
        let _ = writeln!(text, "Сохранённый результат команды:");
        text.push_str(&human_execution_result(result));
    } else {
        let _ = writeln!(text, "Сохранённого результата команды нет.");
    }
    text
}

fn yes_no(value: bool) -> &'static str {
    if value { "да" } else { "нет" }
}

fn human_execution_cleanup(result: &crate::code_review::execution::CleanupResult) -> String {
    let mut text = format!(
        "Задание {}: рабочая область удалена: {}; свидетельства сохранены: {}.\n",
        result.job_id,
        yes_no(result.workspace_removed),
        yes_no(result.evidence_retained),
    );
    if let Some(limitation) = &result.limitation {
        use std::fmt::Write as _;
        let _ = writeln!(text, "Ограничение: {limitation}");
    }
    text
}

fn queue_authenticity_label(
    status: crate::code_review::workflow::SyntaxAuthenticityStatus,
) -> &'static str {
    match status {
        crate::code_review::workflow::SyntaxAuthenticityStatus::Verified => {
            "синтаксическая подлинность подтверждена"
        }
        crate::code_review::workflow::SyntaxAuthenticityStatus::StructureOnly => {
            "проверена только структура и digest"
        }
    }
}

fn human_review_queue_summary(summary: &crate::code_review::review_queue::QueueSummary) -> String {
    use std::fmt::Write as _;
    let mut text = format!(
        "Структурная очередь\nСырых кандидатов: {}; единиц ревью: {} (отдельных: {}, групп: {}).\nКандидатов в группах: {}; представителей: {}; с неизвестной классификацией: {}.\n",
        summary.raw_candidates,
        summary.review_units,
        summary.individual_units,
        summary.group_units,
        summary.grouped_candidates,
        summary.representative_candidates,
        summary.unknown_candidates,
    );
    let _ = writeln!(
        text,
        "Приоритет единиц ревью: {}",
        display_counts(
            &summary
                .units_by_priority
                .iter()
                .map(|(key, value)| (key.as_str(), *value))
                .collect(),
            |key| (*key).to_owned(),
        )
    );
    let _ = writeln!(
        text,
        "Детекторы: {}",
        display_counts(&summary.by_detector, |key| key.clone())
    );
    let _ = writeln!(
        text,
        "Поверхности: {}",
        display_counts(&summary.by_surface, |key| key.clone())
    );
    let _ = writeln!(
        text,
        "Исполнение: {}; структурные роли: {}; роли текста: {}; роли кода: {}",
        display_counts(&summary.by_execution, |key| key.clone()),
        display_counts(&summary.by_structural_role, |key| key.as_str().to_owned()),
        display_counts(&summary.by_text_role, |key| key.as_str().to_owned()),
        display_counts(&summary.by_code_role, |key| key.as_str().to_owned()),
    );
    if !summary.largest_group_sizes.is_empty() {
        let _ = writeln!(
            text,
            "Крупнейшие группы: {}",
            summary
                .largest_group_sizes
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let _ = writeln!(
        text,
        "Классификация и приоритет задают навигацию, а не семантическое решение."
    );
    text
}

fn human_review_queue_list(page: &crate::code_review::review_queue::QueueListPage) -> String {
    use std::fmt::Write as _;
    let mut text = format!(
        "Всего единиц: {}; совпало по фильтрам: {}; смещение: {}; показано: {}; есть следующая страница: {}.\n",
        page.total_units,
        page.matched_units,
        page.offset,
        page.returned_units,
        if page.has_more { "да" } else { "нет" },
    );
    for unit in &page.units {
        let classification = &unit.classification;
        let kind = match unit.kind {
            crate::code_review::review_queue::QueueUnitKind::Individual => "отдельная единица",
            crate::code_review::review_queue::QueueUnitKind::Group => "группа",
        };
        let surfaces = if classification.surfaces.is_empty() {
            "unknown".to_owned()
        } else {
            classification
                .surfaces
                .iter()
                .map(crate::code_review::review_queue::surface_name)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let _ = writeln!(
            text,
            "{} · {} · приоритет {} · детектор {} · кандидатов: {}",
            unit.id,
            kind,
            unit.priority.as_str(),
            unit.detector,
            unit.candidate_count
        );
        let _ = writeln!(
            text,
            "  Классификация: поверхности {}; исполнение {}; структурная роль {}; роль текста {}; роль кода {}.",
            surfaces,
            classification
                .execution
                .as_ref()
                .map_or("unknown", crate::code_review::review_queue::surface_name),
            classification.role.as_str(),
            classification
                .text_role
                .map_or("не применяется", |role| role.as_str()),
            classification.code_role.as_str(),
        );
        if let Some(candidate_id) = &unit.candidate_id {
            let _ = writeln!(text, "  Исходный ID кандидата: {candidate_id}");
        }
        if !unit.representative_candidate_ids.is_empty() {
            let representatives = unit
                .representative_candidate_ids
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(text, "  Представители: {representatives}");
        }
    }
    text
}

fn human_review_queue_group(
    result: &crate::code_review::workflow::ReviewQueueGroupDetail,
) -> String {
    use std::fmt::Write as _;
    let class = &result.unit.signature.classification;
    let mut text = format!(
        "Группа {}\nПриоритет: {}; кандидатов: {}; детектор: {}; семейство путей: {}.\nРоль: {}; исполнение: {}; роль текста: {}; роль кода: {}; происхождение: {}.\n",
        result.unit.id,
        result.unit.priority.as_str(),
        result.unit.candidate_ids().len(),
        result.unit.signature.detector,
        result.unit.signature.path_family,
        class.role.as_str(),
        class
            .execution
            .as_ref()
            .map_or("unknown", crate::code_review::review_queue::surface_name),
        class
            .text_role
            .map_or("не применяется", |role| role.as_str()),
        class.code_role.as_str(),
        class.origin.as_str(),
    );
    let _ = writeln!(text, "Представители:");
    for detail in &result.representatives {
        let line = detail
            .candidate
            .line
            .map_or_else(String::new, |line| format!(":{line}"));
        let snippet = detail
            .candidate
            .snippet
            .as_deref()
            .map(crate::text::bounded_sample)
            .map(|value| format!(" — {}", value.replace(['\r', '\n'], "↵")))
            .unwrap_or_default();
        let _ = writeln!(
            text,
            "  {} {}{}{}",
            detail.candidate.id, detail.candidate.path, line, snippet
        );
    }
    let ids = result
        .unit
        .candidate_ids()
        .iter()
        .take(20)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(
        text,
        "ID кандидатов (первые {}): {ids}",
        result.unit.candidate_ids().len().min(20)
    );
    if result.unit.candidate_ids().len() > 20 {
        let _ = writeln!(
            text,
            "Остальные ID доступны в --json; подробности кандидата: `code-review queue candidate --id ID`."
        );
    }
    let _ = writeln!(
        text,
        "Группа не является общим семантическим решением; неоднородные случаи нужно рассматривать отдельно."
    );
    text
}

fn human_review_queue_candidate(
    result: &crate::code_review::workflow::ReviewQueueCandidateDetail,
) -> String {
    use std::fmt::Write as _;
    let candidate = &result.candidate;
    let class = &result.classification;
    let line = candidate
        .line
        .map_or_else(String::new, |line| format!(":{line}"));
    let snippet = candidate
        .snippet
        .as_deref()
        .map(crate::text::bounded_sample)
        .unwrap_or_default();
    let surfaces = if class.surfaces.is_empty() {
        "unknown".to_owned()
    } else {
        class
            .surfaces
            .iter()
            .map(crate::code_review::review_queue::surface_name)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut text = format!(
        "Кандидат {}\n{}{} · детектор: {} · происхождение: {}\nСигналы: {}\nФрагмент: {}\nПоверхности: {}; исполнение: {}; роль: {}; роль текста: {}; роль кода: {}\nОчередь: {} ({}, приоритет: {}).\n",
        candidate.id,
        candidate.path,
        line,
        candidate.detector,
        candidate.origin.as_str(),
        candidate.signals.join(", "),
        snippet,
        surfaces,
        class
            .execution
            .as_ref()
            .map_or("unknown", crate::code_review::review_queue::surface_name),
        class.role.as_str(),
        class
            .text_role
            .map_or("не применяется", |role| role.as_str()),
        class.code_role.as_str(),
        result.unit.id,
        if result.unit.is_group() {
            "группа"
        } else {
            "отдельный кандидат"
        },
        result.unit.priority.as_str(),
    );
    if result.unit.is_group() {
        let _ = writeln!(
            text,
            "Участников группы: {}; представителей: {}.",
            result.unit.candidate_ids().len(),
            result
                .unit
                .representative_candidate_ids()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let _ = writeln!(
        text,
        "Свидетельство — это сигнал, а не подтверждённое замечание."
    );
    text
}

fn human_semantic_triage_init(
    result: &crate::code_review::workflow::SemanticTriageInitSummary,
) -> String {
    format!(
        "Семантический разбор создан: {}\nКандидатов: {}; пока не рассмотрено: {}.\nHEAD: {}\n",
        result.artifact,
        result.total_candidates,
        result.unreviewed_candidates,
        result.target.head_sha,
    )
}

fn human_semantic_triage_validation(
    result: &crate::code_review::workflow::SemanticTriageValidationSummary,
) -> String {
    let mut text = format!(
        "Семантический разбор прошёл проверку.\nКандидатов: {}; рассмотрено: {}; не рассмотрено: {}.\n",
        result.summary.total_candidates,
        result.summary.reviewed_candidates,
        result.summary.unreviewed_candidates,
    );
    if let Some(path) = &result.canonical_artifact {
        use std::fmt::Write as _;
        let _ = writeln!(text, "Канонический JSON: {path}");
    }
    text
}

fn human_semantic_triage_summary(
    summary: &crate::code_review::semantic_triage::TriageSummary,
) -> String {
    use std::fmt::Write as _;
    let mut text = format!(
        "Сводка семантического разбора\nВсего кандидатов: {}\nРассмотрено: {}\nНе рассмотрено: {}\n",
        summary.total_candidates, summary.reviewed_candidates, summary.unreviewed_candidates,
    );
    let _ = writeln!(
        text,
        "Индивидуальных решений: {}",
        summary.individual_review.decision_count
    );
    for (disposition, count) in &summary.individual_review.by_disposition {
        let _ = writeln!(
            text,
            "  Отдельные решения «{}»: {count}",
            disposition.as_str()
        );
    }
    let _ = writeln!(
        text,
        "Групповых решений: {}; ID кандидатов в группах: {}; ID представителей: {}",
        summary.group_review.decisions.decision_count,
        summary.group_review.covered_candidate_ids,
        summary.group_review.representative_candidate_ids,
    );
    for (disposition, count) in &summary.group_review.decisions.by_disposition {
        let _ = writeln!(
            text,
            "  Групповые решения «{}»: {count}",
            disposition.as_str()
        );
    }
    let _ = writeln!(text, "Замечаний: {}", summary.findings.total_findings);
    for (severity, count) in &summary.findings.by_severity {
        let _ = writeln!(text, "  Серьёзность {}: {count}", severity.as_str());
    }
    for (provenance, count) in &summary.findings.by_provenance {
        let _ = writeln!(text, "  Происхождение {}: {count}", provenance.as_str());
    }
    text
}

fn human_semantic_triage_report(
    result: &crate::code_review::workflow::SemanticTriageReportSummary,
) -> String {
    format!(
        "Markdown-отчёт сохранён: {}\nЗамечаний: {}; нерассмотренных кандидатов: {}.\n",
        result.report, result.total_findings, result.unreviewed_candidates,
    )
}

fn human_delta(result: &crate::code_review::model::ReviewDelta) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Сравнение сигналов детекторов: {} → {}",
        result.before.head_sha, result.after.head_sha
    );
    let mut candidate_counts = std::collections::BTreeMap::new();
    for change in &result.candidates {
        *candidate_counts.entry(change.status).or_insert(0usize) += 1;
    }
    let _ = writeln!(text, "Изменения кандидатов:");
    for (status, count) in candidate_counts {
        let _ = writeln!(text, "  {}: {count}", candidate_status_label(status));
    }
    let mut diagnostic_counts = std::collections::BTreeMap::new();
    for change in &result.diagnostics {
        *diagnostic_counts.entry(change.status).or_insert(0usize) += 1;
    }
    let _ = writeln!(text, "Изменения диагностик:");
    for (status, count) in diagnostic_counts {
        let _ = writeln!(text, "  {}: {count}", candidate_status_label(status));
    }
    for run in &result.tool_runs {
        let _ = writeln!(
            text,
            "Анализатор {}: {} → {} (диагностик: {} → {})",
            run.tool,
            run.before_status,
            run.after_status,
            run.before_diagnostics,
            run.after_diagnostics
        );
        if let Some(message) = &run.after_message {
            let _ = writeln!(text, "  {message}");
        }
    }
    let _ = writeln!(
        text,
        "Сравнение описывает сигналы детекторов и не подтверждает состояние дефекта."
    );
    text
}

fn human_language_scan(result: &crate::code_review::workflow::LanguageSummary) -> String {
    format!(
        "Сканирование текста сохранено: {}\nФайлов: {}\nКандидатов: {}\nПропущено: {}\n",
        result.artifact, result.files, result.candidates, result.skipped
    )
}

fn human_language_check(result: &LanguageCheckOutput) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Найдено кандидатов в тексте: {}",
        result.candidates_total
    );
    let _ = writeln!(
        text,
        "Показано кандидатов: {} из {}",
        result.candidates.len(),
        result.candidates_total
    );
    let _ = writeln!(text, "Проверено файлов: {}", result.files_total);
    let _ = writeln!(text, "Пропущено файлов: {}", result.skipped_total);
    let _ = writeln!(
        text,
        "Выборка усечена: {}",
        if result.candidates_truncated {
            "да"
        } else {
            "нет"
        }
    );
    if let Some(summary) = &result.summary {
        let _ = writeln!(text, "Артефакт: {}", summary.artifact);
    }
    for candidate in &result.candidates {
        let _ = writeln!(
            text,
            "  {}:{} {}: {}",
            candidate.path,
            candidate.line,
            text_context_label(candidate.context),
            candidate.text.replace('\n', "↵")
        );
    }
    text
}

fn human_language_apply(result: &crate::code_review::workflow::LanguageApplySummary) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let mode = if result.applied {
        "Записано"
    } else {
        "Проверка без записи"
    };
    let _ = writeln!(
        text,
        "{mode}: файлов {}, замен {}",
        result.files, result.replacements
    );
    for file in &result.results {
        let _ = writeln!(
            text,
            "  {}: замен {}, {} → {}",
            file.path, file.replacements, file.before_sha256, file.after_sha256
        );
    }
    text
}

fn candidate_status_label(status: crate::code_review::model::CandidateStatus) -> &'static str {
    use crate::code_review::model::CandidateStatus;
    match status {
        CandidateStatus::StillPresent => "остался",
        CandidateStatus::Gone => "исчез",
        CandidateStatus::Changed => "изменился",
        CandidateStatus::New => "появился",
    }
}

fn text_context_label(context: crate::code_review::language::TextContext) -> &'static str {
    use crate::code_review::language::TextContext;
    match context {
        TextContext::Comment => "комментарий",
        TextContext::DocComment => "doc-комментарий",
        TextContext::StringLiteral => "строковый литерал",
        TextContext::MarkdownProse => "текст Markdown",
        TextContext::ConfigurationValue => "значение конфигурации",
        TextContext::ScriptOutput => "текст shell/script",
    }
}

/// Собирает селектор колоды из аргументов CLI.
///
/// Отсутствие всех трёх аргументов означает корневую колоду экспорта
/// (`preorder = 0`): у CrowdAnki-экспорта один корневой узел, и читать «модели
/// экспорта» без адреса колоды означает именно его. Запись такого умолчания не
/// имеет: `create` берёт колоду каждой заметки из самого запроса.
fn deck_selector(
    path: Option<String>,
    crowdanki_uuid: Option<String>,
    preorder: Option<usize>,
) -> DeckSelector {
    let selector = DeckSelector {
        path,
        crowdanki_uuid,
        preorder,
    };
    if selector.is_empty() {
        return DeckSelector {
            preorder: Some(0),
            ..DeckSelector::default()
        };
    }
    selector
}

/// Собирает запрос на вывод заметок из обращения из аргументов CLI.
///
/// `--request` читается целиком и разбирается; `-` означает stdin. Одиночные
/// `--guid` вместе с `--tag` дают тот же запрос, что и файл, поэтому обе формы
/// проходят одни и те же проверки владельца операции.
fn build_retire_request(
    request_file: Option<&Path>,
    guid: &[String],
    tag: Option<&str>,
) -> Result<RetireRequest, DomainError> {
    if let Some(path) = request_file {
        let label = path.display().to_string();
        let raw = read_document(path, retire_op::MAX_REQUEST_BYTES, "запрос")?;
        return retire_op::parse_request_bytes(&raw, &label);
    }

    let (Some(tag), false) = (tag, guid.is_empty()) else {
        return Err(DomainError::new(
            ErrorCode::Usage,
            "для вывода из обращения нужен либо --request, либо --tag с хотя бы одним --guid",
        ));
    };

    let request = RetireRequest {
        tag: tag.to_string(),
        notes: guid
            .iter()
            .map(|guid| RetireSpec {
                note_id: None,
                guid: guid.clone(),
            })
            .collect(),
    };
    retire_op::validate_request(&request)?;
    Ok(request)
}

/// Записывает JSON-документ в канонической форме.
///
/// Канонические байты дают побайтовую воспроизводимость: повторный прогон
/// создаёт тот же файл, а не «почти тот же».
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
        let raw = read_document(path, edit_op::MAX_REQUEST_BYTES, "запрос")?;
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

/// Читает документ запроса из файла или stdin.
///
/// `max_bytes` — предел размера документа, объявленный его владельцем
/// (`edit` и `review-check` используют один и тот же предел запроса правки).
fn read_document(path: &Path, max_bytes: usize, what: &str) -> Result<Vec<u8>, DomainError> {
    let label = path.display().to_string();
    let limit = document_read_limit(max_bytes);
    if label == STDIN_REQUEST_SOURCE {
        return read_bounded(std::io::stdin().lock(), limit)
            .map_err(|error| document_read_error(what, STDIN_REQUEST_SOURCE, &error));
    }

    let file =
        File::open(path).map_err(|error| document_read_error(what, label.as_str(), &error))?;
    read_bounded(file, limit).map_err(|error| document_read_error(what, label.as_str(), &error))
}

/// Предел чтения документа: максимум размера плюс один байт.
///
/// Лишний байт нужен, чтобы владелец документа отличил «ровно предел» от
/// «больше предела» по длине буфера.
const fn document_read_limit(max_bytes: usize) -> u64 {
    max_bytes as u64 + 1
}

/// Читает не более `limit` байтов.
///
/// Предел размера документа проверяется по длине буфера, поэтому неограниченное
/// чтение успело бы занять память под весь входной поток прежде, чем команда
/// сообщила бы о превышении.
fn read_bounded<R: Read>(reader: R, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    reader.take(limit).read_to_end(&mut raw)?;
    Ok(raw)
}

/// Готовит ошибку нечитаемого документа запроса.
fn document_read_error(what: &str, label: &str, error: &std::io::Error) -> DomainError {
    DomainError::with_details(
        ErrorCode::InputUnreadable,
        format!("не удалось прочитать {what} {label}: {error}"),
        crate::details! {
            "path" => label,
            "io_error" => error.to_string(),
        },
    )
}

fn build_criteria(
    guid: Option<&str>,
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
        "не задан критерий поиска: нужен --guid или --field",
    ))
}

/// Приводит счётчик CLI к `usize` без паники.
///
/// Значения уже ограничены clap, а насыщение здесь защищает от 32-битной
/// платформы, где `u64` может не поместиться в `usize`.
const fn to_usize(value: u64) -> usize {
    if value > usize::MAX as u64 {
        usize::MAX
    } else {
        value as usize
    }
}

/// Собирает критерий выбора для `review`.
///
/// `review` и `find` принимают одинаковые критерии по полю: `--field` со
/// значением и, необязательно, `--match`. Взаимная исключительность аргументов
/// проверяется clap, а зависимость `--match` от `--field` — здесь, как и в
/// `find`.
fn build_review_criteria(
    all: bool,
    guid: Option<&str>,
    field: Option<&str>,
    value: Option<&str>,
    match_mode: Option<MatchArg>,
    qa_code: Option<&str>,
) -> Result<ReviewCriteria, DomainError> {
    if match_mode.is_some() && field.is_none() {
        return Err(DomainError::new(
            ErrorCode::Usage,
            "аргумент --match применяется только вместе с --field",
        ));
    }
    if all {
        return Ok(ReviewCriteria::All);
    }
    if let Some(guid) = guid {
        return Ok(ReviewCriteria::Guid {
            guid: guid.to_string(),
        });
    }
    if let Some(code) = qa_code {
        return Ok(ReviewCriteria::QaCode {
            code: code.to_string(),
        });
    }
    if let Some(field) = field {
        return Ok(ReviewCriteria::Field {
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
        "не задан критерий отбора: нужен один из --all, --guid, --field или --qa-code",
    ))
}

/// Готовит stdout/stderr для доменной ошибки.
///
/// В режиме JSON stdout содержит только один корректный документ JSON, а stderr
/// остаётся пустым. В обычном режиме сообщение уходит в stderr, а stdout пуст.
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

/// Реализация подкоманды `code-review learning`.
///
/// Здесь только CLI-слой: разбор аргументов, вызов публичного API
/// `code_review::learning`, ограничение вывода и подготовка русского human
/// output либо JSON DTO. Доменные решения — поддержка, карантин, guardrails и
/// допустимые исходы — остаются в `code_review::learning`: CLI не пересчитывает
/// статистику, не создаёт семантических решений и не меняет исходные артефакты
/// ревью.
///
/// Читающие команды не создают базу: отсутствие истории — это состояние
/// обычного ревью, а не ошибка. Производные артефакты публикуются по правилам
/// набора: `recommend --out` — только как `recommendations.json` в рабочей
/// области своего пакета, `export` и `policy approve` — только по явному
/// безопасному пути внутри репозитория; чужие байты не перезаписываются.
mod learning_cli {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Component, Path, PathBuf};
    use std::process::Command;

    use serde::Serialize;
    use serde_json::Value;

    use crate::cli::{
        LearningCommand, LearningDispositionArg, LearningFeedbackActionArg,
        LearningFeedbackCommand, LearningFeedbackKindArg, LearningPolicyCommand,
        LearningUsefulnessArg,
    };
    use crate::code_review::learning::feedback::{
        self, CaseAudit, FEEDBACK_SCHEMA_VERSION, POLICY_SCHEMA_VERSION,
    };
    use crate::code_review::learning::import::{self, ImportRequest};
    use crate::code_review::learning::lifecycle::ForgetOutcome;
    use crate::code_review::learning::model::{
        CaseRef, FeedbackAction, FeedbackEvent, FeedbackKind, FeedbackResult, HistoryGeneration,
        ImportRecord, ImportStatus, LearningExport, LearningExportManifest, LearningStatus,
        ObservationCounts, PatternReport, PolicyProposal, Recommendation, Recommendations,
        SupportSummary, TrustLevel,
    };
    use crate::code_review::learning::patterns::{self, PatternQuery};
    use crate::code_review::learning::recommend::{self, RecommendRequest};
    use crate::code_review::learning::search::{self, SearchQuery};
    use crate::code_review::learning::store::{self, LearningStore, StoreOptions};
    use crate::code_review::learning::transfer::{self, RestoreSummary};
    use crate::code_review::learning::{LEARNING_POLICY_VERSION, LEARNING_SCHEMA_VERSION};
    use crate::code_review::workflow::{
        review_workspace_file, safe_output_path, write_review_document_once,
    };
    use crate::error::{DomainError, ErrorCode};
    use crate::render::json;

    use super::Rendered;

    /// Имя производного документа рекомендаций в рабочей области ревью.
    const RECOMMENDATIONS_ARTIFACT: &str = "recommendations.json";
    /// Предел числа идентификаторов кандидатов в одном элементе вывода.
    const OUTPUT_CANDIDATE_LIMIT: usize = 3;
    /// Предел числа противоречащих единиц в одном правиле вывода.
    const OUTPUT_CONTRADICTING_LIMIT: usize = 10;
    /// Предел числа исторических случаев в одной подсказке вывода.
    const OUTPUT_CASE_LIMIT: usize = 5;

    /// Выполняет выбранную операцию learning и готовит её вывод.
    pub(super) fn execute(
        db: Option<&Path>,
        command: &LearningCommand,
        json_mode: bool,
    ) -> Result<Rendered, DomainError> {
        dispatch(db, command, json_mode).map_err(mark_temporary)
    }

    /// Помечает временную занятость базы как повторяемую операцию.
    ///
    /// Код `learning_storage_busy` остаётся кодом `t1`; добавка только называет
    /// причину в терминах набора: удержанная транзакция заканчивается сама, а
    /// постоянный конфликт артефакта — нет.
    fn mark_temporary(mut error: DomainError) -> DomainError {
        if error.code == ErrorCode::LearningStorageBusy
            && let Value::Object(details) = &mut error.details
        {
            details.insert("retryable".to_owned(), Value::Bool(true));
            details.insert("resource".to_owned(), Value::String("learning".to_owned()));
        }
        error
    }

    fn dispatch(
        db: Option<&Path>,
        command: &LearningCommand,
        json_mode: bool,
    ) -> Result<Rendered, DomainError> {
        let root = repository_root()?;
        match command {
            LearningCommand::Import {
                pack,
                queue,
                triage,
                execution,
                structure_only,
                variant,
                label,
            } => {
                let options = ImportOptions {
                    pack,
                    queue,
                    triage: triage.as_deref(),
                    execution,
                    structure_only: *structure_only,
                    variant,
                    label: label.as_deref(),
                };
                let outcome = import_history(&root, db, &options)?;
                let human = human_import(&outcome);
                Ok(rendered_output(
                    command.command_name(),
                    json_mode,
                    &outcome,
                    human,
                ))
            }
            LearningCommand::Status => {
                let status = read_status(&root, db)?;
                let human = human_status(&status, false);
                Ok(rendered_output(
                    command.command_name(),
                    json_mode,
                    &status,
                    human,
                ))
            }
            LearningCommand::Validate => {
                let status = validate(&root, db)?;
                let human = human_status(&status, true);
                Ok(rendered_output(
                    command.command_name(),
                    json_mode,
                    &status,
                    human,
                ))
            }
            LearningCommand::Stats {
                include_quarantine,
                limit,
            } => {
                let output = stats(&root, db, *include_quarantine, to_usize(*limit))?;
                let human = human_stats(&output);
                Ok(rendered_output(
                    command.command_name(),
                    json_mode,
                    &output,
                    human,
                ))
            }
            LearningCommand::Patterns {
                detector,
                role,
                code_role,
                origin,
                include_quarantine,
                limit,
                require_supported,
            } => {
                let query = PatternQuery {
                    limit: to_usize(*limit),
                    detector: detector.clone(),
                    role: role.map(|value| value.as_str().to_owned()),
                    code_role: code_role.map(|value| value.as_str().to_owned()),
                    origin: origin.clone(),
                    include_quarantine: *include_quarantine,
                    now: None,
                };
                let output = patterns_report(&root, db, &query, *require_supported)?;
                let human = human_patterns(&output);
                Ok(rendered_output(
                    command.command_name(),
                    json_mode,
                    &output,
                    human,
                ))
            }
            LearningCommand::Search {
                text,
                detector,
                surface,
                origin,
                role,
                code_role,
                disposition,
                provenance,
                severity,
                repository,
                include_quarantine,
                limit,
                offset,
            } => {
                let query = SearchQuery {
                    text: text.clone(),
                    detector: detector.clone(),
                    surface: surface.map(|value| value.as_str().to_owned()),
                    origin: origin.clone(),
                    role: role.map(|value| value.as_str().to_owned()),
                    code_role: code_role.map(|value| value.as_str().to_owned()),
                    disposition: disposition.map(|value| value.as_str().to_owned()),
                    provenance: provenance.clone(),
                    severity: severity.clone(),
                    repository_id: repository.clone(),
                    include_quarantine: *include_quarantine,
                    offset: to_usize(*offset),
                    limit: to_usize(*limit),
                };
                let page = search_history(&root, db, &query)?;
                let human = human_search(&page);
                Ok(rendered_output(
                    command.command_name(),
                    json_mode,
                    &page,
                    human,
                ))
            }
            LearningCommand::Recommend {
                pack,
                queue,
                triage,
                structure_only,
                variant,
                label,
                limit,
                case_limit,
                history_revision,
                policy_version,
                without_learning,
                require_history,
                out,
            } => {
                let options = RecommendOptions {
                    pack,
                    queue,
                    triage: triage.as_deref(),
                    structure_only: *structure_only,
                    variant,
                    label: label.as_deref(),
                    limit: to_usize(*limit),
                    case_limit: to_usize(*case_limit),
                    history_revision: *history_revision,
                    policy_version: *policy_version,
                    without_learning: *without_learning,
                    require_history: *require_history,
                    out: out.as_deref(),
                };
                let output = recommendations(&root, db, &options)?;
                let human = human_recommendations(&output);
                Ok(rendered_output(
                    command.command_name(),
                    json_mode,
                    &output,
                    human,
                ))
            }
            LearningCommand::Feedback { command } => {
                feedback_command(&root, db, command, json_mode)
            }
            LearningCommand::Export { out } => {
                let output = export(&root, db, out)?;
                let human = human_export(&output);
                Ok(rendered_output(
                    command.command_name(),
                    json_mode,
                    &output,
                    human,
                ))
            }
            LearningCommand::Backup { out } => {
                let output = backup(&root, db, out)?;
                let human = human_backup(&output);
                Ok(rendered_output(
                    command.command_name(),
                    json_mode,
                    &output,
                    human,
                ))
            }
            LearningCommand::Restore { archive } => {
                let output = restore(&root, db, archive)?;
                let human = human_restore(&output);
                Ok(rendered_output(
                    command.command_name(),
                    json_mode,
                    &output,
                    human,
                ))
            }
            LearningCommand::Forget { review_id, confirm } => {
                if !confirm {
                    return Err(invalid_request(
                        "удаление истории необратимо: повторите команду с явным --confirm",
                    ));
                }
                let store = open_required(&root, db)?;
                let output = crate::code_review::learning::forget_review(&store, review_id)?;
                let human = human_forget(&output);
                Ok(rendered_output(
                    command.command_name(),
                    json_mode,
                    &output,
                    human,
                ))
            }
            LearningCommand::Policy { command } => policy_command(&root, db, command, json_mode),
        }
    }

    /// Собирает готовый к печати результат команды.
    fn rendered_output<T: Serialize>(
        name: &'static str,
        json_mode: bool,
        value: &T,
        human: String,
    ) -> Rendered {
        Rendered {
            command: name,
            stdout: if json_mode {
                json::generic_json(name, value)
            } else {
                human
            },
            exit: 0,
        }
    }

    // ---- команды ----

    /// Вход импорта одного ревью.
    struct ImportOptions<'a> {
        pack: &'a Path,
        queue: &'a Path,
        triage: Option<&'a Path>,
        execution: &'a [PathBuf],
        structure_only: bool,
        variant: &'a str,
        label: Option<&'a str>,
    }

    fn import_history(
        root: &Path,
        db: Option<&Path>,
        options: &ImportOptions<'_>,
    ) -> Result<import::ImportOutcome, DomainError> {
        if options.execution.len() > 1 {
            return Err(invalid_request(
                "допускается не более одного --execution: запись истории хранит один завершённый результат изоляции",
            ));
        }
        let variant = crate::code_review::learning::workspace_variant(options.variant)?;
        let store = LearningStore::open(database_options(root, db, true)?)?;
        let loaded = import::load_review_with_execution(
            options.pack,
            options.queue,
            options.triage,
            options.execution.first().map(PathBuf::as_path),
            options.structure_only,
        )?;
        let request = ImportRequest {
            workspace_variant: variant,
            workspace_label: options.label.map(str::to_owned),
        };
        import::import_with_outcome(&store, &loaded, &request)
    }

    /// Состояние хранилища без создания базы.
    fn read_status(root: &Path, db: Option<&Path>) -> Result<LearningStatus, DomainError> {
        let options = database_options(root, db, false)?;
        if !options.database.is_file() {
            return Ok(absent_status(&options));
        }
        LearningStore::open(options)?.status()
    }

    /// Проверка схемы, целостности и поиска без изменения данных.
    fn validate(root: &Path, db: Option<&Path>) -> Result<LearningStatus, DomainError> {
        let options = database_options(root, db, false)?;
        if !options.database.is_file() {
            return Err(DomainError::with_details(
                ErrorCode::NotFound,
                "Локальная база learning не найдена: проверять нечего",
                crate::details! {
                    "database" => options.display_path.clone(),
                    "recovery" => store::recovery_paths(&options.display_path),
                },
            ));
        }
        let store = LearningStore::open(options)?;
        let status = store.status()?;
        if status.user_version != LEARNING_SCHEMA_VERSION {
            return Err(DomainError::with_details(
                ErrorCode::LearningSchemaUnsupported,
                format!(
                    "Версия схемы базы learning {} не поддерживается этой сборкой: ожидается {LEARNING_SCHEMA_VERSION}",
                    status.user_version
                ),
                crate::details! { "database" => status.database_path.clone() },
            ));
        }
        // Проверка целостности вызывается явно: `status` сообщает только факт.
        store.integrity_check()?;
        Ok(status)
    }

    /// Агрегированная статистика по ограниченной странице истории.
    fn stats(
        root: &Path,
        db: Option<&Path>,
        include_quarantine: bool,
        limit: usize,
    ) -> Result<StatsOutput, DomainError> {
        let store = open_required(root, db)?;
        let generation = import::generation(&store)?;
        let mut records =
            import::list_imports(&store, include_quarantine, limit.saturating_add(1))?;
        let truncated = records.len() > limit;
        records.truncate(limit);
        let mut observations = ObservationCounts::default();
        for record in &records {
            accumulate(&mut observations, &record.observations);
        }
        let dispositions = feedback::outcome_distribution(&store)?;
        // Итог считает ровно тот набор записей, из которого строится страница:
        // иначе «показано N из M» называло бы причину, которой нет.
        let reviews_total = if include_quarantine {
            generation.trusted_reviews + generation.quarantined_reviews
        } else {
            generation.trusted_reviews
        };
        Ok(StatsOutput {
            schema_version: LEARNING_SCHEMA_VERSION,
            policy_version: LEARNING_POLICY_VERSION,
            database_path: store.options().display_path.clone(),
            generation,
            reviews_total,
            reviews_shown: records.len(),
            reviews_truncated: truncated,
            observations,
            dispositions,
            reviews: records,
            limitations: vec![
                "Статистика опирается на независимые единицы наблюдения, а не на число кандидатов."
                    .to_owned(),
                "Доли исходов не являются precision/recall: история собрана выборочно.".to_owned(),
                "Агрегат наблюдений посчитан по показанной странице записей, а не по всей истории."
                    .to_owned(),
            ],
        })
    }

    /// Отчёт по объяснимым паттернам с фильтрами и лимитом.
    fn patterns_report(
        root: &Path,
        db: Option<&Path>,
        query: &PatternQuery,
        require_supported: bool,
    ) -> Result<PatternsOutput, DomainError> {
        let store = open_required(root, db)?;
        let report = patterns::pattern_report(&store, query)?;
        patterns::require_supported(&report)?;
        if require_supported && report.abstained {
            return Err(DomainError::with_details(
                ErrorCode::InsufficientEvidence,
                "Ни один паттерн не имеет достаточной поддержки: вывод воздерживается",
                crate::details! {
                    "min_support_units" => report.min_support_units,
                    "trusted_units" => report.generation.trusted_units,
                },
            ));
        }
        Ok(PatternsOutput::from_report(&report))
    }

    /// Локальный поиск исторических случаев.
    fn search_history(
        root: &Path,
        db: Option<&Path>,
        query: &SearchQuery,
    ) -> Result<search::SearchPage, DomainError> {
        let store = open_required(root, db)?;
        search::search_history(&store, query)
    }

    /// Вход построения рекомендаций.
    struct RecommendOptions<'a> {
        pack: &'a Path,
        queue: &'a Path,
        triage: Option<&'a Path>,
        structure_only: bool,
        variant: &'a str,
        label: Option<&'a str>,
        limit: usize,
        case_limit: usize,
        history_revision: Option<u64>,
        policy_version: Option<u32>,
        without_learning: bool,
        require_history: bool,
        out: Option<&'a Path>,
    }

    fn recommendations(
        root: &Path,
        db: Option<&Path>,
        options: &RecommendOptions<'_>,
    ) -> Result<RecommendOutput, DomainError> {
        if let Some(requested) = options.policy_version
            && requested != LEARNING_POLICY_VERSION
        {
            return Err(DomainError::with_details(
                ErrorCode::LearningSchemaUnsupported,
                format!(
                    "Запрошена версия политики learning {requested}: эта сборка реализует {LEARNING_POLICY_VERSION}"
                ),
                crate::details! { "policy_version" => LEARNING_POLICY_VERSION },
            ));
        }
        let variant = crate::code_review::learning::workspace_variant(options.variant)?;
        let store = if options.without_learning {
            None
        } else {
            open_optional(root, db)?
        };
        let history = match store.as_ref() {
            Some(store) => Some(import::generation(store)?),
            None => None,
        };
        if let Some(expected) = options.history_revision {
            let actual = history.as_ref().ok_or_else(|| {
                DomainError::with_details(
                    ErrorCode::SourceChanged,
                    "Ожидалась конкретная ревизия истории learning, но история недоступна",
                    crate::details! { "expected_revision" => expected },
                )
            })?;
            if actual.revision != expected {
                return Err(DomainError::with_details(
                    ErrorCode::SourceChanged,
                    format!(
                        "Ожидалась ревизия истории {expected}, действующая — {}",
                        actual.revision
                    ),
                    crate::details! {
                        "expected_revision" => expected,
                        "actual_revision" => actual.revision,
                    },
                ));
            }
        }
        if options.require_history {
            match history.as_ref() {
                None => {
                    return Err(DomainError::with_details(
                        ErrorCode::NotFound,
                        "Проверенная история learning недоступна, а она требуется явно",
                        crate::details! {
                            "database" => database_options(root, db, false)?.display_path,
                        },
                    ));
                }
                Some(actual) if actual.trusted_units == 0 => {
                    return Err(DomainError::with_details(
                        ErrorCode::InsufficientEvidence,
                        "Проверенная история пуста: рекомендации не могут опираться на накопленные случаи",
                        crate::details! { "trusted_units" => actual.trusted_units },
                    ));
                }
                Some(_) => {}
            }
        }
        let loaded = import::load_review(
            options.pack,
            options.queue,
            options.triage,
            options.structure_only,
        )?;
        let request = RecommendRequest {
            workspace_variant: variant,
            workspace_label: options.label.map(str::to_owned),
            limit: options.limit,
            case_limit: options.case_limit,
            now: 0,
        };
        let document = recommend::recommend(store.as_ref(), &loaded, &request)?;
        let mut output = RecommendOutput::from_document(&document, loaded.queue.units.len());
        if let Some(out) = options.out {
            let bytes = to_json_bytes(&document)?;
            let expected = review_workspace_file(
                root,
                options.pack,
                &loaded.pack,
                out,
                RECOMMENDATIONS_ARTIFACT,
            )?;
            output.artifact = Some(publish_document(root, &expected, &bytes)?);
        }
        Ok(output)
    }

    fn feedback_command(
        root: &Path,
        db: Option<&Path>,
        command: &LearningFeedbackCommand,
        json_mode: bool,
    ) -> Result<Rendered, DomainError> {
        match command {
            LearningFeedbackCommand::Record {
                review_id,
                unit_id,
                candidate_id,
                kind,
                action,
                disposition,
                usefulness,
                supersedes_event_id,
                explanation,
                provenance,
                event_id,
            } => {
                let options = FeedbackRecordOptions {
                    review_id,
                    unit_id,
                    candidate_id: candidate_id.as_deref(),
                    kind: *kind,
                    action: *action,
                    disposition: *disposition,
                    usefulness: *usefulness,
                    supersedes_event_id: supersedes_event_id.as_deref(),
                    explanation,
                    provenance,
                    event_id: event_id.as_deref(),
                };
                let result = record_feedback(root, db, &options)?;
                let human = human_feedback_result(&result);
                Ok(rendered_output(
                    "code-review learning feedback record",
                    json_mode,
                    &result,
                    human,
                ))
            }
            LearningFeedbackCommand::List { review_id, unit_id } => {
                let store = open_required(root, db)?;
                let audit = feedback::audit_case(&store, review_id, unit_id)?;
                let human = human_feedback_audit(&audit);
                Ok(rendered_output(
                    "code-review learning feedback list",
                    json_mode,
                    &audit,
                    human,
                ))
            }
            LearningFeedbackCommand::Show { event_id } => {
                let store = open_required(root, db)?;
                let event = feedback::show_event(&store, event_id)?;
                let human = human_feedback_event(&event);
                Ok(rendered_output(
                    "code-review learning feedback show",
                    json_mode,
                    &event,
                    human,
                ))
            }
        }
    }

    /// Вход записи события обратной связи.
    struct FeedbackRecordOptions<'a> {
        review_id: &'a str,
        unit_id: &'a str,
        candidate_id: Option<&'a str>,
        kind: LearningFeedbackKindArg,
        action: LearningFeedbackActionArg,
        disposition: Option<LearningDispositionArg>,
        usefulness: Option<LearningUsefulnessArg>,
        supersedes_event_id: Option<&'a str>,
        explanation: &'a str,
        provenance: &'a str,
        event_id: Option<&'a str>,
    }

    fn record_feedback(
        root: &Path,
        db: Option<&Path>,
        options: &FeedbackRecordOptions<'_>,
    ) -> Result<FeedbackResult, DomainError> {
        let store = LearningStore::open(database_options(root, db, true)?)?;
        let kind = match options.kind {
            LearningFeedbackKindArg::Usefulness => FeedbackKind::RecommendationUsefulness,
            LearningFeedbackKindArg::SemanticOutcomeRevision => {
                FeedbackKind::SemanticOutcomeRevision
            }
        };
        let action = match options.action {
            LearningFeedbackActionArg::Append => FeedbackAction::Append,
            LearningFeedbackActionArg::Retract => FeedbackAction::Retract,
            LearningFeedbackActionArg::Supersede => FeedbackAction::Supersede,
        };
        let effective_disposition = options.disposition.map(LearningDispositionArg::as_str);
        let usefulness = options.usefulness.map(LearningUsefulnessArg::as_str);
        let explanation = crate::code_review::learning::import::sanitize_text(options.explanation);
        if explanation_is_empty(&explanation) {
            return Err(invalid_request(
                "Объяснение утверждения не может быть пустым",
            ));
        }
        let event_id = match options.event_id {
            Some(id) => id.to_owned(),
            None => feedback::event_id_for(&format!(
                "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
                options.review_id,
                options.unit_id,
                options.candidate_id.unwrap_or_default(),
                kind.as_str(),
                action_name(action),
                effective_disposition.unwrap_or_default(),
                usefulness.unwrap_or_default(),
                options.supersedes_event_id.unwrap_or_default(),
                explanation,
                options.provenance,
            )),
        };
        let event = FeedbackEvent {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            event_id: event_id.clone(),
            review_id: options.review_id.to_owned(),
            unit_id: options.unit_id.to_owned(),
            candidate_id: options.candidate_id.map(str::to_owned),
            kind,
            action,
            supersedes_event_id: options.supersedes_event_id.map(str::to_owned),
            effective_disposition: effective_disposition.map(str::to_owned),
            usefulness: usefulness.map(str::to_owned),
            explanation,
            provenance: options.provenance.to_owned(),
            recorded_at: unix_now(),
        };
        match feedback::show_event(&store, &event_id) {
            Ok(existing) => {
                if same_feedback_content(&existing, &event) {
                    if feedback::is_event_active(&store, &existing.event_id)? {
                        return Ok(noop_feedback_result(&store, &event));
                    }
                    return Err(DomainError::with_details(
                        ErrorCode::LearningConflict,
                        format!(
                            "Событие обратной связи {event_id} уже отозвано или заменено; задайте новый --event-id"
                        ),
                        crate::details! { "event_id" => event_id },
                    ));
                }
                return Err(DomainError::with_details(
                    ErrorCode::LearningConflict,
                    format!(
                        "Событие обратной связи {event_id} уже существует с другим содержанием"
                    ),
                    crate::details! { "event_id" => event_id },
                ));
            }
            Err(error) if error.code == ErrorCode::NotFound => {}
            Err(error) => return Err(error),
        }
        feedback::record_feedback(&store, &event)
    }

    fn export(root: &Path, db: Option<&Path>, out: &Path) -> Result<ExportOutput, DomainError> {
        let store = open_required(root, db)?;
        let summary = transfer::export_history(&store)?;
        let bytes = to_json_bytes(&summary.archive)?;
        let artifact = publish_derived_document(root, out, &bytes)?;
        Ok(ExportOutput {
            manifest: summary.manifest,
            artifact,
        })
    }

    fn backup(root: &Path, db: Option<&Path>, out: &Path) -> Result<BackupOutput, DomainError> {
        let store = open_required(root, db)?;
        let target = local_output_path(root, out)?;
        if target.exists() {
            return Err(DomainError::with_details(
                ErrorCode::ReviewArtifactConflict,
                "Файл назначения backup уже существует; перезапись не выполняется",
                crate::details! { "path" => display_path(root, &target) },
            ));
        }
        store.backup_to(&target)?;
        let bytes = fs::metadata(&target)
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        Ok(BackupOutput {
            database_path: store.options().display_path.clone(),
            destination: display_path(root, &target),
            bytes,
        })
    }

    fn restore(
        root: &Path,
        db: Option<&Path>,
        archive: &Path,
    ) -> Result<RestoreOutput, DomainError> {
        let bytes = fs::read(archive).map_err(|error| {
            DomainError::new(
                ErrorCode::InputUnreadable,
                format!("не удалось прочитать архив learning: {error}"),
            )
        })?;
        let document: LearningExport = serde_json::from_slice(&bytes).map_err(|error| {
            DomainError::new(
                ErrorCode::LearningExportInvalid,
                format!("Некорректный JSON архива learning: {error}"),
            )
        })?;
        transfer::verify_export(&document)?;
        let store = LearningStore::open(database_options(root, db, true)?)?;
        let summary = transfer::restore_history(&store, &document)?;
        Ok(RestoreOutput {
            archive: display_path(root, archive),
            summary,
        })
    }

    fn policy_command(
        root: &Path,
        db: Option<&Path>,
        command: &LearningPolicyCommand,
        json_mode: bool,
    ) -> Result<Rendered, DomainError> {
        match command {
            LearningPolicyCommand::Propose { signature, rule_id } => {
                let store = open_required(root, db)?;
                if patterns::support_for_signature(&store, signature, unix_now())?.is_none() {
                    return Err(DomainError::with_details(
                        ErrorCode::NotFound,
                        "Паттерн с такой подписью не найден в проверенной истории",
                        crate::details! { "signature" => signature.clone() },
                    ));
                }
                let proposal = feedback::propose_policy(&store, signature, rule_id, unix_now())?;
                let human = human_policy_proposal(&proposal);
                Ok(rendered_output(
                    "code-review learning policy propose",
                    json_mode,
                    &proposal,
                    human,
                ))
            }
            LearningPolicyCommand::List { limit } => {
                let store = open_required(root, db)?;
                let proposals = feedback::list_proposals(&store, to_usize(*limit))?;
                let output = PolicyListOutput {
                    proposals: proposals.iter().map(PolicySummary::from).collect(),
                    limitations: vec![
                        "Предложения не применяются автоматически и не выполняют suppression."
                            .to_owned(),
                    ],
                };
                let human = human_policy_list(&output);
                Ok(rendered_output(
                    "code-review learning policy list",
                    json_mode,
                    &output,
                    human,
                ))
            }
            LearningPolicyCommand::Show { id } => {
                let store = open_required(root, db)?;
                let proposal = feedback::show_proposal(&store, id)?;
                let human = human_policy_proposal(&proposal);
                Ok(rendered_output(
                    "code-review learning policy show",
                    json_mode,
                    &proposal,
                    human,
                ))
            }
            LearningPolicyCommand::Approve { id, out, note } => {
                let store = open_required(root, db)?;
                let proposal = feedback::show_proposal(&store, id)?;
                if proposal.supporting_cases.is_empty() {
                    return Err(DomainError::with_details(
                        ErrorCode::InsufficientEvidence,
                        "У предложения нет подтверждающих случаев: утверждать нечего",
                        crate::details! { "proposal_id" => proposal.proposal_id.clone() },
                    ));
                }
                let document = ApprovedPolicyArtifact {
                    schema_version: POLICY_SCHEMA_VERSION,
                    policy_version: LEARNING_POLICY_VERSION,
                    generation: proposal.generation,
                    approved: true,
                    auto_applied: false,
                    proposal: &proposal,
                    note: note.as_deref(),
                    limitations: vec![
                        "Артефакт материализован человеком и попадает в Git только его решением."
                            .to_owned(),
                        "SQLite не является владельцем утверждённой политики: база хранит только предложение."
                            .to_owned(),
                        "Автоматическое применение правила и автоматический suppression не выполняются."
                            .to_owned(),
                    ],
                };
                let bytes = to_json_bytes(&document)?;
                let artifact = publish_derived_document(root, out, &bytes)?;
                let output = PolicyApproveOutput {
                    proposal_id: proposal.proposal_id.clone(),
                    rule_id: proposal.rule_id.clone(),
                    auto_applied: false,
                    artifact,
                };
                let human = human_policy_approve(&output);
                Ok(rendered_output(
                    "code-review learning policy approve",
                    json_mode,
                    &output,
                    human,
                ))
            }
        }
    }

    // ---- хранилище и пути ----

    /// Корень текущего репозитория: путь базы и рабочих областей не зависит от cwd.
    fn repository_root() -> Result<PathBuf, DomainError> {
        let cwd = std::env::current_dir().map_err(|error| {
            DomainError::new(
                ErrorCode::GitEvidenceFailed,
                format!("не удалось определить текущий каталог: {error}"),
            )
        })?;
        let output = Command::new("git")
            .current_dir(&cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|error| {
                DomainError::new(
                    ErrorCode::GitEvidenceFailed,
                    format!("не удалось запустить Git: {error}"),
                )
            })?;
        if !output.status.success() {
            return Err(DomainError::new(
                ErrorCode::GitEvidenceFailed,
                "команды learning работают только внутри Git-репозитория",
            ));
        }
        let text = String::from_utf8(output.stdout).map_err(|_| {
            DomainError::new(
                ErrorCode::GitEvidenceFailed,
                "Git вернул некорректный путь корня репозитория",
            )
        })?;
        fs::canonicalize(text.trim()).map_err(|error| {
            DomainError::new(
                ErrorCode::GitEvidenceFailed,
                format!("не удалось разрешить корень репозитория: {error}"),
            )
        })
    }

    /// Параметры открытия базы: относительный `--db` разрешается от корня репозитория.
    fn database_options(
        root: &Path,
        db: Option<&Path>,
        create: bool,
    ) -> Result<StoreOptions, DomainError> {
        let options = match db {
            None => StoreOptions::in_repository(root),
            Some(requested) => {
                if requested.as_os_str().is_empty() {
                    return Err(invalid_request("путь базы learning не может быть пустым"));
                }
                if requested
                    .components()
                    .any(|component| component == Component::ParentDir)
                {
                    return Err(invalid_request(
                        "компонент `..` в пути базы learning запрещён",
                    ));
                }
                let resolved = if requested.is_absolute() {
                    requested.to_path_buf()
                } else {
                    root.join(requested)
                };
                reject_protected_path(root, &resolved, "базы learning")?;
                StoreOptions::at(resolved)
            }
        };
        Ok(StoreOptions { create, ..options })
    }

    /// Открывает существующую базу; отсутствие файла — различимый `not_found`.
    fn open_required(root: &Path, db: Option<&Path>) -> Result<LearningStore, DomainError> {
        let options = database_options(root, db, false)?;
        if !options.database.is_file() {
            return Err(DomainError::with_details(
                ErrorCode::NotFound,
                "Локальная база learning не найдена; импортируйте проверенную историю или укажите --db",
                crate::details! {
                    "database" => options.display_path.clone(),
                    "recovery" => store::recovery_paths(&options.display_path),
                },
            ));
        }
        LearningStore::open(options)
    }

    /// Открывает базу, если она существует: штатный режим без learning.
    fn open_optional(root: &Path, db: Option<&Path>) -> Result<Option<LearningStore>, DomainError> {
        let options = database_options(root, db, false)?;
        if !options.database.is_file() {
            return Ok(None);
        }
        LearningStore::open(options).map(Some)
    }

    /// Состояние отсутствующей истории без создания базы.
    fn absent_status(options: &StoreOptions) -> LearningStatus {
        LearningStatus {
            schema_version: LEARNING_SCHEMA_VERSION,
            policy_version: LEARNING_POLICY_VERSION,
            present: false,
            database_path: options.display_path.clone(),
            user_version: 0,
            journal_mode: "none".to_owned(),
            journal_mode_reason: "файл базы learning отсутствует: режим журнала не выбирался"
                .to_owned(),
            busy_timeout_ms: options.busy_timeout_ms,
            fts5_available: false,
            foreign_keys: false,
            generation: HistoryGeneration {
                revision: 0,
                trusted_reviews: 0,
                quarantined_reviews: 0,
                trusted_units: 0,
            },
            integrity_ok: false,
            unavailable_reason: Some(
                "История learning отсутствует: обычное ревью продолжает работать без неё."
                    .to_owned(),
            ),
            recovery_paths: store::recovery_paths(&options.display_path),
        }
    }

    /// Запрещает служебные каталоги репозитория и выход за его пределы.
    ///
    /// Проверка структурная и не ограничивается строкой запрошенного пути: он
    /// разрешается до существующего предка, поэтому символическая ссылка на
    /// `decks/**`, на служебные данные Git или на каталог вне клона не обходит
    /// запрет.
    fn reject_protected_path(root: &Path, path: &Path, label: &str) -> Result<(), DomainError> {
        let relative = path.strip_prefix(root).map_err(|_| {
            DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("путь {label} должен находиться в текущем репозитории"),
                crate::details! { "path" => display_path(root, path) },
            )
        })?;
        reject_service_components(root, relative, path, label)?;
        let mut ancestor = path.to_path_buf();
        while !ancestor.exists() {
            if !ancestor.pop() {
                return Ok(());
            }
        }
        let canonical = fs::canonicalize(&ancestor).map_err(|error| {
            DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("путь {label} не удалось разрешить: {error}"),
                crate::details! { "path" => display_path(root, path) },
            )
        })?;
        let canonical_relative = canonical.strip_prefix(root).map_err(|_| {
            DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("путь {label} должен находиться в текущем репозитории"),
                crate::details! { "path" => display_path(root, path) },
            )
        })?;
        reject_service_components(root, canonical_relative, path, label)
    }

    /// Запрещает первый компонент пути в служебных каталогах репозитория.
    fn reject_service_components(
        root: &Path,
        relative: &Path,
        path: &Path,
        label: &str,
    ) -> Result<(), DomainError> {
        let first = relative.components().next();
        let protected =
            matches!(first, Some(Component::Normal(name)) if name == "decks" || name == ".git");
        if protected {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("путь {label} не может находиться в служебном каталоге репозитория"),
                crate::details! { "path" => display_path(root, path) },
            ));
        }
        Ok(())
    }

    /// Публикует производный документ learning по безопасному пути набора.
    ///
    /// Проверка пути (служебные каталоги, симлинки, тип существующего файла)
    /// принадлежит каноническому помощнику `workflow::safe_output_path`, а
    /// запись — `workflow::write_review_document_once`: у политики путей один
    /// владелец, а не копия в CLI. Дополнительное условие локальности —
    /// требование самой learning-подсистемы: история, архив и снимок базы
    /// живут в клоне, поэтому разрешённый путь не может вести за его пределы.
    fn publish_derived_document(
        root: &Path,
        requested: &Path,
        bytes: &[u8],
    ) -> Result<PublishedArtifact, DomainError> {
        let target = local_output_path(root, requested)?;
        publish_document(root, &target, bytes)
    }

    /// Разрешает путь производного артефакта learning внутри клона.
    ///
    /// Общая часть `export`, `policy approve` и `backup`: у политики путей один
    /// владелец — канонический `workflow::safe_output_path`, а требование
    /// локальности принадлежит learning-подсистеме.
    fn local_output_path(root: &Path, requested: &Path) -> Result<PathBuf, DomainError> {
        let target = safe_output_path(root, requested, false)?;
        if !target.starts_with(root) {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                "артефакт learning должен находиться внутри текущего репозитория",
                crate::details! { "path" => display_path(root, &target) },
            ));
        }
        Ok(target)
    }

    /// Публикует производный документ в уже разрешённый путь.
    ///
    /// Повтор тех же байтов идемпотентен, другие байты дают конфликт артефакта;
    /// путь записи разрешает вызывающая сторона.
    fn publish_document(
        root: &Path,
        target: &Path,
        bytes: &[u8],
    ) -> Result<PublishedArtifact, DomainError> {
        let created = !target.exists();
        write_review_document_once(target, bytes)?;
        Ok(PublishedArtifact {
            path: display_path(root, target),
            bytes: bytes.len(),
            created,
        })
    }

    /// Показывает путь в выводе относительно корня репозитория.
    ///
    /// Абсолютный путь клона в вывод не попадает: он бесполезен при переносе и
    /// запрещён правилами репозитория. Путь вне корня показывается как есть —
    /// скрывать его нельзя, иначе человек не поймёт, куда записан артефакт.
    fn display_path(root: &Path, path: &Path) -> String {
        path.strip_prefix(root).map_or_else(
            |_| path.display().to_string(),
            |relative| relative.to_string_lossy().replace('\\', "/"),
        )
    }

    /// Доменная ошибка некорректного запроса к CLI.
    fn invalid_request(message: impl Into<String>) -> DomainError {
        DomainError::new(ErrorCode::InvalidRequest, message)
    }

    fn to_usize(value: u64) -> usize {
        usize::try_from(value).unwrap_or(usize::MAX)
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or_default()
    }

    fn to_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, DomainError> {
        serde_json::to_vec_pretty(value).map_err(|error| {
            DomainError::new(
                ErrorCode::Internal,
                format!("не удалось сериализовать документ learning: {error}"),
            )
        })
    }

    fn explanation_is_empty(explanation: &str) -> bool {
        explanation.trim().is_empty()
    }

    fn action_name(action: FeedbackAction) -> &'static str {
        match action {
            FeedbackAction::Append => "append",
            FeedbackAction::Retract => "retract",
            FeedbackAction::Supersede => "supersede",
        }
    }

    /// Сравнивает содержание событий без метки времени записи.
    fn same_feedback_content(left: &FeedbackEvent, right: &FeedbackEvent) -> bool {
        left.event_id == right.event_id
            && left.review_id == right.review_id
            && left.unit_id == right.unit_id
            && left.candidate_id == right.candidate_id
            && left.kind == right.kind
            && left.action == right.action
            && left.supersedes_event_id == right.supersedes_event_id
            && left.effective_disposition == right.effective_disposition
            && left.usefulness == right.usefulness
            && left.explanation == right.explanation
            && left.provenance == right.provenance
    }

    /// Идемпотентный повтор: событие уже записано с тем же содержанием.
    fn noop_feedback_result(store: &LearningStore, event: &FeedbackEvent) -> FeedbackResult {
        FeedbackResult {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            event_id: event.event_id.clone(),
            superseded_event_id: None,
            retracted_event_id: None,
            outcome: feedback::outcome(store, &event.review_id, &event.unit_id)
                .unwrap_or_else(|_| feedback::empty_outcome()),
            limitations: vec![
                "Событие с тем же содержанием уже записано: повтор не создал дубликата.".to_owned(),
            ],
        }
    }

    fn accumulate(target: &mut ObservationCounts, source: &ObservationCounts) {
        target.raw_candidates += source.raw_candidates;
        target.covered_candidates += source.covered_candidates;
        target.individual_decisions += source.individual_decisions;
        target.group_decisions += source.group_decisions;
        target.reviewed_units += source.reviewed_units;
        target.unresolved_units += source.unresolved_units;
        target.findings += source.findings;
        target.findings_direct_candidate += source.findings_direct_candidate;
        target.findings_candidate_assisted += source.findings_candidate_assisted;
        target.findings_independent += source.findings_independent;
        target.unreviewed_candidates += source.unreviewed_candidates;
        target.uncertain_candidates += source.uncertain_candidates;
        target.not_applicable_candidates += source.not_applicable_candidates;
        target.confirmed_candidates += source.confirmed_candidates;
        target.acceptable_candidates += source.acceptable_candidates;
        target.false_positive_candidates += source.false_positive_candidates;
    }

    fn short_sha(sha: &str) -> &str {
        &sha[..sha.len().min(12)]
    }

    fn feature_key(key: &BTreeMap<String, String>) -> String {
        key.iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn push_list(out: &mut String, title: &str, items: &[String]) {
        if items.is_empty() {
            return;
        }
        out.push_str(&format!("{title}:\n"));
        for item in items {
            out.push_str(&format!("  - {item}\n"));
        }
    }

    fn import_status_label(status: ImportStatus) -> &'static str {
        match status {
            ImportStatus::Imported => "сохранено",
            ImportStatus::NoopExisting => "точный повтор: ничего не удвоено",
            ImportStatus::RevisionCreated => "сохранена аудируемая ревизия",
            ImportStatus::Quarantined => "сохранено с карантином",
        }
    }

    // ---- JSON DTO вывода ----

    /// Статистика по ограниченной странице истории.
    #[derive(Serialize)]
    struct StatsOutput {
        schema_version: u32,
        policy_version: u32,
        database_path: String,
        generation: HistoryGeneration,
        reviews_total: usize,
        reviews_shown: usize,
        reviews_truncated: bool,
        observations: ObservationCounts,
        dispositions: BTreeMap<String, usize>,
        reviews: Vec<ImportRecord>,
        limitations: Vec<String>,
    }

    /// Отчёт по паттернам с ограниченными списками.
    #[derive(Serialize)]
    struct PatternsOutput {
        schema_version: u32,
        policy_version: u32,
        generation: HistoryGeneration,
        min_support_units: usize,
        policy: String,
        abstained: bool,
        rules_total: usize,
        rules: Vec<PatternRuleView>,
        limitations: Vec<String>,
    }

    impl PatternsOutput {
        fn from_report(report: &PatternReport) -> Self {
            Self {
                schema_version: report.schema_version,
                policy_version: report.policy_version,
                generation: report.generation,
                min_support_units: report.min_support_units,
                policy: report.policy.clone(),
                abstained: report.abstained,
                rules_total: report.rules.len(),
                rules: report.rules.iter().map(PatternRuleView::from).collect(),
                limitations: report.limitations.clone(),
            }
        }
    }

    /// Одно правило отчёта с ограниченным срезом поддержки.
    #[derive(Serialize)]
    struct PatternRuleView {
        key: BTreeMap<String, String>,
        signature: String,
        support: SupportView,
    }

    impl PatternRuleView {
        fn from(rule: &crate::code_review::learning::model::PatternRule) -> Self {
            Self {
                key: rule.key.clone(),
                signature: rule.signature.clone(),
                support: SupportView::from(&rule.support),
            }
        }
    }

    /// Срез поддержки с явной отметкой усечения списка противоречащих единиц.
    #[derive(Serialize)]
    struct SupportView {
        summary: SupportSummary,
        contradicting_unit_ids_truncated: bool,
    }

    impl SupportView {
        fn from(summary: &SupportSummary) -> Self {
            let mut bounded = summary.clone();
            let truncated = bounded.contradicting_unit_ids.len() > OUTPUT_CONTRADICTING_LIMIT;
            bounded
                .contradicting_unit_ids
                .truncate(OUTPUT_CONTRADICTING_LIMIT);
            Self {
                summary: bounded,
                contradicting_unit_ids_truncated: truncated,
            }
        }
    }

    /// Опубликованный производный артефакт.
    #[derive(Serialize)]
    struct PublishedArtifact {
        path: String,
        bytes: usize,
        created: bool,
    }

    /// Документ рекомендаций без полных списков candidate_ids.
    #[derive(Serialize)]
    struct RecommendOutput {
        schema_version: u32,
        policy_version: u32,
        generation: HistoryGeneration,
        repository_id: String,
        base_sha: String,
        head_sha: String,
        merge_base_sha: String,
        workspace_variant: String,
        review_pack_sha256: String,
        queue_sha256: String,
        input_trust: TrustLevel,
        learning_disabled: bool,
        units_total: usize,
        recommendations_shown: usize,
        recommendations_truncated: bool,
        suggested_order: Vec<String>,
        recommendations: Vec<RecommendationView>,
        limitations: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        artifact: Option<PublishedArtifact>,
    }

    impl RecommendOutput {
        fn from_document(document: &Recommendations, units_total: usize) -> Self {
            let recommendations: Vec<RecommendationView> = document
                .recommendations
                .iter()
                .map(RecommendationView::from)
                .collect();
            Self {
                schema_version: document.schema_version,
                policy_version: document.policy_version,
                generation: document.generation,
                repository_id: document.repository_id.clone(),
                base_sha: document.base_sha.clone(),
                head_sha: document.head_sha.clone(),
                merge_base_sha: document.merge_base_sha.clone(),
                workspace_variant: document.workspace_variant.clone(),
                review_pack_sha256: document.review_pack_sha256.clone(),
                queue_sha256: document.queue_sha256.clone(),
                input_trust: document.input_trust,
                learning_disabled: document.learning_disabled,
                units_total,
                recommendations_shown: recommendations.len(),
                recommendations_truncated: units_total > recommendations.len(),
                suggested_order: document.suggested_order.clone(),
                recommendations,
                limitations: document.limitations.clone(),
                artifact: None,
            }
        }
    }

    /// Одна подсказка: счётчик кандидатов вместо их полного списка.
    #[derive(Serialize)]
    struct RecommendationView {
        unit_id: String,
        candidate_count: usize,
        representative_candidate_ids: Vec<String>,
        queue_priority: String,
        suggested_position: String,
        granularity: String,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pattern_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        support: Option<SupportView>,
        historical_cases: Vec<CaseRefView>,
        limitations: Vec<String>,
        source_reference: String,
    }

    impl RecommendationView {
        fn from(item: &Recommendation) -> Self {
            Self {
                unit_id: item.unit_id.clone(),
                candidate_count: item.candidate_ids.len(),
                representative_candidate_ids: item.representative_candidate_ids.clone(),
                queue_priority: item.queue_priority.clone(),
                suggested_position: item.suggested_position.clone(),
                granularity: item.granularity.clone(),
                reason: item.reason.clone(),
                pattern_signature: item.pattern_signature.clone(),
                support: item.support.as_ref().map(SupportView::from),
                historical_cases: item
                    .historical_cases
                    .iter()
                    .take(OUTPUT_CASE_LIMIT)
                    .map(CaseRefView::from)
                    .collect(),
                limitations: item.limitations.clone(),
                source_reference: item.source_reference.clone(),
            }
        }
    }

    /// Исторический случай с ограниченным списком кандидатов.
    #[derive(Serialize)]
    struct CaseRefView {
        review_id: String,
        unit_id: String,
        candidate_count: usize,
        candidate_ids: Vec<String>,
        candidate_ids_truncated: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        snippet: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        disposition: Option<String>,
        trust: TrustLevel,
        age_days: u64,
    }

    impl CaseRefView {
        fn from(case: &CaseRef) -> Self {
            Self {
                review_id: case.review_id.clone(),
                unit_id: case.unit_id.clone(),
                candidate_count: case.candidate_ids.len(),
                candidate_ids: case
                    .candidate_ids
                    .iter()
                    .take(OUTPUT_CANDIDATE_LIMIT)
                    .cloned()
                    .collect(),
                candidate_ids_truncated: case.candidate_ids.len() > OUTPUT_CANDIDATE_LIMIT,
                path: case.path.clone(),
                snippet: case.snippet.clone(),
                disposition: case.disposition.clone(),
                trust: case.trust,
                age_days: case.age_days,
            }
        }
    }

    /// Результат экспорта переносимого архива.
    #[derive(Serialize)]
    struct ExportOutput {
        manifest: LearningExportManifest,
        artifact: PublishedArtifact,
    }

    /// Результат транзакционного backup.
    #[derive(Serialize)]
    struct BackupOutput {
        database_path: String,
        destination: String,
        bytes: u64,
    }

    /// Результат восстановления архива.
    #[derive(Serialize)]
    struct RestoreOutput {
        archive: String,
        summary: RestoreSummary,
    }

    /// Компактное представление предложения политики.
    #[derive(Serialize)]
    struct PolicySummary {
        proposal_id: String,
        rule_id: String,
        generation: HistoryGeneration,
        key: BTreeMap<String, String>,
        supporting_cases: usize,
        contradicting_cases: usize,
        cautions: Vec<String>,
        auto_applied: bool,
        artifact_path: String,
    }

    impl PolicySummary {
        fn from(proposal: &PolicyProposal) -> Self {
            Self {
                proposal_id: proposal.proposal_id.clone(),
                rule_id: proposal.rule_id.clone(),
                generation: proposal.generation,
                key: proposal.key.clone(),
                supporting_cases: proposal.supporting_cases.len(),
                contradicting_cases: proposal.contradicting_cases.len(),
                cautions: proposal.cautions.clone(),
                auto_applied: proposal.auto_applied,
                artifact_path: proposal.artifact_path.clone(),
            }
        }
    }

    /// Список предложений политики.
    #[derive(Serialize)]
    struct PolicyListOutput {
        proposals: Vec<PolicySummary>,
        limitations: Vec<String>,
    }

    /// Утверждённый артефакт политики, который коммитит человек.
    #[derive(Serialize)]
    struct ApprovedPolicyArtifact<'a> {
        schema_version: u32,
        policy_version: u32,
        generation: HistoryGeneration,
        approved: bool,
        auto_applied: bool,
        proposal: &'a PolicyProposal,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<&'a str>,
        limitations: Vec<String>,
    }

    /// Итог утверждения политики.
    #[derive(Serialize)]
    struct PolicyApproveOutput {
        proposal_id: String,
        rule_id: String,
        auto_applied: bool,
        artifact: PublishedArtifact,
    }

    // ---- русский human output ----

    fn human_status(status: &LearningStatus, validated: bool) -> String {
        let mut out = String::new();
        out.push_str(&format!("База learning: {}\n", status.database_path));
        out.push_str(&format!(
            "Состояние: {}\n",
            if status.present {
                "присутствует"
            } else {
                "отсутствует"
            }
        ));
        out.push_str(&format!(
            "Схема: user_version {}, документ {}, политика {}\n",
            status.user_version, status.schema_version, status.policy_version
        ));
        out.push_str(&format!(
            "Журнал: {} ({})\n",
            status.journal_mode, status.journal_mode_reason
        ));
        out.push_str(&format!(
            "Целостность: {}\n",
            if status.integrity_ok {
                "ok"
            } else {
                "не проверена"
            }
        ));
        out.push_str(&format!(
            "Поиск FTS5: {}\n",
            if status.fts5_available {
                "доступен"
            } else {
                "недоступен; используется подстрочный fallback"
            }
        ));
        out.push_str(&format!(
            "История: ревизия {}, проверенных записей {}, карантинных {}, независимых единиц {}\n",
            status.generation.revision,
            status.generation.trusted_reviews,
            status.generation.quarantined_reviews,
            status.generation.trusted_units
        ));
        if let Some(reason) = status.unavailable_reason.as_deref() {
            out.push_str(&format!("Недоступность: {reason}\n"));
        }
        if validated {
            out.push_str("Проверка: успешно\n");
        }
        if !status.present {
            push_list(&mut out, "Восстановление", &status.recovery_paths);
        }
        out
    }

    fn human_import(outcome: &import::ImportOutcome) -> String {
        let record = &outcome.record;
        let mut out = String::new();
        out.push_str(&format!(
            "Импорт: {} — {}\n",
            import_status_label(outcome.status),
            record.review_id
        ));
        out.push_str(&format!(
            "Снимок: {}@{} (вариант {})\n",
            record.repository_id,
            short_sha(&record.head_sha),
            record.workspace_variant
        ));
        out.push_str(&format!(
            "Доверие: {}; итог рассмотрения: {}\n",
            record.trust.as_str(),
            record.outcome.as_str()
        ));
        let revision_note = match record.revision_of.as_deref() {
            Some(previous) => format!(", аудируемая ревизия записи {previous}"),
            None => String::new(),
        };
        out.push_str(&format!("Ревизия: {}{revision_note}\n", record.revision));
        out.push_str(&format!(
            "Наблюдения: кандидатов {}, рассмотрено единиц {}, нерассмотренных {}, замечаний {} (independent {})\n",
            record.observations.raw_candidates,
            record.observations.reviewed_units,
            record.observations.unresolved_units,
            record.observations.findings,
            record.observations.findings_independent
        ));
        push_list(&mut out, "Ограничения", &record.limitations);
        out
    }

    fn human_stats(output: &StatsOutput) -> String {
        let mut out = String::new();
        out.push_str(&format!("История learning: {}\n", output.database_path));
        out.push_str(&format!(
            "Ревизия {}: проверенных записей {}, карантинных {}, независимых единиц {}\n",
            output.generation.revision,
            output.generation.trusted_reviews,
            output.generation.quarantined_reviews,
            output.generation.trusted_units
        ));
        out.push_str(&format!(
            "Записи: показано {} из {}{}\n",
            output.reviews_shown,
            output.reviews_total,
            if output.reviews_truncated {
                " (увеличьте --limit, чтобы увидеть остальные)"
            } else {
                ""
            }
        ));
        for record in &output.reviews {
            out.push_str(&format!(
                "  {} {} {} {}: кандидатов {}, единиц {}, замечаний {}\n",
                record.review_id,
                short_sha(&record.head_sha),
                record.workspace_variant,
                record.trust.as_str(),
                record.observations.raw_candidates,
                record.observations.reviewed_units,
                record.observations.findings
            ));
        }
        out.push_str(&format!(
            "Наблюдения по показанным записям: кандидатов {}, individual {}, group {}, reviewed {}, unresolved {}, замечаний {} (independent {})\n",
            output.observations.raw_candidates,
            output.observations.individual_decisions,
            output.observations.group_decisions,
            output.observations.reviewed_units,
            output.observations.unresolved_units,
            output.observations.findings,
            output.observations.findings_independent
        ));
        out.push_str("Действующие исходы независимых единиц:\n");
        if output.dispositions.is_empty() {
            out.push_str("  нет данных\n");
        }
        for (disposition, count) in &output.dispositions {
            out.push_str(&format!("  {disposition}: {count}\n"));
        }
        push_list(&mut out, "Ограничения", &output.limitations);
        out
    }

    fn human_patterns(output: &PatternsOutput) -> String {
        let mut out = String::new();
        let supported = output
            .rules
            .iter()
            .filter(|rule| rule.support.summary.level.as_str() == "supported")
            .count();
        out.push_str(&format!(
            "Паттерны: правил {}, подтверждённых {}, воздержание: {}\n",
            output.rules_total,
            supported,
            if output.abstained { "да" } else { "нет" }
        ));
        out.push_str(&format!(
            "Политика {}: минимум поддержки {} независимых единиц; история: проверенных записей {}, единиц {}\n",
            output.policy_version,
            output.min_support_units,
            output.generation.trusted_reviews,
            output.generation.trusted_units
        ));
        for rule in &output.rules {
            let summary = &rule.support.summary;
            out.push_str(&format!(
                "  {} [{}] независимых единиц {}, записей {}, свёрнутых повторов {}, без решения {}, confirmed {}, acceptable {}, false_positive {}, уровень {}\n",
                short_sha(&rule.signature),
                rule.key.get("detector").map_or("unknown", String::as_str),
                summary.support_units,
                summary.support_reviews,
                summary.revised_units,
                summary.unresolved_units,
                summary.confirmed_units,
                summary.acceptable_units,
                summary.false_positive_units,
                summary.level.as_str()
            ));
            out.push_str(&format!("    ключ: {}\n", feature_key(&rule.key)));
            out.push_str(&format!("    {}\n", summary.explanation));
            if rule.support.contradicting_unit_ids_truncated {
                out.push_str(&format!(
                    "    противоречащие единицы: показаны первые {OUTPUT_CONTRADICTING_LIMIT}\n"
                ));
            }
        }
        push_list(&mut out, "Ограничения", &output.limitations);
        out
    }

    /// Поясняет доверие возвращённого случая: карантин не выдаётся за историю.
    fn trust_marker(trust: &str) -> &'static str {
        match trust {
            "ast_authenticated" => " — проверенная история",
            _ => " — карантин, не полноценное основание",
        }
    }

    fn human_search(page: &search::SearchPage) -> String {
        let mut out = String::new();
        let first = if page.cases.is_empty() {
            0
        } else {
            page.offset + 1
        };
        out.push_str(&format!(
            "Поиск: найдено {}, страница {}..{}, продолжение: {}, FTS5: {}\n",
            page.matched,
            first,
            page.offset + page.cases.len(),
            if page.has_more { "да" } else { "нет" },
            if page.fts5_used { "да" } else { "нет" }
        ));
        // Доверие — часть смысла результата, а не деталь: найденная аналогия
        // используется как вспомогательный контекст, поэтому ни фильтр, ни
        // статус возвращённого случая не скрываются.
        out.push_str(&format!(
            "Карантин: {}\n",
            if page.include_quarantine {
                "включён по явному --include-quarantine: такие записи не являются полноценным основанием"
            } else {
                "исключён по умолчанию (включить: --include-quarantine)"
            }
        ));
        for case in &page.cases {
            out.push_str(&format!(
                "  {} [{}] {} {} — {}\n",
                case.case_id,
                case.match_kind.as_str(),
                case.path.as_deref().unwrap_or("путь не сохранён"),
                case.disposition.as_deref().unwrap_or("без решения"),
                case.snippet
            ));
            out.push_str(&format!(
                "    доверие: {}{}\n",
                case.trust,
                trust_marker(&case.trust)
            ));
            out.push_str(&format!("    источник: {}\n", case.source_reference));
        }
        if page.has_more {
            out.push_str(&format!(
                "Показаны не все совпадения: увеличьте --limit (не более {}) или сдвиньте --offset.\n",
                search::MAX_SEARCH_LIMIT
            ));
        }
        out
    }

    fn human_recommendations(output: &RecommendOutput) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Рекомендации: {}@{} (вариант {}), политика {}, история ревизии {}\n",
            output.repository_id,
            short_sha(&output.head_sha),
            output.workspace_variant,
            output.policy_version,
            output.generation.revision
        ));
        out.push_str(&format!(
            "История: проверенных записей {}, независимых единиц {}; режим без learning: {}\n",
            output.generation.trusted_reviews,
            output.generation.trusted_units,
            if output.learning_disabled {
                "да"
            } else {
                "нет"
            }
        ));
        out.push_str(&format!(
            "Подсказки: показано {} из {} единиц очереди{}\n",
            output.recommendations_shown,
            output.units_total,
            if output.recommendations_truncated {
                " (остальные не печатаются: увеличьте --limit)"
            } else {
                ""
            }
        ));
        for item in &output.recommendations {
            out.push_str(&format!(
                "  {}: priority {} → {}, гранулярность {}, кандидатов {}\n",
                item.unit_id,
                item.queue_priority,
                item.suggested_position,
                item.granularity,
                item.candidate_count
            ));
            out.push_str(&format!("    причина: {}\n", item.reason));
            if let Some(support) = item.support.as_ref() {
                out.push_str(&format!(
                    "    поддержка: единиц {}, записей {}, unresolved {}, уровень {}\n",
                    support.summary.support_units,
                    support.summary.support_reviews,
                    support.summary.unresolved_units,
                    support.summary.level.as_str()
                ));
            }
            for case in &item.historical_cases {
                out.push_str(&format!(
                    "    случай: {}/{} {} {}\n",
                    case.review_id,
                    case.unit_id,
                    case.disposition.as_deref().unwrap_or("без решения"),
                    case.path.as_deref().unwrap_or("")
                ));
            }
            out.push_str(&format!(
                "    первичный контекст: {}\n",
                item.source_reference
            ));
            push_list(&mut out, "    Ограничения подсказки", &item.limitations);
        }
        push_list(&mut out, "Ограничения документа", &output.limitations);
        if let Some(artifact) = output.artifact.as_ref() {
            out.push_str(&format!(
                "Артефакт: {} ({}, {} байт)\n",
                artifact.path,
                if artifact.created {
                    "создан"
                } else {
                    "уже существовал с теми же байтами"
                },
                artifact.bytes
            ));
        }
        out
    }

    fn human_feedback_result(result: &FeedbackResult) -> String {
        let mut out = String::new();
        out.push_str(&format!("Обратная связь: {}\n", result.event_id));
        if let Some(id) = result.superseded_event_id.as_deref() {
            out.push_str(&format!("Заменено утверждение: {id}\n"));
        }
        if let Some(id) = result.retracted_event_id.as_deref() {
            out.push_str(&format!("Отозвано утверждение: {id}\n"));
        }
        out.push_str(&format!(
            "Исход: исходный {}, действующий {}\n",
            result
                .outcome
                .original_disposition
                .as_deref()
                .unwrap_or("нет"),
            result
                .outcome
                .effective_disposition
                .as_deref()
                .unwrap_or("нет")
        ));
        if result.outcome.has_conflict {
            out.push_str("Внимание: по случаю есть противоречащие утверждения.\n");
        }
        push_list(&mut out, "Ограничения", &result.limitations);
        out
    }

    fn human_feedback_audit(audit: &CaseAudit) -> String {
        let mut out = String::new();
        out.push_str(&format!("Случай: {}/{}\n", audit.review_id, audit.unit_id));
        out.push_str(&format!(
            "Исход: исходный {}, действующий {}\n",
            audit
                .outcome
                .original_disposition
                .as_deref()
                .unwrap_or("нет"),
            audit
                .outcome
                .effective_disposition
                .as_deref()
                .unwrap_or("нет")
        ));
        if audit.outcome.has_conflict {
            out.push_str("Внимание: по случаю есть противоречащие утверждения.\n");
        }
        out.push_str(&format!("Утверждений: {}\n", audit.events.len()));
        for event in &audit.events {
            out.push_str(&format!(
                "  {} {} {} {} — {}\n",
                event.event_id,
                event.kind.as_str(),
                action_name(event.action),
                event
                    .effective_disposition
                    .as_deref()
                    .unwrap_or(if event.usefulness.is_some() {
                        "оценка полезности"
                    } else {
                        "без нового исхода"
                    }),
                event.explanation
            ));
        }
        out
    }

    fn human_feedback_event(event: &FeedbackEvent) -> String {
        let mut out = String::new();
        out.push_str(&format!("Событие: {}\n", event.event_id));
        out.push_str(&format!(
            "Случай: {}/{} (кандидат {})\n",
            event.review_id,
            event.unit_id,
            event.candidate_id.as_deref().unwrap_or("не указан")
        ));
        out.push_str(&format!(
            "Вид: {}, действие: {}\n",
            event.kind.as_str(),
            action_name(event.action)
        ));
        if let Some(disposition) = event.effective_disposition.as_deref() {
            out.push_str(&format!("Новый исход: {disposition}\n"));
        }
        if let Some(usefulness) = event.usefulness.as_deref() {
            out.push_str(&format!("Полезность рекомендации: {usefulness}\n"));
        }
        if let Some(id) = event.supersedes_event_id.as_deref() {
            out.push_str(&format!("Ссылается на утверждение: {id}\n"));
        }
        out.push_str(&format!(
            "Источник: {}; объяснение: {}\n",
            event.provenance, event.explanation
        ));
        out
    }

    fn human_export(output: &ExportOutput) -> String {
        let manifest = &output.manifest;
        let mut out = String::new();
        out.push_str(&format!(
            "Экспорт истории: {} ({}, {} байт)\n",
            output.artifact.path,
            if output.artifact.created {
                "создан"
            } else {
                "уже существовал с теми же байтами"
            },
            output.artifact.bytes
        ));
        out.push_str(&format!(
            "Архив: схема {}, политика {}, ревизия истории {}, записей {}, единиц {}, замечаний {}, утверждений {}, предложений {}\n",
            manifest.schema_version,
            manifest.policy_version,
            manifest.generation.revision,
            manifest.reviews,
            manifest.units,
            manifest.findings,
            manifest.feedback_events,
            manifest.policy_proposals
        ));
        out.push_str(&format!("Digest тела: {}\n", manifest.payload_sha256));
        out.push_str(
            "Архив не содержит сырых execution logs, credentials и содержимого временных worktrees.\n",
        );
        out
    }

    fn human_backup(output: &BackupOutput) -> String {
        format!(
            "Снимок базы: {} → {} ({} байт)\n",
            output.database_path, output.destination, output.bytes
        )
    }

    fn human_restore(output: &RestoreOutput) -> String {
        let summary = &output.summary;
        let mut out = String::new();
        out.push_str(&format!("Восстановление из {}\n", output.archive));
        out.push_str(&format!(
            "Восстановлено записей: {}, без изменений: {}\n",
            summary.restored_reviews, summary.unchanged_reviews
        ));
        out.push_str(&format!(
            "История: ревизия {}, проверенных записей {}, карантинных {}, независимых единиц {}\n",
            summary.generation.revision,
            summary.generation.trusted_reviews,
            summary.generation.quarantined_reviews,
            summary.generation.trusted_units
        ));
        push_list(&mut out, "Ограничения", &summary.limitations);
        out
    }

    fn human_forget(output: &ForgetOutcome) -> String {
        let removed = &output.removed;
        format!(
            "Удалена запись {}: ревью {}, единиц {}, кандидатов {}, решений {}, замечаний {}, поисковых случаев {}, feedback-событий {}, предложений политики {}.\nУтверждённые policy files и исходные review artifacts не изменены.\n",
            output.review_id,
            removed.reviews,
            removed.units,
            removed.candidates,
            removed.decisions,
            removed.findings,
            removed.search_cases,
            removed.feedback_events,
            removed.policy_proposals,
        )
    }

    fn human_policy_proposal(proposal: &PolicyProposal) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Предложение политики: {} (правило {})\n",
            proposal.proposal_id, proposal.rule_id
        ));
        out.push_str(&format!("Ключ признаков: {}\n", feature_key(&proposal.key)));
        out.push_str(&format!(
            "История: ревизия {}, проверенных записей {}, единиц {}\n",
            proposal.generation.revision,
            proposal.generation.trusted_reviews,
            proposal.generation.trusted_units
        ));
        out.push_str(&format!(
            "Подтверждающих случаев: {}, противоречащих: {}\n",
            proposal.supporting_cases.len(),
            proposal.contradicting_cases.len()
        ));
        for case in &proposal.contradicting_cases {
            out.push_str(&format!(
                "  противоречит: {}/{}\n",
                case.review_id, case.unit_id
            ));
        }
        out.push_str(&format!(
            "Автоприменение: {}\n",
            if proposal.auto_applied {
                "да"
            } else {
                "нет"
            }
        ));
        out.push_str(&format!(
            "Путь утверждённого артефакта: {}\n",
            proposal.artifact_path
        ));
        push_list(&mut out, "Предупреждения", &proposal.cautions);
        out
    }

    fn human_policy_list(output: &PolicyListOutput) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Предложений политики: {}\n",
            output.proposals.len()
        ));
        for proposal in &output.proposals {
            out.push_str(&format!(
                "  {} (правило {}): подтверждающих {}, противоречащих {}, автоприменение {}\n",
                proposal.proposal_id,
                proposal.rule_id,
                proposal.supporting_cases,
                proposal.contradicting_cases,
                if proposal.auto_applied {
                    "да"
                } else {
                    "нет"
                }
            ));
        }
        push_list(&mut out, "Ограничения", &output.limitations);
        out
    }

    fn human_policy_approve(output: &PolicyApproveOutput) -> String {
        format!(
            "Утверждено предложение {} (правило {}): артефакт {} ({}, {} байт); автоприменение: {}\n",
            output.proposal_id,
            output.rule_id,
            output.artifact.path,
            if output.artifact.created {
                "создан"
            } else {
                "уже существовал с теми же байтами"
            },
            output.artifact.bytes,
            if output.auto_applied {
                "да"
            } else {
                "нет"
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_exit_codes_are_stable_and_do_not_overlap_cli_errors() {
        use crate::code_review::execution::ExecutionStatus;

        assert_eq!(execution_exit(ExecutionStatus::Passed), 0);
        assert_eq!(execution_exit(ExecutionStatus::Failed), 9);
        assert_eq!(execution_exit(ExecutionStatus::Incomplete), 10);
        assert_eq!(execution_exit(ExecutionStatus::TimedOut), 11);
        assert_eq!(execution_exit(ExecutionStatus::Cancelled), 12);
        assert_eq!(execution_exit(ExecutionStatus::Unavailable), 127);
        assert_eq!(ErrorCode::ExecutionBusy.exit_code(), 13);
        assert_eq!(ErrorCode::ProcessOperationFailed.exit_code(), 14);
        assert_eq!(ErrorCode::Usage.exit_code(), 2);
        assert_eq!(ErrorCode::Internal.exit_code(), 70);
    }

    fn execution_inspection_fixture() -> crate::code_review::execution::JobInspection {
        use crate::code_review::execution::*;
        use crate::code_review::scope::GitTarget;

        let source = ExecutionSource {
            snapshot: GitTarget {
                repository_id: "fixture-repository".into(),
                base_sha: "a".repeat(40),
                head_sha: "b".repeat(40),
                merge_base_sha: "a".repeat(40),
            },
            review_pack_sha256: "c".repeat(64),
            workspace_variant: None,
        };
        let output = OutputEvidence {
            text: String::new(),
            total_bytes: 0,
            truncated: false,
            utf8_lossy: false,
            log: "logs/stdout.log".into(),
        };
        JobInspection {
            job: JobMetadata {
                schema_version: EXECUTION_SCHEMA_VERSION,
                job_id: "fixture-job".into(),
                source: source.clone(),
                mode: ExecutionMode::IsolatedChecks,
                scope: "runtime".into(),
                namespace: "local".into(),
                owner_nonce: "d".repeat(32),
            },
            lifecycle: LifecycleStatus::Interrupted,
            workspace_removed: false,
            limitations: vec!["Запись конечного состояния прервана.".into()],
            result: Some(ExecutionResult {
                schema_version: EXECUTION_SCHEMA_VERSION,
                job_id: "fixture-job".into(),
                source,
                mode: ExecutionMode::IsolatedChecks,
                scope: "runtime".into(),
                namespace: "local".into(),
                request: CommandRequest {
                    argv: vec!["true".into()],
                    cwd: ".".into(),
                    options: RunOptions::default(),
                },
                argv_sha256: "e".repeat(64),
                lifecycle: LifecycleStatus::Completed,
                status: ExecutionStatus::Passed,
                exit: Some(ProcessExit {
                    code: Some(0),
                    signal: None,
                }),
                stdout: output.clone(),
                stderr: OutputEvidence {
                    log: "logs/stderr.log".into(),
                    ..output
                },
                duration_ms: 1,
                enforcement: Enforcement::default(),
                failure: None,
                cleanup: "evidence_and_workspace_retained; descendants_may_still_exist".into(),
            }),
        }
    }

    #[test]
    fn execution_inspection_human_prioritizes_observed_interruption() {
        let inspection = execution_inspection_fixture();
        let rendered = human_execution_inspection(&inspection);
        assert!(
            rendered.starts_with("Задание fixture-job: состояние прервано"),
            "{rendered}"
        );
        assert!(rendered.contains("Запись конечного состояния прервана."));
        assert!(rendered.contains("Сохранённый результат команды"));
        let json: serde_json::Value = serde_json::from_str(&json::generic_json(
            "code-review execution inspect",
            &inspection,
        ))
        .unwrap();
        assert_eq!(json["result"]["lifecycle"], "interrupted");
        assert_eq!(json["result"]["result"]["lifecycle"], "completed");
        assert_eq!(
            json["result"]["limitations"][0],
            "Запись конечного состояния прервана."
        );
    }

    #[test]
    fn execution_human_uses_russian_labels_and_readable_exit() {
        let inspection = execution_inspection_fixture();
        let rendered = human_execution_result(inspection.result.as_ref().unwrap());
        assert!(rendered.contains("результат успешно"), "{rendered}");
        assert!(rendered.contains("сохранённое состояние завершено"));
        assert!(rendered.contains("Режим: изолированные проверки"));
        assert!(rendered.contains("Код завершения: 0; сигнал: отсутствует"));
        for debug_form in [
            "Some(0)",
            "None",
            "Passed",
            "Completed",
            "IsolatedChecks",
            "(passed)",
            "(completed)",
            "(isolated_checks)",
        ] {
            assert!(!rendered.contains(debug_form), "{rendered}");
        }
    }

    #[test]
    fn execution_inspection_completed_and_prepared_are_distinct() {
        use crate::code_review::execution::LifecycleStatus;
        let mut inspection = execution_inspection_fixture();
        inspection.lifecycle = LifecycleStatus::Completed;
        inspection.limitations.clear();
        assert!(
            human_execution_inspection(&inspection)
                .starts_with("Задание fixture-job: состояние завершено")
        );
        inspection.lifecycle = LifecycleStatus::Prepared;
        inspection.result = None;
        let rendered = human_execution_inspection(&inspection);
        assert!(rendered.contains("состояние подготовлено"));
        assert!(rendered.contains("Сохранённого результата команды нет."));
        assert!(!rendered.contains("успешно (passed)"));
    }

    #[test]
    fn execution_cleanup_human_reports_actual_flags_and_limitation() {
        use crate::code_review::execution::CleanupResult;
        let mut cleanup = CleanupResult {
            job_id: "fixture-job".into(),
            workspace_removed: false,
            evidence_retained: true,
            limitation: Some("Потомки не проверены.".into()),
        };
        let rendered = human_execution_cleanup(&cleanup);
        assert!(rendered.contains("рабочая область удалена: нет; свидетельства сохранены: да"));
        assert!(rendered.contains("Ограничение: Потомки не проверены."));
        cleanup.workspace_removed = true;
        cleanup.evidence_retained = false;
        cleanup.limitation = None;
        let rendered = human_execution_cleanup(&cleanup);
        assert!(rendered.contains("рабочая область удалена: да; свидетельства сохранены: нет"));
        assert!(!rendered.contains("Ограничение:"));
    }

    #[test]
    fn execution_error_json_preserves_distinct_busy_and_process_contracts() {
        let busy = DomainError::with_details(
            ErrorCode::ExecutionBusy,
            "Задание временно занято.",
            serde_json::json!({ "retryable": true, "resource": "fixture-job", "reason": "job_active" }),
        );
        let (stdout, stderr) = render_error("code-review execution run", true, &busy);
        assert!(stderr.is_empty());
        let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["error"]["code"], "execution_busy");
        assert_eq!(json["error"]["details"]["retryable"], true);
        assert_eq!(json["error"]["details"]["reason"], "job_active");
        assert_eq!(busy.exit_code(), 13);
        let failed = DomainError::new(
            ErrorCode::ProcessOperationFailed,
            "Не удалось наблюдать процесс.",
        );
        let (stdout, _) = render_error("code-review execution run", true, &failed);
        let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(json["error"]["code"], "process_operation_failed");
        assert_eq!(failed.exit_code(), 14);
        assert_ne!(
            ErrorCode::ReviewArtifactConflict.as_str(),
            busy.code.as_str()
        );
        assert_eq!(ErrorCode::ReviewArtifactConflict.exit_code(), 7);
    }

    #[test]
    fn queue_authenticity_keeps_machine_fields_and_localizes_human_labels() {
        use crate::code_review::workflow::{QueueAuthenticityEnvelope, SyntaxAuthenticityStatus};
        for (status, canonical) in [
            (SyntaxAuthenticityStatus::StructureOnly, "structure_only"),
            (SyntaxAuthenticityStatus::Verified, "verified"),
        ] {
            let response = QueueAuthenticityEnvelope {
                value: serde_json::json!({ "units": [] }),
                source_digest_valid: true,
                syntax_authenticity: status,
            };
            let json: serde_json::Value =
                serde_json::from_str(&json::generic_json("code-review queue list", &response))
                    .unwrap();
            assert_eq!(json["result"]["source_digest_valid"], true);
            assert_eq!(json["result"]["syntax_authenticity"], canonical);
            assert!(json["result"]["units"].is_array());
        }
        assert_eq!(
            queue_authenticity_label(SyntaxAuthenticityStatus::Verified),
            "синтаксическая подлинность подтверждена"
        );
        assert_eq!(
            queue_authenticity_label(SyntaxAuthenticityStatus::StructureOnly),
            "проверена только структура и digest"
        );
    }

    fn review_queue_candidate_detail() -> crate::code_review::workflow::ReviewQueueCandidateDetail {
        use crate::code_review::model::{CandidateEvidence, CandidateOrigin};
        use crate::code_review::review_queue::{
            ClassificationBasis, CodeRole, GroupingSignature, ReviewPriority, ReviewUnit,
            StructuralClassification, StructuralRole, TextRole, UnitMembers, UnitStatistics,
        };
        use crate::code_review::scope::{FileCategory, FileSurface};

        let classification = StructuralClassification {
            surfaces: vec![FileSurface::Tests],
            surface_basis: ClassificationBasis::FileContext,
            file_category: Some(FileCategory::Rust),
            execution: Some(FileSurface::Tests),
            execution_basis: ClassificationBasis::FileContext,
            origin: CandidateOrigin::IntroducedOrChanged,
            role: StructuralRole::ErrorPath,
            role_basis: ClassificationBasis::Detector,
            text_role: Some(TextRole::HumanComment),
            text_basis: ClassificationBasis::Detector,
            code_role: CodeRole::TestHelper,
            code_basis: ClassificationBasis::SyntaxContext,
            syntax_signature: None,
        };
        let unit = ReviewUnit {
            id: "unit-1".into(),
            members: UnitMembers::Group {
                candidate_ids: vec!["candidate-1".into(), "candidate-2".into()],
                representative_candidate_ids: vec!["candidate-1".into()],
            },
            signature: GroupingSignature {
                detector: "error_path".into(),
                source: "synthetic_detector".into(),
                classification: classification.clone(),
                detector_signals: vec!["unwrap_call".into()],
                text_context: None,
                path_family: "tools/anki-repo/src".into(),
                structural_pattern: None,
            },
            priority: ReviewPriority::Normal,
            priority_signals: Vec::new(),
            statistics: UnitStatistics {
                candidates: 2,
                paths: std::collections::BTreeMap::from([("src/lib.rs".into(), 2)]),
            },
        };

        crate::code_review::workflow::ReviewQueueCandidateDetail {
            candidate: CandidateEvidence {
                id: "candidate-1".into(),
                detector: "error_path".into(),
                path: "src/lib.rs".into(),
                line: Some(7),
                column: None,
                snippet: Some("result.unwrap()".into()),
                origin: CandidateOrigin::IntroducedOrChanged,
                signals: vec!["unwrap_call".into()],
                source: "synthetic_detector".into(),
                metadata: std::collections::BTreeMap::new(),
            },
            classification,
            unit,
        }
    }

    #[test]
    fn review_queue_group_uses_stable_labels() {
        let detail = review_queue_candidate_detail();
        let result = crate::code_review::workflow::ReviewQueueGroupDetail {
            unit: detail.unit,
            representatives: vec![
                crate::code_review::workflow::ReviewQueueRepresentativeDetail {
                    candidate: detail.candidate,
                    classification: detail.classification,
                },
            ],
        };

        let rendered = human_review_queue_group(&result);
        for label in [
            "Роль: error_path",
            "исполнение: tests",
            "роль текста: human_comment",
            "роль кода: test_helper",
            "происхождение: introduced_or_changed",
        ] {
            assert!(rendered.contains(label), "нет метки {label:?}: {rendered}");
        }
        for unstable in [
            "ErrorPath",
            "Some(",
            "HumanComment",
            "TestHelper",
            "IntroducedOrChanged",
        ] {
            assert!(
                !rendered.contains(unstable),
                "найдена нестабильная метка {unstable:?}"
            );
        }
    }

    #[test]
    fn review_queue_candidate_uses_stable_labels() {
        let result = review_queue_candidate_detail();

        let rendered = human_review_queue_candidate(&result);
        for label in [
            "Поверхности: tests",
            "исполнение: tests",
            "роль: error_path",
            "роль текста: human_comment",
            "роль кода: test_helper",
            "происхождение: introduced_or_changed",
            "Участников группы: 2; представителей: candidate-1.",
            "Свидетельство — это сигнал, а не подтверждённое замечание.",
        ] {
            assert!(rendered.contains(label), "нет метки {label:?}: {rendered}");
        }
        for unstable in [
            "ErrorPath",
            "Some(",
            "HumanComment",
            "TestHelper",
            "IntroducedOrChanged",
        ] {
            assert!(
                !rendered.contains(unstable),
                "найдена нестабильная метка {unstable:?}"
            );
        }
    }

    #[test]
    fn review_queue_outputs_mark_missing_text_role_as_not_applicable() {
        use crate::code_review::review_queue::{QueueListItem, QueueListPage, QueueUnitKind};

        let mut detail = review_queue_candidate_detail();
        detail.classification.text_role = None;
        detail.unit.signature.classification.text_role = None;

        assert!(human_review_queue_candidate(&detail).contains("роль текста: не применяется"));

        let group = crate::code_review::workflow::ReviewQueueGroupDetail {
            unit: detail.unit.clone(),
            representatives: vec![
                crate::code_review::workflow::ReviewQueueRepresentativeDetail {
                    candidate: detail.candidate.clone(),
                    classification: detail.classification.clone(),
                },
            ],
        };
        assert!(human_review_queue_group(&group).contains("роль текста: не применяется"));

        let page = QueueListPage {
            total_units: 1,
            matched_units: 1,
            offset: 0,
            limit: 50,
            returned_units: 1,
            has_more: false,
            units: vec![QueueListItem {
                id: detail.unit.id,
                kind: QueueUnitKind::Group,
                candidate_id: None,
                priority: detail.unit.priority,
                classification: detail.classification,
                detector: "error_path".into(),
                candidate_count: 2,
                representative_candidate_ids: vec!["candidate-1".into()],
            }],
        };
        assert!(human_review_queue_list(&page).contains("роль текста не применяется"));
    }

    #[test]
    fn review_queue_summary_uses_stable_enum_labels() {
        use crate::code_review::review_queue::{CodeRole, QueueSummary, StructuralRole, TextRole};

        let mut summary = QueueSummary::default();
        summary.by_execution.insert("production".into(), 4);
        summary
            .by_structural_role
            .insert(StructuralRole::ErrorPath, 1);
        summary.by_text_role.insert(TextRole::HumanDocumentation, 2);
        summary.by_code_role.insert(CodeRole::RuntimeBoundary, 3);

        let rendered = human_review_queue_summary(&summary);
        for label in [
            "production: 4",
            "error_path: 1",
            "human_documentation: 2",
            "runtime_boundary: 3",
        ] {
            assert!(
                rendered.contains(label),
                "summary omits {label:?}: {rendered}"
            );
        }
        assert!(!rendered.contains("runtimeboundary"));
    }

    /// Бесконечный reader: если бы чтение не было ограничено, тест не завершился
    /// бы, поэтому это прямая проверка предела, а не косвенная.
    #[test]
    fn request_read_stops_at_the_limit() {
        let limit = document_read_limit(edit_op::MAX_REQUEST_BYTES);
        let raw = read_bounded(std::io::repeat(b'a'), limit).expect("чтение повторов");

        assert_eq!(raw.len() as u64, limit);
        assert!(
            raw.len() > edit_op::MAX_REQUEST_BYTES,
            "лишний байт нужен, чтобы отличить ровно предел от превышения"
        );
    }

    #[test]
    fn request_read_keeps_short_input_intact() {
        let raw = read_bounded(
            &b"{\"schema_version\": 1}"[..],
            document_read_limit(edit_op::MAX_REQUEST_BYTES),
        )
        .expect("чтение");

        assert_eq!(raw, b"{\"schema_version\": 1}".to_vec());
    }

    #[test]
    fn request_read_keeps_the_boundary_exactly_at_the_limit() {
        let source = vec![b' '; edit_op::MAX_REQUEST_BYTES];
        let raw = read_bounded(&source[..], document_read_limit(edit_op::MAX_REQUEST_BYTES))
            .expect("чтение");

        assert_eq!(raw.len(), edit_op::MAX_REQUEST_BYTES);
        assert!(
            raw.len() <= edit_op::MAX_REQUEST_BYTES,
            "ровно предел не считается превышением"
        );
    }
}
