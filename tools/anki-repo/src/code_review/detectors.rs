//! Детерминированные эвристики для подготовки свидетельств к независимому code review.
//!
//! Результат этого модуля — только кандидаты и свидетельства. Он не подтверждает
//! дефект и не создаёт замечание: каждый сигнал требует проверки по требованиям,
//! контрактам и окружающему коду.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::scope::LineRange;

const SOURCE: &str = "anki_repo.code_review.detectors.v1";

/// Git-состояние одного файла из сравниваемого диапазона.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    /// Поддерживается интерфейсом детектора для входов из неразрешённого индекса Git.
    Unmerged,
    Unknown,
}

/// Данные одного изменённого файла для локальных детекторов.
///
/// Тексты — полные версии базового и нового образа, а не фрагменты diff. Для добавленного или
/// удалённого файла отсутствующее изображение задаётся через `None`. Сам
/// модуль не читает файловую систему и не разрешает refs Git.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInput {
    /// Путь в новой версии; для удаления вызывающая сторона может передать старый путь.
    pub path: String,
    /// Старый путь при rename/copy, если он известен.
    pub previous_path: Option<String>,
    /// Статус файла в диапазоне.
    pub status: FileStatus,
    /// Полный текст файла в базовом коммите, если файл там существовал и был текстовым.
    pub base_text: Option<String>,
    /// Полный текст файла в новом коммите, если файл там существует и является текстовым.
    pub post_text: Option<String>,
    /// Доказанные Git-диапазоны изменённых строк новой версии (верхняя граница исключена).
    /// `None` означает, что позиционное сравнение не было выполнено.
    #[serde(default)]
    pub post_changed_lines: Option<Vec<LineRange>>,
}

/// Происхождение сигнала относительно проверяемого диапазона.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOrigin {
    /// Текст сигнала добавлен или изменён этим диапазоном.
    IntroducedOrChanged,
    /// Этот occurrence строки не пересекает доказанные изменённые диапазоны.
    PreExisting,
    /// По доступным изображениям или статусу нельзя доказать происхождение.
    Unknown,
}

/// Устойчивый тип свидетельства-кандидата.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateType {
    AbsolutePath,
    LocalEndpoint,
    DevelopmentReference,
    RustSuppression,
    CiSuppression,
    ErrorPath,
    UnsafePath,
    TestAdded,
    TestRemoved,
    TestIgnored,
    AssertionRemoved,
    DependencyChange,
    ConfigSurface,
    GeneratedSurface,
    SkillSurface,
    SecuritySurface,
}

impl CandidateType {
    /// Стабильное имя типа в машинном API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AbsolutePath => "absolute_path",
            Self::LocalEndpoint => "local_endpoint",
            Self::DevelopmentReference => "development_reference",
            Self::RustSuppression => "rust_suppression",
            Self::CiSuppression => "ci_suppression",
            Self::ErrorPath => "error_path",
            Self::UnsafePath => "unsafe_path",
            Self::TestAdded => "test_added",
            Self::TestRemoved => "test_removed",
            Self::TestIgnored => "test_ignored",
            Self::AssertionRemoved => "assertion_removed",
            Self::DependencyChange => "dependency_change",
            Self::ConfigSurface => "config_surface",
            Self::GeneratedSurface => "generated_surface",
            Self::SkillSurface => "skill_surface",
            Self::SecuritySurface => "security_surface",
        }
    }
}

/// Машинно-читаемые кандидат и свидетельство. Это не подтверждённое замечание.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    /// Детерминированный идентификатор без случайного UUID.
    pub id: String,
    /// В JSON поле называется `type`.
    #[serde(rename = "type")]
    pub candidate_type: CandidateType,
    /// Путь в новой версии (либо старый путь удалённого файла).
    pub path: String,
    /// Номер строки с единицы; для file-level surface отсутствует.
    pub line: Option<usize>,
    /// Короткая исходная строка или diff-свидетельство.
    pub snippet: Option<String>,
    /// Происхождение сигнала относительно исходного образа.
    pub origin: CandidateOrigin,
    /// Устойчивые машинные коды причин, отсортированные лексикографически.
    pub signals: Vec<String>,
    /// Источник кандидатов детекторов этой версии.
    pub source: String,
    /// Дополнительные сведения; BTreeMap обеспечивает стабильный порядок ключей.
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug)]
struct CandidateAnnotations {
    signals: Vec<String>,
    metadata: BTreeMap<String, String>,
}

/// Собирает кандидаты для списка изменённых файлов.
///
/// Сканирование работает только с переданными исходным и новым образами. Выход отсортирован
/// по пути, типу, строке и идентификатору. `pre_existing` определяется по
/// неизменённому вхождению строки, а не по совпадению текста где-либо в исходном образе.
#[must_use]
pub fn detect(inputs: &[FileInput]) -> Vec<Candidate> {
    let candidates = inputs.iter().flat_map(detect_file).collect::<Vec<_>>();
    sort_candidates(candidates)
}

/// Собирает ограниченные сигналы-кандидаты для одного изменённого файла.
#[must_use]
pub fn detect_file(input: &FileInput) -> Vec<Candidate> {
    let output_path = display_path(input);
    let mut candidates = Vec::new();

    detect_path_surfaces(input, &output_path, &mut candidates);
    detect_dependency_change(input, &output_path, &mut candidates);

    if let Some(post_text) = input.post_text.as_deref() {
        let rust_file =
            is_rust_path(&input.path) || input.previous_path.as_deref().is_some_and(is_rust_path);
        let ci_file = is_ci_or_shell_path(&input.path)
            || input
                .previous_path
                .as_deref()
                .is_some_and(is_ci_or_shell_path);

        for (line_index, line) in post_text.lines().enumerate() {
            let line_number = line_index + 1;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let path_signals = absolute_path_signals(line);
            if !path_signals.is_empty() {
                add_line_candidate(
                    &mut candidates,
                    input,
                    CandidateType::AbsolutePath,
                    &output_path,
                    line_number,
                    line,
                    CandidateAnnotations {
                        signals: path_signals,
                        metadata: BTreeMap::new(),
                    },
                );
            }

            if line_has_local_endpoint(line) {
                add_line_candidate(
                    &mut candidates,
                    input,
                    CandidateType::LocalEndpoint,
                    &output_path,
                    line_number,
                    line,
                    CandidateAnnotations {
                        signals: vec!["loopback_host_with_port".to_owned()],
                        metadata: BTreeMap::new(),
                    },
                );
            }

            let history_signals = development_reference_signals(line);
            if !history_signals.is_empty() {
                add_line_candidate(
                    &mut candidates,
                    input,
                    CandidateType::DevelopmentReference,
                    &output_path,
                    line_number,
                    line,
                    CandidateAnnotations {
                        signals: history_signals,
                        metadata: BTreeMap::new(),
                    },
                );
            }

            let security_signals = security_surface_signals(
                line,
                rust_file && is_rust_code_line(line),
                ci_file && !trimmed.starts_with('#'),
            );
            if !security_signals.is_empty() {
                add_line_candidate(
                    &mut candidates,
                    input,
                    CandidateType::SecuritySurface,
                    &output_path,
                    line_number,
                    line,
                    CandidateAnnotations {
                        signals: security_signals,
                        metadata: BTreeMap::new(),
                    },
                );
            }

            if rust_file && is_rust_code_line(line) {
                let suppression_signals = rust_suppression_signals(line);
                if !suppression_signals.is_empty() {
                    add_line_candidate(
                        &mut candidates,
                        input,
                        CandidateType::RustSuppression,
                        &output_path,
                        line_number,
                        line,
                        CandidateAnnotations {
                            signals: suppression_signals,
                            metadata: BTreeMap::new(),
                        },
                    );
                }

                if line.trim_start().starts_with("#[ignore") {
                    add_line_candidate(
                        &mut candidates,
                        input,
                        CandidateType::TestIgnored,
                        &output_path,
                        line_number,
                        line,
                        CandidateAnnotations {
                            signals: vec!["rust_test_ignore_attribute".to_owned()],
                            metadata: BTreeMap::new(),
                        },
                    );
                }

                let (error_signals, unsafe_signals) = rust_error_signals(line);
                if !error_signals.is_empty() {
                    add_line_candidate(
                        &mut candidates,
                        input,
                        CandidateType::ErrorPath,
                        &output_path,
                        line_number,
                        line,
                        CandidateAnnotations {
                            signals: error_signals,
                            metadata: BTreeMap::new(),
                        },
                    );
                }
                if !unsafe_signals.is_empty() {
                    add_line_candidate(
                        &mut candidates,
                        input,
                        CandidateType::UnsafePath,
                        &output_path,
                        line_number,
                        line,
                        CandidateAnnotations {
                            signals: unsafe_signals,
                            metadata: BTreeMap::new(),
                        },
                    );
                }
            }

            if ci_file {
                let suppression_signals = ci_suppression_signals(line);
                if !suppression_signals.is_empty() {
                    add_line_candidate(
                        &mut candidates,
                        input,
                        CandidateType::CiSuppression,
                        &output_path,
                        line_number,
                        line,
                        CandidateAnnotations {
                            signals: suppression_signals,
                            metadata: BTreeMap::new(),
                        },
                    );
                }
            }

            if is_generated_marker(line) {
                add_line_candidate(
                    &mut candidates,
                    input,
                    CandidateType::GeneratedSurface,
                    &output_path,
                    line_number,
                    line,
                    CandidateAnnotations {
                        signals: vec!["generated_file_marker".to_owned()],
                        metadata: BTreeMap::new(),
                    },
                );
            }
        }
    }

    detect_test_delta(input, &output_path, &mut candidates);
    sort_candidates(candidates)
}

