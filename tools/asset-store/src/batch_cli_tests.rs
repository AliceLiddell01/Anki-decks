use super::*;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn read_kanji_verified(
    root: impl AsRef<std::path::Path>,
    identities: &[crate::model::AssetIdentity],
    validator: &crate::model::ValidatorIdentity,
) -> Result<Vec<crate::store::VerifiedAssetBytes>, crate::error::AssetError> {
    AssetStore::read_verified_with_policy(
        root,
        identities,
        validator,
        &crate::domain::KanjiDomainPolicy,
    )
}

struct Fixture {
    directory: PathBuf,
    store: AssetStore,
}
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "kanji-batch-cli-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let store = AssetStore::open_kanji(StoreOptions::new(directory.join("corpus"))).unwrap();
        Self { directory, store }
    }
    fn start(&self, id: &str, characters: &[char]) {
        let (_, code) = execute_command(
            &self.store,
            self.summary(),
            &BatchCommand::Start {
                batch_id: Some(id.into()),
                characters: characters.iter().map(char::to_string).collect(),
            },
            false,
        )
        .unwrap();
        assert_eq!(code, 0);
    }
    fn load(&self, id: &str) -> KanjiBatch {
        BatchRuntime::open(self.store.root(), id)
            .unwrap()
            .load()
            .unwrap()
            .unwrap()
    }
    fn summary(&self) -> StoreSummary {
        StoreSummary {
            path: self.store.root().display().to_string(),
            store_id: Some(self.store.store_id().into()),
            layout_migrated_on_open: self.store.layout_migrated_on_open(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[derive(Default)]
struct CapturedProgress {
    events: Vec<BatchProgressEvent>,
}

impl BatchProgressSink for CapturedProgress {
    fn emit(&mut self, event: BatchProgressEvent) -> Result<(), AssetError> {
        self.events.push(event);
        Ok(())
    }
}

fn emit_stream_item(
    on_event: &mut dyn FnMut(AcquisitionEvent) -> Result<(), AssetError>,
    index: usize,
    outcome: Result<AcquiredMedia, String>,
) -> Result<(), AcquisitionStreamError> {
    on_event(AcquisitionEvent::ItemStarted { index }).map_err(AcquisitionStreamError::Consumer)?;
    on_event(AcquisitionEvent::ItemCompleted {
        index,
        outcome: Box::new(outcome),
    })
    .map_err(AcquisitionStreamError::Consumer)
}

fn identity(value: char) -> AssetIdentity {
    AssetIdentity::new("kanji", value.to_string()).unwrap()
}
fn glyph(value: char) -> Vec<u8> {
    crate::kanji_validator::synthetic_reference_png(value)
}
fn media(character: &str, bytes: Vec<u8>) -> AcquiredMedia {
    // Источник публикует Unicode статьи как шестнадцатеричную кодовую точку, а не как символ.
    let code = character
        .chars()
        .next()
        .map(|ch| format!("{:X}", u32::from(ch)))
        .expect("символ синтетического материала указан");
    let evidence = crate::yarxi::AcquisitionEvidence {
        provider: "synthetic".into(),
        provider_version: "1".into(),
        article_unicode: code.clone(),
        article_number: Some(1),
        frequency_index: None,
        selection: SelectionResult::LeftmostPngFallback,
        target: crate::yarxi::AcquisitionTarget::PreferredSource,
        fallback_absence_proof: Some("synthetic".into()),
        rendered_font_sample: None,
        source_url: "https://example.invalid/synthetic.png".into(),
        browser_runtime: None,
        tls_exception: None,
        acquisition_attempts: 1,
    };
    AcquiredMedia {
        character: character.into(),
        source_url: evidence.source_url.clone(),
        article_unicode: code,
        selection: evidence.selection,
        bytes,
        evidence,
    }
}

fn media_with_codes(character: &str, article_unicode: &str, bytes: Vec<u8>) -> AcquiredMedia {
    let mut acquired = media(character, bytes);
    acquired.article_unicode = article_unicode.into();
    acquired.evidence.article_unicode = article_unicode.into();
    acquired
}

#[test]
fn provider_article_unicode_is_a_hex_code_point_not_the_character() {
    let fixture = Fixture::new();
    fixture.start("identity", &['漢']);
    // Источник публикует Unicode статьи как шестнадцатеричную кодовую точку (U+6F22 -> "6F22").
    // Сравнение с самим символом приводило бы к ошибке при любом реальном получении.
    let (state, _, issues) = run_batch(&fixture.store, "identity", 1, |characters| {
        assert_eq!(characters, ["漢"]);
        Ok(vec![Ok(media_with_codes("漢", "6F22", glyph('漢')))])
    })
    .unwrap();
    assert!(issues.is_empty());
    assert!(matches!(
        state.items[0].attempts[0].result,
        BatchAttemptInput::Candidate { .. }
    ));
    assert_eq!(
        state.items[0].current_sha256.as_deref(),
        Some(sha256_hex(glyph('漢')).as_str())
    );

    for wrong in ["漢", "6F23", ""] {
        let fixture = Fixture::new();
        fixture.start("identity", &['漢']);
        let (state, _, _) = run_batch(&fixture.store, "identity", 1, |_| {
            Ok(vec![Ok(media_with_codes("漢", wrong, glyph('漢')))])
        })
        .unwrap();
        assert!(
            matches!(
                &state.items[0].attempts[0].result,
                BatchAttemptInput::Failed { code, .. } if code == "source_identity_mismatch"
            ),
            "article_unicode {wrong:?} должен отвергаться"
        );
        assert_eq!(state.items[0].current_sha256, None);
    }
}

#[test]
fn cli_parser_supports_structured_batch_actions_and_rejects_free_text() {
    let cli = Cli::try_parse_from([
        "kanji-assets",
        "--output",
        "json",
        "batch",
        "decide",
        "--batch-id",
        "fixture",
        "--character",
        "漢",
        "--sha256",
        &"a".repeat(64),
        "--action",
        "confirm",
        "--reason",
        "漢 - подтверждён",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Command::Batch {
            command: BatchCommand::Decide {
                action: BatchActionArg::Confirm,
                ..
            }
        }
    ));
    assert!(
        Cli::try_parse_from([
            "kanji-assets",
            "batch",
            "decide",
            "--batch-id",
            "fixture",
            "--character",
            "漢",
            "--sha256",
            &"a".repeat(64),
            "--action",
            "подтверждён",
            "--reason",
            "explicit"
        ])
        .is_err()
    );
    let default_run =
        Cli::try_parse_from(["kanji-assets", "batch", "run", "--batch-id", "fixture"]).unwrap();
    let Command::Batch { command } = &default_run.command else {
        panic!("команда пакета должна разбираться как batch");
    };
    assert!(matches!(
        command,
        BatchCommand::Run {
            rounds: MAX_ACQUISITION_ROUNDS,
            ..
        }
    ));
    assert!(prevalidate(command).is_ok());

    let too_many = Cli::try_parse_from([
        "kanji-assets",
        "batch",
        "run",
        "--batch-id",
        "fixture",
        "--rounds",
        &(MAX_ACQUISITION_ROUNDS + 1).to_string(),
    ])
    .unwrap();
    let Command::Batch { command } = &too_many.command else {
        panic!("команда пакета должна разбираться как batch");
    };
    assert!(prevalidate(command).is_err());
}

#[test]
fn human_reason_uses_one_byte_limit_at_cli_boundary() {
    assert!(validate_reason(&"r".repeat(MAX_HUMAN_REASON_BYTES)).is_ok());
    assert!(validate_reason(&"r".repeat(MAX_HUMAN_REASON_BYTES + 1)).is_err());
    assert!(validate_reason(" \n\t ").is_err());
}

#[test]
fn domain_round_limit_rejects_overflow_before_acquisition() {
    let fixture = Fixture::new();
    fixture.start("round-limit", &['漢']);
    let mut called = false;
    let result = run_batch(
        &fixture.store,
        "round-limit",
        MAX_ACQUISITION_ROUNDS + 1,
        |_| {
            called = true;
            Ok(Vec::new())
        },
    );
    assert!(result.is_err());
    assert!(!called);
}

#[test]
fn human_cli_output_uses_russian_labels() {
    let fixture = Fixture::new();
    let output = execute(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Start {
            batch_id: Some("localized".into()),
            characters: vec!["漢".into()],
        },
        OutputFormat::Human,
        false,
    );
    assert_eq!(output.exit_code, 0);
    assert!(
        output
            .stdout
            .contains("Операция: создание пакета; результат: создан")
    );
    assert!(
        output
            .stdout
            .contains("попыток_получения=0 разных_кандидатов_SHA-256=0")
    );
    assert!(!output.stdout.contains("attempts="));
    assert!(!output.stdout.contains("review:"));
}

#[test]
fn start_status_review_are_json_and_idempotent() {
    let fixture = Fixture::new();
    fixture.start("status", &['漢']);
    let command = BatchCommand::Status {
        batch_id: "status".into(),
    };
    let result = execute(
        &fixture.store,
        fixture.summary(),
        &command,
        OutputFormat::Json,
        false,
    );
    assert_eq!(result.exit_code, 0);
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["batch_id"], "status");
    assert_eq!(value["counts"]["requested"], 1);
    assert_eq!(value["items"][0]["item_outcome"], "unresolved");
    assert!(
        value["batch"]["items"][0]["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    fixture.start("status", &['漢']);
    assert!(
        execute_command(
            &fixture.store,
            fixture.summary(),
            &BatchCommand::Start {
                batch_id: Some("status".into()),
                characters: vec!["字".into()]
            },
            false
        )
        .is_err()
    );
    let review = execute(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Review {
            batch_id: "status".into(),
        },
        OutputFormat::Json,
        false,
    );
    assert_eq!(review.exit_code, 0);
    let value: serde_json::Value = serde_json::from_str(&review.stdout).unwrap();
    assert!(PathBuf::from(value["review_artifact"].as_str().unwrap()).is_file());
}

#[test]
fn acquisition_releases_runtime_lock_and_finishes_all_items_before_retry() {
    let fixture = Fixture::new();
    fixture.start("breadth", &['元', '漢', '字']);
    let mut requests = Vec::new();
    let (state, _, _) = run_batch(&fixture.store, "breadth", 2, |characters| {
        // Повторное открытие той же flock во время вызова источника подтверждает,
        // что блокировка снята.
        let _probe = BatchRuntime::open(fixture.store.root(), "breadth").unwrap();
        requests.push(characters.to_vec());
        Ok(characters
            .iter()
            .map(|value| {
                if value == "元" {
                    Ok(media(value, glyph('元')))
                } else {
                    Err("синтетическая ошибка сети".into())
                }
            })
            .collect())
    })
    .unwrap();
    assert_eq!(requests[0], ["元", "漢", "字"]);
    assert_eq!(requests[1], ["漢", "字"]);
    assert!(state.items[0].is_ready());
    assert_eq!(state.items[0].attempts.len(), 1);
    assert_eq!(state.items[1].attempts.len(), 2);
    assert_eq!(state.items[2].attempts.len(), 2);
    assert_eq!(
        state.items[0].attempts[0].result.clone(),
        fixture.load("breadth").items[0].attempts[0].result
    );
    let record = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(record[0].bytes, glyph('元'));
    assert!(
        record[0].record.domain_metadata.as_ref().unwrap()["yarxi"]["source_url"]
            .as_str()
            .unwrap()
            .contains("example.invalid")
    );
}

#[test]
fn completed_identity_is_durable_before_provider_returns_and_resume_skips_it() {
    let fixture = Fixture::new();
    fixture.start("partial-checkpoint", &['元', '漢', '字']);
    let mut progress = CapturedProgress::default();
    let first = run_batch_with_stream_and_progress(
        &fixture.store,
        "partial-checkpoint",
        1,
        |characters, on_event| {
            assert_eq!(characters, ["元", "漢", "字"]);
            // Во время ожидания поставщика другой владелец может открыть среду пакета.
            let probe = BatchRuntime::open(fixture.store.root(), "partial-checkpoint").unwrap();
            drop(probe);
            on_event(AcquisitionEvent::SessionStarted { session: 1 })
                .map_err(AcquisitionStreamError::Consumer)?;
            on_event(AcquisitionEvent::ItemStarted { index: 0 })
                .map_err(AcquisitionStreamError::Consumer)?;
            on_event(AcquisitionEvent::Heartbeat {
                index: 0,
                attempt: 1,
            })
            .map_err(AcquisitionStreamError::Consumer)?;
            on_event(AcquisitionEvent::ItemCompleted {
                index: 0,
                outcome: Box::new(Ok(media("元", glyph('元')))),
            })
            .map_err(AcquisitionStreamError::Consumer)?;

            // Обработчик элемента вернулся только после атомарного сохранения.
            let checkpoint = fixture.load("partial-checkpoint");
            assert_eq!(checkpoint.items[0].attempts.len(), 1);
            let BatchAttemptInput::Candidate { candidate } =
                &checkpoint.items[0].attempts[0].result
            else {
                panic!("полученный кандидат должен быть сохранён до продолжения поставщика")
            };
            let runtime = BatchRuntime::open(fixture.store.root(), "partial-checkpoint").unwrap();
            assert_eq!(runtime.read_candidate(candidate).unwrap(), glyph('元'));
            for item in &checkpoint.items[1..] {
                assert!(item.attempts.is_empty());
                assert_eq!(item.generation, 0);
                assert_eq!(item.status, BatchItemStatus::Unresolved);
            }
            Err(AcquisitionStreamError::Provider(
                "синтетическое прерывание после первого сохранённого результата".into(),
            ))
        },
        &mut StoreSnapshotReader,
        &mut progress,
    );
    assert!(first.is_err());
    assert!(progress.events.iter().any(|event| {
        event.event == "item_checkpointed"
            && event
                .identity
                .as_ref()
                .is_some_and(|identity| identity.key == "元")
            && event.outcome.as_deref() == Some("candidate_recorded")
    }));
    let heartbeat = progress
        .events
        .iter()
        .find(|event| event.event == "heartbeat")
        .unwrap();
    let checkpoint_event = progress
        .events
        .iter()
        .find(|event| event.event == "item_checkpointed")
        .unwrap();
    assert_eq!(heartbeat.identity.as_ref().unwrap().key, "元");
    assert!(heartbeat.elapsed_ms <= checkpoint_event.elapsed_ms);
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.event == "run_stopped")
    );
    assert_eq!(
        fixture.load("partial-checkpoint").items[0].attempts.len(),
        1
    );

    let mut resumed_requests = Vec::new();
    let mut resumed_progress = CapturedProgress::default();
    let (state, _, _) = run_batch_with_stream_and_progress(
        &fixture.store,
        "partial-checkpoint",
        1,
        |characters, on_event| {
            resumed_requests.push(characters.to_vec());
            assert_eq!(characters, ["漢", "字"]);
            for (index, character) in characters.iter().enumerate() {
                emit_stream_item(
                    on_event,
                    index,
                    Ok(media(character, glyph(character.chars().next().unwrap()))),
                )?;
            }
            Ok(())
        },
        &mut StoreSnapshotReader,
        &mut resumed_progress,
    )
    .unwrap();
    assert_eq!(resumed_requests, [vec!["漢".to_owned(), "字".to_owned()]]);
    assert!(state.is_resolved());
    assert!(state.items.iter().all(|item| item.attempts.len() == 1));
    assert!(resumed_progress.events.iter().any(|event| {
        event.event == "run_finished" && event.outcome.as_deref() == Some("resolved")
    }));
}

#[test]
fn interrupted_frontier_tail_stays_pending_and_retry_fairness_is_preserved() {
    let fixture = Fixture::new();
    fixture.start("frontier-tail", &['元', '漢', '字']);
    let interrupted = run_batch_with_stream_and_progress(
        &fixture.store,
        "frontier-tail",
        1,
        |_characters, on_event| {
            emit_stream_item(on_event, 0, Ok(media("元", glyph('元'))))?;
            Err(AcquisitionStreamError::Provider(
                "синтетическая остановка сессии".into(),
            ))
        },
        &mut StoreSnapshotReader,
        &mut CapturedProgress::default(),
    );
    assert!(interrupted.is_err());

    let state = fixture.load("frontier-tail");
    assert_eq!(state.items[0].attempts.len(), 1);
    for item in &state.items[1..] {
        assert!(item.attempts.is_empty());
        assert_eq!(item.generation, 0);
        assert_eq!(item.status, BatchItemStatus::Unresolved);
        assert!(item.aggregate.distinct_valid_hashes.is_empty());
    }
    assert_eq!(state.next_round(), [identity('漢'), identity('字')]);

    let mut resumed_requests = Vec::new();
    let (state, _, _) = run_batch_with_stream_and_progress(
        &fixture.store,
        "frontier-tail",
        1,
        |characters, on_event| {
            resumed_requests.push(characters.to_vec());
            assert_eq!(characters, ["漢", "字"]);
            emit_stream_item(on_event, 0, Err("ошибка получения символа 漢".into()))?;
            assert_eq!(fixture.load("frontier-tail").next_round(), [identity('字')]);
            emit_stream_item(on_event, 1, Err("ошибка получения символа 字".into()))?;
            assert_eq!(
                fixture.load("frontier-tail").next_round(),
                [identity('漢'), identity('字')],
                "вторая попытка соседей разрешена только после первой попытки всей очереди раунда"
            );
            Ok(())
        },
        &mut StoreSnapshotReader,
        &mut CapturedProgress::default(),
    )
    .unwrap();
    assert_eq!(resumed_requests, [vec!["漢".to_owned(), "字".to_owned()]]);
    assert_eq!(state.items[0].attempts.len(), 1);
    assert_eq!(state.items[1].attempts.len(), 1);
    assert_eq!(state.items[2].attempts.len(), 1);
    assert_eq!(state.next_round(), [identity('漢'), identity('字')]);
}

#[test]
fn partial_checkpoint_error_response_reports_durable_change_and_saved_tail() {
    for interrupted in [true, false] {
        let mut fixture = Fixture::new();
        let batch_id = "partial-error-response";
        fixture.start(batch_id, &['元', '漢', '字']);
        fixture.store = AssetStore::open_kanji(StoreOptions::new(fixture.store.root())).unwrap();
        assert!(!fixture.store.did_mutate_on_open());
        let initial_revision = fixture.load(batch_id).revision;
        let error = run_batch_with_stream_and_progress(
            &fixture.store,
            batch_id,
            2,
            |_characters, on_event| {
                emit_stream_item(
                    on_event,
                    0,
                    Err("сохранённая ошибка первого символа".into()),
                )?;
                if interrupted {
                    Err(AcquisitionStreamError::Interrupted)
                } else {
                    Err(AcquisitionStreamError::Provider(
                        "сессия браузера недоступна".into(),
                    ))
                }
            },
            &mut StoreSnapshotReader,
            &mut CapturedProgress::default(),
        )
        .unwrap_err();
        let final_output = render_batch_error(
            &fixture.store,
            fixture.summary(),
            OutputFormat::Json,
            BatchRunFailure {
                batch_id,
                operation: "batch_run",
                error,
                initial_revision: Some(initial_revision),
                diagnostic_log: None,
            },
        );
        assert!(final_output.stderr.is_empty());
        let response: serde_json::Value = serde_json::from_str(&final_output.stdout).unwrap();
        assert_eq!(response["batch_id"], batch_id);
        assert_eq!(response["changed"], true);
        assert_eq!(response["items"][0]["acquisition_attempts"], 1);
        assert_eq!(
            response["batch"]["items"][0]["attempts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            response["batch"]["revision"],
            fixture.load(batch_id).revision
        );
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains(if interrupted {
                    "Ctrl+C"
                } else {
                    "сессия браузера недоступна"
                })
        );
        for index in 1..3 {
            assert_eq!(response["items"][index]["acquisition_attempts"], 0);
            assert!(
                response["batch"]["items"][index]["attempts"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(fixture.load(batch_id).items[index].generation, 0);
        }
    }
}

#[test]
fn batch_error_without_durable_mutation_reports_unchanged_existing_state() {
    let mut fixture = Fixture::new();
    fixture.start("unchanged-error", &['漢']);
    fixture.store = AssetStore::open_kanji(StoreOptions::new(fixture.store.root())).unwrap();
    assert!(!fixture.store.did_mutate_on_open());
    let revision = fixture.load("unchanged-error").revision;
    let final_output = render_batch_error(
        &fixture.store,
        fixture.summary(),
        OutputFormat::Json,
        BatchRunFailure {
            batch_id: "unchanged-error",
            operation: "batch_run",
            error: AssetError::new(ErrorCode::IoFailure, "ошибка запуска браузера"),
            initial_revision: Some(revision),
            diagnostic_log: None,
        },
    );
    let response: serde_json::Value = serde_json::from_str(&final_output.stdout).unwrap();
    assert_eq!(response["changed"], false);
    assert_eq!(response["batch_id"], "unchanged-error");
    assert_eq!(response["batch"]["revision"], revision);
    assert_eq!(response["items"][0]["acquisition_attempts"], 0);
}

#[test]
fn signal_accepted_in_last_checkpoint_stops_before_second_round_acquisition() {
    use std::sync::atomic::AtomicBool;
    struct StopAtCheckpoint<'a> {
        stopped: &'a AtomicBool,
        events: Vec<BatchProgressEvent>,
    }
    impl BatchProgressSink for StopAtCheckpoint<'_> {
        fn emit(&mut self, event: BatchProgressEvent) -> Result<(), AssetError> {
            if event.event == "item_checkpointed" {
                self.stopped.store(true, Ordering::SeqCst);
            }
            self.events.push(event);
            Ok(())
        }
    }
    let fixture = Fixture::new();
    fixture.start("stop-round-boundary", &['漢']);
    let stopped = AtomicBool::new(false);
    let mut progress = StopAtCheckpoint {
        stopped: &stopped,
        events: Vec::new(),
    };
    let mut acquisitions = 0;
    let error = run_batch_with_stream_and_progress(
        &fixture.store,
        "stop-round-boundary",
        2,
        |characters, on_event| {
            acquisitions += 1;
            assert_eq!(characters, ["漢"]);
            emit_stream_item(on_event, 0, Err("ошибка получения первого раунда".into()))?;
            // Сигнал принят из синхронного callback после надёжного сохранения.
            // Поставщик обязан проверить тот же флаг остановки до успешного возврата.
            assert_eq!(
                fixture.load("stop-round-boundary").items[0].attempts.len(),
                1
            );
            assert!(stopped.load(Ordering::SeqCst));
            Err(AcquisitionStreamError::Interrupted)
        },
        &mut StoreSnapshotReader,
        &mut progress,
    )
    .unwrap_err();
    assert!(error.message.contains("batch_interrupted"));
    assert_eq!(acquisitions, 1);
    let saved = fixture.load("stop-round-boundary");
    assert_eq!(saved.items[0].attempts.len(), 1);
    assert_eq!(saved.next_round(), [identity('漢')]);
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "round_started")
            .count(),
        1
    );
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.event == "run_stopped")
    );
    assert!(
        !progress
            .events
            .iter()
            .any(|event| event.event == "run_finished")
    );
}

