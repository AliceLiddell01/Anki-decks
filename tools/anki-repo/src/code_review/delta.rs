//! Detector-level сравнение evidence snapshots без semantic verdict.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{DomainError, ErrorCode};

use super::model::{
    CandidateChange, CandidateEvidence, CandidateOrigin, CandidateStatus, DiagnosticChange,
    REVIEW_SCHEMA_VERSION, ReviewDelta, ReviewPack, SnapshotIdentity, ToolRunChange,
};

/// Сравнивает два pack одной репозитории и одной базовой ревизии.
///
/// Удаление candidate означает только, что detector больше не видит сигнал.
/// Оно не подтверждает исправление semantic finding.
pub fn compare(before: &ReviewPack, after: &ReviewPack) -> Result<ReviewDelta, DomainError> {
    validate_review_pack(before)?;
    validate_review_pack(after)?;
    if before.target.repository_id != after.target.repository_id
        || before.target.base_sha != after.target.base_sha
        || before.scope.merge_base_sha != after.scope.merge_base_sha
    {
        return Err(DomainError::with_details(
            ErrorCode::BaselineMismatch,
            "review-pack относятся к разным репозиториям или базовым коммитам",
            crate::details! {
                "before_repository_id" => before.target.repository_id,
                "after_repository_id" => after.target.repository_id,
                "before_base_sha" => before.target.base_sha,
                "after_base_sha" => after.target.base_sha,
                "before_merge_base_sha" => before.scope.merge_base_sha,
                "after_merge_base_sha" => after.scope.merge_base_sha,
            },
        ));
    }

    let candidates = compare_candidates(before, after);
    let diagnostics = compare_diagnostics(before, after);
    let tool_runs = compare_tool_runs(before, after);
    Ok(ReviewDelta {
        schema_version: REVIEW_SCHEMA_VERSION,
        before: identity(before),
        after: identity(after),
        candidates,
        diagnostics,
        tool_runs,
    })
}

pub(crate) fn validate_review_pack(pack: &ReviewPack) -> Result<(), DomainError> {
    if pack.schema_version != REVIEW_SCHEMA_VERSION {
        return Err(DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!(
                "неподдерживаемая версия review-pack: {}",
                pack.schema_version
            ),
        ));
    }
    if pack.target.base_sha.is_empty()
        || pack.target.head_sha.is_empty()
        || pack.target.repository_id.is_empty()
    {
        return Err(DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            "review-pack не содержит полную snapshot identity",
        ));
    }
    Ok(())
}

fn identity(pack: &ReviewPack) -> SnapshotIdentity {
    SnapshotIdentity {
        repository_id: pack.target.repository_id.clone(),
        base_sha: pack.target.base_sha.clone(),
        head_sha: pack.target.head_sha.clone(),
    }
}