fn detect_path_surfaces(input: &FileInput, output_path: &str, candidates: &mut Vec<Candidate>) {
    let paths = input
        .previous_path
        .as_deref()
        .into_iter()
        .chain(std::iter::once(input.path.as_str()))
        .collect::<BTreeSet<_>>();

    let mut surfaces = BTreeMap::<CandidateType, &'static str>::new();
    for path in paths {
        if is_config_path(path) {
            surfaces.insert(CandidateType::ConfigSurface, "configuration_path_touched");
        }
        if is_generated_path(path) {
            surfaces.insert(
                CandidateType::GeneratedSurface,
                "generated_looking_path_touched",
            );
        }
        if is_skill_path(path) {
            surfaces.insert(
                CandidateType::SkillSurface,
                "agent_skill_or_rule_path_touched",
            );
        }
        if is_security_path(path) {
            surfaces.insert(
                CandidateType::SecuritySurface,
                "security_sensitive_path_touched",
            );
        }
    }

    for (candidate_type, signal) in surfaces {
        let mut metadata = BTreeMap::new();
        metadata.insert("surface_only".to_owned(), "true".to_owned());
        push_candidate(
            candidates,
            candidate_type,
            output_path,
            None,
            None,
            file_origin(input),
            CandidateAnnotations {
                signals: vec![signal.to_owned()],
                metadata,
            },
        );
    }
}

fn detect_dependency_change(input: &FileInput, output_path: &str, candidates: &mut Vec<Candidate>) {
    let manifest_path = input
        .previous_path
        .as_deref()
        .filter(|path| is_dependency_path(path))
        .or_else(|| is_dependency_path(&input.path).then_some(input.path.as_str()));
    let Some(manifest_path) = manifest_path else {
        return;
    };
    if !has_comparable_text_images(input) {
        return;
    }

    let base_text = input.base_text.as_deref().unwrap_or_default();
    let post_text = input.post_text.as_deref().unwrap_or_default();
    if !is_lockfile_path(manifest_path) {
        let base_entries = dependency_entries(base_text, manifest_path);
        let post_entries = dependency_entries(post_text, manifest_path);
        if base_entries == post_entries {
            return;
        }
        let (removed, added) = multiset_delta(&base_entries, &post_entries);
        if removed.is_empty() && added.is_empty() {
            return;
        }
        let mut evidence = removed
            .iter()
            .map(|entry| format!("- {entry}"))
            .chain(added.iter().map(|entry| format!("+ {entry}")))
            .collect::<Vec<_>>();
        evidence.sort();
        let (snippet, truncated) = bounded_join(&evidence, 8, 900);
        let mut metadata = BTreeMap::new();
        metadata.insert("added_entry_count".to_owned(), added.len().to_string());
        metadata.insert("removed_entry_count".to_owned(), removed.len().to_string());
        metadata.insert("dependency_file_kind".to_owned(), "manifest".to_owned());
        metadata.insert("evidence_truncated".to_owned(), truncated.to_string());
        push_candidate(
            candidates,
            CandidateType::DependencyChange,
            output_path,
            None,
            snippet,
            changed_text_origin(input),
            CandidateAnnotations {
                signals: vec!["manifest_dependency_entries_changed".to_owned()],
                metadata,
            },
        );
        return;
    }

    if base_text == post_text {
        return;
    }
    let (removed, added) = multiset_delta(
        &base_text.lines().map(str::to_owned).collect::<Vec<_>>(),
        &post_text.lines().map(str::to_owned).collect::<Vec<_>>(),
    );
    let mut evidence = removed
        .iter()
        .map(|entry| format!("- {entry}"))
        .chain(added.iter().map(|entry| format!("+ {entry}")))
        .collect::<Vec<_>>();
    evidence.sort();
    let (snippet, truncated) = bounded_join(&evidence, 8, 900);
    let mut metadata = BTreeMap::new();
    metadata.insert("added_line_count".to_owned(), added.len().to_string());
    metadata.insert("removed_line_count".to_owned(), removed.len().to_string());
    metadata.insert("dependency_file_kind".to_owned(), "lockfile".to_owned());
    metadata.insert("evidence_truncated".to_owned(), truncated.to_string());
    push_candidate(
        candidates,
        CandidateType::DependencyChange,
        output_path,
        None,
        snippet,
        changed_text_origin(input),
        CandidateAnnotations {
            signals: vec!["lockfile_content_changed".to_owned()],
            metadata,
        },
    );
}

fn dependency_entries(text: &str, path: &str) -> Vec<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name == "Cargo.toml" {
        let mut in_dependency_section = false;
        let mut entries = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                let section = trimmed.trim_matches(['[', ']']).to_ascii_lowercase();
                in_dependency_section = section == "dependencies"
                    || section == "dev-dependencies"
                    || section == "build-dependencies"
                    || section == "workspace.dependencies"
                    || section.starts_with("dependencies.")
                    || section.starts_with("dev-dependencies.")
                    || section.starts_with("build-dependencies.")
                    || section.starts_with("workspace.dependencies.")
                    || section.ends_with(".dependencies")
                    || section.ends_with(".dev-dependencies")
                    || section.ends_with(".build-dependencies")
                    || section.contains(".dependencies.")
                    || section.contains(".dev-dependencies.")
                    || section.contains(".build-dependencies.");
                if in_dependency_section {
                    entries.push(trimmed.to_owned());
                }
                continue;
            }
            if in_dependency_section && !trimmed.is_empty() && !trimmed.starts_with('#') {
                entries.push(trimmed.to_owned());
            }
        }
        return entries;
    }

    if name == "package.json" {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Vec::new();
        };
        let Some(root) = value.as_object() else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for section in [
            "dependencies",
            "devDependencies",
            "peerDependencies",
            "optionalDependencies",
        ] {
            if let Some(dependencies) = root.get(section).and_then(serde_json::Value::as_object) {
                for (package, version) in dependencies {
                    entries.push(format!("{section}.{package}={}", compact_json(version)));
                }
            }
        }
        entries.sort();
        return entries;
    }

    if name.starts_with("requirements") && name.ends_with(".txt") {
        return text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect();
    }

    if name == "go.mod" {
        let mut in_dependency_block = false;
        let mut entries = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("require (") || trimmed.starts_with("replace (") {
                in_dependency_block = true;
                continue;
            }
            if in_dependency_block && trimmed == ")" {
                in_dependency_block = false;
                continue;
            }
            if trimmed.starts_with("require ")
                || trimmed.starts_with("replace ")
                || in_dependency_block && !trimmed.is_empty() && !trimmed.starts_with("//")
            {
                entries.push(trimmed.to_owned());
            }
        }
        return entries;
    }

    if name == "pyproject.toml" {
        let mut section = String::new();
        let mut in_project_dependency_array = false;
        let mut entries = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                section = trimmed.trim_matches(['[', ']']).to_ascii_lowercase();
                in_project_dependency_array = false;
                continue;
            }
            let project_dependencies = section == "project"
                && trimmed.split_once('=').is_some_and(|(key, value)| {
                    matches!(key.trim(), "dependencies" | "optional-dependencies")
                        && value.contains('[')
                });
            if in_project_dependency_array {
                entries.push(trimmed.to_owned());
                if trimmed.contains(']') {
                    in_project_dependency_array = false;
                }
                continue;
            }
            if (section.contains("dependenc")
                || section.contains("requirements")
                || project_dependencies)
                && !trimmed.is_empty()
                && !trimmed.starts_with('#')
            {
                entries.push(trimmed.to_owned());
                if project_dependencies && !trimmed.contains(']') {
                    in_project_dependency_array = true;
                }
            }
        }
        return entries;
    }

    if name == "Pipfile" {
        let mut in_dependency_section = false;
        let mut entries = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                let section = trimmed.trim_matches(['[', ']']).to_ascii_lowercase();
                in_dependency_section = matches!(section.as_str(), "packages" | "dev-packages");
                continue;
            }
            if in_dependency_section && !trimmed.is_empty() && !trimmed.starts_with('#') {
                entries.push(trimmed.to_owned());
            }
        }
        return entries;
    }

    if name == "Gemfile" {
        return text
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("gem "))
            .map(str::to_owned)
            .collect();
    }

    if name == "composer.json" {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Vec::new();
        };
        let Some(root) = value.as_object() else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for section in ["require", "require-dev"] {
            if let Some(dependencies) = root.get(section).and_then(serde_json::Value::as_object) {
                for (package, version) in dependencies {
                    entries.push(format!("{section}.{package}={}", compact_json(version)));
                }
            }
        }
        entries.sort();
        return entries;
    }

    Vec::new()
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".to_owned())
}