#[test]
fn human_progress_localizes_event_and_outcome_without_changing_machine_values() {
    let event = BatchProgressEvent {
        schema_version: 1,
        operation: "batch_run",
        event: "browser_session_started",
        batch_id: "localized-progress".into(),
        elapsed_ms: 100,
        round: Some(1),
        round_limit: 2,
        round_completed: 0,
        round_total: 1,
        run_completed: 0,
        batch_total: 1,
        identity: None,
        session: Some(1),
        attempt: None,
        outcome: Some("candidate_recorded".into()),
        reason: None,
    };
    let mut human = Vec::new();
    write_progress_event(&event, OutputFormat::Human, &mut human).unwrap();
    let human = String::from_utf8(human).unwrap();
    assert!(human.contains("сессия браузера запущена"));
    assert!(human.contains("кандидат сохранён"));
    for machine in [
        "browser_session_started",
        "candidate_recorded",
        "identity=",
        "browser session",
    ] {
        assert!(!human.contains(machine));
    }
    let mut machine = Vec::new();
    write_progress_event(&event, OutputFormat::Json, &mut machine).unwrap();
    let machine: serde_json::Value = serde_json::from_slice(&machine).unwrap();
    assert_eq!(machine["event"], "browser_session_started");
    assert_eq!(machine["outcome"], "candidate_recorded");
}

