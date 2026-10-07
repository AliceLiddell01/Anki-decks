use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const DEFAULT_POLICY: &str = include_str!("../policy.toml");
const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub target_warning_gib: u64,
    pub target_hard_limit_gib: u64,
    #[serde(default)]
    pub target_warning_bytes: Option<u64>,
    #[serde(default)]
    pub target_hard_limit_bytes: Option<u64>,
    pub external_cache_hard_limit_gib: u64,
    pub external_cache_goal_gib: u64,
    #[serde(default)]
    pub external_cache_hard_limit_bytes: Option<u64>,
    #[serde(default)]
    pub external_cache_goal_bytes: Option<u64>,
    pub external_cache_min_age_days: u64,
    #[serde(default)]
    pub external_cache_min_age_seconds: Option<u64>,
    pub orphan_min_age_hours: u64,
    #[serde(default)]
    pub orphan_min_age_seconds: Option<u64>,
    pub tmp_top_entries: usize,
}

impl Policy {
    pub fn defaults() -> Result<Self, String> {
        let policy: Self = toml::from_str(DEFAULT_POLICY)
            .map_err(|error| format!("встроенная политика GC повреждена: {error}"))?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn from_path(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|error| {
            format!("не удалось прочитать политику {}: {error}", path.display())
        })?;
        let policy: Self = toml::from_str(&text).map_err(|error| {
            format!("не удалось разобрать политику {}: {error}", path.display())
        })?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn warning_bytes(&self) -> u64 {
        self.target_warning_bytes
            .unwrap_or_else(|| self.target_warning_gib.saturating_mul(GIB))
    }

    pub fn hard_limit_bytes(&self) -> u64 {
        self.target_hard_limit_bytes
            .unwrap_or_else(|| self.target_hard_limit_gib.saturating_mul(GIB))
    }

    pub fn external_hard_limit_bytes(&self) -> u64 {
        self.external_cache_hard_limit_bytes
            .unwrap_or_else(|| self.external_cache_hard_limit_gib.saturating_mul(GIB))
    }

    pub fn external_goal_bytes(&self) -> u64 {
        self.external_cache_goal_bytes
            .unwrap_or_else(|| self.external_cache_goal_gib.saturating_mul(GIB))
    }

    pub fn orphan_min_age(&self) -> Duration {
        Duration::from_secs(
            self.orphan_min_age_seconds
                .unwrap_or_else(|| self.orphan_min_age_hours.saturating_mul(60 * 60)),
        )
    }

    pub fn external_cache_min_age(&self) -> Duration {
        Duration::from_secs(self.external_cache_min_age_seconds.unwrap_or_else(|| {
            self.external_cache_min_age_days
                .saturating_mul(24 * 60 * 60)
        }))
    }

    fn validate(&self) -> Result<(), String> {
        if self.warning_bytes() == 0
            || self.warning_bytes() >= self.hard_limit_bytes()
            || self.external_goal_bytes() > self.external_hard_limit_bytes()
            || self.external_cache_min_age().is_zero()
            || self.orphan_min_age().is_zero()
            || self.tmp_top_entries == 0
            || self.tmp_top_entries > 500
        {
            return Err(
                "значения политики GC противоречат друг другу или вне допустимых границ".into(),
            );
        }
        Ok(())
    }
}

pub fn workspace_root(
    explicit: Option<&Path>,
    installed: bool,
) -> Result<std::path::PathBuf, String> {
    if let Some(path) = explicit {
        return canonical_workspace(path);
    }
    if let Some(value) = std::env::var_os("ANKI_REPOSITORY_ROOT") {
        return canonical_workspace(Path::new(&value));
    }
    if installed && let Some(path) = installed_root_config()? {
        return canonical_workspace(&path);
    }
    let mut cursor = std::env::current_dir().map_err(|error| error.to_string())?;
    loop {
        if cursor.join("Cargo.toml").is_file() {
            return canonical_workspace(&cursor);
        }
        if !cursor.pop() {
            break;
        }
    }
    Err("не удалось найти рабочую область Cargo; укажите --workspace-root".into())
}

fn canonical_workspace(path: &Path) -> Result<std::path::PathBuf, String> {
    let root = path.canonicalize().map_err(|error| {
        format!(
            "не удалось определить корень рабочей области {}: {error}",
            path.display()
        )
    })?;
    if !root.join("Cargo.toml").is_file() {
        return Err(format!("в {} нет Cargo.toml", root.display()));
    }
    Ok(root)
}

fn installed_root_config() -> Result<Option<std::path::PathBuf>, String> {
    let Some(config_home) = config_home() else {
        return Ok(None);
    };
    let path = config_home.join("anki-decks/repository-maintenance-install.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("не удалось прочитать {}: {error}", path.display())),
    };
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct InstalledRoot {
        managed_by: String,
        schema: u32,
        repository_root: std::path::PathBuf,
    }
    let config: InstalledRoot = toml::from_str(&text).map_err(|error| {
        format!(
            "повреждена конфигурация установки {}: {error}",
            path.display()
        )
    })?;
    if config.schema != 1 || config.managed_by != "repository-maintenance" {
        return Err(format!(
            "неподдерживаемая версия конфигурации установки {}",
            path.display()
        ));
    }
    Ok(Some(config.repository_root))
}

pub fn config_home() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })
}

pub fn cache_home() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_thresholds_are_configurable_and_validated() {
        let policy: Policy = toml::from_str(
            "target_warning_gib=1\ntarget_hard_limit_gib=2\nexternal_cache_hard_limit_gib=3\nexternal_cache_goal_gib=1\nexternal_cache_min_age_days=7\norphan_min_age_hours=24\ntmp_top_entries=4\n",
        )
        .unwrap();
        policy.validate().unwrap();
        assert_eq!(policy.warning_bytes(), GIB);
        assert_eq!(policy.hard_limit_bytes(), GIB * 2);
        assert_eq!(policy.external_goal_bytes(), GIB);
        assert_eq!(policy.external_hard_limit_bytes(), GIB * 3);
    }

    #[test]
    fn invalid_hysteresis_is_rejected() {
        let policy = Policy {
            target_warning_gib: 20,
            target_hard_limit_gib: 20,
            target_warning_bytes: None,
            target_hard_limit_bytes: None,
            external_cache_hard_limit_gib: 5,
            external_cache_goal_gib: 6,
            external_cache_hard_limit_bytes: None,
            external_cache_goal_bytes: None,
            external_cache_min_age_days: 0,
            external_cache_min_age_seconds: None,
            orphan_min_age_hours: 0,
            orphan_min_age_seconds: None,
            tmp_top_entries: 0,
        };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn byte_and_second_overrides_allow_small_reproducible_test_limits() {
        let policy: Policy = toml::from_str(
            "target_warning_gib=1\ntarget_hard_limit_gib=2\ntarget_warning_bytes=1\ntarget_hard_limit_bytes=2\nexternal_cache_hard_limit_gib=3\nexternal_cache_goal_gib=1\nexternal_cache_hard_limit_bytes=3\nexternal_cache_goal_bytes=1\nexternal_cache_min_age_days=7\nexternal_cache_min_age_seconds=4\norphan_min_age_hours=24\norphan_min_age_seconds=5\ntmp_top_entries=4\n",
        )
        .unwrap();
        policy.validate().unwrap();
        assert_eq!(policy.hard_limit_bytes(), 2);
        assert_eq!(policy.external_hard_limit_bytes(), 3);
        assert_eq!(policy.orphan_min_age(), Duration::from_secs(5));
        assert_eq!(policy.external_cache_min_age(), Duration::from_secs(4));
    }
}