fn is_dependency_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "pyproject.toml"
            | "poetry.lock"
            | "uv.lock"
            | "Pipfile"
            | "Pipfile.lock"
            | "go.mod"
            | "go.sum"
            | "Gemfile"
            | "Gemfile.lock"
            | "composer.json"
            | "composer.lock"
            | "requirements.txt"
            | "gradle.lockfile"
    ) || (name.starts_with("requirements") && name.ends_with(".txt"))
}

fn is_lockfile_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "Cargo.lock"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "poetry.lock"
            | "uv.lock"
            | "Pipfile.lock"
            | "go.sum"
            | "Gemfile.lock"
            | "composer.lock"
            | "gradle.lockfile"
    )
}

fn multiset_delta(base: &[String], post: &[String]) -> (Vec<String>, Vec<String>) {
    let mut base_counts = BTreeMap::<&str, usize>::new();
    let mut post_counts = BTreeMap::<&str, usize>::new();
    for value in base {
        *base_counts.entry(value).or_default() += 1;
    }
    for value in post {
        *post_counts.entry(value).or_default() += 1;
    }
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let values = base_counts
        .keys()
        .chain(post_counts.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for value in values {
        let base_count = base_counts.get(value).copied().unwrap_or_default();
        let post_count = post_counts.get(value).copied().unwrap_or_default();
        if base_count > post_count {
            removed.extend(std::iter::repeat_n(
                value.to_owned(),
                base_count - post_count,
            ));
        } else if post_count > base_count {
            added.extend(std::iter::repeat_n(
                value.to_owned(),
                post_count - base_count,
            ));
        }
    }
    (removed, added)
}

fn detect_test_delta(input: &FileInput, output_path: &str, candidates: &mut Vec<Candidate>) {
    let is_rust =
        is_rust_path(&input.path) || input.previous_path.as_deref().is_some_and(is_rust_path);
    if !is_rust {
        return;
    }
    if !has_comparable_text_images(input) {
        return;
    }
    let base_text = input.base_text.as_deref().unwrap_or_default();
    let post_text = input.post_text.as_deref().unwrap_or_default();
    let whole_file_is_test =
        is_test_path(&input.path) || input.previous_path.as_deref().is_some_and(is_test_path);
    let base_evidence = rust_test_evidence(base_text, whole_file_is_test);
    let post_evidence = rust_test_evidence(post_text, whole_file_is_test);
    let base_tests = &base_evidence.functions;
    let post_tests = &post_evidence.functions;

    for (name, definition) in post_tests {
        if !base_tests.contains_key(name) {
            let mut metadata = BTreeMap::new();
            metadata.insert("test_name".to_owned(), name.clone());
            push_candidate(
                candidates,
                CandidateType::TestAdded,
                output_path,
                Some(definition.name_line),
                Some(definition.signature.clone()),
                file_origin(input),
                CandidateAnnotations {
                    signals: vec!["test_function_added".to_owned()],
                    metadata,
                },
            );
        }
    }
    for (name, definition) in base_tests {
        if !post_tests.contains_key(name) {
            let mut metadata = BTreeMap::new();
            metadata.insert("test_name".to_owned(), name.clone());
            push_candidate(
                candidates,
                CandidateType::TestRemoved,
                output_path,
                Some(definition.name_line),
                Some(definition.signature.clone()),
                removed_text_origin(input),
                CandidateAnnotations {
                    signals: vec!["test_function_removed".to_owned()],
                    metadata,
                },
            );
        }
    }

    let base_count = base_evidence.assertion_count;
    let post_count = post_evidence.assertion_count;
    let decrease = base_count.saturating_sub(post_count);
    if decrease > 0 {
        // Изменённое выражение и удалённое выражение могут одновременно не иметь
        // точного текстового соответствия. Счётчик доказывает только уменьшение
        // числа assertions в test-контексте, но не конкретную удалённую строку.
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "total_assertion_count_delta".to_owned(),
            format!("-{decrease}"),
        );
        metadata.insert("base_assertion_count".to_owned(), base_count.to_string());
        metadata.insert("post_assertion_count".to_owned(), post_count.to_string());
        push_candidate(
            candidates,
            CandidateType::AssertionRemoved,
            output_path,
            None,
            None,
            removed_text_origin(input),
            CandidateAnnotations {
                signals: vec!["test_assertion_count_decreased".to_owned()],
                metadata,
            },
        );
    }
}

#[derive(Debug)]
struct TestFunction {
    name_line: usize,
    signature: String,
}

#[derive(Default)]
struct RustTestEvidence {
    functions: BTreeMap<String, TestFunction>,
    assertion_count: usize,
}

/// Ограниченный лексический разбор: комментарии и литералы не являются элементами Rust.
/// Контекст передаётся внутрь тела тестовой функции либо элемента с `#[cfg(test)]`.
fn rust_test_evidence(text: &str, whole_file_is_test: bool) -> RustTestEvidence {
    let tokens = rust_tokens(text);
    let lines = text.lines().collect::<Vec<_>>();
    let mut evidence = RustTestEvidence::default();
    let mut contexts = vec![whole_file_is_test];
    let mut pending_test = false;
    let mut pending_test_function = false;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].value;
        if token == "#"
            && tokens
                .get(index + 1)
                .is_some_and(|token| token.value == "[")
        {
            let mut end = index + 2;
            let mut depth = 1;
            while end < tokens.len() && depth > 0 {
                match tokens[end].value {
                    "[" => depth += 1,
                    "]" => depth -= 1,
                    _ => {}
                }
                end += 1;
            }
            if depth == 0 {
                let attribute = tokens[index + 2..end - 1]
                    .iter()
                    .map(|token| token.value)
                    .collect::<String>();
                let test_attribute = is_test_attribute(&format!("#[{attribute}]"));
                pending_test_function |= test_attribute;
                pending_test |= test_attribute || attribute == "cfg(test)";
            }
            index = end;
            continue;
        }
        match token {
            "fn" if pending_test_function => {
                if let Some(name) = tokens.get(index + 1).filter(|token| {
                    token
                        .value
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_alphabetic() || ch == '_')
                }) {
                    let line_index = tokens[index].line_number - 1;
                    evidence
                        .functions
                        .entry(name.value.to_owned())
                        .or_insert_with(|| TestFunction {
                            name_line: line_index + 1,
                            signature: lines.get(line_index).unwrap_or(&"").trim().to_owned(),
                        });
                }
                pending_test_function = false;
            }
            "{" => {
                contexts.push(*contexts.last().unwrap_or(&false) || pending_test);
                pending_test = false;
                pending_test_function = false;
            }
            "}" => {
                if contexts.len() > 1 {
                    contexts.pop();
                }
                pending_test = false;
                pending_test_function = false;
            }
            ";" | "=" => {
                pending_test = false;
                pending_test_function = false;
            }
            _ => {}
        }
        if *contexts.last().unwrap_or(&false)
            && matches!(
                token,
                "assert"
                    | "assert_eq"
                    | "assert_ne"
                    | "debug_assert"
                    | "debug_assert_eq"
                    | "debug_assert_ne"
            )
            && tokens
                .get(index + 1)
                .is_some_and(|token| token.value == "!")
            && tokens
                .get(index + 2)
                .is_some_and(|token| matches!(token.value, "(" | "[" | "{"))
        {
            evidence.assertion_count += 1;
        }
        index += 1;
    }
    evidence
}