#[test]
fn progress_jsonl_is_separate_from_the_single_final_json_response() {
    let fixture = Fixture::new();
    fixture.start("progress-contract", &['漢']);
    let event = BatchProgressEvent {
        schema_version: 1,
        operation: "batch_run",
        event: "item_checkpointed",
        batch_id: "progress-contract".into(),
        elapsed_ms: 1250,
        round: Some(1),
        round_limit: 5,
        round_completed: 1,
        round_total: 1,
        run_completed: 1,
        batch_total: 1,
        identity: Some(identity('漢')),
        session: Some(1),
        attempt: None,
        outcome: Some("acquisition_failed".into()),
        reason: None,
    };
    let mut stderr = Vec::new();
    write_progress_event(&event, OutputFormat::Json, &mut stderr).unwrap();
    let progress_line = std::str::from_utf8(&stderr).unwrap();
    assert_eq!(progress_line.lines().count(), 1);
    let progress_json: serde_json::Value = serde_json::from_str(progress_line).unwrap();
    assert_eq!(progress_json["event"], "item_checkpointed");
    assert_eq!(progress_json["round_completed"], 1);
    assert_eq!(progress_json["identity"]["key"], "漢");

    let final_output = execute(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Status {
            batch_id: "progress-contract".into(),
        },
        OutputFormat::Json,
        false,
    );
    assert!(final_output.stderr.is_empty());
    let final_json: serde_json::Value = serde_json::from_str(&final_output.stdout).unwrap();
    assert_eq!(final_json["operation"], "batch_status");
    assert_eq!(final_json["batch_id"], "progress-contract");
}

