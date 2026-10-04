use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use asset_store::cli::KanjiCharacter;
use asset_store::hashing::sha256_hex;
use asset_store::kanji_validator::KanjiImageValidator;
use asset_store::model::{
    AssetIdentity, AssetRecord, DetectedFormat, LifecycleState, Provenance, SemanticStatus,
};
use asset_store::validation::SemanticValidator;
use asset_store::yarxi::{
    AcquisitionEvent, AcquisitionStreamError, AcquisitionTarget, SelectionResult,
    acquire_many_stream_with_target_in_workspace,
};
use clap::Parser;
use image::GenericImageView;
use serde_json::{Value, json};

#[path = "common/evidence_zip.rs"]
mod evidence_zip;
use evidence_zip::EvidenceRun;

#[derive(Debug, Parser)]
#[command(
    name = "yarxi_dark_acceptance",
    about = "Получить, проверить и просмотреть PNG образцов Yarxi в тёмной теме"
)]
struct Args {
    /// JSON-план в формате {"items":[{"character":"漢"}]}.
    /// Путь может находиться вне checkout.
    #[arg(long, value_name = "PATH")]
    plan: PathBuf,

    /// Новый ZIP вне checkout и run workspace; parent должен существовать.
    /// По умолчанию: системный temp root/anki-decks-evidence/<уникальный run>.zip.
    #[arg(long, value_name = "ZIP")]
    output: Option<PathBuf>,

    /// Внешний текстовый transcript проверок; безопасная копия попадёт в ZIP.
    #[arg(long, value_name = "PATH")]
    transcript: Option<PathBuf>,

    /// JSON результатов fault injection; отдельная безопасная копия в ZIP.
    #[arg(long, value_name = "PATH")]
    fault_evidence: Option<PathBuf>,