fn is_test_attribute(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("#[test]")
        || trimmed.starts_with("#[test(")
        || trimmed.starts_with("#[tokio::test]")
        || trimmed.starts_with("#[tokio::test(")
        || trimmed.starts_with("#[async_std::test]")
        || trimmed.starts_with("#[async_std::test(")
        || trimmed.starts_with("#[rstest]")
        || trimmed.starts_with("#[rstest(")
}

struct RustToken<'a> {
    value: &'a str,
    line_number: usize,
}

fn rust_tokens(text: &str) -> Vec<RustToken<'_>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut line_number = 1;
    while index < bytes.len() {
        let scanned_start = index;
        if bytes[index..].starts_with(b"//") {
            index = text[index..].find('\n').map_or(bytes.len(), |n| index + n);
        } else if bytes[index..].starts_with(b"/*") {
            let mut depth = 1;
            index += 2;
            while index < bytes.len() && depth > 0 {
                if bytes[index..].starts_with(b"/*") {
                    depth += 1;
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
        } else if let Some(end) = rust_literal_end(text, index) {
            index = end;
        } else {
            let start = index;
            let first = text[index..].chars().next().expect("существующий символ");
            index += first.len_utf8();
            if first.is_alphabetic() || first == '_' {
                while index < bytes.len() {
                    let next = text[index..].chars().next().expect("существующий символ");
                    if !next.is_alphanumeric() && next != '_' {
                        break;
                    }
                    index += next.len_utf8();
                }
            }
            if !first.is_whitespace() {
                tokens.push(RustToken {
                    value: &text[start..index],
                    line_number,
                });
            }
        }
        line_number += bytes[scanned_start..index]
            .iter()
            .filter(|ch| **ch == b'\n')
            .count();
    }
    tokens
}

fn rust_literal_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut opener = start;
    if matches!(bytes[opener], b'b' | b'c') {
        opener += 1;
    }
    if bytes.get(opener) == Some(&b'r') {
        let hashes = opener + 1;
        opener = hashes;
        while bytes.get(opener) == Some(&b'#') {
            opener += 1;
        }
        if bytes.get(opener) == Some(&b'"') {
            let closer = format!("\"{}", "#".repeat(opener - hashes));
            return Some(
                text[opener + 1..]
                    .find(&closer)
                    .map_or(bytes.len(), |n| opener + 1 + n + closer.len()),
            );
        }
        return None;
    }
    let quote = *bytes.get(opener)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    if quote == b'\'' {
        // Lifetime (`'a`) не является literal. Char literal содержит один символ
        // либо escape и завершающую одинарную кавычку.
        let mut end = opener + 1;
        if bytes.get(end) == Some(&b'\\') {
            end += 2;
            if bytes.get(end - 1) == Some(&b'u') && bytes.get(end) == Some(&b'{') {
                end = text[end..].find('}').map_or(bytes.len(), |n| end + n + 1);
            }
        } else {
            end += text.get(end..)?.chars().next()?.len_utf8();
        }
        return (bytes.get(end) == Some(&b'\'')).then_some(end + 1);
    }
    let mut end = opener + 1;
    while end < bytes.len() {
        if bytes[end] == b'\\' {
            end = (end + 2).min(bytes.len());
        } else if bytes[end] == quote {
            return Some(end + 1);
        } else {
            end += 1;
        }
    }
    Some(bytes.len())
}

fn has_macro_invocation(line: &str, name: &str) -> bool {
    macro_invocation_count(line, name) > 0
}

fn macro_invocation_count(line: &str, name: &str) -> usize {
    let needle = format!("{name}!");
    line.match_indices(&needle)
        .filter(|(index, _)| {
            let before_ok = *index == 0
                || !line[..*index]
                    .chars()
                    .next_back()
                    .is_some_and(is_identifier_char);
            let after_index = *index + needle.len();
            let suffix = line[after_index..].trim_start();
            before_ok && suffix.starts_with('(')
        })
        .count()
}

fn absolute_path_signals(line: &str) -> Vec<String> {
    let mut signals = BTreeSet::new();
    let local_prefixes = [
        "/home/",
        "/Users/",
        "/mnt/c/Users/",
        "/root/",
        "/tmp/",
        "/var/tmp/",
        "/home/runner/",
        "/workspace/",
        "/workspaces/",
    ];
    if local_prefixes.iter().any(|prefix| line.contains(prefix)) {
        signals.insert("local_environment_absolute_path".to_owned());
    }
    let lower = line.to_ascii_lowercase();
    if ["\\users\\", "/users/", "\\documents and settings\\"]
        .iter()
        .any(|prefix| lower.contains(prefix))
    {
        signals.insert("local_environment_absolute_path".to_owned());
    }
    if has_quoted_unix_absolute_path(line) || has_windows_absolute_path(line) {
        signals.insert("absolute_path_literal".to_owned());
    }
    signals.into_iter().collect()
}

fn has_quoted_unix_absolute_path(line: &str) -> bool {
    for (index, quote) in line.char_indices() {
        if quote != '\'' && quote != '"' && quote != '`' {
            continue;
        }
        let rest = &line[index + quote.len_utf8()..];
        if !rest.starts_with('/') || rest.starts_with("//") {
            continue;
        }
        let end = rest
            .find(quote)
            .or_else(|| rest.find(|character: char| character.is_whitespace()))
            .unwrap_or(rest.len());
        let candidate = &rest[..end];
        if candidate.matches('/').count() >= 2 && !candidate.contains("://") {
            return true;
        }
    }
    false
}

fn has_windows_absolute_path(line: &str) -> bool {
    let bytes = line.as_bytes();
    if bytes.len() < 5 {
        return false;
    }
    for index in 0..bytes.len() - 2 {
        if bytes[index].is_ascii_alphabetic()
            && bytes[index + 1] == b':'
            && (bytes[index + 2] == b'\\' || bytes[index + 2] == b'/')
            && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric() && bytes[index - 1] != b'_')
        {
            let remainder = &bytes[index + 3..];
            if remainder.iter().any(|byte| *byte == b'\\' || *byte == b'/') {
                return true;
            }
        }
    }
    false
}

fn line_has_local_endpoint(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ["localhost:", "127.0.0.1:", "[::1]:"]
        .iter()
        .any(|host| has_port_after(&lower, host))
}

fn has_port_after(line: &str, host: &str) -> bool {
    line.match_indices(host).any(|(index, _)| {
        let after = &line[index + host.len()..];
        let digits = after
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        digits
            .parse::<u16>()
            .is_ok_and(|port| port > 0 && !digits.is_empty())
    })
}

fn development_reference_signals(line: &str) -> Vec<String> {
    let lower = line.to_lowercase();
    let mut signals = BTreeSet::new();
    if contains_numbered_marker(&lower, "pull request")
        || contains_numbered_marker(&lower, "pr")
        || contains_numbered_marker(&lower, "pull/")
    {
        signals.insert("numbered_pull_request_reference".to_owned());
    }
    if has_contextual_sha(&lower) {
        signals.insert("contextual_commit_sha_reference".to_owned());
    }
    if has_numbered_stage(&lower) && has_development_context(&lower) {
        signals.insert("numbered_development_stage_reference".to_owned());
    }
    signals.into_iter().collect()
}

fn contains_numbered_marker(line: &str, marker: &str) -> bool {
    line.match_indices(marker).any(|(index, _)| {
        let before_ok = index == 0
            || !line[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric);
        let mut suffix = &line[index + marker.len()..];
        if !before_ok {
            return false;
        }
        suffix = suffix.trim_start();
        if suffix.starts_with('#') || suffix.starts_with('-') || suffix.starts_with('_') {
            suffix = &suffix[1..];
            suffix = suffix.trim_start();
        }
        suffix
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
    })
}

fn has_contextual_sha(line: &str) -> bool {
    let bytes = line.as_bytes();
    let contexts = ["sha", "commit", "revision", "snapshot", "head", "base"];
    let mut start = 0;
    while start < bytes.len() {
        if !bytes[start].is_ascii_hexdigit() {
            start += 1;
            continue;
        }
        let end = (start..bytes.len())
            .find(|index| !bytes[*index].is_ascii_hexdigit())
            .unwrap_or(bytes.len());
        let token_len = end - start;
        if (7..=40).contains(&token_len) {
            let context_start = start.saturating_sub(32);
            let context =
                String::from_utf8_lossy(&bytes[context_start..start]).to_ascii_lowercase();
            if has_sha_context(&context, &contexts) {
                return true;
            }
        }
        start = end.max(start + 1);
    }
    false
}

