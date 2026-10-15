//! Долговременный локальный журнал операций с ресурсами.
//!
//! Журнал живёт в `<store-root>/.journal/events.jsonl` рядом с `manifest.json`, но
//! вне `.runtime/batches/**`: он переживает удаление локального состояния одного
//! пакета. Каждая строка — один JSON-объект; файл только дополняется.
//!
//! Журнал — история переходов, а не второй манифест. Источником истины о текущем
//! `VERIFIED`-корпусе остаётся `manifest.json`; журнал лишь объясняет, как это
//! состояние получилось, и позволяет обнаружить изменение, зафиксированное в
//! манифесте, но не закрытое событием результата.
//!
//! Запись выполняется только под уже удерживаемой эксклюзивной блокировкой store:
//! ни один worker пакета не обращается к журналу, поэтому многоворкерное получение
//! не замедляется.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{Mode, OFlags, openat};
use serde::{Deserialize, Serialize};

use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::model::{AssetIdentity, LifecycleState, Manifest, Provenance};
use crate::store::{directory_entry_error, ensure_dir_entry, open_directory_at, sync_directory};

/// Каталог журнала в корне хранилища.
pub const JOURNAL_DIR: &str = ".journal";
/// Единственный файл журнала: `NDJSON`, только дополнение.
pub const JOURNAL_FILE: &str = "events.jsonl";
/// Версия формата события.
pub const JOURNAL_SCHEMA_VERSION: u32 = 1;
/// Идентичность события уровня всего хранилища, а не отдельного ресурса.
pub const STORE_WIDE_IDENTITY: &str = "*";

/// Верхняя граница читаемого журнала: защита от неограниченного чтения.
const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
/// Верхняя граница сохраняемой технической причины.
const MAX_REASON_CHARS: usize = 400;
/// Разделитель компонентов ключа идемпотентности: управляющий символ не встречается
/// ни в одном доменном значении.
const FIELD_SEPARATOR: char = '\u{1f}';

static EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Операция, о которой сообщает событие журнала.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalOperation {
    CandidateWritten,
    VerifiedPublished,
    VerifiedReused,
    TrustGranted,
    TrustRevoked,
    Revalidated,
    Quarantined,
    Removed,
    Recovered,
    LayoutMigrated,
    Baseline,
}

impl JournalOperation {
    /// Стабильное имя операции в формате `snake_case`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CandidateWritten => "candidate_written",
            Self::VerifiedPublished => "verified_published",
            Self::VerifiedReused => "verified_reused",
            Self::TrustGranted => "trust_granted",
            Self::TrustRevoked => "trust_revoked",
            Self::Revalidated => "revalidated",
            Self::Quarantined => "quarantined",
            Self::Removed => "removed",
            Self::Recovered => "recovered",
            Self::LayoutMigrated => "layout_migrated",
            Self::Baseline => "baseline",
        }
    }

    /// Разбирает имя операции для фильтра `journal list --operation`.
    pub fn parse(value: &str) -> Option<Self> {
        const ALL: [JournalOperation; 11] = [
            JournalOperation::CandidateWritten,
            JournalOperation::VerifiedPublished,
            JournalOperation::VerifiedReused,
            JournalOperation::TrustGranted,
            JournalOperation::TrustRevoked,
            JournalOperation::Revalidated,
            JournalOperation::Quarantined,
            JournalOperation::Removed,
            JournalOperation::Recovered,
            JournalOperation::LayoutMigrated,
            JournalOperation::Baseline,
        ];
        ALL.into_iter()
            .find(|operation| operation.as_str() == value)
    }
}

impl std::fmt::Display for JournalOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Итог операции, зафиксированный событием.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalOutcome {
    /// Намерение записано до изменения состояния.
    Pending,
    /// Изменение состояния зафиксировано.
    Completed,
    /// Операция отказала до фиксации; состояние не изменилось.
    Refused,
    /// Незакрытое намерение сверено с фактическим состоянием: изменение состоялось.
    Recovered,
    /// Незакрытое намерение сверено с фактическим состоянием: изменения не было.
    Aborted,
    /// Незакрытое намерение не совпало ни с ожидаемым, ни с прежним состоянием.
    Unknown,
}

