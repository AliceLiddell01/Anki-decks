//! Versioned state и доменные типы, общие для всех asset consumers.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Версия формата persistent manifest.
pub const MANIFEST_SCHEMA_VERSION: u32 = 3;

/// Логическая identity asset; имя файла в identity не участвует.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetIdentity {
    /// Тип или namespace asset, например `kanji`.
    pub namespace: String,
    /// Стабильный ключ, заданный вызывающей предметной областью.
    pub key: String,
}

impl AssetIdentity {
    /// Создаёт и проверяет логическую identity.
    pub fn new(namespace: impl Into<String>, key: impl Into<String>) -> Result<Self, String> {
        let identity = Self {
            namespace: namespace.into(),
            key: key.into(),
        };
        identity.validate()?;
        Ok(identity)
    }

    /// Проверяет безопасные ограничения формата identity.
    pub fn validate(&self) -> Result<(), String> {
        let namespace_ok = !self.namespace.is_empty()
            && self.namespace.len() <= 64
            && self.namespace.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            });
        if !namespace_ok {
            return Err(
                "namespace должен содержать до 64 символов a-z, 0-9, '.', '_' или '-'".into(),
            );
        }
        if self.key.is_empty() || self.key.len() > 512 || self.key.chars().any(char::is_control) {
            return Err(
                "key должен быть непустым, не длиннее 512 байт и без управляющих символов".into(),
            );
        }
        Ok(())
    }
}

impl fmt::Display for AssetIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.namespace, self.key)
    }
}

/// Установленный по magic bytes формат, не зависящий от расширения исходника.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectedFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
    Tiff,
    Unknown,
}

impl DetectedFormat {
    /// Классифицирует распространённые бинарные форматы по сигнатуре.
    pub fn from_signature(bytes: &[u8]) -> Self {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Self::Png
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Self::Jpeg
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Self::Gif
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Self::Webp
        } else if bytes.starts_with(b"BM") {
            Self::Bmp
        } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
            Self::Tiff
        } else {
            Self::Unknown
        }
    }
}

/// Физическое положение файла в пределах store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Pending,
    Verified,
    Quarantined,
}

impl LifecycleState {
    /// Стабильное machine-readable имя состояния.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verified => "verified",
            Self::Quarantined => "quarantined",
        }
    }
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Результат semantic decision внешнего domain validator'а.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticStatus {
    Verified,
    Rejected,
    Uncertain,
    Corrupt,
}

impl SemanticStatus {
    /// Стабильное machine-readable имя результата.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Rejected => "rejected",
            Self::Uncertain => "uncertain",
            Self::Corrupt => "corrupt",
        }
    }
}

/// Идентичность и версия semantic validator'а.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorIdentity {
    pub id: String,
    pub version: String,
}

impl ValidatorIdentity {
    /// Создаёт и проверяет явную пару id/version.
    pub fn new(id: impl Into<String>, version: impl Into<String>) -> Result<Self, String> {
        let identity = Self {
            id: id.into(),
            version: version.into(),
        };
        if identity.id.is_empty()
            || identity.version.is_empty()
            || identity.id.len() > 128
            || identity.version.len() > 128
            || identity.id.chars().any(char::is_control)
            || identity.version.chars().any(char::is_control)
        {
            return Err("validator id/version должны быть непустыми строками до 128 байт".into());
        }
        Ok(identity)
    }
}

/// Одно проверяемое свидетельство domain validator'а.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationEvidence {
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Не сохранённый ещё ответ validator'а; его принимает только lifecycle API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDecision {
    pub status: SemanticStatus,
    pub evidence: Vec<ValidationEvidence>,
}

impl SemanticDecision {
    /// Создаёт решение с явным status и evidence.
    pub fn new(status: SemanticStatus, evidence: Vec<ValidationEvidence>) -> Self {
        Self { status, evidence }
    }
}

/// Provenance источника bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// Стабильный тип источника; поддерживается `local_import`.
    pub source_kind: String,
    /// Имя явно переданного файла; абсолютный host path не сохраняется.
    pub source_name: String,
}

/// Semantic decision, привязанный к hash и версии конкретного validator'а.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationRecord {
    pub status: SemanticStatus,
    pub validator: ValidatorIdentity,
    pub content_sha256: String,
    pub evidence: Vec<ValidationEvidence>,
}

/// Одна актуальная версия логического asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetRecord {
    pub identity: AssetIdentity,
    /// Relative path сохраняется явно и при чтении сверяется с lifecycle/hash.
    pub storage_path: String,
    pub sha256: String,
    pub byte_length: u64,
    pub format: DetectedFormat,
    pub provenance: Provenance,
    pub lifecycle: LifecycleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationRecord>,
    /// Kanji consumer хранит здесь character и Unicode code points; generic core
    /// сохраняет extension metadata без интерпретации.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_metadata: Option<serde_json::Value>,
}

/// Канонический persistent manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub store_id: String,
    pub revision: u64,
    pub assets: Vec<AssetRecord>,
}

impl Manifest {
    /// Пустое состояние нового store.
    pub fn empty(store_id: String) -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            store_id,
            revision: 0,
            assets: Vec::new(),
        }
    }
}