fn has_sha_context(context: &str, contexts: &[&str]) -> bool {
    context
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| {
            contexts.iter().any(|context_word| {
                token == *context_word || (*context_word == "sha" && token.starts_with("sha"))
            })
        })
}

fn has_numbered_stage(line: &str) -> bool {
    ["stage", "phase", "iteration", "этап", "фаза", "итерация"]
        .iter()
        .any(|marker| contains_numbered_marker(line, marker))
}

fn has_development_context(line: &str) -> bool {
    [
        "review",
        "pull request",
        "pr ",
        "codex",
        "migration",
        "migrate",
        "snapshot",
        "fix ",
        "issue",
        "temporary",
        "временн",
        "ревью",
        "мигр",
    ]
    .iter()
    .any(|context| line.contains(context))
}

fn rust_suppression_signals(line: &str) -> Vec<String> {
    let trimmed = line.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let mut signals = BTreeSet::new();
    if lower.contains("#[allow(") {
        signals.insert("rust_allow_attribute".to_owned());
    }
    if lower.contains("#[expect(") {
        signals.insert("rust_expect_attribute".to_owned());
    }
    if lower.contains("cfg_attr(") && lower.contains("allow(") {
        signals.insert("conditional_rust_allow_attribute".to_owned());
    }
    if trimmed.starts_with("#[ignore") {
        signals.insert("rust_test_ignore_attribute".to_owned());
    }
    signals.into_iter().collect()
}

fn ci_suppression_signals(line: &str) -> Vec<String> {
    let lower = line.to_ascii_lowercase();
    let trimmed = lower.trim();
    let mut signals = BTreeSet::new();
    if !trimmed.starts_with('#')
        && trimmed.starts_with("continue-on-error:")
        && trimmed
            .split_once(':')
            .is_some_and(|(_, value)| value.trim().trim_matches(['\'', '"']) == "true")
    {
        signals.insert("ci_continue_on_error_enabled".to_owned());
    }
    if !trimmed.starts_with('#') && has_shell_or_true(&lower) {
        signals.insert("shell_failure_followed_by_true".to_owned());
    }
    if trimmed.starts_with("set +e")
        && trimmed[6..]
            .chars()
            .next()
            .is_none_or(|character| character.is_whitespace() || character == '#')
    {
        signals.insert("shell_errexit_disabled".to_owned());
    }
    signals.into_iter().collect()
}

fn has_shell_or_true(line: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
            continue;
        }
        if character == '\'' || character == '"' {
            quote = Some(character);
            continue;
        }
        if character != '|' || !line[index..].starts_with("||") {
            continue;
        }
        let after = line[index + 2..].trim_start();
        if !after.starts_with("true") {
            continue;
        }
        let suffix = &after[4..];
        if suffix.chars().next().is_none_or(|character| {
            character.is_whitespace() || character == ';' || character == '#'
        }) {
            return true;
        }
    }
    false
}

fn rust_error_signals(line: &str) -> (Vec<String>, Vec<String>) {
    let mut errors = BTreeSet::new();
    let mut unsafe_signals = BTreeSet::new();
    if has_empty_call(line, "unwrap") {
        errors.insert("unwrap_call".to_owned());
    }
    if has_named_call(line, "expect") {
        errors.insert("expect_call".to_owned());
    }
    for macro_name in ["panic", "todo", "unimplemented"] {
        if has_macro_invocation(line, macro_name) {
            errors.insert(format!("{macro_name}_macro"));
        }
    }
    if has_named_call(line, "exit") && line.contains("process::") {
        errors.insert("explicit_process_exit".to_owned());
    }
    if has_named_call(line, "unwrap_unchecked") {
        unsafe_signals.insert("unwrap_unchecked_call".to_owned());
    }
    if has_unsafe_construct(line) {
        unsafe_signals.insert("rust_unsafe_construct".to_owned());
    }
    (
        errors.into_iter().collect(),
        unsafe_signals.into_iter().collect(),
    )
}

fn security_surface_signals(line: &str, rust_file: bool, ci_file: bool) -> Vec<String> {
    let lower = line.to_ascii_lowercase();
    let mut signals = BTreeSet::new();
    if rust_file {
        if lower.contains("command::new(") || lower.contains("std::process::command") {
            signals.insert("process_spawn_surface".to_owned());
        }
        if [
            "std::fs::write(",
            "fs::write(",
            "std::fs::remove_file(",
            "fs::remove_file(",
            "std::fs::remove_dir_all(",
            "fs::remove_dir_all(",
            "std::fs::rename(",
            "fs::rename(",
            "file::create(",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern))
        {
            signals.insert("filesystem_mutation_surface".to_owned());
        }
        if [
            "canonicalize(",
            "set_current_dir(",
            "strip_prefix(",
            "path::join(",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern))
        {
            signals.insert("filesystem_path_handling_surface".to_owned());
        }
        if [
            "reqwest::",
            "ureq::",
            "hyper::",
            "tcpstream::connect(",
            "tcplistener::bind(",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern))
        {
            signals.insert("network_client_or_listener_surface".to_owned());
        }
        if ["serde_json::from_", "serde_yaml::from_", "toml::from_"]
            .iter()
            .any(|pattern| lower.contains(pattern))
        {
            signals.insert("structured_deserialization_surface".to_owned());
        }
        if lower.contains("std::env::var(") || lower.contains("env::var(") {
            signals.insert("environment_value_read_surface".to_owned());
            if [
                "secret",
                "token",
                "password",
                "credential",
                "api_key",
                "private_key",
            ]
            .iter()
            .any(|term| lower.contains(term))
            {
                signals.insert("environment_secret_read_surface".to_owned());
            }
        }
        if lower.contains("std::env::args(")
            || lower.contains("std::env::args_os(")
            || lower.contains("stdin().read_line(")
        {
            signals.insert("untrusted_process_input_surface".to_owned());
        }
        if has_unsafe_construct(line) {
            signals.insert("unsafe_runtime_surface".to_owned());
        }
    }
    if ci_file {
        if lower.contains("pull_request_target") {
            signals.insert("privileged_pull_request_workflow_surface".to_owned());
        }
        if lower.contains("secrets.") || lower.contains("secrets[") {
            signals.insert("workflow_secret_access_surface".to_owned());
        }
        let trimmed = lower.trim_start();
        if trimmed.starts_with("uses:") || trimmed.starts_with("- uses:") {
            signals.insert("workflow_action_dependency_surface".to_owned());
        }
        if trimmed.starts_with("run:")
            && ["curl ", "wget ", "rm -rf", "sudo ", "eval "]
                .iter()
                .any(|pattern| lower.contains(pattern))
        {
            signals.insert("workflow_shell_command_surface".to_owned());
        }
    }
    signals.into_iter().collect()
}

fn has_empty_call(line: &str, name: &str) -> bool {
    find_named_calls(line, name).any(|after_name| {
        let after = line[after_name..].trim_start();
        after.starts_with('(') && after[1..].trim_start().starts_with(')')
    })
}

fn has_named_call(line: &str, name: &str) -> bool {
    find_named_calls(line, name).next().is_some()
}

fn find_named_calls<'a>(line: &'a str, name: &'a str) -> impl Iterator<Item = usize> + 'a {
    line.match_indices(name).filter_map(move |(index, _)| {
        let before_ok = index == 0
            || !line[..index]
                .chars()
                .next_back()
                .is_some_and(is_identifier_char);
        let after_name = index + name.len();
        let after = line[after_name..].trim_start();
        let after_ok = after.starts_with('(');
        (before_ok && after_ok).then_some(after_name)
    })
}

fn has_unsafe_construct(line: &str) -> bool {
    line.match_indices("unsafe").any(|(index, _)| {
        let before_ok = index == 0
            || !line[..index]
                .chars()
                .next_back()
                .is_some_and(is_identifier_char);
        let suffix = line[index + "unsafe".len()..].trim_start();
        before_ok
            && (suffix.starts_with('{')
                || suffix.starts_with("fn ")
                || suffix.starts_with("impl ")
                || suffix.starts_with("trait "))
    })
}

fn is_rust_code_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    !trimmed.starts_with("//")
        && !trimmed.starts_with("/*")
        && !trimmed.starts_with('*')
        && !trimmed.starts_with("*/")
}