#[test]
fn reuse_trusted_assets_and_publication_resume_do_not_acquire_again() {
    let fixture = Fixture::new();
    fixture.start("original", &['元']);
    let (state, _, _) = run_batch(&fixture.store, "original", 1, |characters| {
        Ok(characters
            .iter()
            .map(|value| Ok(media(value, glyph('元'))))
            .collect())
    })
    .unwrap();
    assert!(state.is_resolved());
    fixture.start("reuse", &['元']);
    let (state, changed, _) = run_batch(&fixture.store, "reuse", 5, |_| {
        panic!("для доверенных байтов получение не запускается")
    })
    .unwrap();
    assert!(!changed);
    assert!(state.is_resolved());
    assert_eq!(state.items[0].status, BatchItemStatus::ExistingVerified);
    // Имитируем канонический коммит до отметки и сохранения состояния.
    let mut runtime = BatchRuntime::open(fixture.store.root(), "original").unwrap();
    let mut before_mark = runtime.load().unwrap().unwrap();
    before_mark.items[0].published_sha256 = None;
    before_mark.items[0].publication_source = None;
    before_mark.revision += 1;
    runtime.save(&before_mark).unwrap();
    drop(runtime);
    let (state, _, _) = run_batch(&fixture.store, "original", 5, |_| {
        panic!("возобновление публикации не должно запускать получение")
    })
    .unwrap();
    assert!(state.is_resolved());
}

#[test]
fn human_confirm_reject_and_targeted_reacquire_cross_owner_boundaries() {
    let fixture = Fixture::new();
    fixture.start("review", &['元', '漢', '字']);
    let (state, _, _) = run_batch(&fixture.store, "review", 5, |characters| {
        Ok(characters
            .iter()
            .map(|value| Ok(media(value, glyph('元'))))
            .collect())
    })
    .unwrap();
    assert!(state.items[0].is_ready());
    assert_eq!(state.review_queue().len(), 2);
    // Кандидат REJECTED показывает, что другой эталон Unicode ближе, поэтому он
    // не считается независимым пригодным образцом этого символа.
    assert!(state.items[1].aggregate.distinct_valid_hashes.is_empty());
    assert_eq!(state.items[1].attempts.len(), 5);
    let confirm_hash = state.items[1].current_sha256.clone().unwrap();
    let (state, issues) = decide_exact(
        &fixture.store,
        "review",
        HumanBatchDecision {
            identity: identity('漢'),
            candidate_sha256: confirm_hash.clone(),
            action: HumanBatchAction::Confirm,
            reason: "синтетическое явное подтверждение человека".into(),
        },
    )
    .unwrap();
    assert!(issues.is_empty());
    assert!(state.items[1].is_ready());
    let human = read_kanji_verified(
        fixture.store.root(),
        &[identity('漢')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(
        human[0].record.current_human_decision(),
        Some(HumanDecision::Approve)
    );
    assert_ne!(
        human[0].record.validation.as_ref().unwrap().status,
        SemanticStatus::Verified
    );
    let reject_hash = state.items[2].current_sha256.clone().unwrap();
    let (state, issues) = decide_exact(
        &fixture.store,
        "review",
        HumanBatchDecision {
            identity: identity('字'),
            candidate_sha256: reject_hash.clone(),
            action: HumanBatchAction::Reject,
            reason: "синтетический повреждённый артефакт".into(),
        },
    )
    .unwrap();
    assert!(issues.is_empty());
    assert_eq!(state.next_round(), [identity('字')]);
    assert!(
        read_kanji_verified(
            fixture.store.root(),
            &[identity('字')],
            &KanjiImageValidator::validator_identity()
        )
        .is_err()
    );
    let owner = fixture
        .store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == identity('字'))
        .unwrap();
    assert_eq!(owner.current_human_decision(), Some(HumanDecision::Reject));
    let (state, _, _) = run_batch(&fixture.store, "review", 1, |characters| {
        assert_eq!(characters, ["字"]);
        Ok(vec![Ok(media("字", glyph('字')))])
    })
    .unwrap();
    assert_ne!(
        state.items[2].current_sha256.as_deref(),
        Some(reject_hash.as_str())
    );
    assert!(state.items[0].is_ready());
    assert!(state.items[1].is_ready());
    // Новый кандидат может получить индивидуальный VERIFIED или ждать уточнения;
    // целевой повтор не должен запускать получение для уже готовых соседей.
    assert_eq!(state.items[2].attempts.len(), 6);
}

#[test]
fn confirm_of_exact_asset_without_valid_automated_evidence_does_not_approve_owner() {
    let fixture = Fixture::new();
    fixture.start("confirm-without-evidence", &['漢']);
    let bytes = b"GIF89a bytes that do not encode an image";
    let hash = sha256_hex(bytes);
    let mut runtime = BatchRuntime::open(fixture.store.root(), "confirm-without-evidence").unwrap();
    let mut state = runtime.load().unwrap().unwrap();
    let candidate = runtime
        .persist_candidate(
            bytes,
            ValidationRecord {
                status: SemanticStatus::Uncertain,
                validator: KanjiImageValidator::validator_identity(),
                content_sha256: hash.clone(),
                evidence: vec![ValidationEvidence {
                    kind: "pixel_reference_comparison".into(),
                    summary: "синтетическое свидетельство для теста границы подтверждения".into(),
                    details: None,
                }],
            },
            true,
        )
        .unwrap();
    state
        .record_attempt(&identity('漢'), BatchAttemptInput::Candidate { candidate })
        .unwrap();
    runtime.save(&state).unwrap();
    drop(runtime);

    assert!(fixture.store.verify_integrity().unwrap().is_empty());
    let (response, exit_code) = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Decide {
            batch_id: "confirm-without-evidence".into(),
            character: "漢".into(),
            sha256: hash,
            action: BatchActionArg::Confirm,
            reason: "проверить, что автоматическое свидетельство обязательно".into(),
        },
        false,
    )
    .unwrap();

    assert_eq!(exit_code, 3);
    assert_eq!(response.outcome, "publication_blocked");
    assert!(!response.issues.is_empty());
    assert!(!response.items[0].effective_verified);
    let owner = fixture
        .store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == identity('漢'))
        .unwrap();
    assert_ne!(owner.current_human_decision(), Some(HumanDecision::Approve));
    assert_eq!(
        owner.validation.as_ref().unwrap().status,
        SemanticStatus::Corrupt
    );
}

