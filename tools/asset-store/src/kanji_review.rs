//! Производный текстовый пакет проверки. Источник истины — KanjiBatch;
//! редактируемый TSV никогда не исполняется и не сканируется по маске файлов.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::batch::{
    BatchCandidate, BatchRuntime, HumanBatchAction, HumanBatchDecision, KanjiBatch,
};
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::kanji_domain::parse_kanji_character;
use crate::model::{AssetIdentity, ValidatorIdentity};

const SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewRow {
    pub identity: AssetIdentity,
    pub generation: u32,
    pub attempts: usize,
    pub sha256: String,
    pub candidate_path: String,
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr: Option<crate::kanji_ocr::OcrSuggestion>,
}

impl ReviewRow {
    fn columns(&self) -> String {
        let mut columns = format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            self.identity.key,
            self.filename,
            self.sha256,
            self.generation,
            self.attempts,
            self.candidate_path
        );
        if let Some(ocr) = &self.ocr {
            columns.push_str(&format!(
                "\t{}\t{}\t{}\t{}",
                ocr.observed.as_deref().unwrap_or("-"),
                ocr.matches_expected,
                ocr.failure.as_deref().unwrap_or("-"),
                ocr.key()
            ));
        }
        columns
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewSnapshot {
    pub schema_version: u32,
    pub batch_id: String,
    pub validator: ValidatorIdentity,
    pub rows: Vec<ReviewRow>,
}

pub(crate) struct ReviewBundle {
    pub id: String,
    pub directory: PathBuf,
    pub decisions: PathBuf,
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

pub(crate) fn candidate<'a>(
    state: &'a KanjiBatch,
    row: &ReviewRow,
) -> Result<&'a BatchCandidate, AssetError> {
    let item = state
        .items
        .iter()
        .find(|item| item.identity == row.identity)
        .ok_or_else(|| invalid("строка review не принадлежит пакету"))?;
    item.attempts
        .iter()
        .rev()
        .find_map(|attempt| match &attempt.result {
            crate::batch::BatchAttemptInput::Candidate { candidate }
                if candidate.sha256 == row.sha256 =>
            {
                Some(candidate)
            }
            _ => None,
        })
        .or_else(|| {
            item.existing_candidate
                .as_ref()
                .filter(|candidate| candidate.sha256 == row.sha256)
        })
        .filter(|candidate| candidate.storage_path == row.candidate_path)
        .ok_or_else(|| invalid("кандидат review отсутствует или заменён"))
}

