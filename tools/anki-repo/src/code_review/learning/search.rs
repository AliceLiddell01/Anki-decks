//! Локальный структурный и текстовый поиск исторических случаев.
//!
//! Базовый путь — индексированные SQL-запросы и локальный FTS5. Если сборка
//! SQLite не поддерживает FTS5, используется документированный воспроизводимый
//! fallback на подстрочный поиск с Unicode-свёрткой регистра — без сети, LLM,
//! embeddings и внешних индексов. Все запросы параметризованы: сохранённый текст
//! остаётся данными и никогда не превращается в SQL или команду.

use std::collections::BTreeSet;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{DomainError, ErrorCode};

use super::import::MAX_STORED_SNIPPET_BYTES;
use super::store::{LearningStore, map_error};

/// Версия схемы результата поиска.
///
/// Версия 2 добавляет аддитивное поле `include_quarantine`: поиск по умолчанию
/// не смешивает карантин с доверенной историей, а сообщает, какой фильтр к нему
/// применили.
pub const SEARCH_SCHEMA_VERSION: u32 = 2;

/// Размер страницы поиска по умолчанию.
pub const DEFAULT_SEARCH_LIMIT: usize = 25;
/// Максимальный размер страницы поиска.
pub const MAX_SEARCH_LIMIT: usize = 200;

/// Вид совпадения: приблизительная схожесть не выдаётся за точную.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMatchKind {
    /// Совпали и структурные фильтры, и текст.
    ExactStructural,
    /// Совпал только текст.
    Textual,
    /// Совпали структурные фильтры: случай тематически близок, но не тот же.
    Thematic,
}

impl SearchMatchKind {
    /// Стабильная машиночитаемая метка.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExactStructural => "exact_structural",
            Self::Textual => "textual",
            Self::Thematic => "thematic",
        }
    }
}

/// Запрос поиска исторических случаев.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    /// Текстовая подстрока: короткая фраза, а не исполняемый запрос.
    pub text: Option<String>,
    /// Ограничение по детектору.
    pub detector: Option<String>,
    /// Ограничение по поверхности исполнения (`production`/`tests`).
    pub surface: Option<String>,
    /// Ограничение по происхождению сигнала.
    pub origin: Option<String>,
    /// Ограничение по структурной роли.
    pub role: Option<String>,
    /// Ограничение по роли кода.
    pub code_role: Option<String>,
    /// Ограничение по решению ревьюера.
    pub disposition: Option<String>,
    /// Ограничение по происхождению замечания.
    pub provenance: Option<String>,
    /// Ограничение по серьёзности замечания.
    pub severity: Option<String>,
    /// Ограничение по репозиторию.
    pub repository_id: Option<String>,
    /// Включать карантинные записи (по умолчанию исключены).
    ///
    /// Поиск — читатель истории, а не основание для вывода: записи пониженного
    /// доверия смешивались бы с доверенными аналогиями, поэтому по умолчанию они
    /// не возвращаются и включаются только явным согласием вызывающего.
    pub include_quarantine: bool,
    /// Смещение страницы.
    pub offset: usize,
    /// Размер страницы.
    pub limit: usize,
}

/// Один найденный исторический случай.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCase {
    /// Идентификатор случая истории.
    pub case_id: String,
    /// Запись ревью.
    pub review_id: String,
    /// Единица очереди, если случай к ней привязан.
    pub unit_id: Option<String>,
    /// Кандидат, если случай к нему привязан.
    pub candidate_id: Option<String>,
    /// Замечание, если случай описывает замечание.
    pub finding_id: Option<String>,
    /// Вид сохранённого случая.
    pub kind: String,
    /// Вид совпадения.
    pub match_kind: SearchMatchKind,
    /// Компактный ограниченный фрагмент текста.
    pub snippet: String,
    /// Решение ревьюера, если оно известно.
    pub disposition: Option<String>,
    /// Происхождение замечания, если оно известно.
    pub provenance: Option<String>,
    /// Серьёзность замечания, если она известна.
    pub severity: Option<String>,
    /// Детектор связанной единицы, если он известен.
    pub detector: Option<String>,
    /// Repo-relative путь образца, если он сохранён.
    pub path: Option<String>,
    /// Явный указатель на первичный источник.
    pub source_reference: String,
    /// Уровень доверия записи.
    pub trust: String,
}