impl JournalOutcome {
    /// Стабильное имя итога в формате `snake_case`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Refused => "refused",
            Self::Recovered => "recovered",
            Self::Aborted => "aborted",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for JournalOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Снимок фактически проверенного состояния хранилища на момент `baseline`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalSnapshot {
    pub assets: u64,
    pub verified: u64,
    pub pending: u64,
    pub quarantined: u64,
    pub manifest_revision: u64,
}

/// Одно событие журнала: одна строка `events.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEvent {
    pub schema_version: u32,
    pub seq: u64,
    pub event_id: String,
    pub event_key: String,
    pub recorded_at: String,
    pub domain: String,
    pub namespace: String,
    pub identity: String,
    pub operation: JournalOperation,
    pub outcome: JournalOutcome,
    pub intent_id: Option<String>,
    pub manifest_revision: Option<u64>,
    pub previous_sha256: Option<String>,
    pub new_sha256: Option<String>,
    pub previous_lifecycle: Option<String>,
    pub new_lifecycle: Option<String>,
    /// Состояние записи после события: `true` — `verified`, `false` — не
    /// `verified` или записи нет, `null` — событие не относится к одной записи.
    pub verified: Option<bool>,
    pub run_id: Option<String>,
    pub batch_id: Option<String>,
    pub provenance: Option<String>,
    pub reason: Option<String>,
    pub snapshot: Option<JournalSnapshot>,
}

impl JournalEvent {
    /// Истина, если событие закрывает интент и потому само интентом не является.
    pub const fn closes_intent(&self) -> bool {
        self.intent_id.is_some()
    }

    /// Истина, если это незакрытое намерение.
    pub const fn is_pending_intent(&self) -> bool {
        matches!(self.outcome, JournalOutcome::Pending)
    }
}

/// Незакрытый интент, перечисляемый проверкой журнала.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JournalOpenIntent {
    pub event_id: String,
    pub seq: u64,
    pub identity: String,
    pub namespace: String,
    pub operation: JournalOperation,
    pub recorded_at: String,
}

/// Готовое к записи событие без вычисляемых полей.
#[derive(Debug, Clone)]
pub struct JournalDraft {
    domain: String,
    namespace: String,
    identity: String,
    operation: JournalOperation,
    outcome: JournalOutcome,
    intent_id: Option<String>,
    manifest_revision: Option<u64>,
    previous_sha256: Option<String>,
    new_sha256: Option<String>,
    previous_lifecycle: Option<String>,
    new_lifecycle: Option<String>,
    verified: Option<bool>,
    run_id: Option<String>,
    batch_id: Option<String>,
    provenance: Option<String>,
    reason: Option<String>,
    snapshot: Option<JournalSnapshot>,
}

impl JournalDraft {
    /// Создаёт событие для одного ресурса.
    pub fn new(
        domain: &str,
        identity: &AssetIdentity,
        operation: JournalOperation,
        outcome: JournalOutcome,
    ) -> Self {
        Self::store_wide(
            domain,
            &identity.namespace,
            &identity.key,
            operation,
            outcome,
        )
    }

    /// Создаёт событие уровня всего хранилища, не привязанное к одному ресурсу.
    pub fn store_wide(
        domain: &str,
        namespace: &str,
        identity: &str,
        operation: JournalOperation,
        outcome: JournalOutcome,
    ) -> Self {
        Self {
            domain: domain.to_owned(),
            namespace: namespace.to_owned(),
            identity: identity.to_owned(),
            operation,
            outcome,
            intent_id: None,
            manifest_revision: None,
            previous_sha256: None,
            new_sha256: None,
            previous_lifecycle: None,
            new_lifecycle: None,
            verified: None,
            run_id: None,
            batch_id: None,
            provenance: None,
            reason: None,
            snapshot: None,
        }
    }

    /// Указывает интент, который закрывает это событие.
    #[must_use]
    pub fn closing(mut self, intent_id: &str) -> Self {
        self.intent_id = Some(intent_id.to_owned());
        self
    }

    /// Указывает ревизию манифеста, к которой относится событие.
    #[must_use]
    pub fn revision(mut self, revision: Option<u64>) -> Self {
        self.manifest_revision = revision;
        self
    }

    /// Указывает прежний и новый `SHA-256`.
    #[must_use]
    pub fn sha(mut self, previous: Option<&str>, next: Option<&str>) -> Self {
        self.previous_sha256 = previous.map(str::to_owned);
        self.new_sha256 = next.map(str::to_owned);
        self
    }

    /// Указывает прежнее и новое состояние жизненного цикла.
    #[must_use]
    pub fn lifecycle(
        mut self,
        previous: Option<LifecycleState>,
        next: Option<LifecycleState>,
    ) -> Self {
        self.previous_lifecycle = previous.map(|state| state.as_str().to_owned());
        self.new_lifecycle = next.map(|state| state.as_str().to_owned());
        self
    }

