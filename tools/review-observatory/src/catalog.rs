//! Индекс наблюдения обновляется только в явно запущенном HTTP-процессе.
use crate::{
    dto::{Availability, Envelope, Provenance, Query},
    files::{self, MAX_JSON_BYTES, MAX_SMALL_BYTES, SafeRoot},
};
use anki_repo::code_review::{
    execution,
    learning::{
        self, LearningStore, StoreOptions,
        patterns::{self, PatternQuery},
    },
    model::{CandidateEvidence, REVIEW_SCHEMA_VERSION, ReviewPack},
    review_queue::{self, ReviewQueue, ReviewUnit},
    semantic_triage::{self, SemanticTriage},
};
use anki_repo::error::ErrorCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub fn value<T: Serialize>(v: T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}
pub fn label<T: Serialize>(v: T) -> String {
    value(v).as_str().unwrap_or("unknown").into()
}
pub fn priority_rank(priority: Option<&str>) -> u8 {
    match priority {
        Some("high") => 0,
        Some("normal") => 1,
        Some("low") => 2,
        _ => 3,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub id: String,
    pub namespace: String,
    pub head_sha: String,
    pub base_sha: Option<String>,
    pub workspace_variant: String,
    pub updated_unix_ms: Option<u64>,
    pub candidate_count: Option<usize>,
    pub unit_count: Option<usize>,
    pub finding_count: Option<usize>,
    pub coverage: Option<Value>,
    pub queue_summary: Option<Value>,
    pub artifacts: BTreeMap<String, Availability>,
    pub state: Availability,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateRow {
    pub id: String,
    pub detector: String,
    pub path: String,
    pub line: Option<usize>,
    pub origin: String,
    pub unit_id: Option<String>,
    pub priority: Option<String>,
    pub role: Option<String>,
    pub unknown: Option<bool>,
    pub surfaces: Option<Vec<String>>,
    pub disposition: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnitSummary {
    pub id: String,
    pub kind: String,
    pub priority: String,
    pub detector: String,
    pub role: String,
    pub candidate_count: usize,
    pub representative_candidate_ids: Vec<String>,
    pub unknown: bool,
    pub classification: Value,
}
pub fn unit_summary(unit: &ReviewUnit) -> UnitSummary {
    UnitSummary {
        id: unit.id.clone(),
        kind: if unit.is_group() {
            "group"
        } else {
            "individual"
        }
        .into(),
        priority: label(unit.priority),
        detector: unit.signature.detector.clone(),
        role: label(unit.signature.classification.role),
        candidate_count: unit.candidate_ids().len(),
        representative_candidate_ids: unit.representative_candidate_ids().to_vec(),
        unknown: unit.signature.classification.is_unknown(),
        classification: value(&unit.signature.classification),
    }
}

#[derive(Clone)]
pub struct ReviewData {
    pub directory: PathBuf,
    pub stamps: String,
    pub summary: ReviewSummary,
    pub provenance: Vec<Provenance>,
    pub diagnostics: Vec<String>,
    pub pack: Option<ReviewPack>,
    pub queue: Option<ReviewQueue>,
    pub triage: Option<SemanticTriage>,
    pub candidates: Vec<CandidateEvidence>,
    pub candidate_index: BTreeMap<String, usize>,
    pub unit_by_candidate: BTreeMap<String, usize>,
    pub unit_index: BTreeMap<String, usize>,
    pub decisions: BTreeMap<String, Value>,
    pub report: Option<String>,
    pub recommendations: Option<Value>,
}
impl ReviewData {
    pub fn row(&self, c: &CandidateEvidence) -> CandidateRow {
        let unit = self
            .unit_by_candidate
            .get(&c.id)
            .and_then(|i| self.queue.as_ref()?.units.get(*i));
        let classification = self
            .queue
            .as_ref()
            .and_then(|q| q.classifications.get(&c.id));
        CandidateRow {
            id: c.id.clone(),
            detector: c.detector.clone(),
            path: c.path.clone(),
            line: c.line,
            origin: c.origin.as_str().into(),
            unit_id: unit.map(|u| u.id.clone()),
            priority: unit.map(|u| label(u.priority)),
            role: classification.map(|c| label(c.role)),
            unknown: classification.map(|c| c.is_unknown()),
            surfaces: classification.map(|c| c.surfaces.iter().map(label).collect()),
            disposition: self
                .decisions
                .get(&c.id)
                .and_then(|d| d["disposition"].as_str())
                .map(str::to_string),
        }
    }
    pub fn candidate_rows(&self, q: &Query) -> Vec<CandidateRow> {
        let mut rows: Vec<_> = self
            .candidates
            .iter()
            .map(|c| self.row(c))
            .filter(|r| {
                q.matches(&format!("{} {} {}", r.id, r.path, r.detector))
                    && q.detector.as_ref().is_none_or(|v| v == &r.detector)
                    && q.priority
                        .as_ref()
                        .is_none_or(|v| Some(v) == r.priority.as_ref())
                    && q.role.as_ref().is_none_or(|v| Some(v) == r.role.as_ref())
                    && q.unit_id
                        .as_ref()
                        .is_none_or(|v| Some(v) == r.unit_id.as_ref())
                    && q.disposition.as_ref().is_none_or(|v| {
                        if v == "unreviewed" {
                            r.disposition.is_none()
                        } else {
                            Some(v) == r.disposition.as_ref()
                        }
                    })
                    && q.unknown.is_none_or(|v| r.unknown == Some(v))
                    && q.surface.as_ref().is_none_or(|surface| {
                        self.queue
                            .as_ref()
                            .and_then(|q| q.classifications.get(&r.id))
                            .is_some_and(|c| c.surfaces.iter().any(|s| label(s) == *surface))
                    })
            })
            .collect();
        match q.sort.as_deref().unwrap_or("priority") {
            "path" => rows.sort_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id))),
            "detector" => rows.sort_by(|a, b| a.detector.cmp(&b.detector).then(a.id.cmp(&b.id))),
            "id" => rows.sort_by(|a, b| a.id.cmp(&b.id)),
            _ => rows.sort_by(|a, b| {
                priority_rank(a.priority.as_deref())
                    .cmp(&priority_rank(b.priority.as_deref()))
                    .then(a.id.cmp(&b.id))
            }),
        };
        rows
    }
    pub fn envelope<T>(&self, data: T) -> Envelope<T> {
        Envelope {
            api_version: 1,
            state: self.summary.state,
            data: Some(data),
            provenance: self.provenance.clone(),
            diagnostics: self.diagnostics.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Health {
    pub components: Vec<Value>,
    pub read_only: bool,
    pub poll_interval_ms: u64,
    pub limitations: Vec<String>,
}
#[derive(Clone)]
pub struct Catalog {
    pub revision: u64,
    pub observed_unix_ms: u64,
    pub reviews: Vec<Arc<ReviewData>>,
    pub jobs: Vec<Value>,
    pub learning: Envelope<Value>,
    pub learning_collections: BTreeMap<String, Vec<Value>>,
    pub health: Health,
    pub fingerprint: String,
}
impl Default for Catalog {
    fn default() -> Self {
        Self {
            revision: 0,
            observed_unix_ms: now_ms(),
            reviews: Vec::new(),
            jobs: Vec::new(),
            learning: Envelope::absent(
                Availability::NotObserved,
                "Первичный снимок ещё не загружен",
            ),
            learning_collections: BTreeMap::new(),
            health: Health {
                components: Vec::new(),
                read_only: true,
                poll_interval_ms: 2000,
                limitations: Vec::new(),
            },
            fingerprint: String::new(),
        }
    }
}

const DOCUMENTS: &[&str] = &[
    "review.json",
    "review-queue.json",
    "semantic-triage.json",
    "review-report.md",
    "recommendations.json",
];
fn read_document<T: serde::de::DeserializeOwned>(
    root: &SafeRoot,
    dir: &Path,
    name: &str,
    provenance: &mut Vec<Provenance>,
    states: &mut BTreeMap<String, Availability>,
    diagnostics: &mut Vec<String>,
) -> Option<T> {
    let key = match name {
        "review.json" => "review",
        "review-queue.json" => "queue",
        "semantic-triage.json" => "triage",
        "review-report.md" => "report",
        _ => "recommendations",
    };
    let path = dir.join(name);
    match std::fs::symlink_metadata(root.path.join(&path)) {
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            states.insert(key.into(), Availability::Missing);
            return None;
        }
        Err(error) => {
            states.insert(key.into(), Availability::Unavailable);
            diagnostics.push(format!("{}: metadata недоступны: {error}", path.display()));
            return None;
        }
    }
    match root.read(&path, MAX_JSON_BYTES) {
        Ok((bytes, source)) => {
            provenance.push(source);
            match serde_json::from_slice(&bytes) {
                Ok(doc) => {
                    states.insert(key.into(), Availability::Available);
                    Some(doc)
                }
                Err(_) => {
                    states.insert(key.into(), Availability::Invalid);
                    diagnostics.push(format!(
                        "{}: невалидный JSON или неподдерживаемый формат",
                        path.display()
                    ));
                    None
                }
            }
        }
        Err(error) => {
            states.insert(key.into(), Availability::Unavailable);
            diagnostics.push(format!("{}: {error}", path.display()));
            None
        }
    }
}
fn invalidate(
    states: &mut BTreeMap<String, Availability>,
    diagnostics: &mut Vec<String>,
    key: &str,
    message: String,
) {
    states.insert(key.into(), Availability::Invalid);
    diagnostics.push(message);
}
fn load_review(root: &SafeRoot, dir: &Path, stamps: String) -> ReviewData {
    let mut provenance = Vec::new();
    let mut states = BTreeMap::new();
    let mut diagnostics = Vec::new();
    let mut pack: Option<ReviewPack> = read_document(
        root,
        dir,
        "review.json",
        &mut provenance,
        &mut states,
        &mut diagnostics,
    );
    let mut queue: Option<ReviewQueue> = read_document(
        root,
        dir,
        "review-queue.json",
        &mut provenance,
        &mut states,
        &mut diagnostics,
    );
    let mut triage: Option<SemanticTriage> = read_document(
        root,
        dir,
        "semantic-triage.json",
        &mut provenance,
        &mut states,
        &mut diagnostics,
    );
    let mut recommendations: Option<learning::Recommendations> = read_document(
        root,
        dir,
        "recommendations.json",
        &mut provenance,
        &mut states,
        &mut diagnostics,
    );
    let parts: Vec<_> = dir
        .components()
        .map(|p| p.as_os_str().to_string_lossy().to_string())
        .collect();
    let namespace = parts[2].clone();
    let head = parts[3].clone();
    if let Some(p) = &pack {
        let candidates = p.all_candidates();
        let ids: BTreeSet<_> = candidates.iter().map(|c| &c.id).collect();
        let digest = provenance
            .iter()
            .find(|p| p.path.ends_with("/review.json"))
            .and_then(|p| p.sha256.as_deref())
            .unwrap_or("");
        let valid = semantic_triage::validate_source(
            &semantic_triage::source_identity(p, digest),
            p,
            digest,
        )
        .is_ok();
        if p.schema_version != REVIEW_SCHEMA_VERSION
            || p.target.head_sha != head
            || ids.len() != candidates.len()
            || !valid
        {
            invalidate(
                &mut states,
                &mut diagnostics,
                "review",
                "ReviewPack: версия, HEAD или идентификаторы невалидны".into(),
            );
            pack = None;
        }
    }
    if let Some(p) = &pack {
        let digest = provenance
            .iter()
            .find(|p| p.path.ends_with("/review.json"))
            .and_then(|p| p.sha256.as_deref())
            .unwrap_or("");
        if let Some(q) = &queue
            && let Err(error) = review_queue::validate(q, p, digest)
        {
            invalidate(&mut states, &mut diagnostics, "queue", error.to_string());
            queue = None;
        }
        if let Some(t) = &triage
            && let Err(error) = semantic_triage::validate(t, p, digest)
        {
            invalidate(&mut states, &mut diagnostics, "triage", error.to_string());
            triage = None;
        }
    } else {
        if queue.is_some() {
            states.insert("queue".into(), Availability::Incomplete);
        }
        if triage.is_some() {
            states.insert("triage".into(), Availability::Incomplete);
        }
        queue = None;
        triage = None;
    }
    if let Some(document) = &recommendations {
        let expected_review = provenance
            .iter()
            .find(|p| p.path.ends_with("/review.json"))
            .and_then(|p| p.sha256.as_ref());
        let expected_queue = provenance
            .iter()
            .find(|p| p.path.ends_with("/review-queue.json"))
            .and_then(|p| p.sha256.as_ref());
        if document.schema_version != learning::recommend::RECOMMENDATIONS_SCHEMA_VERSION
            || Some(&document.review_pack_sha256) != expected_review
            || Some(&document.queue_sha256) != expected_queue
            || document.head_sha != head
        {
            invalidate(
                &mut states,
                &mut diagnostics,
                "recommendations",
                "recommendations.json: версия или точные digest не соответствуют источникам".into(),
            );
            recommendations = None;
        }
    }
    let recommendations = recommendations.map(value);
    let report = match root.read(&dir.join("review-report.md"), MAX_SMALL_BYTES) {
        Ok((bytes, source)) => {
            provenance.push(source);
            match String::from_utf8(bytes) {
                Ok(report) => {
                    states.insert("report".into(), Availability::Available);
                    Some(report)
                }
                Err(_) => {
                    invalidate(
                        &mut states,
                        &mut diagnostics,
                        "report",
                        "review-report.md: невалидный UTF-8".into(),
                    );
                    None
                }
            }
        }
        Err(error) => {
            let missing = std::fs::symlink_metadata(root.path.join(dir).join("review-report.md"))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
            states.insert(
                "report".into(),
                if missing {
                    Availability::Missing
                } else {
                    Availability::Unavailable
                },
            );
            if !missing {
                diagnostics.push(format!("review-report.md: {error}"));
            }
            None
        }
    };
    let candidates = pack
        .as_ref()
        .map(|p| p.all_candidates())
        .unwrap_or_default();
    let candidate_index = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.clone(), i))
        .collect();
    let mut unit_by_candidate = BTreeMap::new();
    let mut unit_index = BTreeMap::new();
    if let Some(q) = &queue {
        for (i, u) in q.units.iter().enumerate() {
            unit_index.insert(u.id.clone(), i);
            for id in u.candidate_ids() {
                unit_by_candidate.insert(id.clone(), i);
            }
        }
    }
    let mut decisions = BTreeMap::new();
    if let Some(t) = &triage {
        for d in &t.individual_decisions {
            decisions.insert(d.candidate_id.clone(), value(d));
        }
        for d in &t.group_decisions {
            let compact = json!({"id":d.id,"kind":"group","candidate_count":d.candidate_ids.len(),"representative_candidate_ids":d.representative_candidate_ids,"disposition":d.disposition,"reason_code":d.reason_code,"explanation":d.explanation,"finding_ids":d.finding_ids});
            for id in &d.candidate_ids {
                decisions.insert(id.clone(), compact.clone());
            }
        }
    }
    let coverage = triage
        .as_ref()
        .zip(pack.as_ref())
        .map(|(t, p)| value(semantic_triage::summarize(t, p)));
    let state = if ["review", "queue", "triage"]
        .iter()
        .any(|name| states.get(*name) == Some(&Availability::Invalid))
    {
        Availability::Invalid
    } else if pack.is_none() {
        *states.get("review").unwrap_or(&Availability::Missing)
    } else if queue.is_none()
        || triage.is_none()
        || ["report", "recommendations"].iter().any(|name| {
            states
                .get(*name)
                .is_some_and(|s| matches!(s, Availability::Invalid | Availability::Unavailable))
        })
    {
        Availability::Incomplete
    } else {
        Availability::Available
    };
    let summary = ReviewSummary {
        id: files::opaque_id(dir),
        namespace,
        head_sha: head,
        base_sha: pack.as_ref().map(|p| p.target.base_sha.clone()),
        workspace_variant: if parts.len() > 4 {
            parts[4].clone()
        } else {
            "root".into()
        },
        updated_unix_ms: provenance.iter().filter_map(|p| p.modified_unix_ms).max(),
        candidate_count: pack.as_ref().map(|_| candidates.len()),
        unit_count: queue.as_ref().map(|q| q.units.len()),
        finding_count: triage.as_ref().map(|t| t.findings.len()),
        queue_summary: queue.as_ref().map(|q| value(&q.summary)),
        coverage,
        artifacts: states,
        state,
    };
    ReviewData {
        directory: dir.into(),
        stamps,
        summary,
        provenance,
        diagnostics,
        pack,
        queue,
        triage,
        candidates,
        candidate_index,
        unit_by_candidate,
        unit_index,
        decisions,
        report,
        recommendations,
    }
}
fn children(root: &SafeRoot, path: &Path, diagnostics: &mut Vec<String>) -> Vec<String> {
    match root.children_checked(path) {
        Ok(scan) => {
            diagnostics.extend(scan.diagnostics);
            scan.names
        }
        Err(error) => {
            diagnostics.push(error);
            Vec::new()
        }
    }
}
fn directories(root: &SafeRoot, diagnostics: &mut Vec<String>) -> Vec<PathBuf> {
    let base = Path::new(".anki-repo/review");
    let mut result = Vec::new();
    for namespace in children(root, base, diagnostics)
        .into_iter()
        .filter(|n| files::namespace(n))
    {
        let ns = base.join(namespace);
        for head in children(root, &ns, diagnostics)
            .into_iter()
            .filter(|h| files::hex(h, 40) || files::hex(h, 64))
        {
            let dir = ns.join(head);
            result.push(dir.clone());
            for variant in children(root, &dir, diagnostics)
                .into_iter()
                .filter(|v| files::variant(v))
            {
                result.push(dir.join(variant));
            }
        }
    }
    result
}
fn load_jobs(
    root: &SafeRoot,
    reviews: &[Arc<ReviewData>],
    previous: &Catalog,
    diagnostics: &mut Vec<String>,
) -> Vec<Value> {
    let mut jobs = Vec::new();
    'reviews: for review in reviews {
        let runs = review.directory.join("runs");
        for id in children(root, &runs, diagnostics)
            .into_iter()
            .filter(|id| files::hex(id, 32))
        {
            if jobs.len() >= 1000 {
                diagnostics
                    .push("Достигнут глобальный предел 1000 execution jobs; список неполон".into());
                break 'reviews;
            }
            let dir = runs.join(&id);
            let stamps = [
                "job.json",
                "state.json",
                "result.json",
                "source-review.json",
                "active.lock",
            ]
            .iter()
            .map(|f| root.stamp(&dir.join(f)))
            .collect::<Vec<_>>()
            .join("|")
                + if root.lock_active(&dir.join("active.lock")) {
                    "|lock_active"
                } else {
                    "|lock_inactive"
                };
            if let Some(cached) = previous.jobs.iter().find(|j| {
                j["id"] == id
                    && j["review_id"] == review.summary.id
                    && j["artifact_stamp"] == stamps
            }) {
                jobs.push(cached.clone());
                continue;
            }
            // Предварительно подтверждаем только файлы известных типов; inspect_job
            // дополнительно доказывает lifecycle, nonce, result и активную блокировку.
            let safe = ["job.json", "state.json", "source-review.json"]
                .iter()
                .all(|f| {
                    root.length(&dir.join(f)).is_ok_and(|n| {
                        n <= if *f == "source-review.json" {
                            MAX_JSON_BYTES
                        } else {
                            MAX_SMALL_BYTES
                        }
                    })
                });
            let inspection = if safe {
                execution::inspect_job(&root.path.join(&dir)).map_err(|e| e.to_string())
            } else {
                Err("Неполный или небезопасный execution artifact".into())
            };
            let job = match inspection {
                Ok(i) => {
                    let result = i.result.as_ref();
                    json!({"id":id,"review_id":review.summary.id,"mode":i.job.mode,"scope":i.job.scope,"lifecycle":i.lifecycle,"status":result.map(|r|r.status),"duration_ms":result.map(|r|r.duration_ms),"exit":result.and_then(|r|r.exit.as_ref()),"workspace_removed":i.workspace_removed,"state":"available","limitations":i.limitations,"enforcement":result.map(|r|&r.enforcement),"cleanup":result.map(|r|&r.cleanup),"request":result.map(|r|&r.request),"provenance":[{"path":dir.to_string_lossy(),"sha256":null,"modified_unix_ms":null}]})
                }
                Err(error) => {
                    json!({"id":id,"review_id":review.summary.id,"mode":null,"scope":null,"lifecycle":null,"status":null,"duration_ms":null,"exit":null,"workspace_removed":null,"state":"incomplete","limitations":[error],"enforcement":null,"cleanup":null,"request":null,"provenance":[{"path":dir.to_string_lossy(),"sha256":null,"modified_unix_ms":null}]})
                }
            };
            let mut job = job;
            job["artifact_stamp"] = json!(stamps);
            jobs.push(job);
        }
    }
    jobs.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    jobs
}
fn database_rows(store: &LearningStore, sql: &str) -> Result<Vec<Value>, String> {
    store
        .read(|read| {
            let mut statement = read
                .transaction()
                .prepare(sql)
                .map_err(|e| learning::store::map_error(&e, "Читаю строки learning"))?;
            let names: Vec<String> = statement
                .column_names()
                .iter()
                .map(|n| n.to_string())
                .collect();
            let rows = statement
                .query_map([], |row| {
                    let mut output = serde_json::Map::new();
                    for (index, name) in names.iter().enumerate() {
                        let v = match row.get_ref(index)? {
                            rusqlite::types::ValueRef::Null => Value::Null,
                            rusqlite::types::ValueRef::Integer(n) => json!(n),
                            rusqlite::types::ValueRef::Real(n) => json!(n),
                            rusqlite::types::ValueRef::Text(t) => {
                                let text = String::from_utf8_lossy(t);
                                if name.ends_with("_json") {
                                    serde_json::from_str(&text).unwrap_or_else(|_| json!(text))
                                } else {
                                    json!(text)
                                }
                            }
                            rusqlite::types::ValueRef::Blob(_) => Value::Null,
                        };
                        output.insert(name.clone(), v);
                    }
                    Ok(Value::Object(output))
                })
                .map_err(|e| learning::store::map_error(&e, "Запрашиваю строки learning"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| learning::store::map_error(&e, "Собираю строки learning"))
        })
        .map_err(|e| e.to_string())
}
fn learning_snapshot(root: &SafeRoot) -> (Envelope<Value>, BTreeMap<String, Vec<Value>>) {
    let path = match root.database_path() {
        Ok(path) => path,
        Err(error) => {
            return (
                Envelope::absent(
                    if std::fs::symlink_metadata(root.path.join(".anki-repo/learning/state.sqlite"))
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                    {
                        Availability::Missing
                    } else {
                        Availability::Unavailable
                    },
                    error,
                ),
                BTreeMap::new(),
            );
        }
    };
    let options = StoreOptions {
        database: path,
        display_path: ".anki-repo/learning/state.sqlite".into(),
        busy_timeout_ms: 250,
        create: false,
    };
    let store = match LearningStore::open_read_only(options) {
        Ok(s) => s,
        Err(e) => {
            return (
                Envelope::absent(
                    if e.code == ErrorCode::LearningSchemaUnsupported {
                        Availability::Incomplete
                    } else {
                        Availability::Unavailable
                    },
                    e.to_string(),
                ),
                BTreeMap::new(),
            );
        }
    };
    let result = (|| -> Result<_, String> {
        let before = learning::import::generation(&store).map_err(|e| e.to_string())?;
        let mut report = patterns::pattern_report(
            &store,
            &PatternQuery {
                limit: 10000,
                ..PatternQuery::default()
            },
        )
        .map_err(|e| e.to_string())?;
        let rows = database_rows(
            &store,
            "SELECT 'imports' AS name, COUNT(*) AS count FROM learning_import UNION ALL SELECT 'units', COUNT(*) FROM learning_unit UNION ALL SELECT 'candidates',COUNT(*) FROM learning_candidate UNION ALL SELECT 'decisions',COUNT(*) FROM learning_decision UNION ALL SELECT 'findings',COUNT(*) FROM learning_finding UNION ALL SELECT 'feedback',COUNT(*) FROM learning_feedback",
        )?;
        let after = learning::import::generation(&store).map_err(|e| e.to_string())?;
        if before.revision != after.revision || report.generation.revision != before.revision {
            return Err("История изменилась между транзакциями чтения; следующий снимок повторит наблюдение".into());
        }
        let mut counts = BTreeMap::new();
        for row in rows {
            if let Some(name) = row["name"].as_str() {
                counts.insert(name.to_string(), row["count"].clone());
            }
        }
        let patterns = report.rules.iter().map(value).collect::<Vec<_>>();
        report.rules.truncate(50);
        let summary = json!({"generation":before,"schema_version":learning::LEARNING_SCHEMA_VERSION,"policy_version":learning::LEARNING_POLICY_VERSION,"counts":counts,"patterns":report});
        Ok((summary, patterns))
    })();
    match result {
        Ok((summary, patterns)) => {
            let mut envelope = Envelope::available(summary);
            envelope.provenance.push(Provenance {
                path: ".anki-repo/learning/state.sqlite".into(),
                sha256: None,
                modified_unix_ms: None,
            });
            let mut collections = BTreeMap::new();
            collections.insert("patterns".into(), patterns);
            (envelope, collections)
        }
        Err(e) => (
            Envelope::absent(Availability::Unavailable, e),
            BTreeMap::new(),
        ),
    }
}

pub fn refresh(root: &SafeRoot, previous: &Catalog) -> Catalog {
    let mut reviews = Vec::new();
    let mut diagnostics: Vec<String> = Vec::new();
    let dirs = directories(root, &mut diagnostics);
    let mut remaining_bytes = 256 * 1024 * 1024u64;
    if dirs.len() > 256 {
        diagnostics
            .push("Показаны первые 256 snapshots: достигнут предел памяти наблюдателя".into());
    }
    for dir in dirs.into_iter().take(256) {
        let bytes = DOCUMENTS
            .iter()
            .map(|name| root.length(&dir.join(name)).unwrap_or(0))
            .fold(0u64, u64::saturating_add);
        if bytes > remaining_bytes {
            diagnostics.push(
                "Достигнут общий бюджет 256 MiB артефактов; часть snapshots не загружена".into(),
            );
            continue;
        }
        remaining_bytes -= bytes;
        let stamps = DOCUMENTS
            .iter()
            .map(|name| root.stamp(&dir.join(name)))
            .collect::<Vec<_>>()
            .join("|");
        if let Some(old) = previous
            .reviews
            .iter()
            .find(|r| r.directory == dir && r.stamps == stamps)
        {
            reviews.push(old.clone());
        } else {
            reviews.push(Arc::new(load_review(root, &dir, stamps)));
        }
    }
    reviews.sort_by(|a, b| {
        b.summary
            .updated_unix_ms
            .cmp(&a.summary.updated_unix_ms)
            .then(a.summary.id.cmp(&b.summary.id))
    });
    let mut execution_diagnostics = Vec::new();
    let jobs = load_jobs(root, &reviews, previous, &mut execution_diagnostics);
    let db_stamp = [
        ".anki-repo/learning/state.sqlite",
        ".anki-repo/learning/state.sqlite-wal",
    ]
    .iter()
    .map(|p| root.stamp(Path::new(p)))
    .collect::<Vec<_>>()
    .join("|");
    let old_db_stamp = previous
        .health
        .components
        .iter()
        .find(|c| c["name"] == "learning")
        .and_then(|c| c["stamp"].as_str());
    let (learning, mut learning_collections) = if old_db_stamp == Some(&db_stamp)
        && previous.learning.state != Availability::Unavailable
    {
        (
            previous.learning.clone(),
            previous.learning_collections.clone(),
        )
    } else {
        learning_snapshot(root)
    };
    let mut recommendations = Vec::new();
    for review in &reviews {
        if let Some(document) = &review.recommendations
            && let Some(rows) = document["recommendations"].as_array()
        {
            for (rank, row) in rows.iter().enumerate() {
                recommendations.push(json!({"review_id":review.summary.id,"unit_id":row["unit_id"],"baseline_priority":row["queue_priority"],"suggested_position":row["suggested_position"],"recommended_rank":document["suggested_order"].as_array().and_then(|ids|ids.iter().position(|id|id==&row["unit_id"])).map(|i|i+1).unwrap_or(rank+1),"reason":row["reason"],"granularity":row["granularity"],"candidate_count":row["candidate_count"],"history_generation":document["generation"],"source_reference":row["source_reference"],"support":row["support"],"historical_cases":row["historical_cases"],"limitations":row["limitations"],"provenance":review.provenance.iter().filter(|p|p.path.ends_with("/recommendations.json")).collect::<Vec<_>>()}));
            }
        }
    }
    learning_collections.insert("recommendations".into(), recommendations);
    let review_state = if !diagnostics.is_empty() {
        Availability::Incomplete
    } else if reviews.is_empty() {
        Availability::Missing
    } else if reviews
        .iter()
        .any(|r| r.summary.state == Availability::Invalid)
    {
        Availability::Invalid
    } else if reviews
        .iter()
        .any(|r| r.summary.state != Availability::Available)
    {
        Availability::Incomplete
    } else {
        Availability::Available
    };
    let health=Health {components:vec![json!({"name":"review_artifacts","state":review_state,"diagnostics":diagnostics}),json!({"name":"execution","state":if !execution_diagnostics.is_empty(){"incomplete"}else if jobs.is_empty(){"not_observed"}else if jobs.iter().any(|j|j["state"]!="available"){"incomplete"}else{"available"},"diagnostics":execution_diagnostics}),json!({"name":"learning","state":learning.state,"diagnostics":learning.diagnostics,"stamp":db_stamp})],read_only:true,poll_interval_ms:2000,limitations:vec!["Наблюдаются только durable артефакты. Деятельность LLM, токены и действия вне этих источников не наблюдаются.".into(),"Просмотр страниц не является семантическим рассмотрением кандидатов.".into(),"Сырые stdout, stderr, логи, runtime и tracked source не публикуются.".into(),"Валидность очереди проверяется по сохранённым структурам и digest; AST и внешние инструменты при HTTP-чтении не запускаются.".into()]};
    let fingerprint=format!("{:x}",sha2::Sha256::digest(serde_json::to_vec(&json!({"reviews":reviews.iter().map(|r|json!({"summary":r.summary,"stamps":r.stamps})).collect::<Vec<_>>(),"jobs":jobs,"learning":learning,"health":health})).unwrap_or_default()));
    let revision = if fingerprint == previous.fingerprint {
        previous.revision
    } else {
        previous.revision + 1
    };
    Catalog {
        revision,
        observed_unix_ms: now_ms(),
        reviews,
        jobs,
        learning,
        learning_collections,
        health,
        fingerprint,
    }
}

pub fn learning_page(
    root: &SafeRoot,
    collection: &str,
    q: &Query,
) -> Result<Envelope<crate::dto::Page<Value>>, String> {
    let (page, limit) = q.bounds()?;
    let table = match collection {
        "observations" => "learning_import",
        "feedback" => "learning_feedback",
        "decisions" => "learning_decision",
        "findings" => "learning_finding",
        _ => return Err("Неизвестная learning collection".into()),
    };
    let search = match collection {
        "observations" => "review_id || ' ' || head_sha || ' ' || trust",
        "feedback" => "review_id || ' ' || unit_id || ' ' || explanation",
        "decisions" => "review_id || ' ' || disposition || ' ' || explanation",
        _ => "review_id || ' ' || title || ' ' || description",
    };
    let root_path = root.database_path()?;
    let store = LearningStore::open_read_only(StoreOptions {
        database: root_path,
        display_path: ".anki-repo/learning/state.sqlite".into(),
        busy_timeout_ms: 250,
        create: false,
    })
    .map_err(|e| e.to_string())?;
    if collection == "observations" {
        let (total, imports) = learning::import::list_imports_page(
            &store,
            true,
            (page - 1) * limit,
            limit,
            q.q.as_deref().unwrap_or(""),
        )
        .map_err(|e| e.to_string())?;
        let mut envelope = Envelope::available(crate::dto::Page {
            items: imports.into_iter().map(value).collect(),
            total,
            page,
            limit,
        });
        envelope.provenance.push(Provenance {
            path: ".anki-repo/learning/state.sqlite".into(),
            sha256: None,
            modified_unix_ms: None,
        });
        return Ok(envelope);
    }
    let result = store
        .read(|read| {
            let predicate = format!("instr(lower({search}), lower(?1)) > 0");
            let needle = q.q.as_deref().unwrap_or("");
            let total: i64 = read
                .transaction()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"),
                    [needle],
                    |row| row.get(0),
                )
                .map_err(|e| learning::store::map_error(&e, "Считаю строки learning"))?;
            let order = match collection {
                "observations" => "revision DESC, review_id",
                "feedback" => "recorded_at DESC,event_id",
                "decisions" => "review_id,decision_id",
                _ => "review_id,finding_id",
            };
        let columns=match collection {
            "decisions"=>"review_id,decision_id,kind,disposition,reason_code,explanation,candidate_count,json_array_length(covered_json) AS covered_count",
            "findings"=>"review_id,finding_id,severity,provenance,title,description,signature,json_array_length(linked_unit_ids_json) AS linked_unit_count",
            _=>"*",
        };
        let mut statement = read
                .transaction()
                .prepare(&format!(
                "SELECT {columns} FROM {table} WHERE {predicate} ORDER BY {order} LIMIT ?2 OFFSET ?3"
                ))
                .map_err(|e| learning::store::map_error(&e, "Читаю страницу learning"))?;
            let names: Vec<String> = statement
                .column_names()
                .iter()
                .map(|n| n.to_string())
                .collect();
            let rows = statement
                .query_map(
                    rusqlite::params![needle, limit as i64, ((page - 1) * limit) as i64],
                    |row| {
                        let mut item = serde_json::Map::new();
                        for (i, name) in names.iter().enumerate() {
                            let v = match row.get_ref(i)? {
                                rusqlite::types::ValueRef::Null => Value::Null,
                                rusqlite::types::ValueRef::Integer(n) => json!(n),
                                rusqlite::types::ValueRef::Real(n) => json!(n),
                                rusqlite::types::ValueRef::Text(text) => {
                                    let text = String::from_utf8_lossy(text);
                                    if name.ends_with("_json") {
                                        serde_json::from_str(&text).unwrap_or_else(|_| json!(text))
                                    } else {
                                        json!(text)
                                    }
                                }
                                rusqlite::types::ValueRef::Blob(_) => Value::Null,
                            };
                            item.insert(name.trim_end_matches("_json").into(), v);
                        }
                        Ok(Value::Object(item))
                    },
                )
                .map_err(|e| learning::store::map_error(&e, "Запрашиваю страницу learning"))?;
            let items = rows
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| learning::store::map_error(&e, "Собираю страницу learning"))?;
            Ok(crate::dto::Page {
                items,
                total: total as usize,
                page,
                limit,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut envelope = Envelope::available(result);
    envelope.provenance.push(Provenance {
        path: ".anki-repo/learning/state.sqlite".into(),
        sha256: None,
        modified_unix_ms: None,
    });
    Ok(envelope)
}