/// Страница результатов поиска.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchPage {
    /// Версия схемы результата.
    pub schema_version: u32,
    /// Использован ли FTS5-индекс.
    pub fts5_used: bool,
    /// Учитывались ли записи пониженного доверия: фактический фильтр страницы.
    pub include_quarantine: bool,
    /// Всего совпадений до страницы.
    pub matched: usize,
    /// Смещение запроса.
    pub offset: usize,
    /// Фактический размер страницы.
    pub limit: usize,
    /// Есть ли продолжение.
    pub has_more: bool,
    /// Результаты страницы.
    pub cases: Vec<SearchCase>,
}

/// Ищет исторические случаи по структурным фильтрам и текстовым описаниям.
pub fn search_history(
    store: &LearningStore,
    query: &SearchQuery,
) -> Result<SearchPage, DomainError> {
    let limit = if query.limit == 0 {
        DEFAULT_SEARCH_LIMIT
    } else {
        query.limit
    };
    if limit > MAX_SEARCH_LIMIT {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            format!("Размер страницы поиска превышает предел {MAX_SEARCH_LIMIT}"),
        ));
    }
    if query
        .text
        .as_deref()
        .is_some_and(|text| text.trim().chars().count() < 2)
    {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "Текстовая подстрока поиска должна содержать не менее двух символов",
        ));
    }
    let structural = has_structural_filters(query);
    let text_present = query
        .text
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty());
    store.read(|read| {
        let fts5_used = text_present
            && query
                .text
                .as_deref()
                .is_some_and(|text| text.trim().chars().count() >= 3)
            && store.fts5_available()
            && fts_index_present(read)?;
        let mut matched: Vec<String> = Vec::new();
        let mut text_hits: BTreeSet<String> = BTreeSet::new();
        let mut structural_hits: BTreeSet<String> = BTreeSet::new();
        if text_present {
            text_hits = text_matches(read, query, fts5_used)?;
        }
        if structural || !text_present {
            structural_hits = structural_matches(read, query)?;
        }
        let mut cases: Vec<String> = match (text_present, structural) {
            (true, true) => text_hits.intersection(&structural_hits).cloned().collect(),
            (true, false) => text_hits.iter().cloned().collect(),
            _ => structural_hits.iter().cloned().collect(),
        };
        cases.sort();
        cases.dedup();
        let total = cases.len();
        let start = query.offset.min(total);
        let end = query.offset.saturating_add(limit).min(total);
        for case_id in &cases[start..end] {
            matched.push(case_id.clone());
        }
        let mut results = Vec::new();
        for case_id in &matched {
            let kind = if text_present && structural {
                SearchMatchKind::ExactStructural
            } else if text_present {
                SearchMatchKind::Textual
            } else {
                SearchMatchKind::Thematic
            };
            if let Some(case) = load_case(read, case_id, kind)? {
                results.push(case);
            }
        }
        Ok(SearchPage {
            schema_version: SEARCH_SCHEMA_VERSION,
            // Факт, а не предположение: сообщается то, чем поиск действительно
            // пользовался на этом снимке истории.
            fts5_used,
            include_quarantine: query.include_quarantine,
            matched: total,
            offset: query.offset,
            limit,
            has_more: end < total,
            cases: results,
        })
    })
}

fn has_structural_filters(query: &SearchQuery) -> bool {
    query.detector.is_some()
        || query.surface.is_some()
        || query.origin.is_some()
        || query.role.is_some()
        || query.code_role.is_some()
        || query.disposition.is_some()
        || query.provenance.is_some()
        || query.severity.is_some()
        || query.repository_id.is_some()
}