    /// Указывает, получила ли запись реально состояние `verified`.
    #[must_use]
    pub fn verified(mut self, verified: Option<bool>) -> Self {
        self.verified = verified;
        self
    }

    /// Указывает сведения об источнике в форме `source_kind:source_name`.
    #[must_use]
    pub fn provenance(mut self, provenance: Option<&Provenance>) -> Self {
        self.provenance = provenance
            .map(|provenance| format!("{}:{}", provenance.source_kind, provenance.source_name));
        self
    }

    /// Указывает сведения об источнике уже готовой строкой `source_kind:source_name`.
    ///
    /// Закрывающее событие переносит источник из своего интента, чтобы у обоих
    /// событий одного перехода он совпадал.
    #[must_use]
    pub fn provenance_text(mut self, provenance: Option<&str>) -> Self {
        self.provenance = provenance.map(str::to_owned);
        self
    }

    /// Указывает краткую безопасную техническую причину.
    #[must_use]
    pub fn reason(mut self, reason: &str) -> Self {
        self.reason = Some(sanitize_reason(reason));
        self
    }

    /// Прикладывает снимок состояния для события `baseline`.
    #[must_use]
    pub fn snapshot(mut self, snapshot: JournalSnapshot) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    /// Указывает запуск, которому принадлежит операция.
    #[must_use]
    pub fn run_id(mut self, run_id: Option<String>) -> Self {
        self.run_id = run_id;
        self
    }

    /// Указывает пакет, которому принадлежит операция.
    #[must_use]
    pub fn batch_id(mut self, batch_id: Option<String>) -> Self {
        self.batch_id = batch_id;
        self
    }

    /// Ключ идемпотентности события.
    ///
    /// В материал входит `outcome`: интент и закрывающее его событие результата
    /// описывают один и тот же переход одной и той же записи, и при неизменном
    /// состоянии (отказ до фиксации) их ключи иначе совпали бы, а закрывающее
    /// событие было бы отброшено как дубликат интента.
    pub fn event_key(&self) -> String {
        let material = format!(
            "{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}",
            self.domain,
            self.namespace,
            self.identity,
            self.operation.as_str(),
            self.outcome.as_str(),
            self.previous_sha256.as_deref().unwrap_or("-"),
            self.new_sha256.as_deref().unwrap_or("-"),
            self.manifest_revision
                .map_or_else(|| "-".to_owned(), |revision| revision.to_string()),
            sep = FIELD_SEPARATOR,
        );
        sha256_hex(material.as_bytes())
    }
}

/// Разобранный журнал и диагностика чтения.
#[derive(Debug, Clone, Default)]
pub struct JournalRead {
    pub events: Vec<JournalEvent>,
    pub torn_tail: bool,
    pub corrupt_lines: Vec<u64>,
}

/// Результат проверки целостности журнала.
#[derive(Debug, Clone, Serialize)]
pub struct JournalCheck {
    pub event_count: u64,
    pub torn_tail: bool,
    pub corrupt_lines: Vec<u64>,
    pub open_intents: Vec<JournalOpenIntent>,
    pub problems: Vec<String>,
}

impl JournalCheck {
    /// Истина, если проверка не нашла ни одной проблемы.
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

#[derive(Debug)]
struct JournalState {
    events: Vec<JournalEvent>,
    keys: BTreeSet<String>,
    torn_tail: bool,
    corrupt_lines: Vec<u64>,
    last_seq: u64,
    file_size: u64,
    file_modified: Option<SystemTime>,
}

/// Открытый журнал одного хранилища.
///
/// Дескриптор каталога и разобранное состояние кешируются в памяти: файл
/// перечитывается только тогда, когда его размер или время изменения отличаются
/// от последнего чтения. Это не мешает другому процессу: он пишет под той же
/// эксклюзивной блокировкой store.
#[derive(Debug)]
pub struct Journal {
    root: File,
    directory: Option<File>,
    state: Option<JournalState>,
    run_id: Option<String>,
    batch_id: Option<String>,
}

impl Journal {
    /// Открывает журнал, не создавая каталог и не читая файл.
    pub fn open(root: &File) -> Result<Self, AssetError> {
        let root = root.try_clone().map_err(|error| {
            AssetError::io("не удалось клонировать дескриптор store root", error)
        })?;
        Ok(Self {
            root,
            directory: None,
            state: None,
            run_id: None,
            batch_id: None,
        })
    }

