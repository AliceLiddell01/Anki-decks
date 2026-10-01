use std::collections::BTreeSet;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use asset_store::hashing::sha256_hex;
use asset_store::jpdb::{JpdbPitchOutcome, JpdbPitchProvider, JpdbPitchQuery};
use asset_store::model::{
    AssetIdentity, AssetRecord, DetectedFormat, LifecycleState, Provenance, SemanticStatus,
};
use asset_store::pitch_accent::PitchAccentImageValidator;
use asset_store::validation::SemanticValidator;
use clap::Parser;
use image::GenericImageView;
use serde_json::{Value, json};

#[derive(Debug, Parser)]
#[command(
    name = "jpdb_pitch_acceptance",
    about = "Получить и проверить browser-rendered pitch-accent PNG с JPDB"
)]
struct Args {
    /// JSON-план: {"items":[{"surface":"幽霊","reading":"ゆうれい"}]}.
    /// Поле reading необязательно; путь может находиться вне checkout.
    #[arg(long, value_name = "PATH")]
    plan: PathBuf,

    /// Новый каталог HTML-отчёта. Каталог должен быть вне checkout и ещё не существовать.
    #[arg(long, value_name = "DIR")]
    output: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanItem {
    surface: String,
    reading: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run(Args::parse()).await
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let plan_path = args.plan.canonicalize()?;
    let plan: Value = serde_json::from_slice(&fs::read(&plan_path)?)?;
    let items = read_plan_items(&plan)?;
    let report_dir = create_report_dir(args.output.as_deref())?;

    let queries: Vec<_> = items
        .iter()
        .map(|item| JpdbPitchQuery {
            surface: item.surface.clone(),
            reading: item.reading.clone(),
        })
        .collect();
    let outcomes = JpdbPitchProvider::acquire_many(&queries).await;
    let mut rows = Vec::with_capacity(items.len().max(outcomes.len()));
    let mut all_checks_passed = outcomes.len() == items.len();

    for (index, (item, outcome)) in items.iter().zip(outcomes.iter()).enumerate() {
        let mut row = json!({
            "surface": item.surface,
            "requested_reading": item.reading,
            "outcome": outcome_name(outcome),
        });

        match outcome {
            JpdbPitchOutcome::Acquired { asset } => {
                let metadata = &asset.metadata;
                let bytes = &asset.bytes;
                let sha256 = sha256_hex(bytes);
                let png_signature_valid = bytes.starts_with(b"\x89PNG\r\n\x1a\n");
                let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Png);
                let dimensions = decoded.as_ref().ok().map(GenericImageView::dimensions);

                // Сохраняются только исходные bytes outcome Acquired. Невалидный PNG
                // остаётся доступен как .bin и не встраивается в HTML под видом картинки.
                let extension = if png_signature_valid && dimensions.is_some() {
                    "png"
                } else {
                    "bin"
                };
                let image_relative_path = format!("images/{:04}.{extension}", index + 1);
                fs::create_dir_all(report_dir.join("images"))?;
                fs::write(report_dir.join(&image_relative_path), bytes)?;

                let metadata_matches_plan = metadata.surface == item.surface
                    && item
                        .reading
                        .as_deref()
                        .is_none_or(|reading| metadata.reading == reading);
                let provisional = AssetRecord {
                    identity: AssetIdentity::new("pitch_accent", metadata.surface.clone())?,
                    // Временная запись передаёт production validator фактические metadata
                    // provider. Harness не создаёт canonical storage или consumer filename.
                    storage_path: String::new(),
                    consumer_filename: String::new(),
                    sha256: sha256.clone(),
                    byte_length: bytes.len() as u64,
                    format: DetectedFormat::from_signature(bytes),
                    provenance: Provenance {
                        source_kind: "jpdb_browser_render".into(),
                        source_name: format!("jpdb-vocabulary-{}.png", metadata.jpdb_vocabulary_id),
                    },
                    lifecycle: LifecycleState::Pending,
                    validation: None,
                    human_attestation: None,
                    domain_metadata: Some(serde_json::to_value(metadata)?),
                };
                let validation =
                    PitchAccentImageValidator.validate(&provisional, &mut Cursor::new(bytes));

                match validation {
                    Ok(decision) => {
                        let semantic_status = decision.status.as_str();
                        let verified = semantic_status == SemanticStatus::Verified.as_str()
                            && metadata_matches_plan
                            && png_signature_valid
                            && dimensions.is_some();
                        all_checks_passed &= verified;
                        extend_object(
                            &mut row,
                            json!({
                                "jpdb_vocabulary_id": metadata.jpdb_vocabulary_id,
                                "detail_url": metadata.evidence.source_url,
                                "canonical_reading": metadata.reading,
                                "graph_count": metadata.evidence.graph_count,
                                "graphs": metadata.evidence.render.graphs,
                                "dark_theme_proof": metadata.evidence.render.dark_theme,
                                "capture_geometry": {
                                    "selector": metadata.evidence.render.selector,
                                    "capture_rect_css_px": metadata.evidence.render.capture_rect,
                                    "viewport_css_px": [
                                        metadata.evidence.render.viewport_width,
                                        metadata.evidence.render.viewport_height
                                    ],
                                    "page_scale_factor": metadata.evidence.render.page_scale_factor,
                                    "device_scale_factor": metadata.evidence.render.device_scale_factor,
                                    "pixel_dimensions": [
                                        metadata.evidence.render.pixel_width,
                                        metadata.evidence.render.pixel_height
                                    ],
                                },
                                "browser": metadata.evidence.browser,
                                "metadata_matches_plan": metadata_matches_plan,
                                "sha256": sha256,
                                "byte_length": bytes.len(),
                                "png_signature_valid": png_signature_valid,
                                "dimensions": dimensions,
                                "semantic_status": semantic_status,
                                "validation_evidence": decision.evidence,
                                "image_path": image_relative_path,
                                "image_mime": if extension == "png" { "image/png" } else { "application/octet-stream" },
                                "item_status": if verified { "verified_candidate" } else { "candidate_rejected" },
                            }),
                        );
                    }
                    Err(error) => {
                        all_checks_passed = false;
                        extend_object(
                            &mut row,
                            json!({
                                "jpdb_vocabulary_id": metadata.jpdb_vocabulary_id,
                                "detail_url": metadata.evidence.source_url,
                                "canonical_reading": metadata.reading,
                                "graph_count": metadata.evidence.graph_count,
                                "graphs": metadata.evidence.render.graphs,
                                "dark_theme_proof": metadata.evidence.render.dark_theme,
                                "capture_geometry": metadata.evidence.render,
                                "browser": metadata.evidence.browser,
                                "metadata_matches_plan": metadata_matches_plan,
                                "sha256": sha256,
                                "byte_length": bytes.len(),
                                "png_signature_valid": png_signature_valid,
                                "dimensions": dimensions,
                                "semantic_status": Value::Null,
                                "validator_failure": { "code": error.code, "message": error.message },
                                "image_path": image_relative_path,
                                "image_mime": if extension == "png" { "image/png" } else { "application/octet-stream" },
                                "item_status": "validator_failed",
                            }),
                        );
                    }
                }
            }
            JpdbPitchOutcome::NoPitchAccentOnSource { evidence } => {
                extend_object(
                    &mut row,
                    json!({
                        "absence_evidence": evidence,
                        "item_status": "no_pitch_accent_on_source",
                        "semantic_status": Value::Null,
                    }),
                );
            }
            JpdbPitchOutcome::AmbiguousVocabulary {
                surface,
                reading,
                candidates,
            } => {
                extend_object(
                    &mut row,
                    json!({
                        "resolved_surface": surface,
                        "resolved_reading": reading,
                        "candidates": candidates,
                        "item_status": "ambiguous_vocabulary",
                        "semantic_status": Value::Null,
                    }),
                );
            }
            JpdbPitchOutcome::VocabularyNotFound { surface, reading } => {
                extend_object(
                    &mut row,
                    json!({
                        "resolved_surface": surface,
                        "resolved_reading": reading,
                        "item_status": "vocabulary_not_found",
                        "semantic_status": Value::Null,
                    }),
                );
            }
            JpdbPitchOutcome::Failed { error } => {
                all_checks_passed = false;
                extend_object(
                    &mut row,
                    json!({
                        "failure": error,
                        "item_status": "acquisition_failed",
                        "semantic_status": Value::Null,
                    }),
                );
            }
        }
        let observed_dimensions = row["dimensions"]
            .as_array()
            .filter(|dimensions| dimensions.len() == 2)
            .and_then(|dimensions| Some([dimensions[0].as_u64()?, dimensions[1].as_u64()?]));
        if let Some(visual_reference) =
            visual_reference(&item.surface, outcome_name(outcome), observed_dimensions)
        {
            extend_object(&mut row, json!({ "visual_reference": visual_reference }));
        }
        rows.push(row);
    }