    /// Разрешить обработку промежуточной страницы TLS-предупреждения только для www.yarxi.su.
    #[arg(long)]
    allow_insecure_tls: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanItem {
    group: Option<String>,
    character: String,
    article_number: Option<u32>,
    frequency_index: Option<u32>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    run(Args::parse())
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let plan_path = args.plan.canonicalize()?;
    let plan_bytes = fs::read(&plan_path)?;
    let plan: Value = serde_json::from_slice(&plan_bytes)?;
    let items = read_plan_items(&plan)?;
    let run = EvidenceRun::start(
        "yarxi_dark_acceptance",
        args.output.as_deref(),
        &checkout_root()?,
    )?;
    if let Some(transcript) = &args.transcript
        && let Err(error) = run.add_transcript(transcript)
    {
        return run.finish(Err(error), Ok(()));
    }
    if let Some(source) = &args.fault_evidence
        && let Err(error) = run.add_fault_evidence(source)
    {
        return run.finish(Err(error), Ok(()));
    }
    let log = match run.log_guard() {
        Ok(log) => log,
        Err(error) => return run.finish(Err(error), Ok(())),
    };
    let result = log.with_default(|| run_body(&items, &plan_path, args.allow_insecure_tls, &run));
    run.finish(result, log.finish())
}

fn run_body(
    items: &[PlanItem],
    plan_path: &Path,
    allow_insecure_tls: bool,
    run: &EvidenceRun,
) -> Result<(), Box<dyn std::error::Error>> {
    let report_dir = run.report_dir();
    fs::create_dir(report_dir.join("images"))?;

    let characters: Vec<_> = items.iter().map(|item| item.character.clone()).collect();
    let mut acquisitions = Vec::new();
    let acquisition_result = acquire_many_stream_with_target_in_workspace(
        &characters,
        allow_insecure_tls,
        AcquisitionTarget::RenderedFontSamplePng,
        run.path(),
        |event| {
            let event_value = match event {
                AcquisitionEvent::SessionStarted { session } => {
                    json!({"event":"session_started","session":session})
                }
                AcquisitionEvent::SessionEnded {
                    session,
                    processed,
                    stop_reason,
                } => {
                    json!({"event":"session_ended","session":session,"processed":processed,"stop_reason":stop_reason.map(|reason|reason.summary())})
                }
                AcquisitionEvent::SessionRotated {
                    next_session,
                    reason,
                } => {
                    json!({"event":"session_rotated","next_session":next_session,"reason":reason.summary()})
                }
                AcquisitionEvent::ItemStarted { index } => {
                    json!({"event":"item_started","index":index})
                }
                AcquisitionEvent::RetryStarted { index, attempt } => {
                    json!({"event":"retry_started","index":index,"attempt":attempt})
                }
                AcquisitionEvent::RetryRecoveryStarted { index, attempt } => {
                    json!({"event":"retry_recovery_started","index":index,"attempt":attempt})
                }
                AcquisitionEvent::Heartbeat { index, attempt } => {
                    json!({"event":"heartbeat","index":index,"attempt":attempt})
                }
                AcquisitionEvent::ItemCompleted { index, outcome } => {
                    if index != acquisitions.len() || index >= items.len() {
                        return Err(asset_store::error::AssetError::new(
                            asset_store::error::ErrorCode::InvalidTransition,
                            "provider выдал непоследовательный индекс",
                        ));
                    }
                    let evidence = match outcome.as_ref() {
                        Ok(media) => {
                            json!({"acquisition":media.evidence,"source_url":media.source_url,"sha256":sha256_hex(&media.bytes),"byte_length":media.bytes.len()})
                        }
                        Err(error) => {
                            json!({"failure":{"stage":"acquisition","code":"provider_item_failed","message":error,"diagnostic":null}})
                        }
                    };
                    acquisitions.push(*outcome);
                    json!({"event":"item_completed","index":index,"result":evidence})
                }
            };
            run.event(event_value).map_err(|error| {
                asset_store::error::AssetError::new(
                    asset_store::error::ErrorCode::IoFailure,
                    error.to_string(),
                )
            })
        },
    );
    let session_failure = acquisition_result.err().map(|error| match error {
        AcquisitionStreamError::Interrupted => json!({"stage":"interrupt","code":"acquisition_interrupted","message":"Получен Ctrl+C; browser закрыт","diagnostic":null}),
        AcquisitionStreamError::Provider(message) => json!({"stage":"acquisition","code":"provider_session_failed","message":message,"diagnostic":null}),
        AcquisitionStreamError::Consumer(error) => json!({"stage":"progress","code":error.code.as_str(),"message":error.message,"diagnostic":null}),
    });
    let processed_count = acquisitions.len();
    if session_failure.is_some() {
        run.event(json!({"event":"session_failure","failure":session_failure}))?;
    }

    let validator = KanjiImageValidator::new();
    let mut rows = Vec::with_capacity(items.len());
    let mut all_verified = session_failure.is_none() && processed_count == items.len();

    for (item, acquisition) in items.iter().zip(acquisitions) {
        let codepoint = unicode_codepoint(&item.character)?;
        let image_relative_path = format!("images/{codepoint}.png");
        let media = match acquisition {
            Ok(media) => media,
            Err(error) => {
                all_verified = false;
                rows.push(item_row(
                    item,
                    &codepoint,
                    "acquisition_failed",
                    json!({
                        "failure": error,
                    }),
                ));
                continue;
            }
        };

        let metadata_matches = metadata_matches_plan(item, &media, &codepoint);
        let png_signature_valid = media.bytes.starts_with(b"\x89PNG\r\n\x1a\n");
        let decoded = image::load_from_memory_with_format(&media.bytes, image::ImageFormat::Png);
        let dimensions = decoded.as_ref().ok().map(GenericImageView::dimensions);
        let sha256 = sha256_hex(&media.bytes);
        if png_signature_valid && dimensions.is_some() {
            fs::write(report_dir.join(&image_relative_path), &media.bytes)?;
        }

        let provisional = AssetRecord {
            identity: AssetIdentity::new("kanji", item.character.clone())?,
            // Валидатор использует только identity и фактический формат; запись не сохраняется,
            // поэтому здесь нет утверждения о storage path или имени для consumer.
            storage_path: String::new(),
            consumer_filename: String::new(),
            sha256: sha256.clone(),
            byte_length: media.bytes.len() as u64,
            format: DetectedFormat::from_signature(&media.bytes),
            provenance: Provenance {
                source_kind: media.evidence.provider.clone(),
                source_name: "yarxi-rendered-dark-font-sample.png".into(),
            },
            lifecycle: LifecycleState::Pending,
            validation: None,
            human_attestation: None,
            domain_metadata: None,
        };
        let decision = validator.validate(&provisional, &mut Cursor::new(&media.bytes));
        let decision = match decision {
            Ok(decision) => decision,
            Err(error) => {
                all_verified = false;
                rows.push(item_row(
                    item,
                    &codepoint,
                    "validator_failed",
                    json!({
                        "acquisition": media.evidence,
                        "source_url": media.source_url,
                        "sha256": sha256,
                        "byte_length": media.bytes.len(),
                        "dimensions": dimensions,
                        "failure": { "code": error.code, "message": error.message },
                        "image_path": image_relative_path,
                    }),
                ));
                continue;
            }
        };

        let status = decision.status.as_str();
        let verified = status == SemanticStatus::Verified.as_str()
            && metadata_matches
            && png_signature_valid
            && dimensions.is_some();
        if !verified {
            all_verified = false;
        }
        rows.push(item_row(item, &codepoint, if verified {
            "verified_candidate"
        } else {
            "candidate_rejected"
        }, json!({
            "acquisition": media.evidence,
            "source_url": media.source_url,
            "selection": media.selection,
            "sha256": sha256,
            "byte_length": media.bytes.len(),
            "png_signature_valid": png_signature_valid,
            "dimensions": dimensions,
            "metadata_matches_plan": metadata_matches,
            "semantic_status": status,
            "validation_evidence": decision.evidence,
            "image_path": image_relative_path,
            "failure": if verified { Value::Null } else { json!({
                "reason": "Не совпали сведения захвата, метаданные плана или результат семантической проверки.",
            }) },
        })));
    }

    for item in &items[processed_count..] {
        rows.push(item_row(
            item,
            &unicode_codepoint(&item.character)?,
            "not_started_session_failure",
            json!({"session_failure_ref":"#/session_failure"}),
        ));
    }
    for row in &rows {
        run.event(json!({"event":"item_result", "result":row}))?;
    }

    let run_status = if all_verified {
        "automatic_checks_passed_waiting_for_user_review"
    } else {
        "verification_failed"
    };
    save_evidence(
        run_status,
        plan_path,
        items,
        &rows,
        processed_count,
        session_failure.as_ref(),
        &report_dir,
    )?;

    let verified_count = rows
        .iter()
        .filter(|row| row["outcome"].as_str() == Some("verified_candidate"))
        .count();
    if !all_verified {
        return Err(format!(
            "Автоматическую проверку прошли {verified_count}/{} кандидатов",
            items.len()
        )
        .into());
    }

    println!(
        "Автоматическая проверка прошла для {verified_count} PNG; проверьте изображения вручную в ZIP evidence."
    );
    Ok(())
}

fn metadata_matches_plan(
    item: &PlanItem,
    media: &asset_store::yarxi::AcquiredMedia,
    expected_codepoint: &str,
) -> bool {
    media.character == item.character
        && media.evidence.target == AcquisitionTarget::RenderedFontSamplePng
        && media.selection == SelectionResult::RenderedFontSamplePng
        && media.evidence.selection == SelectionResult::RenderedFontSamplePng
        && media.evidence.rendered_font_sample.is_some()
        && plan_metadata_matches(
            item,
            expected_codepoint,
            &media.evidence.article_unicode,
            media.evidence.article_number,
            media.evidence.frequency_index,
        )
        && media.source_url == "https://www.yarxi.su/"
}

fn plan_metadata_matches(
    item: &PlanItem,
    expected_codepoint: &str,
    article_unicode: &str,
    article_number: Option<u32>,
    frequency_index: Option<u32>,
) -> bool {
    let expected_unicode = expected_codepoint
        .strip_prefix("U+")
        .or_else(|| expected_codepoint.strip_prefix("u+"))
        .unwrap_or(expected_codepoint);
    article_unicode.eq_ignore_ascii_case(expected_unicode)
        && item
            .article_number
            .is_none_or(|expected| article_number == Some(expected))
        && item
            .frequency_index
            .is_none_or(|expected| frequency_index == Some(expected))
}

fn item_row(item: &PlanItem, codepoint: &str, outcome: &str, extra: Value) -> Value {
    let mut row = json!({
        "group": item.group,
        "character": item.character,
        "unicode_codepoint": codepoint,
        "expected_article_number": item.article_number,
        "expected_frequency_index": item.frequency_index,
        "outcome": outcome,
    });
    if let (Some(row), Some(extra)) = (row.as_object_mut(), extra.as_object()) {
        row.extend(
            extra
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
    row
}

fn checkout_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let package_dir = Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize()?;
    let workspace_root = package_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("не удалось определить корень checkout")?;
    Ok(workspace_root.canonicalize()?)
}

fn read_plan_items(plan: &Value) -> Result<Vec<PlanItem>, Box<dyn std::error::Error>> {
    let values = plan["items"]
        .as_array()
        .ok_or("plan.items должен быть массивом")?;
    if values.is_empty() {
        return Err("plan.items не должен быть пустым".into());
    }

    let mut items = Vec::with_capacity(values.len());
    let mut seen = BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        let character = value["character"]
            .as_str()
            .ok_or_else(|| format!("plan.items[{index}].character должен быть строкой"))?;
        KanjiCharacter::from_str(character)
            .map_err(|error| format!("plan.items[{index}].character: {error}"))?;
        if !seen.insert(character.to_owned()) {
            return Err(format!("повтор символа в плане: {character}").into());
        }

        let codepoint = unicode_codepoint(character)?;
        if let Some(expected) = optional_string(value, "unicode_codepoint", index)? {
            let normalized = expected
                .strip_prefix("U+")
                .or_else(|| expected.strip_prefix("u+"))
                .unwrap_or(expected);
            if !normalized.eq_ignore_ascii_case(&codepoint[2..]) {
                return Err(format!(
                    "plan.items[{index}].unicode_codepoint не соответствует символу {character}"
                )
                .into());
            }
        }

        let group = optional_string(value, "group", index)?
            .map(str::trim)
            .map(str::to_owned);
        if group.as_deref() == Some("") {
            return Err(format!("plan.items[{index}].group не должен быть пустым").into());
        }
        items.push(PlanItem {
            group,
            character: character.to_owned(),
            article_number: optional_u32(value, "yarxi_article_number", index)?,
            frequency_index: optional_u32(value, "frequency_index", index)?,
        });
    }
    Ok(items)
}

fn optional_string<'a>(
    value: &'a Value,
    key: &str,
    index: usize,
) -> Result<Option<&'a str>, Box<dyn std::error::Error>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(format!("plan.items[{index}].{key} должен быть строкой").into()),
    }
}