#[test]
fn exact_source_cas_fails_before_manifest_mutation() {
    let fixture = Fixture::new();
    let source = fixture.directory.join("candidate.png");
    fs::write(&source, glyph('元')).unwrap();
    let before = fixture.store.verify_integrity().unwrap();
    let error = fixture
        .store
        .ingest(IngestRequest {
            identity: identity('元'),
            source_path: source,
            expected_source_sha256: Some("a".repeat(64)),
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::IntegrityMismatch);
    assert_eq!(fixture.store.verify_integrity().unwrap(), before);
}

#[test]
fn aggregate_publication_is_exact_versioned_and_decoded() {
    let fixture = Fixture::new();
    fixture.start("aggregate", &['漢']);
    // Байты должны приниматься рабочим валидатором: агрегат не повышает кандидата
    // REJECTED, поэтому синтетика строится на эталоне 漢, а независимые метрики
    // задаются сохранёнными свидетельствами.
    let first = glyph('漢');
    let mut image = image::load_from_memory(&first).unwrap().to_rgba8();
    image.put_pixel(0, 0, image::Rgba([254, 255, 255, 255]));
    let mut encoded = Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let second = encoded.into_inner();
    let mut runtime = BatchRuntime::open(fixture.store.root(), "aggregate").unwrap();
    let mut state = runtime.load().unwrap().unwrap();
    for (bytes, distance, margin) in [(&first, 0.025, 0.010), (&second, 0.005, 0.005)] {
        let record = ValidationRecord {
            status: SemanticStatus::Uncertain,
            validator: KanjiImageValidator::validator_identity(),
            content_sha256: sha256_hex(bytes),
            evidence: vec![ValidationEvidence {
                kind: "pixel_reference_comparison".into(),
                summary: "синтетические независимые метрики".into(),
                details: Some(
                    serde_json::json!({"expected_distance": distance, "nearest_margin": margin, "nearest_other": "元"}),
                ),
            }],
        };
        let candidate = runtime.persist_candidate(bytes, record, true).unwrap();
        state
            .record_attempt(&identity('漢'), BatchAttemptInput::Candidate { candidate })
            .unwrap();
    }
    runtime.save(&state).unwrap();
    assert!(!state.is_resolved());
    drop(runtime);
    let (state, _, issues) = run_batch(&fixture.store, "aggregate", 5, |_| {
        panic!("публикация агрегата должна продолжиться с точных байтов")
    })
    .unwrap();
    assert!(issues.is_empty());
    assert!(state.is_resolved());
    assert_eq!(
        state.items[0].publication_source,
        Some(BatchTrustSource::Aggregate)
    );
    let read = read_kanji_verified(
        fixture.store.root(),
        &[identity('漢')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(read[0].bytes, second);
    let evidence = &read[0].record.validation.as_ref().unwrap().evidence;
    assert!(
        evidence
            .iter()
            .any(|evidence| evidence.kind == "kanji_batch_aggregate")
    );
    assert!(
        evidence
            .iter()
            .any(|evidence| evidence.kind == "pixel_reference_comparison")
    );
}

#[test]
fn rejected_candidate_never_votes_or_publishes_as_aggregate() {
    let fixture = Fixture::new();
    fixture.start("rejected", &['漢']);
    let base = glyph('漢');
    let variants: Vec<Vec<u8>> = (0..5u8)
        .map(|index| {
            let mut image = image::load_from_memory(&base).unwrap().to_rgba8();
            image.put_pixel(0, 0, image::Rgba([index + 1, 111, 222, 255]));
            let mut encoded = Cursor::new(Vec::new());
            image
                .write_to(&mut encoded, image::ImageFormat::Png)
                .unwrap();
            encoded.into_inner()
        })
        .collect();
    let mut hashes: Vec<_> = variants.iter().map(sha256_hex).collect();
    hashes.sort();
    hashes.dedup();
    assert_eq!(
        hashes.len(),
        5,
        "синтетические кандидаты должны различаться"
    );
    // Один REJECTED с привлекательными метриками и четыре UNCERTAIN: прежний
    // фильтр включал REJECTED в среднее, поэтому порог проходился, а выбирался
    // именно отклонённый кандидат с минимальной дистанцией.
    let plan = [
        (SemanticStatus::Rejected, 0.015, -0.015),
        (SemanticStatus::Uncertain, 0.021, 0.009),
        (SemanticStatus::Uncertain, 0.021, 0.009),
        (SemanticStatus::Uncertain, 0.021, 0.009),
        (SemanticStatus::Uncertain, 0.021, 0.009),
    ];
    let mut runtime = BatchRuntime::open(fixture.store.root(), "rejected").unwrap();
    let mut state = runtime.load().unwrap().unwrap();
    for (bytes, (status, distance, margin)) in variants.iter().zip(plan) {
        let record = ValidationRecord {
            status,
            validator: KanjiImageValidator::validator_identity(),
            content_sha256: sha256_hex(bytes),
            evidence: vec![ValidationEvidence {
                kind: "pixel_reference_comparison".into(),
                summary: "синтетические независимые метрики".into(),
                details: Some(
                    serde_json::json!({"expected_distance": distance, "nearest_margin": margin, "nearest_other": "漠"}),
                ),
            }],
        };
        let candidate = runtime.persist_candidate(bytes, record, true).unwrap();
        state
            .record_attempt(&identity('漢'), BatchAttemptInput::Candidate { candidate })
            .unwrap();
    }
    let rejected_hash = sha256_hex(&variants[0]);
    runtime.save(&state).unwrap();
    drop(runtime);
    let (state, _, _) = run_batch(&fixture.store, "rejected", 5, |_| {
        panic!("возобновление не должно запускать получение")
    })
    .unwrap();
    let item = &state.items[0];
    assert!(!item.aggregate.accepted);
    assert_eq!(item.aggregate.distinct_valid_hashes.len(), 4);
    assert!(
        !item
            .aggregate
            .distinct_valid_hashes
            .contains(&rejected_hash)
    );
    assert_ne!(
        item.aggregate.selected_sha256.as_deref(),
        Some(rejected_hash.as_str())
    );
    assert_ne!(item.status, BatchItemStatus::AutoVerified);
    assert!(item.published_sha256.is_none());
    assert_eq!(item.status, BatchItemStatus::AwaitingHuman);
    assert!(
        fixture
            .store
            .verify_integrity()
            .unwrap()
            .iter()
            .all(|record| record.sha256 != rejected_hash),
        "отклонённые байты не должны попасть в канонический корпус"
    );
}

#[test]
fn stale_browser_result_is_skipped_after_concurrent_resolution() {
    let fixture = Fixture::new();
    fixture.start("stale", &['元']);
    let (state, _, _) = run_batch(&fixture.store, "stale", 1, |_| {
        let mut runtime = BatchRuntime::open(fixture.store.root(), "stale").unwrap();
        let mut state = runtime.load().unwrap().unwrap();
        let bytes = glyph('元');
        let outcome = fixture
            .store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: identity('元'),
                    bytes,
                    provenance: Provenance {
                        source_kind: "synthetic".into(),
                        source_name: "concurrent.png".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &KanjiImageValidator::new(),
            )
            .unwrap();
        state
            .mark_existing_ready(&identity('元'), &outcome.sha256)
            .unwrap();
        runtime.save(&state).unwrap();
        Ok(vec![Ok(media("元", glyph('元')))])
    })
    .unwrap();
    assert!(state.items[0].attempts.is_empty());
    assert_eq!(state.items[0].current_sha256, Some(sha256_hex(glyph('元'))));
}

#[test]
fn explicit_reacquire_preserves_current_canonical_trust() {
    let fixture = Fixture::new();
    fixture.start("reacquire", &['元']);
    let (state, _, _) = run_batch(&fixture.store, "reacquire", 1, |characters| {
        Ok(characters
            .iter()
            .map(|value| Ok(media(value, glyph('元'))))
            .collect())
    })
    .unwrap();
    let hash = state.items[0].current_sha256.clone().unwrap();
    let (state, issues) = decide_exact(
        &fixture.store,
        "reacquire",
        HumanBatchDecision {
            identity: identity('元'),
            candidate_sha256: hash.clone(),
            action: HumanBatchAction::Reacquire,
            reason: "явное новое получение".into(),
        },
    )
    .unwrap();
    assert!(issues.is_empty());
    assert_eq!(state.next_round(), [identity('元')]);
    let owner = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(owner[0].record.sha256, hash);
    assert_ne!(
        owner[0].record.current_human_decision(),
        Some(HumanDecision::Reject)
    );
    let (state, _, issues) = run_batch(&fixture.store, "reacquire", 1, |_| {
        Ok(vec![Ok(media("元", glyph('元')))])
    })
    .unwrap();
    assert!(issues.is_empty());
    assert!(state.is_resolved());
}

#[test]
fn targeted_owner_validation_leaves_unrelated_pending_candidate_untouched() {
    let fixture = Fixture::new();
    let mut hashes = Vec::new();
    for (index, character) in ['漢', '字'].into_iter().enumerate() {
        let bytes = glyph('元');
        let path = fixture.directory.join(format!("candidate-{index}.png"));
        fs::write(&path, &bytes).unwrap();
        let outcome = fixture
            .store
            .ingest(IngestRequest {
                identity: identity(character),
                source_path: path,
                expected_source_sha256: Some(sha256_hex(&bytes)),
                domain_metadata: None,
                replace_expected_sha256: None,
            })
            .unwrap();
        hashes.push(outcome.asset.sha256);
    }
    let report = fixture
        .store
        .validate_exact(&identity('漢'), &hashes[0], &KanjiImageValidator::new())
        .unwrap();
    assert_eq!(report.considered, 1);
    assert_eq!(report.attempts[0].identity, identity('漢'));
    let neighbor = fixture
        .store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == identity('字'))
        .unwrap();
    assert_eq!(neighbor.lifecycle, LifecycleState::Pending);
    assert!(neighbor.validation.is_none());
}

#[test]
fn persisted_rejection_before_owner_commit_resumes_before_acquisition() {
    let fixture = Fixture::new();
    fixture.start("rejection-resume", &['漢']);
    let (state, _, _) = run_batch(&fixture.store, "rejection-resume", 5, |_| {
        Ok(vec![Ok(media("漢", glyph('元')))])
    })
    .unwrap();
    let hash = state.items[0].current_sha256.clone().unwrap();
    let mut runtime = BatchRuntime::open(fixture.store.root(), "rejection-resume").unwrap();
    let mut state = runtime.load().unwrap().unwrap();
    let candidate = current_candidate(&state.items[0]).unwrap().clone();
    let bytes = runtime.read_candidate(&candidate).unwrap();
    state
        .decide(
            HumanBatchDecision {
                identity: identity('漢'),
                candidate_sha256: hash.clone(),
                action: HumanBatchAction::Reject,
                reason: "сохранённое намерение пользователя до прерванной записи владельца".into(),
            },
            &bytes,
        )
        .unwrap();
    runtime.save(&state).unwrap();
    drop(runtime);
    run_batch(&fixture.store, "rejection-resume", 1, |_| {
        let owner = fixture
            .store
            .verify_integrity()
            .unwrap()
            .into_iter()
            .find(|record| record.identity == identity('漢'))
            .unwrap();
        assert_eq!(owner.sha256, hash);
        assert_eq!(owner.current_human_decision(), Some(HumanDecision::Reject));
        Ok(vec![Err("синтетический повтор недоступен".into())])
    })
    .unwrap();
}

fn publish_reference(fixture: &Fixture, batch_id: &str) -> String {
    fixture.start(batch_id, &['元']);
    let (state, _, issues) = run_batch(&fixture.store, batch_id, 1, |_| {
        Ok(vec![Ok(media("元", glyph('元')))])
    })
    .unwrap();
    assert!(issues.is_empty());
    assert!(state.is_resolved());
    state.items[0].current_sha256.clone().unwrap()
}

fn distinct_reference_bytes() -> Vec<u8> {
    let mut image = image::load_from_memory(&glyph('元')).unwrap().to_rgba8();
    image.put_pixel(0, 0, image::Rgba([254, 255, 255, 255]));
    let mut encoded = Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    encoded.into_inner()
}

#[test]
fn confirm_published_auto_and_existing_candidates_records_owner_approval() {
    for reused in [false, true] {
        let fixture = Fixture::new();
        let hash = publish_reference(&fixture, "auto");
        let batch_id = if reused {
            fixture.start("reused", &['元']);
            "reused"
        } else {
            "auto"
        };
        let expected_status = if reused {
            BatchItemStatus::ExistingVerified
        } else {
            BatchItemStatus::AutoVerified
        };
        assert_eq!(fixture.load(batch_id).items[0].status, expected_status);
        let (state, issues) = decide_exact(
            &fixture.store,
            batch_id,
            HumanBatchDecision {
                identity: identity('元'),
                candidate_sha256: hash.clone(),
                action: HumanBatchAction::Confirm,
                reason: "явное подтверждение уже опубликованных точных байтов".into(),
            },
        )
        .unwrap();
        assert!(issues.is_empty());
        assert!(state.is_resolved());
        assert_eq!(state.items[0].status, BatchItemStatus::HumanVerified);
        assert_eq!(
            state.items[0].publication_source,
            Some(BatchTrustSource::Human)
        );
        let owner = read_kanji_verified(
            fixture.store.root(),
            &[identity('元')],
            &KanjiImageValidator::validator_identity(),
        )
        .unwrap();
        assert_eq!(owner[0].record.sha256, hash);
        assert_eq!(
            owner[0].record.current_human_decision(),
            Some(HumanDecision::Approve)
        );
        assert_eq!(fixture.load(batch_id), state);
    }
}

#[test]
fn reject_existing_asset_with_approval_from_an_older_validator() {
    struct PriorValidator;
    impl SemanticValidator for PriorValidator {
        fn identity(&self) -> ValidatorIdentity {
            ValidatorIdentity::new("prior-kanji-validator", "1").unwrap()
        }
        fn validate(
            &self,
            _: &AssetRecord,
            _: &mut dyn Read,
        ) -> Result<SemanticDecision, ValidatorFailure> {
            Ok(SemanticDecision::new(
                SemanticStatus::Verified,
                vec![ValidationEvidence {
                    kind: "prior_validator".into(),
                    summary: "проверка предыдущей версии валидатора".into(),
                    details: None,
                }],
            ))
        }
    }

    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "prior-validator-source");
    fixture
        .store
        .validate_exact(&identity('元'), &hash, &PriorValidator)
        .unwrap();
    fixture
        .store
        .attest(HumanAttestationRequest {
            identity: identity('元'),
            expected_sha256: hash.clone(),
            decision: HumanDecision::Approve,
            reason: "явное подтверждение точных байтов".into(),
        })
        .unwrap();

    fixture.start("prior-validator-reject", &['元']);
    assert_eq!(
        fixture.load("prior-validator-reject").items[0].status,
        BatchItemStatus::ExistingVerified
    );
    let decision = HumanBatchDecision {
        identity: identity('元'),
        candidate_sha256: hash,
        action: HumanBatchAction::Reject,
        reason: "явный отказ после повторной проверки".into(),
    };
    let (state, issues) =
        decide_exact(&fixture.store, "prior-validator-reject", decision.clone()).unwrap();

    assert!(issues.is_empty());
    assert_eq!(state.items[0].status, BatchItemStatus::Reacquire);
    assert_eq!(state.items[0].human_decisions.last(), Some(&decision));
    let owner_record = fixture
        .store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == identity('元'))
        .unwrap();
    assert_eq!(
        owner_record.current_human_decision(),
        Some(HumanDecision::Reject)
    );
}