    if outcomes.len() > items.len() {
        for outcome in outcomes.iter().skip(items.len()) {
            all_checks_passed = false;
            rows.push(json!({
                "outcome": outcome_name(outcome),
                "item_status": "unmatched_provider_result",
                "semantic_status": Value::Null,
            }));
        }
    }
    if outcomes.len() < items.len() {
        for item in items.iter().skip(outcomes.len()) {
            rows.push(json!({
                "surface": item.surface,
                "requested_reading": item.reading,
                "outcome": "missing_provider_result",
                "item_status": "provider_result_missing",
                "semantic_status": Value::Null,
            }));
        }
    }

    let run_status = if all_checks_passed {
        "automatic_checks_passed_waiting_for_user_review"
    } else {
        "verification_failed"
    };
    save_evidence(run_status, &plan_path, &items, &rows, &report_dir)?;

    let verified_count = rows
        .iter()
        .filter(|row| row["item_status"].as_str() == Some("verified_candidate"))
        .count();
    if !all_checks_passed {
        return Err(format!(
            "Автоматическая проверка не прошла для всего плана; подтверждено PNG: {verified_count}/{}; отчёт: {}",
            items.len(),
            report_dir.join("index.html").display()
        )
        .into());
    }

    println!(
        "Автоматические проверки завершены; PNG-кандидатов подтверждено: {verified_count}. Ассеты не опубликованы, проверьте исходы и изображения вручную. Отчёт: {}",
        report_dir.join("index.html").display()
    );
    Ok(())
}