/// Текстовые совпадения: FTS5 либо честный подстрочный fallback.
/// Проверяет фактическое наличие FTS5-таблицы: сборка может её поддерживать,
/// а конкретная база — не иметь (например, восстановленная из старого backup).
fn fts_index_present(read: &super::store::LearningRead<'_>) -> Result<bool, DomainError> {
    let count: i64 = read.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'learning_search_fts'",
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn text_matches(
    read: &super::store::LearningRead<'_>,
    query: &SearchQuery,
    use_fts5: bool,
) -> Result<BTreeSet<String>, DomainError> {
    let Some(text) = query.text.as_deref().map(str::trim) else {
        return Ok(BTreeSet::new());
    };
    let mut hits = BTreeSet::new();
    // Доверие проверяется в том же запросе, что и совпадение: иначе счётчик
    // совпадений и страница расходились бы, а карантин попадал бы в вывод.
    if use_fts5 {
        let mut statement = read
            .transaction()
            .prepare(
                "SELECT f.case_id
                 FROM learning_search_fts AS f
                 JOIN learning_search AS s ON s.case_id = f.case_id
                 JOIN learning_import AS i ON i.review_id = s.review_id
                 WHERE learning_search_fts MATCH ?1
                   AND (?2 = 1 OR i.trust = 'ast_authenticated')",
            )
            .map_err(|error| map_error(&error, "не удалось подготовить текстовый поиск"))?;
        let rows = statement
            .query_map(
                params![fts_query(text), i64::from(query.include_quarantine)],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| map_error(&error, "не удалось выполнить текстовый поиск"))?;
        for row in rows {
            hits.insert(
                row.map_err(|error| map_error(&error, "не удалось выполнить текстовый поиск"))?,
            );
        }
        return Ok(hits);
    }
    let needle = text.to_lowercase();
    let mut statement = read
        .transaction()
        .prepare(
            "SELECT s.case_id, s.text
             FROM learning_search AS s
             JOIN learning_import AS i ON i.review_id = s.review_id
             WHERE (?1 = 1 OR i.trust = 'ast_authenticated')",
        )
        .map_err(|error| map_error(&error, "не удалось подготовить текстовый поиск"))?;
    let rows = statement
        .query_map(params![i64::from(query.include_quarantine)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| map_error(&error, "не удалось выполнить текстовый поиск"))?;
    for row in rows {
        let (case_id, stored_text) =
            row.map_err(|error| map_error(&error, "не удалось выполнить текстовый поиск"))?;
        if stored_text.to_lowercase().contains(&needle) {
            hits.insert(case_id);
        }
    }
    Ok(hits)
}

/// Экранирует пользовательскую подстроку как фразу FTS5.
///
/// Текст никогда не подставляется в SQL: он передаётся параметром, а кавычки
/// удваиваются, чтобы фраза не могла стать выражением FTS5.
fn fts_query(text: &str) -> String {
    let escaped = text.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// Структурные совпадения по сохранённым классификациям.
fn structural_matches(
    read: &super::store::LearningRead<'_>,
    query: &SearchQuery,
) -> Result<BTreeSet<String>, DomainError> {
    let mut statement = read
        .transaction()
        .prepare(
            "SELECT s.case_id
             FROM learning_search AS s
             JOIN learning_import AS i ON i.review_id = s.review_id
             LEFT JOIN learning_unit AS u ON u.review_id = s.review_id AND u.unit_id = s.unit_id
             LEFT JOIN learning_candidate AS c
                ON c.review_id = s.review_id AND c.candidate_id = s.candidate_id
             LEFT JOIN learning_finding AS f
                ON f.review_id = s.review_id AND f.finding_id = s.finding_id
             WHERE (?1 IS NULL OR u.detector = ?1)
               AND (?2 IS NULL OR c.execution = ?2)
               AND (?3 IS NULL OR c.origin = ?3)
               AND (?4 IS NULL OR u.role = ?4)
               AND (?5 IS NULL OR u.code_role = ?5)
               AND (?6 IS NULL OR s.disposition = ?6)
               AND (?7 IS NULL OR s.provenance = ?7)
               AND (?8 IS NULL OR s.severity = ?8)
               AND (?9 IS NULL OR i.repository_id = ?9)
               AND (?10 = 1 OR i.trust = 'ast_authenticated')",
        )
        .map_err(|error| map_error(&error, "не удалось подготовить структурный поиск"))?;
    let rows = statement
        .query_map(
            params![
                query.detector,
                query.surface,
                query.origin,
                query.role,
                query.code_role,
                query.disposition,
                query.provenance,
                query.severity,
                query.repository_id,
                i64::from(query.include_quarantine),
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| map_error(&error, "не удалось выполнить структурный поиск"))?;
    let mut hits = BTreeSet::new();
    for row in rows {
        hits.insert(
            row.map_err(|error| map_error(&error, "не удалось выполнить структурный поиск"))?,
        );
    }
    Ok(hits)
}

fn load_case(
    read: &super::store::LearningRead<'_>,
    case_id: &str,
    match_kind: SearchMatchKind,
) -> Result<Option<SearchCase>, DomainError> {
    read.transaction()
        .query_row(
            "SELECT s.case_id, s.review_id, s.unit_id, s.candidate_id, s.finding_id, s.kind, s.text,
                    i.repository_id, i.head_sha, i.trust,
                    u.disposition, f.provenance, f.severity, u.detector, c.path
             FROM learning_search AS s
             JOIN learning_import AS i ON i.review_id = s.review_id
             LEFT JOIN learning_unit AS u ON u.review_id = s.review_id AND u.unit_id = s.unit_id
             LEFT JOIN learning_finding AS f
                ON f.review_id = s.review_id AND f.finding_id = s.finding_id
             LEFT JOIN learning_candidate AS c
                ON c.review_id = s.review_id AND c.candidate_id = s.candidate_id
             WHERE s.case_id = ?1",
            params![case_id],
            |row| {
                let text: String = row.get(6)?;
                let repository: String = row.get(7)?;
                let head: String = row.get(8)?;
                Ok(SearchCase {
                    case_id: row.get(0)?,
                    review_id: row.get(1)?,
                    unit_id: row.get(2)?,
                    candidate_id: row.get(3)?,
                    finding_id: row.get(4)?,
                    kind: row.get(5)?,
                    match_kind,
                    snippet: compact_snippet(&text),
                    disposition: row.get(10)?,
                    provenance: row.get(11)?,
                    severity: row.get(12)?,
                    detector: row.get(13)?,
                    path: row.get(14)?,
                    source_reference: format!(
                        "{repository}@{head}: review.json и review-queue.json"
                    ),
                    trust: row.get(9)?,
                })
            },
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(map_error(&other, "не удалось прочитать найденный случай")),
        })
}

/// Ограничивает фрагмент текста вывода.
fn compact_snippet(text: &str) -> String {
    let mut snippet = text.trim().to_owned();
    if snippet.chars().count() > MAX_STORED_SNIPPET_BYTES {
        snippet = snippet.chars().take(MAX_STORED_SNIPPET_BYTES).collect();
        snippet.push('…');
    }
    snippet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_is_escaped_as_a_phrase() {
        assert_eq!(fts_query("unwrap"), "\"unwrap\"");
        assert_eq!(fts_query("a\" OR b"), "\"a\"\" OR b\"");
        assert_eq!(fts_query("NEAR(x)"), "\"NEAR(x)\"");
    }

    #[test]
    fn snippet_is_bounded() {
        let long = "я".repeat(MAX_STORED_SNIPPET_BYTES + 10);
        let snippet = compact_snippet(&long);
        assert_eq!(snippet.chars().count(), MAX_STORED_SNIPPET_BYTES + 1);
        assert!(snippet.ends_with('…'));
    }
}
