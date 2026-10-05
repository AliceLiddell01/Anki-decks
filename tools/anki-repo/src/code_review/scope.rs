//! Неизменяемый Git-снимок диапазона для механического evidence.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Верхняя граница полного текстового образа одного файла.
pub const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ScopeError {
    #[error("Не удалось выполнить Git: {0}")]
    Io(#[from] std::io::Error),
    #[error("Git не смог выполнить {operation}: {detail}")]
    Git { operation: String, detail: String },
    #[error("Недоступный Git ref «{reference}»: {detail}")]
    InvalidRef { reference: String, detail: String },
    #[error("Невозможно разобрать машинный ответ Git: {0}")]
    InvalidOutput(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitTarget {
    /// Локальная идентичность репозитория без раскрытия путей и remote URL.
    pub repository_id: String,
    pub base_sha: String,
    pub head_sha: String,
    pub merge_base_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileCategory {
    Rust,
    Markdown,
    Toml,
    Yaml,
    Json,
    Shell,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSurface {
    Production,
    Tests,
    Ci,
    Config,
    Docs,
    Generated,
    Data,
    AgentContext,
    Dependencies,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageState {
    Missing,
    Text,
    Binary,
    Oversized,
    InvalidUtf8,
    Gitlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileImage {
    pub state: ImageState,
    pub size: u64,
    pub object_id: Option<String>,
    pub mode: Option<String>,
    /// Полный текст доступен внутренним detectors, но не раздувает JSON pack.
    #[serde(skip)]
    pub text: Option<String>,
}

impl FileImage {
    fn missing() -> Self {
        Self {
            state: ImageState::Missing,
            size: 0,
            object_id: None,
            mode: None,
            text: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    /// Номер первой строки, начиная с единицы.
    pub start: u64,
    /// Исключённая верхняя граница.
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedFile {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: FileStatus,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    pub category: FileCategory,
    pub surfaces: Vec<FileSurface>,
    pub binary: bool,
    pub base_changed_lines: Vec<LineRange>,
    pub post_changed_lines: Vec<LineRange>,
    /// Для rename образ берётся по previous_path из merge base.
    pub base: FileImage,
    pub post: FileImage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectedScope {
    pub target: GitTarget,
    pub text_image_limit_bytes: u64,
    pub files: Vec<ScopedFile>,
}

/// Собирает `base...head` только из Git objects; индекс и рабочие файлы не читает.
pub fn collect_scope(
    repo_root: &Path,
    base_ref: &str,
    head_ref: &str,
) -> Result<CollectedScope, ScopeError> {
    let base_sha = resolve_ref(repo_root, base_ref)?;
    let head_sha = resolve_ref(repo_root, head_ref)?;
    let merge_base_sha = output_text(git(repo_root, &["merge-base", &base_sha, &head_sha])?)?;
    let common_dir = output_text(git(
        repo_root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)?;
    let common_dir = Path::new(&common_dir).canonicalize()?;
    // Идентичность одна для всех worktree данного репозитория.
    let repository_id = format!(
        "{:x}",
        Sha256::digest(common_dir.as_os_str().as_encoded_bytes())
    );
    let target = GitTarget {
        repository_id,
        base_sha,
        head_sha,
        merge_base_sha,
    };
    let statuses = git(
        repo_root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames=50%",
            "--name-status",
            "-z",
            &target.merge_base_sha,
            &target.head_sha,
            "--",
        ],
    )?;
    let stats = git(
        repo_root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames=50%",
            "--numstat",
            "-z",
            &target.merge_base_sha,
            &target.head_sha,
            "--",
        ],
    )?;
    let stats = parse_numstat(&stats.stdout)?;
    let mut files = Vec::new();
    for (path, previous_path, status) in parse_statuses(&statuses.stdout)? {
        let (additions, deletions) = stats
            .get(&path)
            .copied()
            .ok_or_else(|| ScopeError::InvalidOutput(format!("нет numstat для «{path}»")))?;
        let base_path = previous_path.as_deref().unwrap_or(&path);
        let base = image(repo_root, &target.merge_base_sha, base_path)?;
        let post = image(repo_root, &target.head_sha, &path)?;
        let binary = additions.is_none()
            || matches!(base.state, ImageState::Binary)
            || matches!(post.state, ImageState::Binary);
        let (category, surfaces) = classify_path(&path);
        let (base_changed_lines, post_changed_lines) = if binary
            || (!matches!(base.state, ImageState::Text | ImageState::Missing))
            || (!matches!(post.state, ImageState::Text | ImageState::Missing))
        {
            (Vec::new(), Vec::new())
        } else {
            changed_lines(repo_root, &target, &path, &base, &post)?
        };
        files.push(ScopedFile {
            path,
            previous_path,
            status,
            additions,
            deletions,
            category,
            surfaces,
            binary,
            base_changed_lines,
            post_changed_lines,
            base,
            post,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(CollectedScope {
        target,
        text_image_limit_bytes: MAX_TEXT_BYTES,
        files,
    })
}

fn changed_lines(
    repo_root: &Path,
    target: &GitTarget,
    path: &str,
    base: &FileImage,
    post: &FileImage,
) -> Result<(Vec<LineRange>, Vec<LineRange>), ScopeError> {
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--unified=0",
        "--inter-hunk-context=0",
        "--no-color",
    ];
    match (&base.object_id, &post.object_id) {
        (Some(base_id), Some(post_id)) => args.extend([base_id.as_str(), post_id.as_str()]),
        _ => args.extend([&target.merge_base_sha, &target.head_sha, "--", path]),
    }
    let output = git(repo_root, &args)?;
    let mut base_lines = Vec::new();
    let mut post_lines = Vec::new();
    // Разбираются только стабильные заголовки unified hunks, не человеческие stat.
    // Строки содержимого имеют обязательный префикс +, - или пробел.
    for line in output.stdout.split(|&byte| byte == b'\n') {
        if !line.starts_with(b"@@ -") {
            continue;
        }
        let line = utf8(line)?;
        let mut parts = line.split_ascii_whitespace();
        if parts.next() != Some("@@") {
            continue;
        }
        let base_range = parse_hunk_range(parts.next(), '-')?;
        let post_range = parse_hunk_range(parts.next(), '+')?;
        if parts.next() != Some("@@") {
            return Err(ScopeError::InvalidOutput(
                "неверный заголовок hunk".to_owned(),
            ));
        }
        if let Some(range) = base_range {
            base_lines.push(range);
        }
        if let Some(range) = post_range {
            post_lines.push(range);
        }
    }
    Ok((base_lines, post_lines))
}

fn parse_hunk_range(token: Option<&str>, prefix: char) -> Result<Option<LineRange>, ScopeError> {
    let invalid = || ScopeError::InvalidOutput("неверный диапазон hunk".to_owned());
    let token = token
        .and_then(|value| value.strip_prefix(prefix))
        .ok_or_else(invalid)?;
    let (start, count) = token.split_once(',').unwrap_or((token, "1"));
    let start: u64 = start.parse().map_err(|_| invalid())?;
    let count: u64 = count.parse().map_err(|_| invalid())?;
    if count == 0 {
        return Ok(None);
    }
    Ok(Some(LineRange {
        start,
        end: start.checked_add(count).ok_or_else(invalid)?,
    }))
}

fn git_command(repo_root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(repo_root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .args(["--no-pager", "-c", "color.ui=false"])
        .args(args);
    command
}

fn git(repo_root: &Path, args: &[&str]) -> Result<Output, ScopeError> {
    let output = git_command(repo_root, args).output()?;
    if !output.status.success() {
        return Err(ScopeError::Git {
            operation: args.first().unwrap_or(&"Git").to_string(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(output)
}

fn output_text(output: Output) -> Result<String, ScopeError> {
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|_| ScopeError::InvalidOutput("ответ должен быть UTF-8".to_owned()))
}

fn resolve_ref(repo_root: &Path, reference: &str) -> Result<String, ScopeError> {
    let revision = format!("{reference}^{{commit}}");
    let output = git_command(
        repo_root,
        &["rev-parse", "--verify", "--end-of-options", &revision],
    )
    .output()?;
    if !output.status.success() {
        return Err(ScopeError::InvalidRef {
            reference: reference.to_owned(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    output_text(output)
}

fn utf8(bytes: &[u8]) -> Result<String, ScopeError> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| ScopeError::InvalidOutput("путь Git содержит байты вне UTF-8".to_owned()))
}

type StatusRecord = (String, Option<String>, FileStatus);

fn parse_statuses(bytes: &[u8]) -> Result<Vec<StatusRecord>, ScopeError> {
    let mut tokens = bytes.split(|&byte| byte == 0).peekable();
    let mut records = Vec::new();
    while let Some(status) = tokens.next() {
        if status.is_empty() && tokens.peek().is_none() {
            break;
        }
        let kind = match status.first() {
            Some(b'A') => FileStatus::Added,
            Some(b'M') => FileStatus::Modified,
            Some(b'D') => FileStatus::Deleted,
            Some(b'R') => FileStatus::Renamed,
            Some(b'C') => FileStatus::Copied,
            Some(b'T') => FileStatus::TypeChanged,
            _ => {
                return Err(ScopeError::InvalidOutput(format!(
                    "неподдерживаемый статус «{}»",
                    String::from_utf8_lossy(status)
                )));
            }
        };
        let first = utf8(
            tokens
                .next()
                .ok_or_else(|| ScopeError::InvalidOutput("нет пути после статуса".to_owned()))?,
        )?;
        let (path, previous) = if matches!(kind, FileStatus::Renamed | FileStatus::Copied) {
            let second = utf8(tokens.next().ok_or_else(|| {
                ScopeError::InvalidOutput("нет нового пути rename/copy".to_owned())
            })?)?;
            (second, Some(first))
        } else {
            (first, None)
        };
        records.push((path, previous, kind));
    }
    Ok(records)
}

type Numstat = BTreeMap<String, (Option<u64>, Option<u64>)>;

fn parse_numstat(bytes: &[u8]) -> Result<Numstat, ScopeError> {
    let mut tokens = bytes.split(|&byte| byte == 0).peekable();
    let mut stats = BTreeMap::new();
    while let Some(record) = tokens.next() {
        if record.is_empty() && tokens.peek().is_none() {
            break;
        }
        let mut parts = record.splitn(3, |&byte| byte == b'\t');
        let additions = parse_count(parts.next())?;
        let deletions = parse_count(parts.next())?;
        let path = parts
            .next()
            .ok_or_else(|| ScopeError::InvalidOutput("нет пути numstat".to_owned()))?;
        let path =
            if path.is_empty() {
                tokens.next().ok_or_else(|| {
                    ScopeError::InvalidOutput("нет старого пути numstat".to_owned())
                })?;
                utf8(tokens.next().ok_or_else(|| {
                    ScopeError::InvalidOutput("нет нового пути numstat".to_owned())
                })?)?
            } else {
                utf8(path)?
            };
        stats.insert(path, (additions, deletions));
    }
    Ok(stats)
}

fn parse_count(bytes: Option<&[u8]>) -> Result<Option<u64>, ScopeError> {
    match bytes {
        Some(b"-") => Ok(None),
        Some(bytes) => utf8(bytes)?
            .parse::<u64>()
            .map(Some)
            .map_err(|_| ScopeError::InvalidOutput("неверный счётчик numstat".to_owned())),
        None => Err(ScopeError::InvalidOutput("нет счётчика numstat".to_owned())),
    }
}

fn image(repo_root: &Path, sha: &str, path: &str) -> Result<FileImage, ScopeError> {
    let entry = git(repo_root, &["ls-tree", "-z", sha, "--", path])?;
    if entry.stdout.is_empty() {
        return Ok(FileImage::missing());
    }
    let metadata = entry
        .stdout
        .split(|&byte| byte == b'\t')
        .next()
        .ok_or_else(|| ScopeError::InvalidOutput("нет метаданных ls-tree".to_owned()))?;
    let metadata = utf8(metadata)?;
    let mut parts = metadata.split(' ');
    let mode = parts.next().unwrap_or_default().to_owned();
    let object_type = parts.next().unwrap_or_default();
    let object_id = parts
        .next()
        .ok_or_else(|| ScopeError::InvalidOutput("нет object id ls-tree".to_owned()))?;
    let mut image = FileImage {
        state: ImageState::Gitlink,
        size: 0,
        object_id: Some(object_id.to_owned()),
        mode: Some(mode),
        text: None,
    };
    if object_type == "commit" {
        return Ok(image);
    }
    if object_type != "blob" {
        return Err(ScopeError::InvalidOutput(format!(
            "объект «{path}» не является blob"
        )));
    }
    image.size = output_text(git(repo_root, &["cat-file", "-s", object_id])?)?
        .parse()
        .map_err(|_| ScopeError::InvalidOutput("неверный размер blob".to_owned()))?;
    if image.size > MAX_TEXT_BYTES {
        image.state = ImageState::Oversized;
        return Ok(image);
    }
    let bytes = git(repo_root, &["cat-file", "blob", object_id])?.stdout;
    if bytes.contains(&0) {
        image.state = ImageState::Binary;
    } else if let Ok(text) = String::from_utf8(bytes) {
        image.state = ImageState::Text;
        image.text = Some(text);
    } else {
        image.state = ImageState::InvalidUtf8;
    }
    Ok(image)
}

/// Классификация следует форматам и сегментам пути, а не списку файлов текущего PR.
pub fn classify_path(path: &str) -> (FileCategory, Vec<FileSurface>) {
    let file = Path::new(path);
    let name = file
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let extension = file
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let category = match extension.to_ascii_lowercase().as_str() {
        "rs" => FileCategory::Rust,
        "md" | "markdown" => FileCategory::Markdown,
        "toml" => FileCategory::Toml,
        "yml" | "yaml" => FileCategory::Yaml,
        "json" => FileCategory::Json,
        "sh" | "bash" | "zsh" => FileCategory::Shell,
        _ => FileCategory::Other,
    };
    let segments: Vec<_> = path.split('/').collect();
    let mut surfaces = Vec::new();
    if path.starts_with("decks/") || segments.contains(&"fixtures") {
        surfaces.push(FileSurface::Data);
    }
    if path.starts_with(".agents/") || path.starts_with(".codex/") || name == "AGENTS.md" {
        surfaces.push(FileSurface::AgentContext);
    }
    if segments.contains(&"tests")
        || segments.contains(&"test")
        || name.ends_with("_tests.rs")
        || name.ends_with("_test.rs")
    {
        surfaces.push(FileSurface::Tests);
    }
    if path.starts_with(".github/") || path.starts_with(".gitlab/") || name == ".gitlab-ci.yml" {
        surfaces.push(FileSurface::Ci);
    }
    if matches!(
        category,
        FileCategory::Toml | FileCategory::Yaml | FileCategory::Json
    ) || segments.contains(&"config")
        || name.starts_with('.')
    {
        surfaces.push(FileSurface::Config);
    }
    if matches!(category, FileCategory::Markdown) || segments.contains(&"docs") {
        surfaces.push(FileSurface::Docs);
    }
    if segments.contains(&"generated")
        || segments.contains(&"target")
        || name.ends_with(".lock")
        || name.contains(".min.")
    {
        surfaces.push(FileSurface::Generated);
    }
    if matches!(
        name,
        "Cargo.toml" | "Cargo.lock" | "package.json" | "package-lock.json"
    ) {
        surfaces.push(FileSurface::Dependencies);
    }
    if surfaces.is_empty() || segments.contains(&"src") {
        surfaces.push(FileSurface::Production);
    }
    surfaces.sort();
    surfaces.dedup();
    (category, surfaces)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    struct Repo {
        dir: TempDir,
    }

    impl Repo {
        fn new() -> Self {
            let repo = Self {
                dir: TempDir::new("code-review-scope"),
            };
            repo.run(&["init", "-b", "main"]);
            repo.run(&["config", "user.name", "Тест"]);
            repo.run(&["config", "user.email", "fixture@example.invalid"]);
            repo.run(&["config", "commit.gpgsign", "false"]);
            repo.run(&["config", "core.autocrlf", "false"]);
            repo
        }

        fn run(&self, args: &[&str]) -> String {
            output_text(git(self.dir.path(), args).expect("Git fixture должен работать"))
                .expect("вывод fixture должен быть UTF-8")
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let path = self.dir.path().join(path);
            fs::create_dir_all(path.parent().expect("у файла есть родитель"))
                .expect("каталог fixture должен создаваться");
            fs::write(path, bytes).expect("файл fixture должен записываться");
        }

        fn commit(&self) -> String {
            self.run(&["add", "--all"]);
            self.run(&["commit", "--allow-empty", "-m", "Тестовый снимок"]);
            self.run(&["rev-parse", "HEAD"])
        }

        fn scope(&self, base: &str, head: &str) -> CollectedScope {
            collect_scope(self.dir.path(), base, head).expect("снимок fixture должен собираться")
        }
    }

    #[test]
    fn collects_add_modify_delete_rename_binary_and_unusual_paths() {
        let repo = Repo::new();
        repo.write("modify.rs", b"before\n");
        repo.write("delete.rs", b"deleted\n");
        repo.write("old name.txt", b"retained content\n");
        let base = repo.commit();
        repo.run(&["mv", "old name.txt", "новое имя.txt"]);
        fs::remove_file(repo.dir.path().join("delete.rs")).expect("файл удаляется");
        repo.write("modify.rs", b"after\n");
        repo.write("новый файл.rs", "// Человеческий текст\n".as_bytes());
        repo.write("data.bin", b"\0\x01\xff");
        repo.write("colon: [literal]*\tfile.rs", b"literal path\n");
        let head = repo.commit();
        let scope = repo.scope(&base, &head);
        let files: BTreeMap<_, _> = scope
            .files
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect();
        assert_eq!(files["modify.rs"].status, FileStatus::Modified);
        assert_eq!(files["modify.rs"].base.text.as_deref(), Some("before\n"));
        assert_eq!(files["modify.rs"].post.text.as_deref(), Some("after\n"));
        assert_eq!(files["modify.rs"].additions, Some(1));
        assert_eq!(files["modify.rs"].deletions, Some(1));
        assert_eq!(
            files["modify.rs"].post_changed_lines,
            vec![LineRange { start: 1, end: 2 }]
        );
        assert_eq!(files["delete.rs"].status, FileStatus::Deleted);
        assert_eq!(files["delete.rs"].post.state, ImageState::Missing);
        assert_eq!(files["новый файл.rs"].status, FileStatus::Added);
        assert_eq!(files["новое имя.txt"].status, FileStatus::Renamed);
        assert_eq!(
            files["новое имя.txt"].previous_path.as_deref(),
            Some("old name.txt")
        );
        assert_eq!(
            files["новое имя.txt"].base.text,
            files["новое имя.txt"].post.text
        );
        assert!(files["data.bin"].binary);
        assert_eq!(files["data.bin"].post.state, ImageState::Binary);
        assert_eq!(files["data.bin"].additions, None);
        assert_eq!(
            files["colon: [literal]*\tfile.rs"].post.text.as_deref(),
            Some("literal path\n")
        );
        assert_eq!(scope, repo.scope(&base, &head));
        assert_eq!(
            serde_json::to_vec(&scope).unwrap(),
            serde_json::to_vec(&repo.scope(&base, &head)).unwrap()
        );
        repo.run(&["branch", "review-base", &base]);
        repo.run(&["branch", "review-head", &head]);
        assert_eq!(
            serde_json::to_vec(&scope).unwrap(),
            serde_json::to_vec(&repo.scope("review-base", "review-head")).unwrap()
        );
        assert!(
            scope
                .files
                .windows(2)
                .all(|files| files[0].path < files[1].path)
        );
    }

    #[test]
    fn empty_diff_and_missing_ref_are_explicit() {
        let repo = Repo::new();
        let sha = repo.commit();
        assert!(repo.scope(&sha, &sha).files.is_empty());
        let error = collect_scope(repo.dir.path(), "absent-reference", &sha)
            .expect_err("несуществующий ref отклоняется");
        assert!(matches!(error, ScopeError::InvalidRef { .. }));
        assert!(error.to_string().contains("Недоступный Git ref"));
    }

    #[test]
    fn detached_explicit_sha_and_dirty_index_do_not_change_worktree() {
        let repo = Repo::new();
        repo.write("source.rs", b"base\n");
        let base = repo.commit();
        repo.write("source.rs", b"head\n");
        let head = repo.commit();
        repo.run(&["checkout", "--detach", &base]);
        repo.write("source.rs", b"staged\n");
        repo.run(&["add", "source.rs"]);
        repo.write("source.rs", b"dirty\n");
        repo.write("untracked", b"preserved\n");
        let before = repo.run(&["status", "--porcelain=v1", "-z"]);
        let scope = repo.scope(&base, &head);
        assert_eq!(scope.files[0].post.text.as_deref(), Some("head\n"));
        assert_eq!(repo.run(&["rev-parse", "HEAD"]), base);
        assert_eq!(before, repo.run(&["status", "--porcelain=v1", "-z"]));
        assert_eq!(
            fs::read(repo.dir.path().join("source.rs")).unwrap(),
            b"dirty\n"
        );
        assert_eq!(
            fs::read(repo.dir.path().join("untracked")).unwrap(),
            b"preserved\n"
        );
    }

    #[test]
    fn scope_uses_merge_base_when_base_has_diverged() {
        let repo = Repo::new();
        repo.write("shared.rs", b"initial\n");
        let common = repo.commit();
        repo.run(&["checkout", "-b", "feature"]);
        repo.write("feature.rs", b"feature\n");
        let head = repo.commit();
        repo.run(&["checkout", "main"]);
        repo.write("base-only.rs", b"base\n");
        let base = repo.commit();
        let scope = repo.scope(&base, &head);
        assert_eq!(scope.target.merge_base_sha, common);
        assert_eq!(scope.files.len(), 1);
        assert_eq!(scope.files[0].path, "feature.rs");
    }

    #[test]
    fn large_and_invalid_utf8_images_are_observable_without_full_text_in_json() {
        let repo = Repo::new();
        let base = repo.commit();
        repo.write("large.md", &vec![b'a'; MAX_TEXT_BYTES as usize + 1]);
        repo.write("invalid.txt", b"invalid \xff\n");
        repo.write("normal.rs", b"text hidden from serialization\n");
        let head = repo.commit();
        let scope = repo.scope(&base, &head);
        assert_eq!(scope.files[0].post.state, ImageState::InvalidUtf8);
        assert_eq!(scope.files[1].post.state, ImageState::Oversized);
        assert!(scope.files[1].post.text.is_none());
        let json = serde_json::to_string(&scope).unwrap();
        assert!(!json.contains("text hidden from serialization"));
        assert!(!json.contains(repo.dir.path().to_str().unwrap()));
    }

    #[test]
    fn classes_follow_formats_and_path_surfaces() {
        assert_eq!(
            classify_path("tools/sample/src/lib.rs"),
            (FileCategory::Rust, vec![FileSurface::Production])
        );
        assert!(
            classify_path("tests/example.rs")
                .1
                .contains(&FileSurface::Tests)
        );
        assert!(
            classify_path(".github/workflows/check.yml")
                .1
                .contains(&FileSurface::Ci)
        );
        assert!(
            classify_path("decks/arbitrary/deck.json")
                .1
                .contains(&FileSurface::Data)
        );
        assert!(
            classify_path(".agents/skills/example/SKILL.md")
                .1
                .contains(&FileSurface::AgentContext)
        );
        assert!(
            classify_path("Cargo.lock")
                .1
                .contains(&FileSurface::Dependencies)
        );
    }

    #[test]
    fn provenance_distinguishes_unchanged_context_and_insertions() {
        let repo = Repo::new();
        repo.write("source.rs", b"unchanged\nold\ncontext\n");
        let base = repo.commit();
        repo.write("source.rs", b"unchanged\nnew\ncontext\nadded\n");
        let head = repo.commit();
        let scope = repo.scope(&base, &head);
        assert_eq!(
            scope.files[0].base_changed_lines,
            vec![LineRange { start: 2, end: 3 }]
        );
        assert_eq!(
            scope.files[0].post_changed_lines,
            vec![
                LineRange { start: 2, end: 3 },
                LineRange { start: 4, end: 5 }
            ]
        );
    }
}