fn outcome_name(outcome: &JpdbPitchOutcome) -> &'static str {
    match outcome {
        JpdbPitchOutcome::Acquired { .. } => "acquired",
        JpdbPitchOutcome::NoPitchAccentOnSource { .. } => "no_pitch_accent_on_source",
        JpdbPitchOutcome::AmbiguousVocabulary { .. } => "ambiguous_vocabulary",
        JpdbPitchOutcome::VocabularyNotFound { .. } => "vocabulary_not_found",
        JpdbPitchOutcome::Failed { .. } => "failed",
    }
}

fn reference_dimensions(surface: &str) -> Option<[u32; 2]> {
    match surface {
        "くすぐったい" => Some([321, 84]),
        "クラブ" => Some([162, 87]),
        "乙女" => Some([177, 84]),
        "吹き抜ける" => Some([273, 84]),
        _ => None,
    }
}

fn visual_reference(
    surface: &str,
    outcome: &str,
    observed_dimensions: Option<[u64; 2]>,
) -> Option<Value> {
    let expected_dimensions = reference_dimensions(surface)?;
    Some(if outcome == "acquired" {
        let comparison_status = match observed_dimensions {
            Some(observed) if observed == expected_dimensions.map(u64::from) => "match",
            Some(_) => "source_drift",
            None => "unavailable",
        };
        json!({
            "expected_native_png_dimensions_px": expected_dimensions,
            "observed_native_png_dimensions_px": observed_dimensions,
            "comparison_status": comparison_status,
        })
    } else {
        json!({
            "expected_native_png_dimensions_px": expected_dimensions,
            "observed_native_png_dimensions_px": Value::Null,
            "comparison_status": "not_acquired",
            "outcome": outcome,
        })
    })
}

