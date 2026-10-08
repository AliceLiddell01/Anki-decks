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

use serde::Serialize;

use crate::cli::{
    Cli, CodeReviewCommand, Command, LanguageCommand, MatchArg, ReviewQueueCommand,
    SemanticTriageCommand,
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
            } => {
                let result = crate::code_review::workflow::collect(
                    base,
                    head,
                    out_dir.as_deref(),
                    *run_clippy,
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
            } => {
                let result = crate::code_review::workflow::verify(
                    baseline,
                    head,
                    out_dir.as_deref(),
                    *run_clippy,
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
                    let result =
                        crate::code_review::workflow::list_review_queue(pack, queue, &options)?;
                    Ok(Rendered {
                        command: "code-review queue list",
                        stdout: if cli.json {
                            json::generic_json("code-review queue list", result)
                        } else {
                            human_review_queue_list(&result)
                        },
                        exit: 0,
                    })
                }
                ReviewQueueCommand::Validate { pack, queue } => {
                    let result = crate::code_review::workflow::validate_review_queue(pack, queue)?;
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
                ReviewQueueCommand::Summary { pack, queue } => {
                    let result = crate::code_review::workflow::summarize_review_queue(pack, queue)?;
                    Ok(Rendered {
                        command: "code-review queue summary",
                        stdout: if cli.json {
                            json::generic_json("code-review queue summary", result)
                        } else {
                            human_review_queue_summary(&result)
                        },
                        exit: 0,
                    })
                }
                ReviewQueueCommand::Group { pack, queue, id } => {
                    let result =
                        crate::code_review::workflow::expand_review_queue_group(pack, queue, id)?;
                    Ok(Rendered {
                        command: "code-review queue group",
                        stdout: if cli.json {
                            json::generic_json("code-review queue group", result)
                        } else {
                            human_review_queue_group(&result)
                        },
                        exit: 0,
                    })
                }
                ReviewQueueCommand::Candidate { pack, queue, id } => {
                    let result = crate::code_review::workflow::inspect_review_queue_candidate(
                        pack, queue, id,
                    )?;
                    Ok(Rendered {
                        command: "code-review queue candidate",
                        stdout: if cli.json {
                            json::generic_json("code-review queue candidate", result)
                        } else {
                            human_review_queue_candidate(&result)
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
        "Структурная очередь проверена: {}.\nСырых кандидатов: {}; единиц ревью: {}; отдельных: {}; групп: {}.\n",
        if result.valid {
            "валидна"
        } else {
            "невалидна"
        },
        result.summary.raw_candidates,
        result.summary.review_units,
        result.summary.individual_units,
        result.summary.group_units,
    )
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
                .map_or("unknown", |role| role.as_str()),
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
        class.text_role.map_or("unknown", |role| role.as_str()),
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
        class.text_role.map_or("unknown", |role| role.as_str()),
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

#[cfg(test)]
mod tests {
    use super::*;

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