#[test]
fn stale_reject_never_materializes_or_demotes_newer_owner_sha() {
    let fixture = Fixture::new();
    let old_hash = publish_reference(&fixture, "old-batch");
    let new_bytes = distinct_reference_bytes();
    let new = fixture
        .store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity('元'),
                bytes: new_bytes.clone(),
                provenance: Provenance {
                    source_kind: "synthetic".into(),
                    source_name: "newer.png".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: Some(old_hash.clone()),
            },
            &KanjiImageValidator::new(),
        )
        .unwrap();
    assert_eq!(new.status, SemanticStatus::Verified);
    assert_ne!(new.sha256, old_hash);
    let before = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    let (state, issues) = decide_exact(
        &fixture.store,
        "old-batch",
        HumanBatchDecision {
            identity: identity('元'),
            candidate_sha256: old_hash.clone(),
            action: HumanBatchAction::Reject,
            reason: "отказ относится только к прежним проверенным байтам".into(),
        },
    )
    .unwrap();
    assert!(issues.is_empty());
    assert_eq!(state.items[0].status, BatchItemStatus::Reacquire);
    assert_eq!(
        state.items[0]
            .human_decisions
            .last()
            .unwrap()
            .candidate_sha256,
        old_hash
    );
    let after = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(after[0].record, before[0].record);
    assert_eq!(after[0].bytes, new_bytes);
    let (state, _, _) = run_batch(&fixture.store, "old-batch", 1, |_| {
        Ok(vec![Err("синтетическое получение недоступно".into())])
    })
    .unwrap();
    assert!(!state.is_resolved());
    let after_resume = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(after_resume[0].record, before[0].record);
}

