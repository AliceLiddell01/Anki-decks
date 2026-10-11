//! Версионируемая read-only граница: неизвестное значение остаётся `null`.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    Missing,
    Incomplete,
    Invalid,
    Unavailable,
    NotObserved,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provenance {
    pub path: String,
    pub sha256: Option<String>,
    pub modified_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub api_version: u32,
    pub state: Availability,
    pub data: Option<T>,
    pub provenance: Vec<Provenance>,
    pub diagnostics: Vec<String>,
}
impl<T> Envelope<T> {
    pub fn available(data: T) -> Self {
        Self {
            api_version: 1,
            state: Availability::Available,
            data: Some(data),
            provenance: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
    pub fn absent(state: Availability, message: impl Into<String>) -> Self {
        Self {
            api_version: 1,
            state,
            data: None,
            provenance: Vec::new(),
            diagnostics: vec![message.into()],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub page: usize,
    pub limit: usize,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub page: Option<usize>,
    pub limit: Option<usize>,
    pub q: Option<String>,
    pub sort: Option<String>,
    pub priority: Option<String>,
    pub detector: Option<String>,
    pub role: Option<String>,
    pub unit_id: Option<String>,
    pub review_id: Option<String>,
    pub unknown: Option<bool>,
    pub disposition: Option<String>,
    pub surface: Option<String>,
}
impl Query {
    pub fn bounds(&self) -> Result<(usize, usize), String> {
        let page = self.page.unwrap_or(1);
        let limit = self.limit.unwrap_or(50);
        if page == 0 || page > 1_000_000 || limit == 0 || limit > 100 {
            return Err("page: 1..1000000, limit: 1..100".into());
        }
        for value in [
            &self.q,
            &self.sort,
            &self.priority,
            &self.detector,
            &self.role,
            &self.unit_id,
            &self.review_id,
            &self.disposition,
            &self.surface,
        ]
        .into_iter()
        .flatten()
        {
            if value.len() > 512 || value.chars().any(char::is_control) {
                return Err("Фильтр слишком длинный или содержит управляющие символы".into());
            }
        }
        Ok((page, limit))
    }
    pub fn matches(&self, value: &str) -> bool {
        self.q
            .as_ref()
            .is_none_or(|q| value.to_lowercase().contains(&q.to_lowercase()))
    }
    pub fn paginate<T>(&self, items: impl IntoIterator<Item = T>) -> Result<Page<T>, String> {
        let (page, limit) = self.bounds()?;
        let items: Vec<T> = items.into_iter().collect();
        let total = items.len();
        Ok(Page {
            items: items
                .into_iter()
                .skip((page - 1) * limit)
                .take(limit)
                .collect(),
            total,
            page,
            limit,
        })
    }
}