fn compare_candidates(before: &ReviewPack, after: &ReviewPack) -> Vec<CandidateChange> {
    let old = before.all_candidates();
    let new = after.all_candidates();
    let before_paths = rename_aliases(before);
    let after_paths = rename_aliases(after);

    let mut old_by_semantic: BTreeMap<SemanticCandidateKey, Vec<usize>> = BTreeMap::new();
    let mut new_by_semantic: BTreeMap<SemanticCandidateKey, Vec<usize>> = BTreeMap::new();
    for (index, candidate) in old.iter().enumerate() {
        old_by_semantic
            .entry(semantic_key(candidate, &before_paths))
            .or_default()
            .push(index);
    }
    for (index, candidate) in new.iter().enumerate() {
        new_by_semantic
            .entry(semantic_key(candidate, &after_paths))
            .or_default()
            .push(index);
    }

    let mut old_used = BTreeSet::new();
    let mut new_used = BTreeSet::new();
    let mut changes = Vec::new();
    for (key, old_indexes) in &old_by_semantic {
        let Some(new_indexes) = new_by_semantic.get(key) else {
            continue;
        };
        let pairs = pair_by_line(old_indexes, new_indexes, &old, &new);
        for (old_index, new_index) in pairs {
            old_used.insert(old_index);
            new_used.insert(new_index);
            changes.push(CandidateChange {
                status: CandidateStatus::StillPresent,
                before: Some(old[old_index].clone()),
                after: Some(new[new_index].clone()),
            });
        }
    }

    // Одинаковое направление и файл, но иные snippet/signals — изменённый signal.
    let mut old_by_location: BTreeMap<LocationKey, Vec<usize>> = BTreeMap::new();
    let mut new_by_location: BTreeMap<LocationKey, Vec<usize>> = BTreeMap::new();
    for (index, candidate) in old.iter().enumerate() {
        if !old_used.contains(&index) {
            old_by_location
                .entry(location_key(candidate, &before_paths))
                .or_default()
                .push(index);
        }
    }
    for (index, candidate) in new.iter().enumerate() {
        if !new_used.contains(&index) {
            new_by_location
                .entry(location_key(candidate, &after_paths))
                .or_default()
                .push(index);
        }
    }
    for (key, old_indexes) in old_by_location {
        let Some(new_indexes) = new_by_location.get(&key) else {
            continue;
        };
        let pairs = pair_changed_nearby(&old_indexes, new_indexes, &old, &new);
        for (old_index, new_index) in pairs {
            old_used.insert(old_index);
            new_used.insert(new_index);
            changes.push(CandidateChange {
                status: CandidateStatus::Changed,
                before: Some(old[old_index].clone()),
                after: Some(new[new_index].clone()),
            });
        }
    }
    for (index, candidate) in old.iter().enumerate() {
        if !old_used.contains(&index) {
            changes.push(CandidateChange {
                status: CandidateStatus::Gone,
                before: Some(candidate.clone()),
                after: None,
            });
        }
    }
    for (index, candidate) in new.iter().enumerate() {
        if !new_used.contains(&index) {
            changes.push(CandidateChange {
                status: CandidateStatus::New,
                before: None,
                after: Some(candidate.clone()),
            });
        }
    }
    changes.sort_by(|left, right| {
        let left_candidate = left.after.as_ref().or(left.before.as_ref());
        let right_candidate = right.after.as_ref().or(right.before.as_ref());
        (
            left_candidate.map(|item| item.path.as_str()),
            left_candidate.map(|item| item.detector.as_str()),
            left_candidate.and_then(|item| item.line),
            left.status,
        )
            .cmp(&(
                right_candidate.map(|item| item.path.as_str()),
                right_candidate.map(|item| item.detector.as_str()),
                right_candidate.and_then(|item| item.line),
                right.status,
            ))
    });
    changes
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SemanticCandidateKey {
    detector: String,
    path: String,
    snippet: Option<String>,
    signals: Vec<String>,
    origin: CandidateOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LocationKey {
    detector: String,
    path: String,
}

fn semantic_key(
    candidate: &CandidateEvidence,
    aliases: &BTreeMap<String, String>,
) -> SemanticCandidateKey {
    SemanticCandidateKey {
        detector: candidate.detector.clone(),
        path: canonical_path(&candidate.path, aliases),
        snippet: candidate.snippet.clone(),
        signals: candidate.signals.clone(),
        origin: candidate.origin,
    }
}

fn location_key(candidate: &CandidateEvidence, aliases: &BTreeMap<String, String>) -> LocationKey {
    LocationKey {
        detector: candidate.detector.clone(),
        path: canonical_path(&candidate.path, aliases),
    }
}

fn canonical_path(path: &str, aliases: &BTreeMap<String, String>) -> String {
    aliases
        .get(path)
        .cloned()
        .unwrap_or_else(|| path.to_owned())
}

fn rename_aliases(pack: &ReviewPack) -> BTreeMap<String, String> {
    pack.scope
        .files
        .iter()
        .filter_map(|file| {
            file.previous_path
                .as_ref()
                .map(|old| (file.path.clone(), old.clone()))
        })
        .collect()
}

fn pair_by_line(
    old_indexes: &[usize],
    new_indexes: &[usize],
    old: &[CandidateEvidence],
    new: &[CandidateEvidence],
) -> Vec<(usize, usize)> {
    let mut old_indexes = old_indexes.to_vec();
    let mut new_indexes = new_indexes.to_vec();
    old_indexes.sort_by_key(|index| (old[*index].line, old[*index].id.as_str()));
    new_indexes.sort_by_key(|index| (new[*index].line, new[*index].id.as_str()));
    old_indexes.into_iter().zip(new_indexes).collect()
}

fn compare_diagnostics(before: &ReviewPack, after: &ReviewPack) -> Vec<DiagnosticChange> {
    let old_keys: Vec<_> = before.diagnostics.iter().map(diagnostic_key).collect();
    let new_keys: Vec<_> = after.diagnostics.iter().map(diagnostic_key).collect();
    let mut old_used = BTreeSet::new();
    let mut new_used = BTreeSet::new();
    let mut changes = Vec::new();
    for (old_index, old_key) in old_keys.iter().enumerate() {
        if let Some(new_index) = new_keys.iter().enumerate().find_map(|(index, key)| {
            (!new_used.contains(&index) && key == old_key).then_some(index)
        }) {
            old_used.insert(old_index);
            new_used.insert(new_index);
            changes.push(DiagnosticChange {
                status: CandidateStatus::StillPresent,
                before: Some(before.diagnostics[old_index].clone()),
                after: Some(after.diagnostics[new_index].clone()),
            });
        }
    }
    for (old_index, old_diagnostic) in before.diagnostics.iter().enumerate() {
        if old_used.contains(&old_index) {
            continue;
        }
        let changed = after
            .diagnostics
            .iter()
            .enumerate()
            .find(|(new_index, diagnostic)| {
                !new_used.contains(new_index)
                    && diagnostic_identity(old_diagnostic) == diagnostic_identity(diagnostic)
            });
        if let Some((new_index, new_diagnostic)) = changed {
            old_used.insert(old_index);
            new_used.insert(new_index);
            changes.push(DiagnosticChange {
                status: CandidateStatus::Changed,
                before: Some(old_diagnostic.clone()),
                after: Some(new_diagnostic.clone()),
            });
        } else {
            changes.push(DiagnosticChange {
                status: CandidateStatus::Gone,
                before: Some(old_diagnostic.clone()),
                after: None,
            });
        }
    }
    for (new_index, diagnostic) in after.diagnostics.iter().enumerate() {
        if !new_used.contains(&new_index) {
            changes.push(DiagnosticChange {
                status: CandidateStatus::New,
                before: None,
                after: Some(diagnostic.clone()),
            });
        }
    }
    changes.sort_by_key(diagnostic_change_key);
    changes
}

fn diagnostic_key(diagnostic: &super::diagnostics::CodeReviewDiagnostic) -> serde_json::Value {
    serde_json::to_value(diagnostic).unwrap_or(serde_json::Value::Null)
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct DiagnosticIdentity {
    tool: String,
    code: Option<String>,
    package_id: String,
    spans: Vec<DiagnosticSpanIdentity>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct DiagnosticSpanIdentity {
    file_name: String,
    line_start: u64,
    line_end: u64,
}

fn diagnostic_identity(
    diagnostic: &super::diagnostics::CodeReviewDiagnostic,
) -> DiagnosticIdentity {
    DiagnosticIdentity {
        tool: serde_json::to_string(&diagnostic.source.tool).unwrap_or_default(),
        code: diagnostic.code.as_ref().map(|code| code.code.clone()),
        package_id: diagnostic.source.package_id.clone().unwrap_or_default(),
        spans: diagnostic
            .spans
            .iter()
            .map(|span| DiagnosticSpanIdentity {
                file_name: span.file_name.clone(),
                line_start: span.line_start,
                line_end: span.line_end,
            })
            .collect(),
    }
}

fn diagnostic_change_key(change: &DiagnosticChange) -> (String, String, u64, CandidateStatus) {
    let diagnostic = change.after.as_ref().or(change.before.as_ref());
    diagnostic.map_or_else(
        || (String::new(), String::new(), 0, change.status),
        |item| {
            let span = item.spans.first();
            (
                span.map_or_else(String::new, |span| span.file_name.clone()),
                item.code
                    .as_ref()
                    .map_or_else(|| item.message.clone(), |code| code.code.clone()),
                span.map_or(0, |span| span.line_start),
                change.status,
            )
        },
    )
}

fn compare_tool_runs(before: &ReviewPack, after: &ReviewPack) -> Vec<ToolRunChange> {
    let old: BTreeMap<_, _> = before
        .tool_runs
        .iter()
        .map(|run| (run.tool.as_str(), run))
        .collect();
    let new: BTreeMap<_, _> = after
        .tool_runs
        .iter()
        .map(|run| (run.tool.as_str(), run))
        .collect();
    let tools: BTreeSet<_> = old.keys().chain(new.keys()).copied().collect();
    tools
        .into_iter()
        .map(|tool| {
            let before_run = old.get(tool);
            let after_run = new.get(tool);
            let before_status = before_run.map_or("missing", |run| run.status.as_str());
            let after_status = after_run.map_or("missing", |run| run.status.as_str());
            ToolRunChange {
                tool: tool.to_owned(),
                before_status: before_status.to_owned(),
                after_status: after_status.to_owned(),
                status_changed: before_status != after_status,
                before_diagnostics: before_run.map_or(0, |run| run.diagnostic_count),
                after_diagnostics: after_run.map_or(0, |run| run.diagnostic_count),
                after_message: after_run.and_then(|run| run.message.clone()),
            }
        })
        .collect()
}

/// Считает candidate изменённым только рядом с прежней строкой.
/// Далёкий сигнал того же широкого detector-а в другом месте остаётся парой
/// Gone + New, а не выглядит продолжением прежнего сигнала.
fn pair_changed_nearby(
    old_indexes: &[usize],
    new_indexes: &[usize],
    old: &[CandidateEvidence],
    new: &[CandidateEvidence],
) -> Vec<(usize, usize)> {
    const MAX_LINE_DRIFT: usize = 3;
    let mut old_indexes = old_indexes.to_vec();
    let mut new_indexes = new_indexes.to_vec();
    old_indexes.sort_by_key(|index| (old[*index].line, old[*index].id.as_str()));
    new_indexes.sort_by_key(|index| (new[*index].line, new[*index].id.as_str()));
    let mut used = BTreeSet::new();
    let mut pairs = Vec::new();
    for old_index in old_indexes {
        let nearest = new_indexes
            .iter()
            .enumerate()
            .filter(|(position, _)| !used.contains(position))
            .filter_map(|(position, new_index)| {
                let distance = match (old[old_index].line, new[*new_index].line) {
                    (Some(before), Some(after)) => before.abs_diff(after),
                    (None, None) => 0,
                    _ => return None,
                };
                (distance <= MAX_LINE_DRIFT).then_some((distance, position, *new_index))
            })
            .min_by_key(|(distance, position, new_index)| {
                (*distance, new[*new_index].id.as_str(), *position)
            });
        if let Some((_, position, new_index)) = nearest {
            used.insert(position);
            pairs.push((old_index, new_index));
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_review::diagnostics::DiagnosticTool;
    use crate::code_review::model::{CandidateEvidence, ReviewFile, ReviewScope, ToolRunEvidence};
    use crate::code_review::scope::{FileCategory, FileStatus, GitTarget};
    use crate::code_review::{diagnostics::CodeReviewDiagnostic, language::LanguageScan};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn candidate(id: &str, line: usize, snippet: &str) -> CandidateEvidence {
        CandidateEvidence {
            id: id.into(),
            detector: "error_path".into(),
            path: "src/lib.rs".into(),
            line: Some(line),
            column: None,
            snippet: Some(snippet.into()),
            origin: CandidateOrigin::IntroducedOrChanged,
            signals: vec!["unwrap_call".into()],
            source: "rust_detector".into(),
            metadata: BTreeMap::new(),
        }
    }

    fn tool_run(status: &str, count: usize) -> ToolRunEvidence {
        ToolRunEvidence {
            tool: "clippy".into(),
            status: status.into(),
            exit_status: None,
            diagnostic_count: count,
            malformed_lines: 0,
            ignored_records: 0,
            stderr_summary: None,
            message: None,
        }
    }

    fn diagnostic(message: &str) -> CodeReviewDiagnostic {
        serde_json::from_value(json!({
            "severity": "warning",
            "code": {"code": "clippy::unwrap_used"},
            "message": message,
            "children": [],
            "spans": [{
                "file_name": "src/lib.rs",
                "byte_start": 10,
                "byte_end": 17,
                "line_start": 2,
                "line_end": 2,
                "column_start": 4,
                "column_end": 11,
                "is_primary": true,
                "source_lines": []
            }],
            "source": {
                "tool": DiagnosticTool::Clippy,
                "package_id": "test-package",
                "target_name": "lib"
            }
        }))
        .expect("fixture diagnostic разбирается")
    }

    fn pack(
        head: &str,
        candidates: Vec<CandidateEvidence>,
        diagnostics: Vec<CodeReviewDiagnostic>,
        run: ToolRunEvidence,
    ) -> ReviewPack {
        ReviewPack {
            schema_version: REVIEW_SCHEMA_VERSION,
            target: GitTarget {
                repository_id: "repo-test".into(),
                base_sha: "base-sha".into(),
                head_sha: head.into(),
                merge_base_sha: "base-sha".into(),
            },
            scope: ReviewScope {
                merge_base_sha: "base-sha".into(),
                text_image_limit_bytes: 1024,
                files: vec![ReviewFile {
                    path: "src/lib.rs".into(),
                    previous_path: None,
                    status: FileStatus::Modified,
                    additions: Some(1),
                    deletions: Some(1),
                    category: FileCategory::Rust,
                    surfaces: Vec::new(),
                    binary: false,
                    base_state: crate::code_review::scope::ImageState::Text,
                    base_size: 20,
                    base_object_id: Some("base-blob".into()),
                    base_changed_lines: Vec::new(),
                    post_state: crate::code_review::scope::ImageState::Text,
                    post_size: 20,
                    post_object_id: Some("post-blob".into()),
                    post_changed_lines: Vec::new(),
                }],
            },
            diagnostics,
            candidates,
            language: LanguageScan {
                schema_version: 1,
                files: Vec::new(),
                candidates: Vec::new(),
                skipped: Vec::new(),
            },
            dependencies: Vec::new(),
            tests: Vec::new(),
            suppressions: Vec::new(),
            risk_surfaces: Vec::new(),
            tool_runs: vec![run],
        }
    }

    #[test]
    fn compares_candidates_and_diagnostics_without_claiming_findings_are_fixed() {
        let before = pack(
            "head-a",
            vec![
                candidate("same-old-line", 10, "unwrap()"),
                candidate("changed-old", 20, "expect(\"old\")"),
                candidate("gone", 30, "todo!()"),
            ],
            vec![diagnostic("unused result")],
            tool_run("diagnostics", 1),
        );
        let after = pack(
            "head-b",
            vec![
                candidate("same-new-line", 12, "unwrap()"),
                candidate("changed-new", 20, "expect(\"new\")"),
                candidate("new", 40, "panic!()"),
            ],
            vec![diagnostic("unused value")],
            tool_run("unavailable", 0),
        );
        let delta = compare(&before, &after).expect("снимки совместимы");
        assert!(delta.candidates.iter().any(|change| {
            change.status == CandidateStatus::StillPresent
                && change
                    .before
                    .as_ref()
                    .is_some_and(|candidate| candidate.id == "same-old-line")
        }));
        assert!(delta.candidates.iter().any(|change| {
            change.status == CandidateStatus::Changed
                && change
                    .after
                    .as_ref()
                    .is_some_and(|candidate| candidate.id == "changed-new")
        }));
        assert!(
            delta
                .candidates
                .iter()
                .any(|change| change.status == CandidateStatus::Gone)
        );
        assert!(
            delta
                .candidates
                .iter()
                .any(|change| change.status == CandidateStatus::New)
        );
        assert!(
            delta
                .diagnostics
                .iter()
                .any(|change| change.status == CandidateStatus::Changed)
        );
        assert!(delta.tool_runs[0].status_changed);
        let json = serde_json::to_string(&delta).expect("delta сериализуется");
        assert!(!json.contains("finding_fixed"));
        assert!(json.contains("still_present"));
    }

    #[test]
    fn refuses_unrelated_baselines_and_unknown_pack_versions() {
        let before = pack("head-a", Vec::new(), Vec::new(), tool_run("skipped", 0));
        let mut unrelated = before.clone();
        unrelated.target.repository_id = "another-repo".into();
        assert_eq!(
            compare(&before, &unrelated)
                .expect_err("другая репозитория отклоняется")
                .code,
            ErrorCode::BaselineMismatch
        );

        let mut unsupported = before.clone();
        unsupported.schema_version += 1;
        assert_eq!(
            compare(&before, &unsupported)
                .expect_err("неизвестная схема отклоняется")
                .code,
            ErrorCode::ReviewArtifactInvalid
        );
    }
}