    /// Запоминает запуск и пакет, к которым относятся последующие события.
    pub fn set_context(&mut self, run_id: Option<String>, batch_id: Option<String>) {
        self.run_id = run_id;
        self.batch_id = batch_id;
    }

    /// Разбирает журнал и возвращает события вместе с диагностикой чтения.
    pub fn read(&mut self) -> Result<JournalRead, AssetError> {
        self.refresh()?;
        match &self.state {
            Some(state) => Ok(JournalRead {
                events: state.events.clone(),
                torn_tail: state.torn_tail,
                corrupt_lines: state.corrupt_lines.clone(),
            }),
            None => Ok(JournalRead::default()),
        }
    }

    /// Дописывает событие; `Ok(false)` — событие с таким `event_key` уже записано.
    pub fn append(&mut self, draft: &JournalDraft) -> Result<bool, AssetError> {
        Ok(self.append_inner(draft)?.1)
    }

    /// Дописывает событие и возвращает его; для дубликата возвращается уже
    /// записанное событие, чтобы вызывающий код мог сослаться на его `event_id`.
    pub fn append_and_return(&mut self, draft: &JournalDraft) -> Result<JournalEvent, AssetError> {
        Ok(self.append_inner(draft)?.0)
    }

    fn append_inner(&mut self, draft: &JournalDraft) -> Result<(JournalEvent, bool), AssetError> {
        self.refresh()?;
        let event_key = draft.event_key();
        if let Some(existing) = self.state.as_ref().and_then(|state| {
            state
                .events
                .iter()
                .find(|event| event.event_key == event_key)
                .cloned()
        }) {
            return Ok((existing, false));
        }
        let last_seq = self.state.as_ref().map_or(0, |state| state.last_seq);
        let seq = last_seq.checked_add(1).ok_or_else(|| {
            AssetError::new(ErrorCode::ManifestCorrupt, "счётчик seq журнала переполнен")
        })?;
        let torn_tail = self.state.as_ref().is_some_and(|state| state.torn_tail);
        let event = self.build_event(draft, seq, event_key)?;
        let mut line = Vec::new();
        if torn_tail {
            // Оборванный хвост отделяется, чтобы не склеиться с новой строкой.
            line.push(b'\n');
        }
        serde_json::to_writer(&mut line, &event).map_err(|error| {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!("не удалось сериализовать событие журнала: {error}"),
            )
        })?;
        line.push(b'\n');