pub(crate) fn prepare(
    runtime: &BatchRuntime,
    state: &KanjiBatch,
) -> Result<ReviewBundle, AssetError> {
    state.validate()?;
    let mut rows = Vec::new();
    let suggestions = crate::kanji_ocr::saved_suggestions(runtime)?;
    for item in state.review_queue() {
        let Some(hash) = &item.current_sha256 else {
            continue;
        };
        let mut row = ReviewRow {
            identity: item.identity.clone(),
            generation: item.generation,
            attempts: item.attempts.len(),
            sha256: hash.clone(),
            candidate_path: String::new(),
            filename: String::new(),
            ocr: suggestions
                .get(&item.identity)
                .filter(|ocr| &ocr.source_sha256 == hash)
                .cloned(),
        };
        let selected = item
            .attempts
            .iter()
            .rev()
            .find_map(|attempt| match &attempt.result {
                crate::batch::BatchAttemptInput::Candidate { candidate }
                    if &candidate.sha256 == hash =>
                {
                    Some(candidate)
                }
                _ => None,
            })
            .ok_or_else(|| invalid("нет текущего кандидата для review"))?;
        row.candidate_path = selected.storage_path.clone();
        row.filename = format!(
            "{}-{}.{}",
            item.identity.key,
            hash,
            crate::domain::extension_for_format(selected.format)
        );
        rows.push(row);
    }
    if rows.is_empty() {
        return Err(invalid("нет кандидатов, ожидающих решения человека"));
    }
    rows.sort_by_key(|row| row.ocr.as_ref().map_or(3, |ocr| ocr.group()));
    let snapshot = ReviewSnapshot {
        schema_version: SCHEMA,
        batch_id: state.batch_id.clone(),
        validator: state.policy.validator.clone(),
        rows,
    };
    let encoded = serde_json::to_vec(&snapshot).map_err(|error| invalid(error.to_string()))?;
    let id = sha256_hex(&encoded);
    let directory = runtime.artifact_directory(&format!("review-{id}"))?;
    // Повторный export не перезаписывает уже заполненный файл редактора.
    match directory.read("snapshot.json") {
        Ok(existing) if existing == encoded => {
            for row in &snapshot.rows {
                match directory.read(&row.filename) {
                    Ok(bytes) if sha256_hex(&bytes) == row.sha256 => {}
                    Ok(_) => return Err(invalid("копия изображения review изменена")),
                    Err(error) if error.code == ErrorCode::MissingAssetFile => {
                        let bytes = runtime.read_candidate(candidate(state, row)?)?;
                        if sha256_hex(&bytes) != row.sha256 {
                            return Err(invalid("исходный кандидат review изменён"));
                        }
                        directory.write_if_absent(&row.filename, &bytes)?;
                        if sha256_hex(&directory.read(&row.filename)?) != row.sha256 {
                            return Err(invalid("копия изображения review изменена"));
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            match directory.read("decisions.tsv") {
                Ok(_) => {}
                Err(error) if error.code == ErrorCode::MissingAssetFile => {
                    directory.write_if_absent(
                        "decisions.tsv",
                        decisions_template(&snapshot, &id).as_bytes(),
                    )?;
                    directory.read("decisions.tsv")?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(_) => return Err(invalid("содержимое снимка review не совпадает с его SHA")),
        Err(error) if error.code == ErrorCode::MissingAssetFile => {
            for row in &snapshot.rows {
                let bytes = runtime.read_candidate(candidate(state, row)?)?;
                directory.write(&row.filename, &bytes)?;
            }
            directory.write(
                "decisions.tsv",
                decisions_template(&snapshot, &id).as_bytes(),
            )?;
            // Снимок записывается последним: неполный export безопасно повторяется.
            directory.write("snapshot.json", &encoded)?;
        }
        Err(error) => return Err(error),
    }
    write_preview(&directory, &snapshot)?;
    Ok(ReviewBundle {
        id,
        decisions: directory.path.join("decisions.tsv"),
        directory: directory.path,
    })
}

fn decisions_template(snapshot: &ReviewSnapshot, id: &str) -> String {
    let mut text = format!(
        "# YARXI REVIEW v{SCHEMA}\n# y = ожидаемый символ; e = непригодное изображение; другой один Han = фактический символ; - = пропуск\n# Только первая колонка; разделитель TAB. Сохраните файл перед применением.\n@review\t{SCHEMA}\t{}\t{id}\n",
        snapshot.batch_id
    );
    for row in &snapshot.rows {
        text.push_str(&format!("-\t{}\n", row.columns()));
    }
    text
}

pub(crate) fn read(
    runtime: &BatchRuntime,
    id: &str,
) -> Result<(ReviewSnapshot, Vec<Option<HumanBatchDecision>>), AssetError> {
    crate::batch::validate_hash(id)?;
    let directory = runtime.artifact_directory(&format!("review-{id}"))?;
    let encoded = directory.read("snapshot.json")?;
    if sha256_hex(&encoded) != id {
        return Err(invalid("SHA снимка review не совпадает"));
    }
    let snapshot: ReviewSnapshot =
        serde_json::from_slice(&encoded).map_err(|error| invalid(error.to_string()))?;
    if snapshot.schema_version != SCHEMA || snapshot.batch_id != runtime.batch_id() {
        return Err(invalid("неверная схема или batch_id файла review"));
    }
    for row in &snapshot.rows {
        if row
            .ocr
            .as_ref()
            .is_some_and(|ocr| ocr.identity != row.identity || ocr.source_sha256 != row.sha256)
        {
            return Err(invalid(
                "предложение OCR не принадлежит точным байтам review",
            ));
        }
        if sha256_hex(&directory.read(&row.filename)?) != row.sha256 {
            return Err(invalid("SHA копии изображения review не совпадает"));
        }
    }
    let bytes = directory.read("decisions.tsv")?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| invalid("файл решений должен быть UTF-8"))?;
    let decisions = parse(&snapshot, id, text)?;
    Ok((snapshot, decisions))
}

fn parse(
    snapshot: &ReviewSnapshot,
    id: &str,
    text: &str,
) -> Result<Vec<Option<HumanBatchDecision>>, AssetError> {
    let mut lines = text
        .trim_start_matches('\u{feff}')
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'));
    if lines.next() != Some(format!("@review\t{SCHEMA}\t{}\t{id}", snapshot.batch_id).as_str()) {
        return Err(invalid("изменена подпись, схема или идентичность review"));
    }
    let mut decisions = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, row) in snapshot.rows.iter().enumerate() {
        if !seen.insert(&row.identity) {
            return Err(invalid("повторяющийся Unicode в review"));
        }
        let line = lines
            .next()
            .ok_or_else(|| invalid("в review отсутствуют строки"))?;
        let (action, columns) = line
            .split_once('\t')
            .ok_or_else(|| invalid("в review требуется TAB"))?;
        if columns != row.columns() {
            return Err(invalid(format!("изменены метаданные строки {}", index + 1)));
        }
        let (action, reason) = match action {
            "" | "-" => {
                decisions.push(None);
                continue;
            }
            "y" => (
                HumanBatchAction::Confirm,
                "expected_character_confirmed".to_owned(),
            ),
            "e" => (
                HumanBatchAction::RejectAndHold,
                "unusable_image; explicit retry required".to_owned(),
            ),
            other => {
                let character = parse_kanji_character(other)
                    .map_err(|_| invalid("решение: y, e, -, либо один другой Han"))?;
                if other == row.identity.key {
                    return Err(invalid(
                        "для подтверждения ожидаемого Unicode используйте y",
                    ));
                }
                (
                    HumanBatchAction::RejectAndHold,
                    format!("actual_character={character}; explicit retry required"),
                )
            }
        };
        decisions.push(Some(HumanBatchDecision {
            identity: row.identity.clone(),
            candidate_sha256: row.sha256.clone(),
            action,
            reason: format!("review:{id}; {reason}"),
        }));
    }
    if lines.next().is_some() {
        return Err(invalid("лишние или повторяющиеся строки review"));
    }
    Ok(decisions)
}

fn write_preview(
    directory: &crate::batch_runtime::ArtifactDirectory,
    snapshot: &ReviewSnapshot,
) -> Result<(), AssetError> {
    use crate::batch::html_escape;
    let mut html = String::from(
        "<!doctype html><html lang=\"ru\"><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src 'self'; style-src 'unsafe-inline'\"><title>Ручная проверка</title><style>body{font-family:system-ui;margin:2rem}article{border:1px solid #999;padding:1rem;margin:1rem 0}img{max-width:300px}pre{white-space:pre-wrap;overflow-wrap:anywhere}</style><h1>Предложения OCR и решения человека</h1><p>Просмотрите изображения. OCR может ошибаться при совпадении метки. Только явное решение человека подтверждает байты.</p>",
    );
    let mut group = None;
    for row in &snapshot.rows {
        let next_group = row.ocr.as_ref().map_or(3, |ocr| ocr.group());
        if group != Some(next_group) {
            html.push_str(match next_group {
                0 => "<h2>Совпадения OCR — требуют просмотра</h2>",
                1 => "<h2>Несовпадения Unicode</h2>",
                2 => "<h2>Не распознано / отказ</h2>",
                _ => "<h2>Без OCR</h2>",
            });
            group = Some(next_group);
        }
        html.push_str(&format!("<article><h3>{}</h3><img src=\"{}\" alt=\"{}\"><pre>SHA: {}\nПоколение: {}\n{}</pre></article>",
            html_escape(&row.identity.key), html_escape(&row.filename), html_escape(&row.identity.key), row.sha256, row.generation,
            html_escape(&row.ocr.as_ref().map_or_else(|| "Без OCR".into(), |ocr| ocr.summary()))));
    }
    html.push_str("</html>");
    directory.write("index.html", html.as_bytes())
}

#[cfg(test)]
#[path = "kanji_review_tests.rs"]
mod tests;
