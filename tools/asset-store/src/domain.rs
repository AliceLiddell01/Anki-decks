//! Domain policy для canonical asset location и consumer-facing имён.

use std::path::{Component, Path};

use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::kanji_domain::parse_kanji_character;
use crate::model::{AssetIdentity, AssetRecord, DetectedFormat};

const KANJI_MAX_ASSET_BYTES: u64 = 8 * 1024 * 1024;

/// Внутреннее расположение asset и имя, которое получает конечный consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalAssetLocation {
    pub storage_path: String,
    pub consumer_filename: String,
}

/// Правила одного независимого asset domain.
///
/// Реализации отвечают за identity, допустимый формат и детерминированный
/// canonical layout. `legacy_location` используется только для чтения и
/// миграции старых manifest; новые записи всегда строятся через
/// `canonical_location`.
pub trait AssetDomainPolicy: std::fmt::Debug + Send + Sync {
    /// Стабильный идентификатор domain, сохраняемый вместе со store.
    fn domain_id(&self) -> &'static str;

    /// Проверяет принадлежность identity этому domain.
    fn validate_identity(&self, identity: &AssetIdentity) -> Result<(), AssetError>;

    /// Возвращает canonical путь внутри store и плоское имя для consumer.
    fn canonical_location(
        &self,
        identity: &AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Result<CanonicalAssetLocation, AssetError>;

    /// Возвращает расположение, использовавшееся прежней схемой, если domain
    /// существовал в ней.
    fn legacy_location(
        &self,
        _identity: &AssetIdentity,
        _sha256: &str,
        _format: DetectedFormat,
    ) -> Option<CanonicalAssetLocation> {
        None
    }

    /// Можно ли хранить этот фактический формат в canonical corpus domain.
    fn is_publishable_format(&self, format: DetectedFormat) -> bool;

    /// Может ли semantic validator поместить такой формат в verified lifecycle.
    /// Generic domain по умолчанию принимает любой фактически определённый
    /// формат, предметные domains сужают этот набор.
    fn allows_verified_format(&self, _format: DetectedFormat) -> bool {
        true
    }

    /// Domain-specific предел размера asset, если он установлен.
    fn max_asset_bytes(&self) -> Option<u64>;

    /// Использует ли domain hash-suffixed immutable object storage вместо
    /// стабильного имени с compare-and-swap публикацией.
    fn content_addressed_storage(&self) -> bool;

    /// Дополнительные имена orphan-файлов, запрещённые предметной областью.
    /// Generic core вызывает правило только для незарегистрированных
    /// content-addressed объектов в canonical area.
    fn is_reserved_orphan_filename(&self, _filename: &str) -> bool {
        false
    }

    /// Проверяет полную canonical location записи перед публикацией.
    fn validate_publishable_record(&self, record: &AssetRecord) -> Result<(), AssetError> {
        self.validate_identity(&record.identity)?;
        if !self.is_publishable_format(record.format) {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "формат {:?} не разрешён для domain {}",
                    record.format,
                    self.domain_id()
                ),
            ));
        }
        let expected = self.canonical_location(&record.identity, &record.sha256, record.format)?;
        validate_safe_consumer_filename(&expected.consumer_filename, record.format)?;
        validate_safe_storage_path(&expected.storage_path)?;
        if record.storage_path != expected.storage_path
            || record.consumer_filename != expected.consumer_filename
        {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "canonical location записи {} не совпадает с policy domain {}",
                    record.identity,
                    self.domain_id()
                ),
            ));
        }
        Ok(())
    }
}

/// Политика для namespace без предметного publishable domain.
///
/// Для совместимости сохраняет прежний hash-suffixed layout. Зарезервированные
/// publishable namespaces должны открываться своей предметной policy.
#[derive(Debug, Clone, Copy, Default)]
pub struct GenericDomainPolicy;

