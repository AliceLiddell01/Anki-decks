use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use asset_store::kanji_validator::KanjiImageValidator;
use asset_store::model::{
    AssetIdentity, AssetRecord, DetectedFormat, LifecycleState, Provenance, SemanticStatus,
};
use asset_store::validation::SemanticValidator;
use asset_store::yarxi::{AcquisitionTarget, SelectionResult, acquire_many_with_target};
use image::GenericImageView;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PLAN_PATH: &str = "tools/asset-store/yarxi-live-sample-plan.json";

#[derive(Debug, Clone)]
struct PlanItem {
    stratum: String,
    character: String,
    unicode_codepoint: String,
    article_number: u32,
    frequency_index: u32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let plan_bytes = fs::read(PLAN_PATH)?;
    let plan: Value = serde_json::from_slice(&plan_bytes)?;
    let items = read_plan_items(&plan)?;
    let report_dir = create_report_dir()?;
    fs::create_dir_all(report_dir.join("images"))?;

    let characters: Vec<_> = items.iter().map(|item| item.character.clone()).collect();
    let acquisitions =
        acquire_many_with_target(&characters, true, AcquisitionTarget::RenderedFontSamplePng)?;
    if acquisitions.len() != items.len() {
        return Err(format!(
            "provider returned {} outcomes for {} planned characters",
            acquisitions.len(),
            items.len()
        )
        .into());
    }

    let validator = KanjiImageValidator::new();
    let mut rows = Vec::with_capacity(items.len());
    let mut all_verified = true;