fn extend_object(target: &mut Value, extra: Value) {
    if let (Some(target), Some(extra)) = (target.as_object_mut(), extra.as_object()) {
        target.extend(
            extra
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
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
        let surface = value["surface"]
            .as_str()
            .ok_or_else(|| format!("plan.items[{index}].surface должен быть строкой"))?;
        validate_plan_text(surface, "surface", index)?;
        let reading = optional_string(value, "reading", index)?;
        if let Some(reading) = reading {
            validate_plan_text(reading, "reading", index)?;
        }
        let key = (surface.to_owned(), reading.map(str::to_owned));
        if !seen.insert(key) {
            return Err(format!("повтор surface/reading в плане: {surface}").into());
        }
        items.push(PlanItem {
            surface: surface.to_owned(),
            reading: reading.map(str::to_owned),
        });
    }
    Ok(items)
}

fn validate_plan_text(
    value: &str,
    name: &str,
    index: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    if value.trim().is_empty() {
        return Err(format!("plan.items[{index}].{name} не должен быть пустым").into());
    }
    if value.chars().any(char::is_control) {
        return Err(
            format!("plan.items[{index}].{name} не должен содержать управляющие символы").into(),
        );
    }
    Ok(())
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

fn create_report_dir(output: Option<&Path>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let checkout = checkout_root()?;
    match output {
        Some(path) => {
            let resolved = resolve_new_output_path(path)?;
            ensure_outside_checkout(&resolved, &checkout)?;
            fs::create_dir(&resolved)?;
            Ok(resolved)
        }
        None => {
            let temp_root = std::env::temp_dir().canonicalize()?;
            ensure_outside_checkout(&temp_root, &checkout)?;
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let report_dir = temp_root.join(format!(
                "jpdb-live-acceptance-{}-{timestamp}",
                std::process::id()
            ));
            fs::create_dir(&report_dir)?;
            Ok(report_dir)
        }
    }
}

fn checkout_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let package_dir = Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize()?;
    let workspace_root = package_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("не удалось определить корень checkout")?;
    Ok(workspace_root.canonicalize()?)
}

fn resolve_new_output_path(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let file_name = absolute
        .file_name()
        .filter(|name| *name != "." && *name != "..")
        .ok_or("--output должен указывать на новый каталог")?;
    let parent = absolute
        .parent()
        .ok_or("не удалось определить родительский каталог --output")?
        .canonicalize()?;
    let resolved = parent.join(file_name);
    if fs::symlink_metadata(&resolved).is_ok() {
        return Err(format!("каталог --output уже существует: {}", resolved.display()).into());
    }
    Ok(resolved)
}

fn ensure_outside_checkout(path: &Path, checkout: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if path.starts_with(checkout) {
        return Err(format!("отчёт должен находиться вне checkout: {}", path.display()).into());
    }
    Ok(())
}

fn save_evidence(
    run_status: &str,
    plan_path: &Path,
    items: &[PlanItem],
    rows: &[Value],
    report_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let report = json!({
        "schema_version": 1,
        "run_status": run_status,
        "plan_path": plan_path.display().to_string(),
        "planned_item_count": items.len(),
        "validator": PitchAccentImageValidator::validator_identity(),
        "items": rows,
    });
    fs::write(
        report_dir.join("evidence.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    save_html_report(run_status, items, rows, report_dir)?;
    Ok(())
}

fn save_html_report(
    run_status: &str,
    items: &[PlanItem],
    rows: &[Value],
    report_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut html = format!(
        r##"<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>JPDB — приёмка pitch-accent PNG</title>
<style>
body{{font:16px/1.5 system-ui,sans-serif;margin:2rem;color:#202428;background:#f4f6f8}}
h1,h2{{line-height:1.2}}.summary{{background:#fff;border:1px solid #c7cdd2;padding:1rem;max-width:75rem}}
.items{{display:grid;grid-template-columns:repeat(auto-fit,minmax(30rem,1fr));gap:1rem}}
article{{background:#fff;border:1px solid #c7cdd2;padding:1rem;min-width:0}}
article h2{{font-size:1.2rem;margin:.1rem 0 .5rem}}.status{{font-weight:700}}
.image-viewport{{overflow:auto;max-width:100%;background:#e4e8eb;border:1px solid #c7cdd2;padding:.5rem}}
.image-viewport img{{display:block;width:auto;height:auto;max-width:none;max-height:none}}
dl{{display:grid;grid-template-columns:max-content 1fr;gap:.15rem .75rem;margin:.8rem 0 0}}
dt{{font-weight:650}}dd{{margin:0;overflow-wrap:anywhere}}code{{font-size:.9em;overflow-wrap:anywhere}}
pre{{white-space:pre-wrap;overflow-wrap:anywhere;background:#f5f6f7;padding:.6rem}}
small{{color:#505960}}
</style>
</head>
<body>
<h1>JPDB — приёмка pitch-accent PNG</h1>
<div class="summary">
<p><strong>Статус прогона:</strong> {}</p>
<p>Элементы плана: {}. Для acquired показываются исходные browser bytes в естественном размере и evidence production validator.</p>
<p><a href="evidence.json">Машиночитаемый отчёт (JSON)</a></p>
</div>
"##,
        escape_html(run_status),
        items.len()
    );

    html.push_str("<section class=\"items\">");
    for item in items {
        let row = rows.iter().find(|row| {
            row["surface"].as_str() == Some(item.surface.as_str())
                && row["requested_reading"].as_str() == item.reading.as_deref()
        });
        append_html_item(&mut html, item, row);
    }
    html.push_str("</section></body></html>\n");
    fs::write(report_dir.join("index.html"), html)?;
    Ok(())
}

fn append_html_item(html: &mut String, item: &PlanItem, row: Option<&Value>) {
    let row = row.cloned().unwrap_or(Value::Null);
    html.push_str(&format!(
        "<article><h2>{}</h2><p class=\"status\">{}</p>",
        escape_html(&item.surface),
        escape_html(
            row["item_status"]
                .as_str()
                .unwrap_or("provider_result_missing")
        )
    ));
    if let Some(image_path) = row["image_path"].as_str() {
        if row["image_mime"].as_str() == Some("image/png") {
            html.push_str(&format!(
                "<div class=\"image-viewport\"><a href=\"{}\"><img src=\"{}\" alt=\"{}\"></a></div>",
                escape_html(image_path),
                escape_html(image_path),
                escape_html(&item.surface)
            ));
        } else {
            html.push_str(&format!(
                "<p>Полученные raw bytes: <a href=\"{}\">скачать</a> (PNG не подтверждён).</p>",
                escape_html(image_path)
            ));
        }
    }

    let mut fields = vec![
        (
            "Запрошенное чтение",
            item.reading.clone().unwrap_or_else(|| "—".into()),
        ),
        ("Исход", value_text(&row["outcome"])),
        ("Статус проверки", value_text(&row["semantic_status"])),
        ("Vocabulary ID", value_text(&row["jpdb_vocabulary_id"])),
        ("Detail URL", value_text(&row["detail_url"])),
        ("Каноническое чтение", value_text(&row["canonical_reading"])),
        ("Число графиков", value_text(&row["graph_count"])),
        ("CSS capture geometry", value_text(&row["capture_geometry"])),
        ("Dark-theme proof", value_text(&row["dark_theme_proof"])),
        ("Pixel dimensions", value_text(&row["dimensions"])),
        (
            "Visual reference comparison",
            value_text(&row["visual_reference"]),
        ),
        ("SHA-256", value_text(&row["sha256"])),
        (
            "Metadata совпадает с планом",
            value_text(&row["metadata_matches_plan"]),
        ),
    ];
    if row["absence_evidence"].is_object() {
        fields.push((
            "Доказательство отсутствия pitch accent",
            value_text(&row["absence_evidence"]),
        ));
    }
    if row["candidates"].is_array() {
        fields.push(("Кандидаты", value_text(&row["candidates"])));
    }
    if row["failure"].is_object() {
        fields.push(("Ошибка provider", value_text(&row["failure"])));
    }
    if row["validator_failure"].is_object() {
        fields.push(("Ошибка validator", value_text(&row["validator_failure"])));
    }

    html.push_str("<dl>");
    for (label, value) in fields {
        html.push_str(&format!(
            "<dt>{}</dt><dd>{}</dd>",
            escape_html(label),
            escape_html(&value)
        ));
    }
    html.push_str("</dl>");
    if row["validation_evidence"].is_array() {
        html.push_str(&format!(
            "<details><summary>Evidence validator</summary><pre>{}</pre></details>",
            escape_html(&value_text(&row["validation_evidence"]))
        ));
    }
    if let Some(browser) = row.get("browser") {
        html.push_str(&format!(
            "<details><summary>Browser runtime</summary><pre>{}</pre></details>",
            escape_html(&value_text(browser))
        ));
    }
    html.push_str("</article>");
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Null => "—".to_owned(),
        Value::String(value) => value.clone(),
        _ => serde_json::to_string_pretty(value).unwrap_or_else(|_| "—".to_owned()),
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
    fn plan_accepts_optional_reading_and_distinct_readings() {
        let items = read_plan_items(&json!({
            "items": [
                { "surface": "幽霊" },
                { "surface": "元気", "reading": "げんき" },
                { "surface": "生", "reading": "せい" },
                { "surface": "生", "reading": "なま" }
            ]
        }))
        .unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].reading, None);
        assert_eq!(items[1].reading.as_deref(), Some("げんき"));
    }

    #[test]
    fn plan_rejects_empty_duplicate_and_invalid_items() {
        assert!(read_plan_items(&json!({ "items": [] })).is_err());
        assert!(read_plan_items(&json!({ "items": [{ "surface": " " }] })).is_err());
        assert!(
            read_plan_items(&json!({
                "items": [
                    { "surface": "元気", "reading": "げんき" },
                    { "surface": "元気", "reading": "げんき" }
                ]
            }))
            .is_err()
        );
        assert!(
            read_plan_items(&json!({ "items": [{ "surface": "元気", "reading": 7 }] })).is_err()
        );
        assert!(read_plan_items(&json!({ "items": [{ "surface": "元\n気" }] })).is_err());
    }

    #[test]
    fn cli_requires_plan_and_accepts_output() {
        assert!(Args::try_parse_from(["jpdb_pitch_acceptance"]).is_err());
        let args = Args::try_parse_from([
            "jpdb_pitch_acceptance",
            "--plan",
            "plan.json",
            "--output",
            "/tmp/report",
        ])
        .unwrap();
        assert_eq!(args.plan, PathBuf::from("plan.json"));
        assert_eq!(args.output, Some(PathBuf::from("/tmp/report")));
    }

    #[test]
    fn reports_must_be_new_and_outside_checkout() {
        let checkout = checkout_root().unwrap();
        let temp_root = std::env::temp_dir().canonicalize().unwrap();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = temp_root.join(format!(
            "jpdb-acceptance-test-{}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir(&parent).unwrap();
        let output = parent.join("report");
        let report_dir = create_report_dir(Some(&output)).unwrap();
        assert_eq!(report_dir, output);
        assert!(!report_dir.starts_with(&checkout));
        assert!(create_report_dir(Some(&output)).is_err());
        assert!(create_report_dir(Some(&checkout.join(".codex/local/report"))).is_err());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn non_acquired_rows_have_no_fake_image() {
        let item = PlanItem {
            surface: "存在しない語".into(),
            reading: None,
        };
        let rows = [json!({
            "surface": item.surface,
            "outcome": "vocabulary_not_found",
            "item_status": "vocabulary_not_found",
            "semantic_status": null,
        })];
        let temp_root = std::env::temp_dir().canonicalize().unwrap();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let report_dir = temp_root.join(format!(
            "jpdb-acceptance-html-test-{}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir(&report_dir).unwrap();
        save_html_report("test", &[item], &rows, &report_dir).unwrap();
        let html = fs::read_to_string(report_dir.join("index.html")).unwrap();
        assert!(html.contains("vocabulary_not_found"));
        assert!(!html.contains("<img"));
        assert!(!report_dir.join("images").exists());
        fs::remove_dir_all(report_dir).unwrap();
    }

    #[test]
    fn known_reference_dimensions_are_report_only_and_non_acquired_has_no_observed_geometry() {
        assert_eq!(reference_dimensions("くすぐったい"), Some([321, 84]));
        assert_eq!(reference_dimensions("クラブ"), Some([162, 87]));
        assert_eq!(reference_dimensions("乙女"), Some([177, 84]));
        assert_eq!(reference_dimensions("吹き抜ける"), Some([273, 84]));
        assert_eq!(reference_dimensions("幽霊"), None);

        let non_acquired = visual_reference("クラブ", "vocabulary_not_found", None).unwrap();
        assert_eq!(
            non_acquired["observed_native_png_dimensions_px"],
            Value::Null
        );
        assert_eq!(non_acquired["outcome"], "vocabulary_not_found");
        assert_eq!(non_acquired["comparison_status"], "not_acquired");
        let match_result = visual_reference("クラブ", "acquired", Some([162, 87])).unwrap();
        assert_eq!(match_result["comparison_status"], "match");
        let drift_result = visual_reference("クラブ", "acquired", Some([163, 87])).unwrap();
        assert_eq!(drift_result["comparison_status"], "source_drift");
    }
}