        let directory = self
            .ensure_directory(true)?
            .ok_or_else(|| AssetError::new(ErrorCode::IoFailure, "каталог журнала недоступен"))?;
        let fd = openat(
            &directory,
            JOURNAL_FILE,
            OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| {
            AssetError::io(
                "не удалось открыть журнал на дополнение",
                std::io::Error::from(error),
            )
        })?;
        let mut file = File::from(fd);
        if !file
            .metadata()
            .map_err(|error| AssetError::io("не удалось проверить файл журнала", error))?
            .is_file()
        {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "журнал должен быть обычным файлом без symlink",
            ));
        }
        file.write_all(&line)
            .map_err(|error| AssetError::io("не удалось записать событие журнала", error))?;
        file.sync_data()
            .map_err(|error| AssetError::io("не удалось синхронизировать журнал", error))?;
        let metadata = file
            .metadata()
            .map_err(|error| AssetError::io("не удалось проверить журнал после записи", error))?;
        let file_size = metadata.len();
        let file_modified = metadata.modified().ok();

        if torn_tail {
            // Разобранное состояние больше не соответствует файлу: строка обрыва
            // стала отдельной и должна быть перечитана как повреждённая.
            self.state = None;
        } else if let Some(state) = self.state.as_mut() {
            state.keys.insert(event.event_key.clone());
            state.last_seq = event.seq;
            state.torn_tail = false;
            state.file_size = file_size;
            state.file_modified = file_modified;
            state.events.push(event.clone());
        }
        Ok((event, true))
    }

    fn build_event(
        &self,
        draft: &JournalDraft,
        seq: u64,
        event_key: String,
    ) -> Result<JournalEvent, AssetError> {
        let recorded_at = format_rfc3339(SystemTime::now());
        let counter = EVENT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let material = format!(
            "{seq}{sep}{event_key}{sep}{recorded_at}{sep}{}{sep}{counter}",
            std::process::id(),
            sep = FIELD_SEPARATOR,
        );
        Ok(JournalEvent {
            schema_version: JOURNAL_SCHEMA_VERSION,
            seq,
            event_id: sha256_hex(material.as_bytes()),
            event_key,
            recorded_at,
            domain: draft.domain.clone(),
            namespace: draft.namespace.clone(),
            identity: draft.identity.clone(),
            operation: draft.operation,
            outcome: draft.outcome,
            intent_id: draft.intent_id.clone(),
            manifest_revision: draft.manifest_revision,
            previous_sha256: draft.previous_sha256.clone(),
            new_sha256: draft.new_sha256.clone(),
            previous_lifecycle: draft.previous_lifecycle.clone(),
            new_lifecycle: draft.new_lifecycle.clone(),
            verified: draft.verified,
            run_id: draft.run_id.clone().or_else(|| self.run_id.clone()),
            batch_id: draft.batch_id.clone().or_else(|| self.batch_id.clone()),
            provenance: draft.provenance.clone(),
            reason: draft.reason.clone(),
            snapshot: draft.snapshot.clone(),
        })
    }

    fn ensure_directory(&mut self, create: bool) -> Result<Option<File>, AssetError> {
        if let Some(directory) = &self.directory {
            let clone = directory.try_clone().map_err(|error| {
                AssetError::io("не удалось клонировать дескриптор журнала", error)
            })?;
            return Ok(Some(clone));
        }
        match open_directory_at(&self.root, JOURNAL_DIR) {
            Ok(directory) => {
                let clone = directory.try_clone().map_err(|error| {
                    AssetError::io("не удалось клонировать дескриптор журнала", error)
                })?;
                self.directory = Some(clone);
                Ok(Some(directory))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                let directory = ensure_dir_entry(&self.root, JOURNAL_DIR)?;
                sync_directory(&self.root)?;
                let clone = directory.try_clone().map_err(|error| {
                    AssetError::io("не удалось клонировать дескриптор журнала", error)
                })?;
                self.directory = Some(clone);
                Ok(Some(directory))
            }
            Err(error) => Err(directory_entry_error(JOURNAL_DIR, error)),
        }
    }

    fn open_events_file(&mut self) -> Result<Option<File>, AssetError> {
        let Some(directory) = self.ensure_directory(false)? else {
            return Ok(None);
        };
        match openat(
            &directory,
            JOURNAL_FILE,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let file = File::from(fd);
                if !file
                    .metadata()
                    .map_err(|error| AssetError::io("не удалось проверить файл журнала", error))?
                    .is_file()
                {
                    return Err(AssetError::new(
                        ErrorCode::BoundaryViolation,
                        "журнал должен быть обычным файлом без symlink",
                    ));
                }
                Ok(Some(file))
            }
            Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(AssetError::io(
                "не удалось открыть файл журнала",
                std::io::Error::from(error),
            )),
        }
    }

    fn refresh(&mut self) -> Result<(), AssetError> {
        let file = self.open_events_file()?;
        let (size, modified) = match &file {
            Some(file) => {
                let metadata = file.metadata().map_err(|error| {
                    AssetError::io("не удалось проверить размер журнала", error)
                })?;
                (metadata.len(), metadata.modified().ok())
            }
            None => (0, None),
        };
        if let Some(state) = &self.state
            && state.file_size == size
            && state.file_modified == modified
        {
            return Ok(());
        }
        if size > MAX_JOURNAL_BYTES {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!("журнал превышает допустимый размер {MAX_JOURNAL_BYTES} байт"),
            ));
        }
        let bytes = match file {
            Some(mut file) => {
                let mut bytes = Vec::with_capacity(size as usize);
                file.read_to_end(&mut bytes)
                    .map_err(|error| AssetError::io("не удалось прочитать журнал", error))?;
                bytes
            }
            None => Vec::new(),
        };
        let (events, torn_tail, corrupt_lines) = parse_events(&bytes);
        let last_seq = events.iter().map(|event| event.seq).max().unwrap_or(0);
        let keys = events
            .iter()
            .map(|event| event.event_key.clone())
            .collect::<BTreeSet<_>>();
        self.state = Some(JournalState {
            events,
            keys,
            torn_tail,
            corrupt_lines,
            last_seq,
            file_size: size,
            file_modified: modified,
        });
        Ok(())
    }
}