    for (item, acquisition) in items.iter().zip(acquisitions) {
        let media = match acquisition {
            Ok(media) => media,
            Err(error) => {
                all_verified = false;
                rows.push(json!({
                    "stratum": item.stratum,
                    "character": item.character,
                    "unicode_codepoint": item.unicode_codepoint,
                    "expected_article_number": item.article_number,
                    "expected_frequency_index": item.frequency_index,
                    "outcome": "acquisition_failed",
                    "failure": error,
                }));
                continue;
            }
        };

        let expected_code = item
            .unicode_codepoint
            .strip_prefix("U+")
            .unwrap_or(&item.unicode_codepoint);
        let metadata_matches = media.character == item.character
            && media.evidence.target == AcquisitionTarget::RenderedFontSamplePng
            && media.selection == SelectionResult::RenderedFontSamplePng
            && media.evidence.selection == SelectionResult::RenderedFontSamplePng
            && media.evidence.rendered_font_sample.is_some()
            && media
                .evidence
                .article_unicode
                .eq_ignore_ascii_case(expected_code)
            && media.evidence.article_number == Some(item.article_number)
            && media.evidence.frequency_index == Some(item.frequency_index)
            && media.source_url == "https://www.yarxi.su/";

        let png_signature_valid = media.bytes.starts_with(b"\x89PNG\r\n\x1a\n");
        let decoded = image::load_from_memory_with_format(&media.bytes, image::ImageFormat::Png);
        let dimensions = decoded.as_ref().ok().map(GenericImageView::dimensions);
        let sha256 = format!("{:x}", Sha256::digest(&media.bytes));
        let image_relative_path = format!("images/{}.png", item.character);
        if png_signature_valid && dimensions.is_some() {
            fs::write(report_dir.join(&image_relative_path), &media.bytes)?;
        }
        let provisional = AssetRecord {
            identity: AssetIdentity::new("kanji", item.character.clone())?,
            storage_path: format!("assets/{}.png", item.character),
            sha256: sha256.clone(),
            byte_length: media.bytes.len() as u64,
            format: DetectedFormat::from_signature(&media.bytes),
            provenance: Provenance {
                source_kind: media.evidence.provider.clone(),
                source_name: "yarxi-rendered-dark-font-sample.png".into(),
            },
            lifecycle: LifecycleState::Pending,
            validation: None,
            domain_metadata: None,
        };
        let decision = validator.validate(&provisional, &mut Cursor::new(&media.bytes));
        let decision = match decision {
            Ok(decision) => decision,
            Err(error) => {
                all_verified = false;
                rows.push(json!({
                    "stratum": item.stratum,
                    "character": item.character,
                    "unicode_codepoint": item.unicode_codepoint,
                    "expected_article_number": item.article_number,
                    "expected_frequency_index": item.frequency_index,
                    "outcome": "validator_failed",
                    "acquisition": media.evidence,
                    "source_url": media.source_url,
                    "sha256": sha256,
                    "byte_length": media.bytes.len(),
                    "dimensions": dimensions,
                    "failure": { "code": error.code, "message": error.message },
                    "image_path": image_relative_path,
                }));
                save_evidence("verification_failed", &items, &rows, &report_dir)?;
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
        rows.push(json!({
            "stratum": item.stratum,
            "character": item.character,
            "unicode_codepoint": item.unicode_codepoint,
            "expected_article_number": item.article_number,
            "expected_frequency_index": item.frequency_index,
            "outcome": if verified { "verified_candidate" } else { "candidate_rejected" },
            "acquisition": media.evidence,
            "source_url": media.source_url,
            "selection": media.selection,
            "sha256": sha256,
            "byte_length": media.bytes.len(),
            "png_signature_valid": png_signature_valid,
            "dimensions": dimensions,
            "metadata_matches_frozen_plan": metadata_matches,
            "semantic_status": status,
            "validation_evidence": decision.evidence,
            "image_path": image_relative_path,
            "failure": if verified { Value::Null } else { json!({
                "reason": "capture, article/frequency identity, or semantic verification did not pass"
            }) },
        }));
    }

    save_evidence(
        if all_verified {
            "automatic_checks_passed_waiting_for_user_review"
        } else {
            "verification_failed"
        },
        &items,
        &rows,
        &report_dir,
    )?;
    if !all_verified {
        let verified_count = rows
            .iter()
            .filter(|row| row["outcome"].as_str() == Some("verified_candidate"))
            .count();
        return Err(format!(
            "automatic checks passed for {verified_count}/{} candidates; report is in {}",
            items.len(),
            report_dir.join("index.html").display()
        )
        .into());
    }

    println!(
        "automatic checks passed for {} rendered dark PNGs in five frozen strata; assets were not published; report={}",
        rows.iter()
            .filter(|row| row["outcome"].as_str() == Some("verified_candidate"))
            .count(),
        report_dir.join("index.html").display()
    );
    Ok(())
}

fn create_report_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let checkout = std::env::current_dir()?.canonicalize()?;
    let temp_root = std::env::temp_dir().canonicalize()?;
    if temp_root.starts_with(&checkout) {
        return Err("system temporary directory is inside the repository checkout".into());
    }
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let report_dir = temp_root.join(format!(
        "yarxi-live-acceptance-{}-{timestamp}",
        std::process::id()
    ));
    fs::create_dir(&report_dir)?;
    Ok(report_dir)
}

fn read_plan_items(plan: &Value) -> Result<Vec<PlanItem>, Box<dyn std::error::Error>> {
    let strata = plan["strata"]
        .as_array()
        .ok_or("plan.strata is not an array")?;
    if strata.len() != 5 {
        return Err(format!("expected five frozen strata, found {}", strata.len()).into());
    }
    let mut items = Vec::new();
    let mut seen = BTreeSet::new();
    for stratum in strata {
        let stratum_id = stratum["id"].as_str().ok_or("stratum.id is not a string")?;
        let verified = stratum["sample_characters"]
            .as_array()
            .ok_or("stratum.sample_characters is not an array")?;
        if verified.len() != 10 {
            return Err(format!("{stratum_id} does not contain ten frozen characters").into());
        }
        let candidates = stratum["candidates"]
            .as_array()
            .ok_or("stratum.candidates is not an array")?;
        for character in verified {
            let character = character
                .as_str()
                .ok_or("sample character is not a string")?;
            if character.chars().count() != 1 || !seen.insert(character.to_owned()) {
                return Err(format!("invalid or duplicate planned character {character}").into());
            }
            let candidate = candidates
                .iter()
                .find(|candidate| candidate["character"].as_str() == Some(character))
                .ok_or_else(|| format!("candidate metadata missing for {character}"))?;
            items.push(PlanItem {
                stratum: stratum_id.to_owned(),
                character: character.to_owned(),
                unicode_codepoint: candidate["unicode_codepoint"]
                    .as_str()
                    .ok_or("candidate unicode_codepoint is not a string")?
                    .to_owned(),
                article_number: candidate["yarxi_article_number"]
                    .as_u64()
                    .ok_or("candidate article number is not an integer")?
                    as u32,
                frequency_index: candidate["frequency_index"]
                    .as_u64()
                    .ok_or("candidate frequency index is not an integer")?
                    as u32,
            });
        }
    }
    if items.len() != 50 {
        return Err(format!(
            "expected 50 unique frozen characters, found {}",
            items.len()
        )
        .into());
    }
    Ok(items)
}

fn save_evidence(
    run_status: &str,
    items: &[PlanItem],
    rows: &[Value],
    report_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = report_dir.join("evidence.json");
    let report = json!({
        "schema_version": 1,
        "run_status": run_status,
        "plan_path": PLAN_PATH,
        "planned_item_count": items.len(),
        "strata": items.iter().fold(BTreeMap::<String, Vec<String>>::new(), |mut map, item| {
            map.entry(item.stratum.clone()).or_default().push(item.character.clone());
            map
        }),
        "validator": KanjiImageValidator::validator_identity(),
        "capture_contract": {
            "target": "rendered_font_sample_png",
            "viewport": [1280, 900],
            "device_scale_factor": 2.175,
            "pixel_edge_bounds": [140, 220],
        },
        "items": rows,
    });
    fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    save_html_report(run_status, items, rows, report_dir)?;
    Ok(())
}

fn save_html_report(
    run_status: &str,
    items: &[PlanItem],
    rows: &[Value],
    report_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut html = String::from(
        r##"<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Yarxi — dark PNG acceptance</title>
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
<h1>Yarxi — проверка 50 тёмных PNG</h1>
<div class="summary">
<p><strong>Статус прогона:</strong> __RUN_STATUS__</p>
<p>Пять замороженных frequency strata, по 10 символов. PNG показываются в естественном размере без растягивания; если плитка выходит за карточку, прокрутите область изображения. Фактические размеры указаны рядом.</p>
<p><a href="evidence.json">Машиночитаемый evidence (JSON)</a></p>
</div>
"##,
    );
    html = html.replace("__RUN_STATUS__", &escape_html(run_status));

    for stratum_id in [
        "rank_1_100",
        "rank_101_250",
        "rank_251_500",
        "rank_501_1000",
        "rank_1001_plus",
    ] {
        let group: Vec<_> = items
            .iter()
            .filter(|item| item.stratum == stratum_id)
            .collect();
        html.push_str(&format!(
            "<section><h2>{} — {} PNG</h2><div class=\"items\">",
            escape_html(stratum_id),
            group.len()
        ));
        for item in group {
            let row = rows
                .iter()
                .find(|row| row["character"].as_str() == Some(&item.character));
            let title = format!("{} {}", item.character, item.unicode_codepoint);
            html.push_str(&format!(
                "<article><h3><span>{}</span><small>{}</small></h3>",
                escape_html(&item.character),
                escape_html(&item.unicode_codepoint)
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
                html.push_str("<p>PNG не сохранён: acquisition ещё не завершился.</p>");
            }

            let row = row.cloned().unwrap_or(Value::Null);
            let sample = &row["acquisition"]["rendered_font_sample"];
            let dimensions = row["dimensions"]
                .as_array()
                .filter(|dimensions| dimensions.len() == 2)
                .map(|dimensions| format!("{} × {} px", dimensions[0], dimensions[1]))
                .unwrap_or_else(|| "—".to_owned());
            let viewport =
                if sample["viewport_width"].is_number() && sample["viewport_height"].is_number() {
                    format!(
                        "{} × {} CSS px",
                        sample["viewport_width"], sample["viewport_height"]
                    )
                } else {
                    "—".to_owned()
                };
            let css_rect = serde_json::to_string(&sample["css_rect"])?;
            let semantic_summary = row["validation_evidence"][0]["summary"]
                .as_str()
                .unwrap_or("—");
            let source_url = row["source_url"].as_str().unwrap_or("—");
            let fields = [
                ("Статья Yarxi", item.article_number.to_string()),
                ("Индекс частотности", item.frequency_index.to_string()),
                (
                    "Source / article",
                    format!("{} / {}", source_url, item.article_number),
                ),
                (
                    "Tile",
                    format!(
                        "{} · {} · {}",
                        sample["selector"].as_str().unwrap_or("—"),
                        sample["class_name"].as_str().unwrap_or("—"),
                        sample["title"].as_str().unwrap_or("—")
                    ),
                ),
                (
                    "Capture",
                    sample["capture"].as_str().unwrap_or("—").to_owned(),
                ),
                ("CSS rect", css_rect),
                ("Размер PNG", dimensions),
                ("Viewport", viewport),
                ("Device scale", sample["device_scale_factor"].to_string()),
                (
                    "Theme",
                    sample["dark_environment"]
                        .as_str()
                        .unwrap_or("—")
                        .to_owned(),
                ),
                (
                    "Font",
                    format!(
                        "{} {}",
                        sample["font_family"].as_str().unwrap_or("—"),
                        sample["font_size"].as_str().unwrap_or("")
                    ),
                ),
                ("SHA-256", row["sha256"].as_str().unwrap_or("—").to_owned()),
                (
                    "Outcome",
                    format!(
                        "{} / {}",
                        row["outcome"].as_str().unwrap_or("pending"),
                        row["semantic_status"].as_str().unwrap_or("—")
                    ),
                ),
                ("Pixel evidence", semantic_summary.to_owned()),
            ];
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

    #[test]
    fn frozen_sample_parser_uses_candidate_metadata() {
        let strata = (0..5)
            .map(|stratum_index| {
                let candidates: Vec<_> = (0..10)
                    .map(|candidate_index| {
                        let character = char::from_u32(
                            0x4e00 + (stratum_index * 10 + candidate_index) as u32,
                        )
                        .unwrap()
                        .to_string();
                        json!({
                            "character": character,
                            "unicode_codepoint": format!("U+{:04X}", character.chars().next().unwrap() as u32),
                            "yarxi_article_number": stratum_index * 10 + candidate_index + 1,
                            "frequency_index": stratum_index * 10 + candidate_index + 1,
                        })
                    })
                    .collect();
                let sample_characters: Vec<_> = candidates
                    .iter()
                    .map(|candidate| candidate["character"].clone())
                    .collect();
                json!({
                    "id": format!("stratum_{stratum_index}"),
                    "sample_characters": sample_characters,
                    "candidates": candidates,
                })
            })
            .collect::<Vec<_>>();
        let items = read_plan_items(&json!({ "strata": strata })).unwrap();

        assert_eq!(items.len(), 50);
        assert_eq!(items[0].article_number, 1);
        assert_eq!(items[49].frequency_index, 50);
        assert_eq!(
            items
                .iter()
                .map(|item| &item.character)
                .collect::<BTreeSet<_>>()
                .len(),
            50
        );
    }

    #[test]
    fn report_directory_is_outside_the_repository() {
        let report_dir = create_report_dir().unwrap();
        let checkout = std::env::current_dir().unwrap().canonicalize().unwrap();

        assert!(!report_dir.starts_with(checkout));
        assert!(report_dir.is_dir());
        fs::remove_dir(report_dir).unwrap();
    }
}