#[test]
fn owner_rejection_in_another_batch_invalidates_cached_status_review_and_run() {
    for review_first in [false, true] {
        let fixture = Fixture::new();
        let hash = publish_reference(&fixture, "batch-a");
        fixture.start("batch-b", &['元']);
        assert_eq!(
            fixture.load("batch-b").items[0].status,
            BatchItemStatus::ExistingVerified
        );
        let (_, issues) = decide_exact(
            &fixture.store,
            "batch-b",
            HumanBatchDecision {
                identity: identity('元'),
                candidate_sha256: hash.clone(),
                action: HumanBatchAction::Reject,
                reason: "явный отказ владельца в другом пакете".into(),
            },
        )
        .unwrap();
        assert!(issues.is_empty());
        let command = if review_first {
            BatchCommand::Review {
                batch_id: "batch-a".into(),
            }
        } else {
            BatchCommand::Status {
                batch_id: "batch-a".into(),
            }
        };
        let (response, code) =
            execute_command(&fixture.store, fixture.summary(), &command, false).unwrap();
        assert_eq!(code, 0);
        assert_eq!(response.counts.effective_verified, 0);
        assert_eq!(response.items[0].state, BatchItemStatus::Reacquire);
        assert!(response.changed);
        let state = fixture.load("batch-a");
        assert_eq!(
            state.items[0].observed_owner_rejections[0].candidate_sha256,
            hash
        );
        assert_eq!(
            state.items[0].observed_owner_rejections[0].identity,
            identity('元')
        );
        let (state, _, _) = run_batch(&fixture.store, "batch-a", 1, |_| {
            Ok(vec![Ok(media("元", glyph('元')))])
        })
        .unwrap();
        assert!(!state.is_resolved());
        assert!(state.items[0].aggregate.distinct_valid_hashes.is_empty());
        assert!(
            read_kanji_verified(
                fixture.store.root(),
                &[identity('元')],
                &KanjiImageValidator::validator_identity()
            )
            .is_err()
        );
        let new_bytes = distinct_reference_bytes();
        let (state, _, _) = run_batch(&fixture.store, "batch-a", 1, |_| {
            Ok(vec![Ok(media("元", new_bytes.clone()))])
        })
        .unwrap();
        assert!(state.is_resolved());
        assert_ne!(state.items[0].current_sha256.as_ref().unwrap(), &hash);
        let owner = read_kanji_verified(
            fixture.store.root(),
            &[identity('元')],
            &KanjiImageValidator::validator_identity(),
        )
        .unwrap();
        assert_eq!(owner[0].bytes, new_bytes);
        assert_ne!(
            owner[0].record.current_human_decision(),
            Some(HumanDecision::Reject)
        );
    }
}

#[test]
fn owner_current_hash_change_clears_saved_readiness() {
    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "saved-ready");
    fixture
        .store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity('元'),
                bytes: distinct_reference_bytes(),
                provenance: Provenance {
                    source_kind: "synthetic".into(),
                    source_name: "changed.png".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: Some(hash),
            },
            &KanjiImageValidator::new(),
        )
        .unwrap();

    let (response, _) = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Status {
            batch_id: "saved-ready".into(),
        },
        false,
    )
    .unwrap();
    assert_eq!(response.counts.effective_verified, 0);
    assert_eq!(response.items[0].state, BatchItemStatus::Reacquire);
    assert!(response.items[0].published_sha256.is_none());
    assert!(!fixture.load("saved-ready").is_resolved());
}

#[test]
fn failed_owner_snapshot_preserves_all_cached_ready_items() {
    let fixture = Fixture::new();
    fixture.start("snapshot-error", &['元', '漢']);
    let (before, _, issues) = run_batch(&fixture.store, "snapshot-error", 1, |characters| {
        Ok(characters
            .iter()
            .map(|character| {
                let value = character.chars().next().unwrap();
                Ok(media(character, glyph(value)))
            })
            .collect())
    })
    .unwrap();
    assert!(issues.is_empty());
    assert!(before.items.iter().all(|item| item.is_ready()));

    let record = fixture
        .store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == identity('元'))
        .unwrap();
    fs::remove_file(fixture.store.root().join(record.storage_path)).unwrap();

    let error = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Status {
            batch_id: "snapshot-error".into(),
        },
        false,
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::MissingAssetFile);
    let after = fixture.load("snapshot-error");
    assert_eq!(after.revision, before.revision);
    assert!(after.items.iter().all(|item| item.is_ready()));
}