/// Проверяет целостность разобранного журнала.
///
/// `manifest_revision_max` — наибольшая ревизия среди фактически присутствующих
/// манифестов: канонического и `.runtime`. Событие не может ссылаться на ревизию,
/// которой в хранилище ещё не было.
pub fn check(read: &JournalRead, manifest_revision_max: u64) -> JournalCheck {
    let mut problems = Vec::new();
    if read.torn_tail {
        problems.push(
            "журнал оканчивается оборванной строкой без завершающего перевода строки".to_owned(),
        );
    }
    for line in &read.corrupt_lines {
        problems.push(format!("строка {line} журнала не разбирается"));
    }

    let mut seen_ids = BTreeSet::new();
    for (index, event) in read.events.iter().enumerate() {
        let expected = index as u64 + 1;
        if event.seq != expected {
            problems.push(format!(
                "нарушена нумерация seq: ожидалось {expected}, получено {}",
                event.seq
            ));
        }
        if !seen_ids.insert(event.event_id.clone()) {
            problems.push(format!("event_id {} повторяется", event.event_id));
        }
        if let Some(intent_id) = &event.intent_id
            && !read
                .events
                .iter()
                .any(|candidate| &candidate.event_id == intent_id)
        {
            problems.push(format!(
                "событие seq={} ссылается на отсутствующий intent_id {intent_id}",
                event.seq
            ));
        }
        if let Some(revision) = event.manifest_revision
            && revision > manifest_revision_max
        {
            problems.push(format!(
                "событие seq={} ссылается на ревизию {revision} больше текущей {manifest_revision_max}",
                event.seq
            ));
        }
    }

    let closed: BTreeSet<&str> = read
        .events
        .iter()
        .filter_map(|event| event.intent_id.as_deref())
        .collect();
    let open_intents: Vec<JournalOpenIntent> = read
        .events
        .iter()
        .filter(|event| event.is_pending_intent() && !closed.contains(event.event_id.as_str()))
        .map(|event| JournalOpenIntent {
            event_id: event.event_id.clone(),
            seq: event.seq,
            identity: event.identity.clone(),
            namespace: event.namespace.clone(),
            operation: event.operation,
            recorded_at: event.recorded_at.clone(),
        })
        .collect();
    for intent in &open_intents {
        problems.push(format!(
            "интент seq={} ({}, {}) не закрыт событием результата",
            intent.seq, intent.operation, intent.identity
        ));
    }

    JournalCheck {
        event_count: read.events.len() as u64,
        torn_tail: read.torn_tail,
        corrupt_lines: read.corrupt_lines.clone(),
        open_intents,
        problems,
    }
}

/// Снимок текущего состояния обоих манифестов хранилища.
pub fn snapshot(canonical: &Manifest, runtime: &Manifest) -> JournalSnapshot {
    let mut verified = 0_u64;
    let mut pending = 0_u64;
    let mut quarantined = 0_u64;
    let mut assets = 0_u64;
    for record in canonical.assets.iter().chain(runtime.assets.iter()) {
        assets = assets.saturating_add(1);
        match record.lifecycle {
            LifecycleState::Verified => verified = verified.saturating_add(1),
            LifecycleState::Pending => pending = pending.saturating_add(1),
            LifecycleState::Quarantined => quarantined = quarantined.saturating_add(1),
        }
    }
    JournalSnapshot {
        assets,
        verified,
        pending,
        quarantined,
        manifest_revision: canonical.revision,
    }
}

/// Суммарная ревизия обеих областей хранилища.
///
/// Событие уровня всего хранилища (`baseline`, `layout_migrated`) не принадлежит
/// одной области, поэтому помечается суммой: она строго возрастает при любой
/// миграции раскладки и всегда не меньше ревизии каждой отдельной области.
pub fn revision_bound(canonical: &Manifest, runtime: &Manifest) -> u64 {
    canonical.revision.saturating_add(runtime.revision)
}

fn parse_events(bytes: &[u8]) -> (Vec<JournalEvent>, bool, Vec<u64>) {
    if bytes.is_empty() {
        return (Vec::new(), false, Vec::new());
    }
    let ends_with_newline = bytes.last() == Some(&b'\n');
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    // Последний элемент — либо пустой хвост после завершающего перевода строки,
    // либо оборванная запись, которая не считается событием.
    lines.pop();
    let torn_tail = !ends_with_newline;
    let mut events = Vec::new();
    let mut corrupt_lines = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let number = index as u64 + 1;
        if line.is_empty() {
            corrupt_lines.push(number);
            continue;
        }
        match serde_json::from_slice::<JournalEvent>(line) {
            Ok(event) if event.schema_version == JOURNAL_SCHEMA_VERSION && event.seq > 0 => {
                events.push(event);
            }
            _ => corrupt_lines.push(number),
        }
    }
    (events, torn_tail, corrupt_lines)
}