fn is_generated_marker(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("@generated")
        || lower.contains("generated by")
        || lower.contains("automatically generated")
        || lower.contains("do not edit")
}

fn is_config_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    lower.starts_with(".github/")
        || lower.starts_with(".cargo/")
        || lower.starts_with(".config/")
        || lower.contains("/config/")
        || name == "cargo.toml"
        || name.ends_with(".toml")
        || name.ends_with(".yaml")
        || name.ends_with(".yml")
        || name.ends_with(".ini")
        || name.ends_with(".conf")
        || name.ends_with(".config")
        || name == ".editorconfig"
        || name == ".gitignore"
}

fn is_generated_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("generated/")
        || lower.starts_with("gen/")
        || lower.contains("/generated/")
        || lower.contains("/gen/")
        || lower.ends_with(".generated.rs")
        || lower.ends_with(".generated.ts")
        || lower.ends_with(".generated.js")
        || lower.ends_with(".pb.rs")
        || lower.ends_with(".pb.go")
}

fn is_skill_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with(".agents/skills/")
        || lower.starts_with(".codex/")
        || lower == "agents.md"
        || lower.ends_with("/agents.md")
        || lower.ends_with("/skill.md")
}

fn is_security_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let segments = lower
        .split('/')
        .map(|segment| segment.rsplit_once('.').map_or(segment, |(stem, _)| stem))
        .collect::<Vec<_>>();
    let terms = [
        "security",
        "auth",
        "authentication",
        "authorization",
        "crypto",
        "cryptography",
        "secret",
        "secrets",
        "credential",
        "credentials",
        "token",
        "password",
        "permission",
        "permissions",
        "policy",
        "sandbox",
        "tls",
        "ssl",
        "key",
        "keys",
        "access_control",
    ];
    segments.iter().any(|segment| {
        let segment = *segment;
        terms.iter().any(|term| {
            segment == *term
                || segment
                    .strip_prefix(*term)
                    .is_some_and(|suffix| suffix.starts_with('_'))
                || segment
                    .strip_suffix(*term)
                    .is_some_and(|prefix| prefix.ends_with('_'))
        })
    })
}

fn is_rust_path(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".rs")
}

fn is_ci_or_shell_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with(".github/workflows/")
        || lower.ends_with(".sh")
        || lower.ends_with(".bash")
        || lower.ends_with(".yml")
        || lower.ends_with(".yaml")
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.ends_with("_test.rs")
        || lower
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with("test_"))
}

fn display_path(input: &FileInput) -> String {
    if input.status == FileStatus::Deleted {
        input
            .previous_path
            .as_deref()
            .unwrap_or(&input.path)
            .to_owned()
    } else {
        input.path.clone()
    }
}

fn file_origin(input: &FileInput) -> CandidateOrigin {
    match input.status {
        FileStatus::Unknown | FileStatus::Unmerged => CandidateOrigin::Unknown,
        FileStatus::Added
        | FileStatus::Modified
        | FileStatus::Deleted
        | FileStatus::Renamed
        | FileStatus::Copied
        | FileStatus::TypeChanged => CandidateOrigin::IntroducedOrChanged,
    }
}

fn changed_text_origin(input: &FileInput) -> CandidateOrigin {
    if input.base_text.is_none() && input.post_text.is_none() {
        return CandidateOrigin::Unknown;
    }
    if input.base_text == input.post_text {
        return CandidateOrigin::PreExisting;
    }
    file_origin(input)
}

fn has_comparable_text_images(input: &FileInput) -> bool {
    matches!(
        (&input.base_text, &input.post_text, input.status),
        (Some(_), Some(_), _)
            | (None, Some(_), FileStatus::Added)
            | (Some(_), None, FileStatus::Deleted)
    )
}

fn removed_text_origin(input: &FileInput) -> CandidateOrigin {
    if input.base_text.is_some() && input.post_text.is_some() {
        file_origin(input)
    } else {
        match input.status {
            FileStatus::Deleted => CandidateOrigin::IntroducedOrChanged,
            FileStatus::Unknown | FileStatus::Unmerged => CandidateOrigin::Unknown,
            _ if input.base_text.is_some() || input.post_text.is_some() => file_origin(input),
            _ => CandidateOrigin::Unknown,
        }
    }
}

fn origin_for_line(input: &FileInput, line_number: usize) -> CandidateOrigin {
    if matches!(input.status, FileStatus::Unknown | FileStatus::Unmerged) {
        return CandidateOrigin::Unknown;
    }
    if input.status == FileStatus::Added {
        return CandidateOrigin::IntroducedOrChanged;
    }
    if input.base_text.is_none() || input.post_text.is_none() {
        return CandidateOrigin::Unknown;
    }
    if input.base_text == input.post_text {
        return CandidateOrigin::PreExisting;
    }
    let Some(ranges) = &input.post_changed_lines else {
        return CandidateOrigin::Unknown;
    };
    if ranges
        .iter()
        .any(|range| range.start <= line_number as u64 && (line_number as u64) < range.end)
    {
        CandidateOrigin::IntroducedOrChanged
    } else {
        CandidateOrigin::PreExisting
    }
}

fn add_line_candidate(
    candidates: &mut Vec<Candidate>,
    input: &FileInput,
    candidate_type: CandidateType,
    path: &str,
    line_number: usize,
    line: &str,
    annotations: CandidateAnnotations,
) {
    push_candidate(
        candidates,
        candidate_type,
        path,
        Some(line_number),
        Some(line.trim().to_owned()),
        origin_for_line(input, line_number),
        annotations,
    );
}

fn push_candidate(
    candidates: &mut Vec<Candidate>,
    candidate_type: CandidateType,
    path: &str,
    line: Option<usize>,
    snippet: Option<String>,
    origin: CandidateOrigin,
    annotations: CandidateAnnotations,
) {
    let CandidateAnnotations {
        mut signals,
        metadata,
    } = annotations;
    signals.sort();
    signals.dedup();
    let id = stable_id(candidate_type, path, line, snippet.as_deref(), &signals);
    candidates.push(Candidate {
        id,
        candidate_type,
        path: path.to_owned(),
        line,
        snippet,
        origin,
        signals,
        source: SOURCE.to_owned(),
        metadata,
    });
}

fn stable_id(
    candidate_type: CandidateType,
    path: &str,
    line: Option<usize>,
    snippet: Option<&str>,
    signals: &[String],
) -> String {
    let material = format!(
        "{}\0{}\0{}\0{}\0{}",
        candidate_type.as_str(),
        path,
        line.map_or_else(|| "-".to_owned(), |number| number.to_string()),
        snippet.unwrap_or_default(),
        signals.join(",")
    );
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in material.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("cand-{hash:016x}")
}

fn sort_candidates(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(
                left.candidate_type
                    .as_str()
                    .cmp(right.candidate_type.as_str()),
            )
            .then(left.line.cmp(&right.line))
            .then(left.id.cmp(&right.id))
    });
    candidates.dedup_by(|left, right| left.id == right.id);
    candidates
}

fn bounded_join(values: &[String], max_items: usize, max_chars: usize) -> (Option<String>, bool) {
    let mut output = String::new();
    let mut included = 0;
    for value in values.iter().take(max_items) {
        let separator = usize::from(!output.is_empty());
        if output.chars().count() + separator + value.chars().count() > max_chars {
            break;
        }
        if separator == 1 {
            output.push('\n');
        }
        output.push_str(value);
        included += 1;
    }
    let truncated = included < values.len();
    let snippet = (!output.is_empty()).then_some(output);
    (snippet, truncated)
}