impl AssetDomainPolicy for GenericDomainPolicy {
    fn domain_id(&self) -> &'static str {
        "generic"
    }

    fn validate_identity(&self, identity: &AssetIdentity) -> Result<(), AssetError> {
        identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        if matches!(identity.namespace.as_str(), "kanji" | "pitch_accent") {
            return Err(AssetError::new(
                ErrorCode::InvalidIdentity,
                format!(
                    "namespace {} должен использовать собственный asset domain",
                    identity.namespace
                ),
            ));
        }
        Ok(())
    }

    fn canonical_location(
        &self,
        identity: &AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Result<CanonicalAssetLocation, AssetError> {
        self.validate_identity(identity)?;
        validate_sha256(sha256)?;
        let key_hash = sha256_hex(identity.key.as_bytes());
        let prefix = format!("{}-{}", identity.namespace, &key_hash[..16]);
        let filename = format!("{prefix}-{sha256}.{}", extension_for_format(format));
        validate_safe_consumer_filename(&filename, format)?;
        Ok(CanonicalAssetLocation {
            storage_path: format!("assets/{filename}"),
            consumer_filename: filename,
        })
    }

    fn legacy_location(
        &self,
        identity: &AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Option<CanonicalAssetLocation> {
        self.canonical_location(identity, sha256, format).ok()
    }

    fn is_publishable_format(&self, _format: DetectedFormat) -> bool {
        // Generic objects can use the lifecycle store, but they never enter a
        // publishable repository corpus.
        false
    }

    fn max_asset_bytes(&self) -> Option<u64> {
        None
    }

    fn content_addressed_storage(&self) -> bool {
        true
    }
}

/// Политика canonical corpus для изображений кандзи.
#[derive(Debug, Clone, Copy, Default)]
pub struct KanjiDomainPolicy;

impl AssetDomainPolicy for KanjiDomainPolicy {
    fn domain_id(&self) -> &'static str {
        "kanji"
    }

    fn validate_identity(&self, identity: &AssetIdentity) -> Result<(), AssetError> {
        identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        if identity.namespace != "kanji" {
            return Err(AssetError::new(
                ErrorCode::InvalidIdentity,
                "kanji domain принимает только namespace `kanji`",
            ));
        }
        parse_kanji_character(&identity.key)
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        Ok(())
    }

    fn canonical_location(
        &self,
        identity: &AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Result<CanonicalAssetLocation, AssetError> {
        self.validate_identity(identity)?;
        validate_sha256(sha256)?;
        let filename = format!("{}.{}", identity.key, extension_for_format(format));
        validate_safe_consumer_filename(&filename, format)?;
        let directory = extension_for_format(format);
        Ok(CanonicalAssetLocation {
            storage_path: format!("assets/{directory}/{filename}"),
            consumer_filename: filename,
        })
    }

    fn legacy_location(
        &self,
        identity: &AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Option<CanonicalAssetLocation> {
        self.validate_identity(identity).ok()?;
        validate_sha256(sha256).ok()?;
        let filename = format!("{}.{}", identity.key, extension_for_format(format));
        Some(CanonicalAssetLocation {
            storage_path: format!("assets/{filename}"),
            consumer_filename: filename,
        })
    }

    fn is_publishable_format(&self, format: DetectedFormat) -> bool {
        matches!(format, DetectedFormat::Gif | DetectedFormat::Png)
    }

    fn allows_verified_format(&self, format: DetectedFormat) -> bool {
        matches!(format, DetectedFormat::Gif | DetectedFormat::Png)
    }

    fn max_asset_bytes(&self) -> Option<u64> {
        Some(KANJI_MAX_ASSET_BYTES)
    }

    fn content_addressed_storage(&self) -> bool {
        false
    }

    fn is_reserved_orphan_filename(&self, filename: &str) -> bool {
        is_legacy_hash_suffixed_kanji_filename(filename)
    }
}

fn is_legacy_hash_suffixed_kanji_filename(filename: &str) -> bool {
    let Some((stem, _)) = filename.rsplit_once('.') else {
        return false;
    };
    let Some((prefix, hash)) = stem.rsplit_once('-') else {
        return false;
    };
    hash.len() == 64 && prefix.chars().count() == 1
}

/// Проверяет, что consumer filename является безопасным одноуровневым именем
/// и его расширение соответствует фактическому формату bytes.
pub fn validate_safe_consumer_filename(
    filename: &str,
    format: DetectedFormat,
) -> Result<(), AssetError> {
    let invalid = || {
        AssetError::new(
            ErrorCode::PathTraversal,
            "consumer filename должен быть безопасным одноуровневым именем",
        )
    };
    if filename.is_empty()
        || filename.len() > 255
        || filename == "."
        || filename == ".."
        || filename.contains(['/', '\\', ':'])
        || filename.chars().any(char::is_control)
        || filename.ends_with(['.', ' '])
    {
        return Err(invalid());
    }
    let path = Path::new(filename);
    if path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(invalid());
    }
    let (stem, extension) = filename.rsplit_once('.').ok_or_else(invalid)?;
    if stem.is_empty() || extension != extension_for_format(format) {
        return Err(invalid());
    }
    let device_name = stem.split('.').next().unwrap_or(stem).to_ascii_uppercase();
    if matches!(device_name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (device_name.len() == 4
            && (device_name.starts_with("COM") || device_name.starts_with("LPT"))
            && device_name.as_bytes()[3].is_ascii_digit()
            && device_name.as_bytes()[3] != b'0')
    {
        return Err(invalid());
    }
    Ok(())
}

/// Проверяет лексически безопасный store-relative путь внутри `assets/`.
pub fn validate_safe_storage_path(storage_path: &str) -> Result<(), AssetError> {
    let invalid = || {
        AssetError::new(
            ErrorCode::PathTraversal,
            "storage_path должен оставаться внутри каталога assets",
        )
    };
    if storage_path.is_empty()
        || storage_path.contains('\\')
        || storage_path.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let mut components = storage_path.split('/');
    if components.next() != Some("assets") {
        return Err(invalid());
    }
    let mut count = 1;
    for component in components {
        count += 1;
        if component.is_empty() || matches!(component, "." | "..") || component.contains(':') {
            return Err(invalid());
        }
    }
    if count < 2 || Path::new(storage_path).is_absolute() {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn extension_for_format(format: DetectedFormat) -> &'static str {
    match format {
        DetectedFormat::Png => "png",
        DetectedFormat::Jpeg => "jpg",
        DetectedFormat::Gif => "gif",
        DetectedFormat::Webp => "webp",
        DetectedFormat::Bmp => "bmp",
        DetectedFormat::Tiff => "tiff",
        DetectedFormat::Unknown => "bin",
    }
}

fn validate_sha256(sha256: &str) -> Result<(), AssetError> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "SHA-256 должен состоять из 64 строчных hex-символов",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn kanji_locations_separate_format_and_keep_flat_consumer_names() {
        let policy = KanjiDomainPolicy;
        let gif = AssetIdentity::new("kanji", "漢").unwrap();
        let png = AssetIdentity::new("kanji", "饅").unwrap();

        assert_eq!(
            policy
                .canonical_location(&gif, HASH, DetectedFormat::Gif)
                .unwrap(),
            CanonicalAssetLocation {
                storage_path: "assets/gif/漢.gif".into(),
                consumer_filename: "漢.gif".into(),
            }
        );
        assert_eq!(
            policy
                .canonical_location(&png, HASH, DetectedFormat::Png)
                .unwrap(),
            CanonicalAssetLocation {
                storage_path: "assets/png/饅.png".into(),
                consumer_filename: "饅.png".into(),
            }
        );
        assert_eq!(
            policy
                .legacy_location(&gif, HASH, DetectedFormat::Gif)
                .unwrap()
                .storage_path,
            "assets/漢.gif"
        );
        let unsupported = policy
            .canonical_location(&gif, HASH, DetectedFormat::Jpeg)
            .unwrap();
        assert_eq!(unsupported.storage_path, "assets/jpg/漢.jpg");
        assert_eq!(unsupported.consumer_filename, "漢.jpg");
        assert!(!policy.is_publishable_format(DetectedFormat::Jpeg));
        assert!(!policy.allows_verified_format(DetectedFormat::Jpeg));
        assert!(policy.allows_verified_format(DetectedFormat::Gif));
        assert!(policy.allows_verified_format(DetectedFormat::Png));
    }

    #[test]
    fn generic_locations_preserve_hash_suffixed_layout() {
        let policy = GenericDomainPolicy;
        let identity = AssetIdentity::new("generic", "one").unwrap();
        let location = policy
            .canonical_location(&identity, HASH, DetectedFormat::Png)
            .unwrap();
        let key_hash = sha256_hex(b"one");
        assert_eq!(
            location.storage_path,
            format!("assets/generic-{}-{HASH}.png", &key_hash[..16])
        );
        assert_eq!(
            location.storage_path,
            format!("assets/{}", location.consumer_filename)
        );
        assert!(policy.content_addressed_storage());
        assert!(!policy.is_publishable_format(DetectedFormat::Png));
        assert!(policy.allows_verified_format(DetectedFormat::Png));
    }

    #[test]
    fn reserved_namespaces_and_unsafe_consumer_names_fail_closed() {
        let generic = GenericDomainPolicy;
        for namespace in ["kanji", "pitch_accent"] {
            let identity = AssetIdentity::new(namespace, "漢").unwrap();
            assert!(generic.validate_identity(&identity).is_err());
        }
        for filename in [
            "../漢.png",
            "dir/漢.png",
            r"dir\漢.png",
            "C:漢.png",
            "CON.png",
            "漢.gif",
            "漢.png\n",
        ] {
            assert!(
                validate_safe_consumer_filename(filename, DetectedFormat::Png).is_err(),
                "имя {filename:?} должно быть отклонено"
            );
        }
    }
}