fn optional_u32(
    value: &Value,
    key: &str,
    index: usize,
) -> Result<Option<u32>, Box<dyn std::error::Error>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let number = value
                .as_u64()
                .ok_or_else(|| format!("plan.items[{index}].{key} должен быть целым числом"))?;
            Ok(Some(u32::try_from(number).map_err(|_| {
                format!("plan.items[{index}].{key} выходит за диапазон u32")
            })?))
        }
    }
}

fn unicode_codepoint(character: &str) -> Result<String, Box<dyn std::error::Error>> {
    let scalar = character
        .chars()
        .next()
        .ok_or("символ не должен быть пустым")?;
    Ok(format!("U+{:04X}", u32::from(scalar)))
}

fn save_evidence(
    run_status: &str,
    _plan_path: &Path,
    items: &[PlanItem],
    rows: &[Value],
    processed_item_count: usize,
    session_failure: Option<&Value>,
    report_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let groups = items
        .iter()
        .fold(BTreeMap::<String, Vec<String>>::new(), |mut map, item| {
            if let Some(group) = &item.group {
                map.entry(group.clone())
                    .or_default()
                    .push(item.character.clone());
            }
            map
        });
    let mut rows = rows.to_vec();
    rows.iter_mut().for_each(evidence_zip::redact);
    let mut report = json!({
        "schema_version": 1,
        "run_status": run_status,
        "planned_item_count": items.len(),
        "groups": groups,
        "validator": KanjiImageValidator::validator_identity(),
        "items": rows,
    });
    report["session_failure"] = session_failure.cloned().unwrap_or(Value::Null);
    report["processed_item_count"] = json!(processed_item_count);
    evidence_zip::redact(&mut report);
    fs::write(
        report_dir.join("evidence.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    save_html_report(run_status, items, &rows, session_failure, report_dir)?;
    Ok(())
}

fn save_html_report(
    run_status: &str,
    items: &[PlanItem],
    rows: &[Value],
    session_failure: Option<&Value>,
    report_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut html = String::from(
        r##"<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Yarxi — приёмка тёмных PNG</title>
<style>
body{font:16px/1.5 system-ui,sans-serif;margin:2rem;color:#202428;background:#f4f6f8}
h1,h2{line-height:1.2}h2{margin-top:2.5rem;border-bottom:1px solid #c7cdd2;padding-bottom:.4rem}
.summary{background:#fff;border:1px solid #c7cdd2;padding:1rem;max-width:70rem}
.items{display:grid;grid-template-columns:repeat(auto-fit,minmax(28rem,1fr));gap:1rem}
article{background:#fff;border:1px solid #c7cdd2;padding:1rem;min-width:0}
article h3{display:flex;justify-content:space-between;margin:.1rem 0 .6rem;font-size:1.4rem}
.image-viewport{overflow:auto;max-width:100%;background:#e4e8eb;border:1px solid #c7cdd2;padding:.5rem}
.image-viewport img{display:block;width:auto;height:auto;max-width:none;max-height:none}
.image-viewport a{display:inline-block}
dl{display:grid;grid-template-columns:max-content 1fr;gap:.15rem .75rem;margin:.8rem 0 0}
dt{font-weight:650}dd{margin:0;overflow-wrap:anywhere}
code{font-size:.9em;overflow-wrap:anywhere}.status{font-weight:700}
small{color:#505960}
</style>
</head>
<body>
<h1>Yarxi — приёмка тёмных PNG</h1>
<div class="summary">
<p><strong>Статус прогона:</strong> __RUN_STATUS__</p>
<p>Кандидатов: __ITEM_COUNT__. Изображения показаны в естественном размере; фактические размеры и сведения захвата указаны рядом.</p>
<p><a href="evidence.json">Машиночитаемый отчёт (JSON)</a></p>
</div>
"##,
    );
    html = html
        .replace("__RUN_STATUS__", &escape_html(run_status))
        .replace("__ITEM_COUNT__", &items.len().to_string());
    if let Some(session_failure) = session_failure {
        let mut safe_failure = session_failure.clone();
        evidence_zip::redact(&mut safe_failure);
        html.push_str(&format!(
            "<section class=\"summary\" id=\"session-failure\"><h2>Ошибка сессии браузера</h2><pre>{}</pre></section>",
            escape_html(&serde_json::to_string_pretty(&safe_failure)?)
        ));
    }

    let groups: BTreeSet<Option<String>> = items.iter().map(|item| item.group.clone()).collect();
    for group_name in groups {
        let group: Vec<_> = items
            .iter()
            .filter(|item| item.group == group_name)
            .collect();
        let label = group_name.as_deref().unwrap_or("Без группы");
        html.push_str(&format!(
            "<section><h2>{} — {} PNG</h2><div class=\"items\">",
            escape_html(label),
            group.len()
        ));
        for item in group {
            let row = rows
                .iter()
                .find(|row| row["character"].as_str() == Some(&item.character));
            let codepoint = unicode_codepoint(&item.character)?;
            let title = format!("{} {codepoint}", item.character);
            html.push_str(&format!(
                "<article><h3><span>{}</span><small>{}</small></h3>",
                escape_html(&item.character),
                escape_html(&codepoint)
            ));
            if let Some(image_path) = row
                .and_then(|row| row["image_path"].as_str())
                .filter(|image_path| report_dir.join(image_path).is_file())
            {
                html.push_str(&format!(
                    "<div class=\"image-viewport\"><a href=\"{}\"><img src=\"{}\" alt=\"{}\"></a></div>",
                    escape_html(image_path),
                    escape_html(image_path),
                    escape_html(&title)
                ));
            } else {
                if row.and_then(|row| row["outcome"].as_str())
                    == Some("not_started_session_failure")
                {
                    html.push_str(
                        "<p>Обработка не началась: сессия браузера завершилась ошибкой.</p>",
                    );
                } else {
                    html.push_str("<p>PNG не сохранён: изображение получить не удалось.</p>");
                }
            }
            if row
                .and_then(|row| row["session_failure_ref"].as_str())
                .is_some()
            {
                html.push_str(
                    "<p><a href=\"#session-failure\">Причина остановки сессии браузера</a></p>",
                );
            }

            let row = row.cloned().unwrap_or(Value::Null);
            let sample = &row["acquisition"]["rendered_font_sample"];
            let mut fields = vec![
                (
                    "Источник",
                    row["source_url"].as_str().unwrap_or("—").to_owned(),
                ),
                ("Код Unicode", codepoint),
                (
                    "Захват",
                    sample["capture"].as_str().unwrap_or("—").to_owned(),
                ),
                (
                    "Размер PNG",
                    row["dimensions"]
                        .as_array()
                        .filter(|dimensions| dimensions.len() == 2)
                        .map(|dimensions| format!("{} × {} px", dimensions[0], dimensions[1]))
                        .unwrap_or_else(|| "—".to_owned()),
                ),
                (
                    "Область просмотра",
                    format_number_pair(
                        &sample["viewport_width"],
                        &sample["viewport_height"],
                        "CSS px",
                    ),
                ),
                (
                    "Масштаб устройства",
                    sample["device_scale_factor"]
                        .as_f64()
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "—".to_owned()),
                ),
                (
                    "Тема",
                    sample["dark_environment"]
                        .as_str()
                        .unwrap_or("—")
                        .to_owned(),
                ),
                (
                    "Шрифт",
                    format!(
                        "{} {}",
                        sample["font_family"].as_str().unwrap_or("—"),
                        sample["font_size"].as_str().unwrap_or("")
                    ),
                ),
                ("SHA-256", row["sha256"].as_str().unwrap_or("—").to_owned()),
                (
                    "Результат",
                    format!(
                        "{} / {}",
                        row["outcome"].as_str().unwrap_or("pending"),
                        row["semantic_status"].as_str().unwrap_or("—")
                    ),
                ),
                (
                    "Сводка проверки пикселей",
                    row["validation_evidence"][0]["summary"]
                        .as_str()
                        .unwrap_or("—")
                        .to_owned(),
                ),
            ];
            if let Some(number) = item.article_number {
                fields.push(("Статья Yarxi", number.to_string()));
            }
            if let Some(index) = item.frequency_index {
                fields.push(("Индекс частотности", index.to_string()));
            }
            if sample["css_rect"].is_object() {
                fields.push((
                    "Прямоугольник CSS",
                    serde_json::to_string(&sample["css_rect"])?,
                ));
            }
            html.push_str("<dl>");
            for (label, value) in fields {
                html.push_str(&format!(
                    "<dt>{}</dt><dd>{}</dd>",
                    escape_html(label),
                    escape_html(&value)
                ));
            }
            html.push_str("</dl></article>");
        }
        html.push_str("</div></section>");
    }
    html.push_str("</body></html>\n");
    fs::write(report_dir.join("index.html"), html)?;
    Ok(())
}

fn format_number_pair(left: &Value, right: &Value, suffix: &str) -> String {
    match (left.as_u64(), right.as_u64()) {
        (Some(left), Some(right)) => format!("{left} × {right} {suffix}"),
        _ => "—".to_owned(),
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn plan_accepts_arbitrary_item_count_and_optional_metadata() {
        let items = read_plan_items(&json!({
            "items": [
                { "character": "漢", "group": "основная" },
                {
                    "character": "走",
                    "group": "другая группа",
                    "unicode_codepoint": "U+8D70",
                    "yarxi_article_number": 123,
                    "frequency_index": 45
                },
                { "character": "字" }
            ]
        }))
        .unwrap();

        assert_eq!(items.len(), 3);
        assert_eq!(items[0].group.as_deref(), Some("основная"));
        assert_eq!(items[0].article_number, None);
        assert_eq!(items[1].article_number, Some(123));
        assert_eq!(items[1].frequency_index, Some(45));
        assert_eq!(items[2].group, None);
    }

    #[test]
    fn plan_rejects_empty_duplicate_or_inconsistent_unicode_items() {
        assert!(read_plan_items(&json!({ "items": [] })).is_err());
        assert!(
            read_plan_items(&json!({
                "items": [{ "character": "漢" }, { "character": "漢" }]
            }))
            .is_err()
        );
        assert!(
            read_plan_items(&json!({
                "items": [{ "character": "漢", "unicode_codepoint": "U+8D70" }]
            }))
            .is_err()
        );
        assert!(read_plan_items(&json!({ "items": [{ "character": "/" }] })).is_err());
    }

    #[test]
    fn supplied_article_metadata_is_checked_while_omitted_metadata_is_optional() {
        let optional = read_plan_items(&json!({ "items": [{ "character": "漢" }] })).unwrap();
        assert!(plan_metadata_matches(
            &optional[0],
            "U+6F22",
            "6F22",
            Some(12),
            Some(34)
        ));
        assert!(!plan_metadata_matches(
            &optional[0],
            "U+6F22",
            "8D70",
            Some(12),
            Some(34)
        ));

        let specified = read_plan_items(&json!({
            "items": [{
                "character": "漢",
                "yarxi_article_number": 12,
                "frequency_index": 34
            }]
        }))
        .unwrap();
        assert!(plan_metadata_matches(
            &specified[0],
            "U+6F22",
            "6F22",
            Some(12),
            Some(34)
        ));
        assert!(!plan_metadata_matches(
            &specified[0],
            "U+6F22",
            "6F22",
            Some(13),
            Some(34)
        ));
    }

    #[test]
    fn cli_tls_is_opt_in_and_plan_is_required() {
        assert!(Args::try_parse_from(["yarxi_dark_acceptance"]).is_err());
        let default_args =
            Args::try_parse_from(["yarxi_dark_acceptance", "--plan", "plan.json"]).unwrap();
        assert!(!default_args.allow_insecure_tls);
        let opted_in = Args::try_parse_from([
            "yarxi_dark_acceptance",
            "--plan",
            "plan.json",
            "--allow-insecure-tls",
        ])
        .unwrap();
        assert!(opted_in.allow_insecure_tls);
    }

    #[test]
    fn zip_output_is_new_and_outside_checkout_and_run() {
        let checkout = checkout_root().unwrap();
        let workspace =
            asset_store::temp_workspace::TempWorkspace::create("acceptance-output-fixture")
                .unwrap();
        let output = workspace.path().join("report.zip");
        assert!(
            evidence_zip::resolve_output(Some(&output), &checkout, workspace.path(), "test")
                .is_err()
        );
        let checkout_output = checkout.join("report.zip");
        assert!(
            evidence_zip::resolve_output(
                Some(&checkout_output),
                &checkout,
                workspace.path(),
                "test"
            )
            .is_err()
        );
        assert!(!checkout_output.exists());
    }

    #[test]
    fn html_groups_are_data_driven_and_unassigned_items_are_included() {
        let items = read_plan_items(&json!({
            "items": [
                { "character": "漢", "group": "custom" },
                { "character": "字" }
            ]
        }))
        .unwrap();
        let workspace =
            asset_store::temp_workspace::TempWorkspace::create("acceptance-fixture").unwrap();

        let report_dir = workspace.path();
        save_html_report("test", &items, &[], None, report_dir).unwrap();
        let html = fs::read_to_string(report_dir.join("index.html")).unwrap();
        assert!(html.contains("custom — 1 PNG"));
        assert!(html.contains("Без группы — 1 PNG"));
        assert!(!html.contains("50 тёмных PNG"));
    }

    #[test]
    fn html_shows_session_failure_and_marks_unstarted_items_distinctly() {
        let items = read_plan_items(&json!({ "items": [{ "character": "漢" }] })).unwrap();
        let workspace =
            asset_store::temp_workspace::TempWorkspace::create("acceptance-fixture").unwrap();
        let report_dir = workspace.path();
        let rows = [json!({
            "character": "漢",
            "outcome": "not_started_session_failure",
            "session_failure_ref": "#/session_failure",
        })];
        let failure = json!({
            "code": "acquisition_interrupted",
            "message": "browser failed <after launch>",
        });

        save_html_report(
            "verification_failed",
            &items,
            &rows,
            Some(&failure),
            report_dir,
        )
        .unwrap();

        let html = fs::read_to_string(report_dir.join("index.html")).unwrap();
        assert!(html.contains("id=\"session-failure\""));
        assert!(html.contains("href=\"#session-failure\""));
        assert!(html.contains("Обработка не началась: сессия браузера завершилась ошибкой."));
        assert!(html.contains("browser failed &lt;after launch&gt;"));
        assert!(!html.contains("PNG не сохранён: изображение получить не удалось."));
    }

    #[test]
    fn evidence_keeps_observed_capture_values_without_a_duplicated_contract() {
        let items = read_plan_items(&json!({ "items": [{ "character": "漢" }] })).unwrap();
        let workspace =
            asset_store::temp_workspace::TempWorkspace::create("acceptance-fixture").unwrap();

        let report_dir = workspace.path();
        let rows = [json!({
            "character": "漢",
            "acquisition": {
                "rendered_font_sample": {
                    "viewport_width": 1280,
                    "viewport_height": 900,
                    "device_scale_factor": 2.175
                }
            }
        })];
        save_evidence(
            "test",
            Path::new("external-plan.json"),
            &items,
            &rows,
            items.len(),
            None,
            report_dir,
        )
        .unwrap();
        let evidence: Value =
            serde_json::from_slice(&fs::read(report_dir.join("evidence.json")).unwrap()).unwrap();
        assert!(evidence.get("capture_contract").is_none());
        assert_eq!(
            evidence["items"][0]["acquisition"]["rendered_font_sample"]["device_scale_factor"],
            2.175
        );
    }
}