fn is_identifier_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &str, base: &str, post: &str) -> FileInput {
        FileInput {
            path: path.to_owned(),
            previous_path: None,
            status: FileStatus::Modified,
            base_text: Some(base.to_owned()),
            post_text: Some(post.to_owned()),
            post_changed_lines: Some(fixture_changed_lines(base, post)),
        }
    }

    // Для этих fixtures общий prefix/suffix однозначен; сложные occurrence
    // регрессии ниже передают точные диапазоны отдельно.
    fn fixture_changed_lines(base: &str, post: &str) -> Vec<LineRange> {
        let base = base.lines().collect::<Vec<_>>();
        let post = post.lines().collect::<Vec<_>>();
        let prefix = base.iter().zip(&post).take_while(|(a, b)| a == b).count();
        let suffix = base[prefix..]
            .iter()
            .rev()
            .zip(post[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        if prefix + suffix == post.len() {
            return Vec::new();
        }
        vec![LineRange {
            start: prefix as u64 + 1,
            end: (post.len() - suffix) as u64 + 1,
        }]
    }

    fn detect_one(input: FileInput) -> Vec<Candidate> {
        detect(&[input])
    }

    fn of_type(candidates: &[Candidate], candidate_type: CandidateType) -> Vec<&Candidate> {
        candidates
            .iter()
            .filter(|candidate| candidate.candidate_type == candidate_type)
            .collect()
    }

    #[test]
    fn absolute_and_local_paths_are_candidates_but_documented_relative_paths_are_not() {
        let candidates = detect_one(input(
            "src/config.rs",
            "const PATH: &str = \"./config/settings.toml\";",
            "const PATH: &str = \"/home/alice/project/config.toml\";\nlet url = \"http://localhost:8080\";",
        ));
        assert_eq!(of_type(&candidates, CandidateType::AbsolutePath).len(), 1);
        assert_eq!(of_type(&candidates, CandidateType::LocalEndpoint).len(), 1);
        assert_eq!(candidates[0].origin, CandidateOrigin::IntroducedOrChanged);
    }

    #[test]
    fn windows_absolute_paths_are_candidates() {
        let candidates = detect_one(input(
            "src/config.rs",
            "let path = \"relative\\file.txt\";",
            "let path = \"C:\\\\Users\\\\Alice\\\\repo\\\\file.txt\";",
        ));
        assert_eq!(of_type(&candidates, CandidateType::AbsolutePath).len(), 1);
    }

    #[test]
    fn ordinary_localhost_without_port_and_relative_paths_are_near_misses() {
        let candidates = detect_one(input(
            "src/lib.rs",
            "let path = \"assets/icon.png\";",
            "let host = \"localhost\";\nlet path = \"assets/icon.png\";",
        ));
        assert!(of_type(&candidates, CandidateType::AbsolutePath).is_empty());
        assert!(of_type(&candidates, CandidateType::LocalEndpoint).is_empty());
    }

    #[test]
    fn history_markers_require_specific_numbered_context() {
        let candidates = detect_one(input(
            "docs/review.md",
            "The legacy migration is temporary.",
            "See PR #42 for details.\nImplementation stage 3 for migration review.\nThe legacy migration is temporary.",
        ));
        let references = of_type(&candidates, CandidateType::DevelopmentReference);
        assert_eq!(references.len(), 2);
        assert!(
            references
                .iter()
                .all(|candidate| { candidate.origin == CandidateOrigin::IntroducedOrChanged })
        );
    }

    #[test]
    fn legacy_migration_temporary_and_generic_stage_words_are_near_misses() {
        let candidates = detect_one(input(
            "src/migration.rs",
            "// legacy migration temporary",
            "// legacy migration temporary\nlet stage = 2;\nlet hash = \"abcdef012345\";",
        ));
        assert!(of_type(&candidates, CandidateType::DevelopmentReference).is_empty());
    }

    #[test]
    fn contextual_sha_and_numbered_test_stage_are_detected() {
        let candidates = detect_one(input(
            "tests/review_test.rs",
            "// fixture",
            "// snapshot commit abcdef0123456789\nfn test_migration_stage_2_review() {}",
        ));
        assert_eq!(
            of_type(&candidates, CandidateType::DevelopmentReference).len(),
            2
        );
    }

    #[test]
    fn sha_context_after_unicode_text_is_safe_and_is_detected() {
        let candidates = detect_one(input(
            "docs/review.md",
            "Ссылка на коммит",
            "Ссылка на snapshot commit abcdef0123456789",
        ));
        assert_eq!(
            of_type(&candidates, CandidateType::DevelopmentReference).len(),
            1
        );
    }

    #[test]
    fn rust_suppressions_and_ci_suppressions_are_narrowly_detected() {
        let rust = detect_one(input(
            "src/lib.rs",
            "// allow(clippy::all)",
            "#[allow(clippy::all)]\n// #[allow(clippy::all)]\n#[cfg_attr(test, allow(dead_code))]",
        ));
        assert_eq!(of_type(&rust, CandidateType::RustSuppression).len(), 2);

        let ci = detect_one(input(
            ".github/workflows/ci.yml",
            "continue-on-error: false",
            "continue-on-error: true\nrun: cargo test || true\nrun: echo \"cargo test || true\"\n# cargo test || true\nrun: cargo test",
        ));
        assert_eq!(of_type(&ci, CandidateType::CiSuppression).len(), 2);
    }

    #[test]
    fn panic_error_and_unsafe_paths_exclude_safe_fallbacks_and_comments() {
        let candidates = detect_one(input(
            "src/runtime.rs",
            "let value = result.unwrap_or_default();",
            "let value = result.unwrap();\nlet safe = result.unwrap_or_default();\n// panic!() in an example\nunsafe { call(); }\nstd::process::exit(2);",
        ));
        assert_eq!(of_type(&candidates, CandidateType::ErrorPath).len(), 2);
        assert_eq!(of_type(&candidates, CandidateType::UnsafePath).len(), 1);
    }

    #[test]
    fn test_add_remove_ignore_and_assertion_reduction_are_candidates() {
        let candidates = detect_one(input(
            "tests/flow.rs",
            "#[test]\nfn test_old() {\n    assert_eq!(1, 1);\n}\n",
            "#[test]\n#[ignore]\nfn test_new() {\n}\n",
        ));
        assert_eq!(of_type(&candidates, CandidateType::TestAdded).len(), 1);
        assert_eq!(of_type(&candidates, CandidateType::TestRemoved).len(), 1);
        assert_eq!(of_type(&candidates, CandidateType::TestIgnored).len(), 1);
        assert_eq!(
            of_type(&candidates, CandidateType::AssertionRemoved).len(),
            1
        );
        assert!(
            of_type(&candidates, CandidateType::TestRemoved)
                .iter()
                .all(|candidate| candidate.origin == CandidateOrigin::IntroducedOrChanged)
        );
    }

    #[test]
    fn harmless_assertion_edit_with_same_count_is_not_assertion_removal() {
        let candidates = detect_one(input(
            "tests/flow.rs",
            "#[test]\nfn test_value() { assert_eq!(actual, 1); }",
            "#[test]\nfn test_value() { assert_eq!(actual, 2); }",
        ));
        assert!(of_type(&candidates, CandidateType::AssertionRemoved).is_empty());
        assert!(of_type(&candidates, CandidateType::TestRemoved).is_empty());
        assert!(of_type(&candidates, CandidateType::TestAdded).is_empty());
    }

    #[test]
    fn existing_line_signal_is_explicitly_pre_existing() {
        let line = "let value = result.unwrap();";
        let candidates = detect_one(input(
            "src/runtime.rs",
            line,
            &format!("{line}\nlet other = result.expect(\"value\");"),
        ));
        let error_paths = of_type(&candidates, CandidateType::ErrorPath);
        assert_eq!(error_paths.len(), 2);
        assert_eq!(error_paths[0].origin, CandidateOrigin::PreExisting);
        assert_eq!(error_paths[1].origin, CandidateOrigin::IntroducedOrChanged);
    }

    #[test]
    fn dependency_entries_are_reported_but_non_dependency_manifest_edits_are_not() {
        let changed = detect_one(input(
            "Cargo.toml",
            "[dependencies]\nserde = \"1\"\n\n[package]\nversion = \"0.1\"",
            "[dependencies]\nserde = \"2\"\n\n[package]\nversion = \"0.1\"",
        ));
        let dependency_candidates = of_type(&changed, CandidateType::DependencyChange);
        assert_eq!(dependency_candidates.len(), 1);
        assert_eq!(
            dependency_candidates[0].origin,
            CandidateOrigin::IntroducedOrChanged
        );

        let unrelated = detect_one(input(
            "Cargo.toml",
            "[package]\nversion = \"0.1\"\n\n[dependencies]\nserde = \"1\"",
            "[package]\nversion = \"0.2\"\n\n[dependencies]\nserde = \"1\"",
        ));
        assert!(of_type(&unrelated, CandidateType::DependencyChange).is_empty());
    }

    #[test]
    fn file_surfaces_are_routing_evidence_and_not_findings() {
        let candidates = detect_one(input(
            ".agents/skills/example/SKILL.md",
            "old prose",
            "new prose",
        ));
        assert_eq!(of_type(&candidates, CandidateType::SkillSurface).len(), 1);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.origin == CandidateOrigin::IntroducedOrChanged)
        );
        let encoded = serde_json::to_value(&candidates[0]).expect("candidate сериализуется");
        assert!(encoded.get("type").is_some());
        assert!(encoded.get("finding").is_none());
    }

    #[test]
    fn generated_and_security_surfaces_are_detected_from_path() {
        let candidates = detect_one(input(
            "src/security/generated/keys.generated.rs",
            "old",
            "new",
        ));
        assert_eq!(
            of_type(&candidates, CandidateType::GeneratedSurface).len(),
            1
        );
        assert_eq!(
            of_type(&candidates, CandidateType::SecuritySurface).len(),
            1
        );

        let near_miss = detect_one(input("src/monkey.rs", "old", "new"));
        assert!(of_type(&near_miss, CandidateType::SecuritySurface).is_empty());
    }

    #[test]
    fn runtime_security_surfaces_are_routed_without_becoming_findings() {
        let candidates = detect_one(input(
            "src/runtime.rs",
            "let value = 3;",
            "let mut command = std::process::Command::new(\"git\");\nlet value = serde_json::from_str::<Value>(&input)?;\nstd::fs::remove_file(path)?;",
        ));
        let surfaces = of_type(&candidates, CandidateType::SecuritySurface);
        assert_eq!(surfaces.len(), 3);
        assert!(
            surfaces
                .iter()
                .all(|candidate| candidate.origin == CandidateOrigin::IntroducedOrChanged)
        );
        assert!(
            surfaces
                .iter()
                .flat_map(|candidate| candidate.signals.iter())
                .any(|signal| signal == "process_spawn_surface")
        );
    }

    #[test]
    fn unknown_status_does_not_claim_pr_origin() {
        let mut data = input("src/lib.rs", "// old", "let value = result.unwrap();");
        data.status = FileStatus::Unknown;
        let candidates = detect(std::slice::from_ref(&data));
        let error_path = of_type(&candidates, CandidateType::ErrorPath)[0];
        assert_eq!(error_path.origin, CandidateOrigin::Unknown);
    }

    #[test]
    fn duplicate_and_moved_lines_use_changed_occurrence_ranges() {
        let line = "#[allow(dead_code)]";
        let duplicated = detect_one(input("src/lib.rs", line, &format!("{line}\n{line}")));
        let suppressions = of_type(&duplicated, CandidateType::RustSuppression);
        assert_eq!(suppressions.len(), 2);
        assert_eq!(suppressions[0].origin, CandidateOrigin::PreExisting);
        assert_eq!(suppressions[1].origin, CandidateOrigin::IntroducedOrChanged);

        let mut moved = input(
            "src/lib.rs",
            &format!("{line}\nfn original() {{}}"),
            &format!("fn original() {{}}\n{line}"),
        );
        // Git доказывает удаление первого occurrence и вставку второго.
        moved.post_changed_lines = Some(vec![LineRange { start: 2, end: 3 }]);
        let candidates = detect_one(moved);
        assert_eq!(
            of_type(&candidates, CandidateType::RustSuppression)[0].origin,
            CandidateOrigin::IntroducedOrChanged
        );
    }

    #[test]
    fn missing_positional_evidence_does_not_infer_origin_from_matching_text() {
        let mut data = input(
            "src/lib.rs",
            "#[allow(dead_code)]",
            "#[allow(dead_code)]\n#[allow(dead_code)]",
        );
        data.post_changed_lines = None;
        assert!(
            of_type(&detect_one(data), CandidateType::RustSuppression)
                .iter()
                .all(|candidate| candidate.origin == CandidateOrigin::Unknown)
        );
    }

    #[test]
    fn comments_and_literals_cannot_declare_test_functions() {
        let old = r###"#[test]
/// Checks fn old_doc_name parsing.
/* fn block_comment() {} */
fn actual_test() {
    let ordinary = "#[test] fn string_test() {}";
    let raw = r##"#[test] fn raw_test() {}"##;
    let byte_string = b"#[test] fn byte_test() {}";
}
"###;
        let post = old.replace("old_doc_name", "new_doc_name");
        let candidates = detect_one(input("src/lib.rs", old, &post));
        assert!(of_type(&candidates, CandidateType::TestAdded).is_empty());
        assert!(of_type(&candidates, CandidateType::TestRemoved).is_empty());
        assert_eq!(
            rust_test_evidence(old, false)
                .functions
                .keys()
                .collect::<Vec<_>>(),
            vec!["actual_test"]
        );
        let prefix = "/* #[test] fn commented_test() {} */\n// #[test]\n";
        assert!(rust_test_evidence(prefix, false).functions.is_empty());
    }

    #[test]
    fn test_attribute_does_not_bind_to_next_unrelated_item() {
        let source = "#[test]\nconst EXAMPLE: &str = \"fn fake_test() {}\";\nfn production() {}";
        assert!(rust_test_evidence(source, false).functions.is_empty());
        let source = "#[tokio::test(flavor = \"current_thread\")]\n#[ignore]\npub(crate) async fn actual_test() {}";
        assert!(
            rust_test_evidence(source, false)
                .functions
                .contains_key("actual_test")
        );
    }

    #[test]
    fn mixed_assertion_edit_and_removal_only_proves_a_count_delta() {
        let candidates = detect_one(input(
            "tests/flow.rs",
            "#[test]\nfn value() {\nassert_eq!(actual, 1);\nassert!(other);\n}",
            "#[test]\nfn value() {\nassert_eq!(actual, 2);\n}",
        ));
        let removed = of_type(&candidates, CandidateType::AssertionRemoved);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].line, None);
        assert_eq!(removed[0].snippet, None);
        assert_eq!(removed[0].metadata["total_assertion_count_delta"], "-1");
        assert_eq!(removed[0].metadata["base_assertion_count"], "2");
        assert_eq!(removed[0].metadata["post_assertion_count"], "1");
    }

    #[test]
    fn production_assertions_are_not_test_evidence_in_a_mixed_file() {
        let test_module = "#[cfg(test)]\nmod tests {\n#[test] fn value() { assert_eq!(1, 1); }\n}";
        let base = format!("fn production() {{ assert!(valid); }}\n{test_module}");
        let post = format!("fn production() {{}}\n{test_module}");
        let candidates = detect_one(input("src/lib.rs", &base, &post));
        assert!(of_type(&candidates, CandidateType::AssertionRemoved).is_empty());
        let base = "#[test] fn value() { assert!(valid); }\nfn production() { assert!(valid); }";
        let post = "#[test] fn value() { assert!(valid); }\nfn production() {}";
        assert!(
            of_type(
                &detect_one(input("src/lib.rs", base, post)),
                CandidateType::AssertionRemoved
            )
            .is_empty()
        );
    }

    #[test]
    fn test_module_helpers_and_test_functions_count_real_macros_only() {
        let base = r###"fn production() { assert!(valid); }
#[cfg(test)] mod checks {
    fn helper() { assert!(valid); }
    #[test] fn test_value() {
        assert_eq!(1, 1);
        let text = "assert!(fake)";
        let raw = r##"assert!(fake)"##;
        /* assert!(fake); */
    }
}
"###;
        let post = base.replace("assert!(valid); }\n    #[test]", "}\n    #[test]");
        let candidates = detect_one(input("src/lib.rs", base, &post));
        let removed = of_type(&candidates, CandidateType::AssertionRemoved);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].metadata["base_assertion_count"], "2");
        assert_eq!(removed[0].metadata["post_assertion_count"], "1");
    }

    #[test]
    fn configuration_paths_have_positive_surface_evidence() {
        let candidates = detect_one(input("settings.toml", "enabled = false", "enabled = true"));
        assert_eq!(of_type(&candidates, CandidateType::ConfigSurface).len(), 1);
    }

    #[test]
    fn candidates_and_identifiers_have_stable_order() {
        let data = input(
            "src/mixed.rs",
            "// old",
            "unsafe { call(); }\nlet path = \"/tmp/example/file\";\nlet value = result.unwrap();",
        );
        let first = detect(std::slice::from_ref(&data));
        let second = detect(std::slice::from_ref(&data));
        assert_eq!(first, second);
        assert!(first.windows(2).all(|pair| {
            pair[0]
                .path
                .cmp(&pair[1].path)
                .then(
                    pair[0]
                        .candidate_type
                        .as_str()
                        .cmp(pair[1].candidate_type.as_str()),
                )
                .then(pair[0].line.cmp(&pair[1].line))
                .then(pair[0].id.cmp(&pair[1].id))
                .is_le()
        }));
        assert!(
            first
                .iter()
                .all(|candidate| candidate.id.starts_with("cand-") && candidate.id.len() == 21)
        );
    }
}