#[test]
fn run_observes_external_owner_reject_before_restoring_cached_auto_ready() {
    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "direct-run");
    fixture
        .store
        .attest(HumanAttestationRequest {
            identity: identity('元'),
            expected_sha256: hash.clone(),
            decision: HumanDecision::Reject,
            reason: "отказ владельца до сверки состояния и проверки".into(),
        })
        .unwrap();
    assert!(fixture.load("direct-run").is_resolved());
    let (state, _, _) = run_batch(&fixture.store, "direct-run", 1, |characters| {
        assert_eq!(characters, ["元"]);
        Ok(vec![Ok(media("元", glyph('元')))])
    })
    .unwrap();
    assert!(!state.is_resolved());
    assert_eq!(state.items[0].status, BatchItemStatus::Reacquire);
    assert_eq!(
        state.items[0].observed_owner_rejections[0].candidate_sha256,
        hash
    );
    assert!(state.items[0].aggregate.distinct_valid_hashes.is_empty());
    assert!(
        read_kanji_verified(
            fixture.store.root(),
            &[identity('元')],
            &KanjiImageValidator::validator_identity()
        )
        .is_err()
    );
}

#[test]
fn expected_validator_trust_loss_invalidates_cached_ready() {
    struct OtherValidator;
    impl SemanticValidator for OtherValidator {
        fn identity(&self) -> ValidatorIdentity {
            ValidatorIdentity::new("synthetic-other", "1").unwrap()
        }
        fn validate(
            &self,
            _: &AssetRecord,
            _: &mut dyn Read,
        ) -> Result<SemanticDecision, ValidatorFailure> {
            Ok(SemanticDecision::new(
                SemanticStatus::Verified,
                vec![ValidationEvidence {
                    kind: "synthetic_other_classifier".into(),
                    summary: "точная идентичность другого валидатора".into(),
                    details: None,
                }],
            ))
        }
    }
    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "validator-loss");
    fixture
        .store
        .validate_exact(&identity('元'), &hash, &OtherValidator)
        .unwrap();
    let (response, _) = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Status {
            batch_id: "validator-loss".into(),
        },
        false,
    )
    .unwrap();
    assert_eq!(response.counts.effective_verified, 0);
    assert_eq!(response.items[0].state, BatchItemStatus::Reacquire);
    assert!(response.batch.items[0].observed_owner_rejections.is_empty());
}

#[test]
fn stale_pinned_validator_blocks_decision_before_mutation() {
    struct OtherValidator;
    impl SemanticValidator for OtherValidator {
        fn identity(&self) -> ValidatorIdentity {
            ValidatorIdentity::new("synthetic-other", "1").unwrap()
        }
        fn validate(
            &self,
            _: &AssetRecord,
            _: &mut dyn Read,
        ) -> Result<SemanticDecision, ValidatorFailure> {
            Ok(SemanticDecision::new(
                SemanticStatus::Verified,
                vec![ValidationEvidence {
                    kind: "synthetic_other_classifier".into(),
                    summary: "точная идентичность другого валидатора".into(),
                    details: None,
                }],
            ))
        }
    }
    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "seed");
    // Пакет создан до обновления рабочего валидатора: закреплённая идентичность
    // отличается от текущей, поэтому ни решение, ни повторное получение не могут
    // публиковать соседние элементы текущим валидатором.
    let pinned = OtherValidator.identity();
    {
        let mut runtime = BatchRuntime::open(fixture.store.root(), "pinned").unwrap();
        let state = KanjiBatch::new("pinned".into(), vec![identity('元')], pinned.clone()).unwrap();
        runtime.save(&state).unwrap();
    }
    let before = fixture.load("pinned");
    assert_eq!(before.policy.validator, pinned);
    let error = decide_exact(
        &fixture.store,
        "pinned",
        HumanBatchDecision {
            identity: identity('元'),
            candidate_sha256: hash,
            action: HumanBatchAction::Confirm,
            reason: "решение с устаревшим закреплённым валидатором".into(),
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidValidatorIdentity);
    let after = fixture.load("pinned");
    assert_eq!(after.revision, before.revision);
    assert!(after.items[0].human_decisions.is_empty());
    assert_eq!(after.items[0].attempts.len(), 0);
    let retry = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Retry {
            batch_id: "pinned".into(),
            character: "元".into(),
            sha256: None,
            reason: "повтор с устаревшим закреплённым валидатором".into(),
        },
        false,
    )
    .unwrap_err();
    assert_eq!(retry.code, ErrorCode::InvalidValidatorIdentity);
    let after = fixture.load("pinned");
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.items[0].generation, 0);
    assert_eq!(after.items[0].attempts.len(), 0);
}

#[test]
fn repeated_exact_confirm_passes_owner_attestation_after_external_reject() {
    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "repeat-confirm");
    let decision = HumanBatchDecision {
        identity: identity('元'),
        candidate_sha256: hash.clone(),
        action: HumanBatchAction::Confirm,
        reason: "явное подтверждение точных текущих байтов".into(),
    };
    decide_exact(&fixture.store, "repeat-confirm", decision.clone()).unwrap();
    fixture
        .store
        .attest(HumanAttestationRequest {
            identity: identity('元'),
            expected_sha256: hash,
            decision: HumanDecision::Reject,
            reason: "параллельный независимый отказ владельца".into(),
        })
        .unwrap();
    let (state, issues) = decide_exact(&fixture.store, "repeat-confirm", decision).unwrap();
    assert!(issues.is_empty());
    assert!(state.is_resolved());
    let owner = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(
        owner[0].record.current_human_decision(),
        Some(HumanDecision::Approve)
    );
}

#[test]
fn run_error_response_exposes_a_flushed_per_run_diagnostic_log() {
    let fixture = Fixture::new();
    let command = BatchCommand::Run {
        batch_id: "diagnostic-missing-batch".into(),
        rounds: 1,
    };
    let output = execute_with_progress(
        &fixture.store,
        fixture.summary(),
        &command,
        OutputFormat::Json,
        false,
        &mut CapturedProgress::default(),
    );
    assert!(!output.stderr.lines().any(|line| !line.is_empty()));
    let response: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
    let log_path = std::path::PathBuf::from(response["diagnostic_log"].as_str().unwrap());
    assert!(log_path.exists());
    assert!(log_path.to_string_lossy().contains("/logs/"));
    let events = fs::read_to_string(log_path)
        .unwrap()
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| event["fields"]["event"] == "run_started")
    );
    let stopped = events
        .iter()
        .find(|event| event["fields"]["event"] == "run_stopped")
        .unwrap();
    assert!(
        stopped["spans"].as_array().unwrap().iter().any(|span| {
            span["name"] == "kanji_batch_run"
                && span["run_id"] == stopped["fields"]["run_id"]
                && span["diagnostic_log"] == response["diagnostic_log"]
        }),
        "стоп-событие: {stopped}"
    );
}

#[test]
fn successful_run_response_exposes_a_flushed_per_run_diagnostic_log() {
    let fixture = Fixture::new();
    publish_reference(&fixture, "diagnostic-success");
    let command = BatchCommand::Run {
        batch_id: "diagnostic-success".into(),
        rounds: 1,
    };
    let output = execute_with_progress(
        &fixture.store,
        fixture.summary(),
        &command,
        OutputFormat::Json,
        false,
        &mut CapturedProgress::default(),
    );
    let response: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
    let log_path = std::path::PathBuf::from(response["diagnostic_log"].as_str().unwrap());
    assert!(log_path.exists());
    assert!(log_path.to_string_lossy().contains("/logs/"));
    let events = fs::read_to_string(log_path)
        .unwrap()
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let finished = events
        .iter()
        .find(|event| event["fields"]["event"] == "run_finished")
        .unwrap();
    assert!(finished["spans"].as_array().unwrap().iter().any(|span| {
        span["name"] == "kanji_batch_run"
            && span["run_id"] == finished["fields"]["run_id"]
            && span["diagnostic_log"] == response["diagnostic_log"]
    }));
}