fn sanitize_reason(reason: &str) -> String {
    reason
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_REASON_CHARS)
        .collect()
}

/// Форматирует момент времени как `RFC3339` в UTC с суффиксом `Z`.
pub fn format_rfc3339(time: SystemTime) -> String {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = duration.as_secs();
    let milliseconds = duration.subsec_millis();
    let days = (seconds / 86_400) as i64;
    let second_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{milliseconds:03}Z",
        second_of_day / 3_600,
        (second_of_day % 3_600) / 60,
        second_of_day % 60,
    )
}

/// Календарная дата из числа дней от `1970-01-01` (алгоритм Хауарда Хиннанта).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::temp_workspace::TempWorkspace;

    struct Fixture {
        workspace: TempWorkspace,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                workspace: TempWorkspace::create("asset-store-journal-tests")
                    .expect("создаётся временный каталог теста"),
            }
        }

        fn path(&self) -> &Path {
            self.workspace.path()
        }

        fn journal(&self) -> Journal {
            let root = File::open(self.path()).expect("открывается корневой каталог теста");
            Journal::open(&root).expect("открывается журнал")
        }

        fn events_file(&self) -> PathBuf {
            self.path().join(JOURNAL_DIR).join(JOURNAL_FILE)
        }

        fn read_events(&self) -> Vec<JournalEvent> {
            let text = fs::read_to_string(self.events_file()).expect("журнал читается с диска");
            text.lines()
                .map(|line| serde_json::from_str::<JournalEvent>(line).expect("строка журнала"))
                .collect()
        }
    }

    fn draft(operation: JournalOperation, outcome: JournalOutcome) -> JournalDraft {
        JournalDraft::store_wide("kanji", "kanji", "日", operation, outcome)
    }

    #[test]
    fn append_assigns_sequence_identity_and_key_and_survives_reopen() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let first = journal
            .append_and_return(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Pending,
            ))
            .expect("первое событие записано");
        let second = journal
            .append_and_return(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Completed,
            ))
            .expect("второе событие записано");

        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        assert_eq!(first.schema_version, JOURNAL_SCHEMA_VERSION);
        assert_ne!(first.event_id, second.event_id);
        assert_ne!(first.event_key, second.event_key);
        assert!(first.is_pending_intent());
        assert!(!first.closes_intent());
        assert!(!second.is_pending_intent());

        // Повторное открытие журнала другим дескриптором — это перезапуск CLI.
        let mut reopened = fixture.journal();
        let read = reopened.read().expect("журнал перечитывается");
        assert!(!read.torn_tail);
        assert!(read.corrupt_lines.is_empty());
        assert_eq!(read.events, vec![first, second]);
    }

    #[test]
    fn repeated_event_key_is_not_appended_twice() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let draft = draft(JournalOperation::VerifiedReused, JournalOutcome::Completed);
        assert!(
            journal.append(&draft).expect("первое событие записано"),
            "первое событие журнала новое"
        );
        assert!(
            !journal.append(&draft).expect("дубликат обработан"),
            "событие с тем же event_key не записывается повторно"
        );
        assert_eq!(fixture.read_events().len(), 1);
    }

    #[test]
    fn intent_and_refusal_are_distinct_events_even_when_state_is_unchanged() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let intent = journal
            .append_and_return(
                &draft(JournalOperation::Revalidated, JournalOutcome::Pending)
                    .revision(Some(7))
                    .sha(Some("aa"), Some("aa"))
                    .verified(Some(false)),
            )
            .expect("интент записан");
        let refused = journal
            .append_and_return(
                &draft(JournalOperation::Revalidated, JournalOutcome::Refused)
                    .closing(&intent.event_id)
                    .revision(Some(7))
                    .sha(Some("aa"), Some("aa"))
                    .verified(Some(false))
                    .reason("валидатор отказал"),
            )
            .expect("отказ записан");
        assert_ne!(intent.event_id, refused.event_id);
        assert_eq!(fixture.read_events().len(), 2);
    }

    #[test]
    fn torn_tail_is_reported_and_does_not_block_the_next_append() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        journal
            .append(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Pending,
            ))
            .expect("первое событие записано");
        journal
            .append(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Completed,
            ))
            .expect("второе событие записано");
        drop(journal);

        // Обрываем последнюю строку: файл без завершающего перевода строки.
        let path = fixture.events_file();
        let text = fs::read_to_string(&path).expect("журнал читается");
        let truncated = text.trim_end_matches('\n');
        fs::write(&path, &truncated[..truncated.len() - 8]).expect("хвост журнала оборван");

        let mut journal = fixture.journal();
        let read = journal.read().expect("оборванный журнал разбирается");
        assert!(read.torn_tail, "оборванный хвост распознан");
        assert_eq!(read.events.len(), 1);
        assert_eq!(read.events[0].seq, 1);

        let appended = journal
            .append_and_return(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Completed,
            ))
            .expect("запись после оборванного хвоста продолжается");
        assert_eq!(appended.seq, 2);

        // После отделения оборванной строки файл снова разбирается, а оборванная
        // запись честно помечается повреждённой строкой 2.
        let mut reopened = fixture.journal();
        let read = reopened.read().expect("журнал перечитывается");
        assert!(!read.torn_tail);
        assert_eq!(read.corrupt_lines, vec![2]);
        assert_eq!(read.events.len(), 2);
        assert_eq!(read.events[1].seq, 2);
    }

    #[test]
    fn check_reports_sequence_gaps_duplicates_and_dangling_intents() {
        let events = vec![
            event(1, "id-1", None, JournalOutcome::Pending),
            event(3, "id-3", None, JournalOutcome::Completed),
            event(4, "id-3", Some("missing"), JournalOutcome::Completed),
        ];
        let read = JournalRead {
            events,
            torn_tail: false,
            corrupt_lines: vec![9],
        };
        let check = check(&read, 5);
        assert!(!check.is_clean());
        assert_eq!(check.event_count, 3);
        assert_eq!(check.corrupt_lines, vec![9]);
        assert!(
            check
                .problems
                .iter()
                .any(|problem| problem.contains("нарушена нумерация seq")),
            "пропуск seq обнаружен: {:?}",
            check.problems
        );
        assert!(
            check
                .problems
                .iter()
                .any(|problem| problem.contains("повторяется")),
            "дубль event_id обнаружен: {:?}",
            check.problems
        );
        assert!(
            check
                .problems
                .iter()
                .any(|problem| problem.contains("отсутствующий intent_id")),
            "висячая ссылка обнаружена: {:?}",
            check.problems
        );
        assert!(
            check
                .problems
                .iter()
                .any(|problem| problem.contains("не закрыт событием результата")),
            "незакрытый интент обнаружен: {:?}",
            check.problems
        );
        assert_eq!(check.open_intents.len(), 1);
        assert_eq!(check.open_intents[0].seq, 1);
    }

    #[test]
    fn check_rejects_revision_beyond_the_manifest() {
        let read = JournalRead {
            events: vec![event(1, "id-1", None, JournalOutcome::Completed)],
            torn_tail: false,
            corrupt_lines: Vec::new(),
        };
        let clean = check(&read, 5);
        assert!(clean.is_clean(), "{:?}", clean.problems);

        let mut future = read.clone();
        future.events[0].manifest_revision = Some(6);
        let stale = check(&future, 5);
        assert!(
            stale
                .problems
                .iter()
                .any(|problem| problem.contains("больше текущей")),
            "ревизия сверх манифеста обнаружена: {:?}",
            stale.problems
        );
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        let formatted = format_rfc3339(UNIX_EPOCH);
        assert_eq!(formatted, "1970-01-01T00:00:00.000Z");
        let formatted =
            format_rfc3339(UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_123));
        assert_eq!(formatted, "2023-11-14T22:13:20.123Z");
    }

    fn event(
        seq: u64,
        event_id: &str,
        intent_id: Option<&str>,
        outcome: JournalOutcome,
    ) -> JournalEvent {
        JournalEvent {
            schema_version: JOURNAL_SCHEMA_VERSION,
            seq,
            event_id: event_id.to_owned(),
            event_key: format!("key-{seq}-{event_id}"),
            recorded_at: "2024-01-01T00:00:00.000Z".to_owned(),
            domain: "kanji".to_owned(),
            namespace: "kanji".to_owned(),
            identity: "日".to_owned(),
            operation: JournalOperation::Revalidated,
            outcome,
            intent_id: intent_id.map(str::to_owned),
            manifest_revision: Some(1),
            previous_sha256: None,
            new_sha256: None,
            previous_lifecycle: None,
            new_lifecycle: None,
            verified: None,
            run_id: None,
            batch_id: None,
            provenance: None,
            reason: None,
            snapshot: None,
        }
    }
}
