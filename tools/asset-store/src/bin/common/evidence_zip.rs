//! Общая граница приёмки: временные данные принадлежат run, наружу выходит один ZIP.
use rustix::fs::{AtFlags, CWD, Mode, OFlags, linkat, open, openat, unlinkat};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asset_store::diagnostics::{OutputMode, RunLogGuard, safe_message, safe_route};
use asset_store::hashing::sha256_hex;
use asset_store::temp_workspace::{
    TempWorkspace, cleanup_legacy_orphans, cleanup_orphans, snapshot,
};
use serde_json::{Value, json};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

type Error = Box<dyn std::error::Error>;
const ORPHAN_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const REQUIRED: &[&str] = &[
    "summary.md",
    "manifest.sha256",
    "raw-evidence/events.jsonl",
    "raw-evidence/technical-failures.json",
    "runtime/report-snapshot.json",
    "timeline.jsonl",
    "commands.json",
    "checks.json",
    "fault-injection.json",
    "metadata.json",
    "hygiene.json",
    "stdout/acceptance.txt",
    "stderr/acceptance.txt",
    "logs/diagnostic.jsonl",
    "reports/index.html",
];

pub struct EvidenceRun {
    workspace: Option<TempWorkspace>,
    output: PathBuf,
    before: Value,
    startup_gc: Value,
    started: u64,
    tool: &'static str,
}

impl EvidenceRun {
    pub fn start(
        tool: &'static str,
        output: Option<&Path>,
        checkout: &Path,
    ) -> Result<Self, Error> {
        // Validate output without creating it; failures must precede provider work.
        let before = serde_json::to_value(snapshot()?)?;
        let startup_gc = json!({"runs": cleanup_orphans(ORPHAN_MIN_AGE)?, "legacy":cleanup_legacy_orphans(ORPHAN_MIN_AGE)?});
        let workspace = TempWorkspace::create(tool)?;
        let result = (|| {
            let output = resolve_output(output, checkout, workspace.path(), tool)?;
            // Проверяем поддержку unnamed staging до запуска сетевого provider.
            drop(Staging::create(&output)?);
            for name in [
                "reports",
                "logs",
                "raw-evidence",
                "runtime",
                "stdout",
                "stderr",
                "plans",
                "store",
            ] {
                fs::create_dir(workspace.path().join(name))?;
            }
            Ok::<_, Error>(output)
        })();
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                let cleanup = workspace.close();
                return Err(combined_error(error, cleanup.err()));
            }
        };
        let run = Self {
            workspace: Some(workspace),
            output,
            before,
            startup_gc,
            started: now(),
            tool,
        };
        if let Err(error) =
            run.event(json!({"event":"run_started", "unix_seconds":run.started, "tool":tool}))
        {
            let mut run = run;
            let cleanup = run.workspace.take().expect("run owner").close();
            return Err(combined_error(error, cleanup.err()));
        }
        Ok(run)
    }

    pub fn path(&self) -> &Path {
        self.workspace
            .as_ref()
            .expect("живой run владеет workspace")
            .path()
    }

    pub fn report_dir(&self) -> PathBuf {
        self.path().join("reports")
    }

    pub fn log_guard(&self) -> Result<RunLogGuard, Error> {
        Ok(RunLogGuard::new(
            File::create(self.path().join("logs/diagnostic.jsonl"))?,
            OutputMode::Json,
        ))
    }

    pub fn add_transcript(&self, source: &Path) -> Result<(), Error> {
        let metadata = fs::symlink_metadata(source)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > 16 * 1024 * 1024
        {
            return Err("--transcript принимает обычный файл размером не более 16 MiB".into());
        }
        fs::write(
            self.path().join("stdout/checks-transcript.txt"),
            sanitized_output(&fs::read(source)?),
        )?;
        self.event(json!({"event":"external_checks_transcript_attached","entry":"stdout/checks-transcript.txt"}))
    }

    pub fn add_fault_evidence(&self, source: &Path) -> Result<(), Error> {
        let metadata = fs::symlink_metadata(source)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > 16 * 1024 * 1024
        {
            return Err(
                "--fault-evidence принимает обычный JSON файл размером не более 16 MiB".into(),
            );
        }
        let mut value: Value = serde_json::from_slice(&fs::read(source)?)?;
        redact(&mut value);
        fs::write(
            self.path().join("fault-injection.json"),
            serde_json::to_vec_pretty(
                &json!({"executed":true,"source":"external_fault_injection","results":value}),
            )?,
        )?;
        self.event(
            json!({"event":"external_fault_evidence_attached","entry":"fault-injection.json"}),
        )
    }

    pub fn event(&self, mut event: Value) -> Result<(), Error> {
        if let Some(fields) = event.as_object_mut() {
            fields.entry("unix_seconds").or_insert(json!(now()));
        }
        redact(&mut event);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path().join("raw-evidence/events.jsonl"))?;
        serde_json::to_writer(&mut file, &event)?;
        writeln!(file)?;
        Ok(())
    }

    /// Handles закрыты вызывающим кодом. Сначала проверяем архив, затем close(),
    /// записываем фактическую hygiene и повторно проверяем окончательный bundle.
    pub fn finish(
        mut self,
        result: Result<(), Error>,
        log_result: Result<(), String>,
    ) -> Result<(), Error> {
        let result = match (result, log_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(error)) => Err(error.into()),
            (Err(error), Err(log_error)) => {
                Err(format!("{error}; diagnostic_flush_failed: {log_error}").into())
            }
        };
        let work_path = self.path().to_owned();
        let prepared = self.prepare_bundle(&result).and_then(|files| {
            let mut staging = Staging::create(&self.output)?;
            staging.write_and_verify(&files)?;
            Ok((staging, files))
        });
        if let Err(error) = &prepared {
            eprintln!(
                "{}",
                json!({"stage":"evidence", "code":"evidence_zip_failed", "message":safe_message(&error.to_string()),"diagnostic":"evidence не опубликован"})
            );
        }
        // An error is emitted before deleting the only unpacked diagnostic information.
        let cleanup = self
            .workspace
            .take()
            .expect("owner существует до close")
            .close();
        let final_gc = cleanup_orphans(ORPHAN_MIN_AGE).and_then(|runs| {
            cleanup_legacy_orphans(ORPHAN_MIN_AGE)
                .map(|legacy| json!({"runs":runs,"legacy":legacy}))
        });
        let after = snapshot();
        if let Err(error) = &cleanup {
            eprintln!(
                "{}",
                json!({"stage":"cleanup", "code":"workspace_cleanup_failed", "message":safe_message(&error.to_string()), "workspace":work_path})
            );
        }
        let (mut archive, mut files) = match prepared {
            Ok(bundle) => bundle,
            Err(error) => return Err(format!("evidence не опубликован: {error}; acceptance: {result:?}; cleanup: {cleanup:?}; final_gc: {final_gc:?}; hygiene: {after:?}").into()),
        };
        let finalized = (|| -> Result<(String, u64), Error> {
            let gc = final_gc.map_err(|error| -> Error {
                format!("final_gc_failed: {error}; workspace_cleanup: {cleanup:?}").into()
            })?;
            let after = after.map_err(|error| -> Error {
                format!("hygiene_snapshot_failed: {error}; workspace_cleanup: {cleanup:?}").into()
            })?;
            let absent = !work_path.exists();
            let cleanup_message = cleanup
                .as_ref()
                .err()
                .map(|error| safe_message(&error.to_string()));
            files.insert("hygiene.json".into(), serde_json::to_vec_pretty(&json!({
            "before":self.before, "after":after, "startup_gc":self.startup_gc, "final_gc":gc,
            "current_workspace":work_path, "current_workspace_absent":absent,
            "cleanup_error":cleanup_message, "final_zip_path":self.output, "final_zip_bytes":0_u64,
            "zip_size_source":"Размер и SHA всего ZIP возвращаются после окончательной проверки; ZIP не содержит собственный hash."
        }))?);
            files.insert(
                "stderr/acceptance.txt".into(),
                format!(
                    "{}{}",
                    result
                        .as_ref()
                        .err()
                        .map(|error| safe_message(&error.to_string()))
                        .unwrap_or_default(),
                    cleanup_message
                        .as_ref()
                        .map(|message| format!("\nworkspace_cleanup_failed: {message}"))
                        .unwrap_or_default()
                )
                .into_bytes(),
            );
            if let Some(message) = &cleanup_message {
                let mut failures: Vec<Value> = serde_json::from_slice(
                    files
                        .get("raw-evidence/technical-failures.json")
                        .ok_or("technical failures entry missing")?,
                )?;
                failures.push(json!({"stage":"cleanup","code":"workspace_cleanup_failed","message":message,"diagnostic":{"workspace":work_path,"absent":absent}}));
                files.insert(
                    "raw-evidence/technical-failures.json".into(),
                    serde_json::to_vec_pretty(&failures)?,
                );
            }
            let mut timeline = files.remove("timeline.jsonl").unwrap_or_default();
            writeln!(
                timeline,
                "{}",
                json!({"event":"workspace_cleanup_finished", "unix_seconds":now(), "absent":absent, "cleanup_error":cleanup_message})
            )?;
            files.insert("timeline.jsonl".into(), timeline);
            let mut stable = false;
            for _ in 0..4 {
                archive.write_and_verify(&files)?;
                let actual_size = archive.file.metadata()?.len();
                let mut hygiene: Value = serde_json::from_slice(
                    files.get("hygiene.json").ok_or("hygiene entry missing")?,
                )?;
                if hygiene["final_zip_bytes"].as_u64() == Some(actual_size) {
                    stable = true;
                    break;
                }
                hygiene["final_zip_bytes"] = json!(actual_size);
                files.insert("hygiene.json".into(), serde_json::to_vec_pretty(&hygiene)?);
            }
            if !stable {
                return Err("ZIP size self-record не стабилен".into());
            }
            let (sha256, bytes) = archive.publish(&self.output)?;
            Ok((sha256, bytes))
        })();
        let (sha256, bytes) = match finalized {
            Ok(published) => published,
            Err(error) => {
                eprintln!(
                    "{}",
                    json!({"stage":"evidence","code":"evidence_zip_failed","message":safe_message(&error.to_string()),"diagnostic":"evidence не опубликован"})
                );
                return Err(format!("evidence не опубликован: {error}; acceptance: {result:?}; workspace_cleanup: {cleanup:?}").into());
            }
        };
        let absent = !work_path.exists();
        println!(
            "LIVE EVIDENCE ZIP\nPath: {}\nSHA-256: {sha256}\nSize: {bytes} bytes",
            self.output.display()
        );
        if let Err(error) = cleanup {
            return Err(combined_error(error.into(), None));
        }
        if !absent {
            return Err("workspace_cleanup_failed: каталог текущего run сохранился".into());
        }
        result
    }

    fn prepare_bundle(
        &self,
        result: &Result<(), Error>,
    ) -> Result<BTreeMap<String, Vec<u8>>, Error> {
        let root = self.path();
        let report = root.join("reports/evidence.json");
        let mut snapshot_value = if report.is_file() {
            serde_json::from_slice(&fs::read(report)?)?
        } else {
            json!({"schema_version":1,"run_status":"acceptance_failed_before_report","items":[]})
        };
        redact(&mut snapshot_value);
        fs::write(
            root.join("runtime/report-snapshot.json"),
            serde_json::to_vec_pretty(&snapshot_value)?,
        )?;
        if !root.join("reports/index.html").exists() {
            fs::write(
                root.join("reports/index.html"),
                "<!doctype html><html lang=ru><meta charset=utf-8><title>Приёмка</title><p>Приёмка завершилась до построения отчёта. См. summary.md и technical-failures.json.</html>",
            )?;
        }
        self.event(json!({"event":"acceptance_finished","unix_seconds":now(),"success":result.is_ok(),"message":result.as_ref().err().map(|error|safe_message(&error.to_string()))}))?;
        let mut failures = Vec::new();
        collect_failures(&snapshot_value, "$", &mut failures);
        if let Err(error) = result {
            failures.push(json!({"stage":"acceptance", "code":"acceptance_failed", "message":safe_message(&error.to_string()), "diagnostic":null}));
        }
        fs::write(
            root.join("raw-evidence/technical-failures.json"),
            serde_json::to_vec_pretty(&failures)?,
        )?;
        if !root.join("logs/diagnostic.jsonl").exists() {
            fs::write(root.join("logs/diagnostic.jsonl"), b"")?;
        }
        let events = fs::read(root.join("raw-evidence/events.jsonl"))?;
        fs::write(root.join("timeline.jsonl"), events)?;
        let mut commands = Vec::new();
        for (label, executable, arguments) in [
            ("commit", "git", vec!["rev-parse", "HEAD"]),
            ("branch", "git", vec!["symbolic-ref", "--short", "HEAD"]),
            ("rustc", "rustc", vec!["--version"]),
            ("cargo", "cargo", vec!["--version"]),
            ("temp_filesystem", "findmnt", vec!["-T", "/tmp"]),
        ] {
            let (status, stdout, stderr) = match Command::new(executable).args(&arguments).output()
            {
                Ok(output) => (
                    output.status.code(),
                    sanitized_output(&output.stdout),
                    sanitized_output(&output.stderr),
                ),
                Err(error) => (None, String::new(), safe_message(&error.to_string())),
            };
            fs::write(root.join(format!("stdout/{label}.txt")), &stdout)?;
            fs::write(root.join(format!("stderr/{label}.txt")), &stderr)?;
            commands.push(json!({"command":executable,"arguments":arguments,"status":status,"stdout":format!("stdout/{label}.txt"),"stderr":format!("stderr/{label}.txt")}));
        }
        commands.push(json!({"command":self.tool,"arguments":["--plan","[внешний проверенный план; путь опущен]"],"success":result.is_ok(),"stdout":"stdout/acceptance.txt","stderr":"stderr/acceptance.txt"}));
        fs::write(
            root.join("commands.json"),
            serde_json::to_vec_pretty(&commands)?,
        )?;
        fs::write(
            root.join("metadata.json"),
            serde_json::to_vec_pretty(
                &json!({"tool":self.tool,"package_version":env!("CARGO_PKG_VERSION"),"started_unix_seconds":self.started,"ended_unix_seconds":now(),"commit":fs::read_to_string(root.join("stdout/commit.txt"))?.trim(),"branch":fs::read_to_string(root.join("stdout/branch.txt"))?.trim(),"browser_versions":"См. acquisition evidence в runtime/report-snapshot.json"}),
            )?,
        )?;
        fs::write(
            root.join("checks.json"),
            serde_json::to_vec_pretty(
                &json!({"executed":["plan validation", "provider acquisition", "PNG decode/hash", "semantic validation", "plan expectations", "ZIP entries/manifest/hash", "explicit workspace cleanup", "startup/final orphan GC"],"acceptance_success":result.is_ok(),"repository_tests":"В этом acceptance binary не запускались"}),
            )?,
        )?;
        if !root.join("fault-injection.json").exists() {
            fs::write(root.join("fault-injection.json"), b"{\"executed\":false,\"results\":[],\"reason\":\"Live provider evidence; fault injection runs must be exported separately\"}")?;
        }
        fs::write(
            root.join("hygiene.json"),
            serde_json::to_vec_pretty(
                &json!({"before":self.before,"startup_gc":self.startup_gc,"cleanup":"pending"}),
            )?,
        )?;
        fs::write(
            root.join("stdout/acceptance.txt"),
            format!(
                "tool={}\nsuccess={}\nreport=reports/index.html\n",
                self.tool,
                result.is_ok()
            ),
        )?;
        fs::write(
            root.join("stderr/acceptance.txt"),
            result
                .as_ref()
                .err()
                .map(|error| safe_message(&error.to_string()))
                .unwrap_or_default(),
        )?;
        fs::write(
            root.join("summary.md"),
            format!(
                "# Приёмка {}\n\nСтатус: {}.\n\nОтчёт: [reports/index.html](reports/index.html). Снимок: [runtime/report-snapshot.json](runtime/report-snapshot.json).\n\nКанонические assets и decks не публикуются этим binary. Проверенные PNG требуют ручного просмотра.\n\nВсе события — raw-evidence/events.jsonl; структурированная диагностика — logs/diagnostic.jsonl. Обработанный interrupt проходит через ту же границу архивации и cleanup. SIGKILL восстанавливается orphan GC следующего запуска.\n\nСводка cleanup — hygiene.json. Hash и размер ZIP возвращаются после публикации.\n",
                self.tool,
                if result.is_ok() {
                    "автоматические проверки завершены; требуется ручной просмотр"
                } else {
                    "ошибка приёмки"
                }
            ),
        )?;
        let mut files = BTreeMap::new();
        // Whitelist excludes plans, store, raw DOM, browser profiles and ownership markers.
        for directory in [
            "reports",
            "raw-evidence",
            "runtime",
            "stdout",
            "stderr",
            "logs",
        ] {
            collect_files(root, &root.join(directory), &mut files)?;
        }
        for name in [
            "summary.md",
            "timeline.jsonl",
            "commands.json",
            "checks.json",
            "fault-injection.json",
            "metadata.json",
            "hygiene.json",
        ] {
            files.insert(name.into(), fs::read(root.join(name))?);
        }
        Ok(files)
    }
}

fn sanitized_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(safe_message)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn redact(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                let key = key.to_ascii_lowercase();
                if [
                    "cookies",
                    "cookie",
                    "authorization",
                    "credentials",
                    "password",
                    "access_token",
                    "refresh_token",
                    "token",
                    "dom",
                    "html",
                    "plan_path",
                ]
                .contains(&key.as_str())
                {
                    *value = Value::String("[опущено]".into());
                } else {
                    redact(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact),
        Value::String(text) => {
            *text = if text.starts_with("https://") || text.starts_with("http://") {
                safe_route(text)
            } else {
                safe_message(text)
            };
        }
        _ => {}
    }
}

fn collect_failures(value: &Value, location: &str, failures: &mut Vec<Value>) {
    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                let path = format!("{location}.{key}");
                if key.contains("failure") && !child.is_null() {
                    failures.push(json!({"location":path,"typed_failure":child}));
                } else {
                    collect_failures(child, &path, failures);
                }
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_failures(value, &format!("{location}[{index}]"), failures);
            }
        }
        _ => {}
    }
}

fn collect_files(
    root: &Path,
    path: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err("ZIP evidence не принимает symlink".into());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_files(root, &entry?.path(), files)?;
        }
    } else if metadata.is_file() {
        let name = path
            .strip_prefix(root)?
            .to_str()
            .ok_or("ZIP entry должен быть UTF-8")?
            .replace('\\', "/");
        files.insert(name, fs::read(path)?);
    } else {
        return Err("ZIP evidence принимает только обычные файлы".into());
    }
    Ok(())
}

pub fn resolve_output(
    output: Option<&Path>,
    checkout: &Path,
    workspace: &Path,
    tool: &str,
) -> Result<PathBuf, Error> {
    let path = if let Some(path) = output {
        path.to_owned()
    } else {
        let parent = std::env::temp_dir()
            .canonicalize()?
            .join("anki-decks-evidence");
        match fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err("evidence root должен быть обычным каталогом".into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&parent)?;
                fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
            }
            Err(error) => return Err(error.into()),
        }
        let parent = parent.canonicalize()?;
        parent.join(format!(
            "{tool}-{}.zip",
            workspace
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("run id должен быть UTF-8")?
        ))
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let filename = absolute
        .file_name()
        .ok_or("--output должен быть новым ZIP файлом")?;
    if absolute
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("zip")
    {
        return Err("--output должен иметь расширение .zip".into());
    }
    let resolved = absolute
        .parent()
        .ok_or("--output не имеет parent")?
        .canonicalize()?
        .join(filename);
    if resolved.starts_with(checkout) || resolved.starts_with(workspace) {
        return Err("ZIP должен находиться вне рабочего каталога checkout и run workspace".into());
    }
    if fs::symlink_metadata(&resolved).is_ok() {
        return Err("--output уже существует".into());
    }
    Ok(resolved)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn combined_error(error: Error, cleanup: Option<io::Error>) -> Error {
    match cleanup {
        Some(cleanup) => format!("{error}; workspace_cleanup_failed: {cleanup}").into(),
        None => error,
    }
}

struct Staging {
    file: File,
    parent: File,
}
impl Staging {
    fn create(output: &Path) -> Result<Self, Error> {
        let parent_path = output.parent().ok_or("ZIP parent отсутствует")?;
        let parent = File::from(open(
            parent_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        // An unnamed inode cannot survive process death and has no publishable path.
        // Refuse filesystems lacking O_TMPFILE; never fall back to an orphan-prone .tmp.
        let file = File::from(openat(
            &parent,
            ".",
            OFlags::TMPFILE | OFlags::RDWR | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        Ok(Self { file, parent })
    }

    fn bytes(&self) -> Result<Vec<u8>, Error> {
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn write_and_verify(&mut self, files: &BTreeMap<String, Vec<u8>>) -> Result<(), Error> {
        let mut files = files.clone();
        let manifest = files
            .iter()
            .map(|(name, bytes)| format!("{}  {name}\n", sha256_hex(bytes)))
            .collect::<String>();
        files.insert("manifest.sha256".into(), manifest.into_bytes());
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        let mut writer = ZipWriter::new(self.file.try_clone()?);
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(0o600);
        for (name, bytes) in &files {
            writer.start_file(name, options)?;
            writer.write_all(bytes)?;
        }
        let file = writer.finish()?;
        file.sync_all()?;
        drop(file);
        verify(&self.bytes()?)?;
        Ok(())
    }

    fn publish(self, output: &Path) -> Result<(String, u64), Error> {
        let bytes = self.bytes()?;
        verify(&bytes)?;
        let hash = sha256_hex(&bytes);
        let name = output.file_name().ok_or("ZIP filename отсутствует")?;
        // /proc supplies the descriptor-owned inode without requiring CAP_DAC_READ_SEARCH.
        // linkat is atomic, does not overwrite an existing filename, and uses the pinned parent.
        let descriptor_path = format!("/proc/self/fd/{}", self.file.as_raw_fd());
        linkat(
            CWD,
            descriptor_path.as_str(),
            &self.parent,
            name,
            AtFlags::SYMLINK_FOLLOW,
        )?;
        let finalize = (|| -> Result<(), Error> {
            self.parent.sync_all()?;
            let published = fs::read(output)?;
            verify(&published)?;
            if published.len() != bytes.len() || sha256_hex(&published) != hash {
                return Err("ZIP publication integrity mismatch".into());
            }
            Ok(())
        })();
        if let Err(error) = finalize {
            let rollback = unlinkat(&self.parent, name, AtFlags::empty());
            return Err(format!("ZIP publication failed: {error}; rollback: {rollback:?}").into());
        }
        Ok((hash, bytes.len() as u64))
    }
}

fn verify(bytes: &[u8]) -> Result<(), Error> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut names = std::collections::BTreeSet::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().to_owned();
        if entry.enclosed_name().is_none() || !names.insert(name) {
            return Err("недопустимое или повторное имя ZIP entry".into());
        }
    }
    for name in REQUIRED {
        if !names.contains(*name) {
            return Err(format!("ZIP missing entry: {name}").into());
        }
    }
    let mut manifest = String::new();
    archive
        .by_name("manifest.sha256")?
        .read_to_string(&mut manifest)?;
    let mut checked = std::collections::BTreeSet::new();
    for line in manifest.lines() {
        let (hash, name) = line.split_once("  ").ok_or("невалидный ZIP manifest")?;
        if name == "manifest.sha256" || !checked.insert(name.to_owned()) {
            return Err("повторный/самоссылочный ZIP manifest".into());
        }
        let mut contents = Vec::new();
        archive.by_name(name)?.read_to_end(&mut contents)?;
        if hash != sha256_hex(&contents) {
            return Err(format!("ZIP manifest hash mismatch: {name}").into());
        }
    }
    names.remove("manifest.sha256");
    if checked != names {
        return Err("ZIP manifest покрывает не все entries".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tool: &'static str) -> (TempWorkspace, EvidenceRun, PathBuf) {
        let output_owner = TempWorkspace::create("evidence-output-fixture").unwrap();
        let output = output_owner.path().join("evidence.zip");
        let run =
            EvidenceRun::start(tool, Some(&output), Path::new("/nonexistent-checkout")).unwrap();
        fs::write(
            run.report_dir().join("evidence.json"),
            b"{\"run_status\":\"synthetic\",\"items\":[]}",
        )
        .unwrap();
        fs::write(
            run.report_dir().join("index.html"),
            b"<!doctype html><p>synthetic</p>",
        )
        .unwrap();
        (output_owner, run, output)
    }

    #[test]
    fn bundle_survives_workspace_cleanup_is_complete_and_has_no_staging() {
        let (output_owner, run, output) = fixture("zip-success-fixture");
        let work = run.path().to_owned();
        let log = run.log_guard().unwrap();
        run.event(json!({"event":"item_completed", "outcome":"verified"}))
            .unwrap();
        run.finish(Ok(()), log.finish()).unwrap();
        assert!(!work.exists());
        let bytes = fs::read(&output).unwrap();
        verify(&bytes).unwrap();
        assert_eq!(sha256_hex(&bytes).len(), 64);
        let mut archive = ZipArchive::new(Cursor::new(&bytes)).unwrap();
        let mut hygiene = String::new();
        archive
            .by_name("hygiene.json")
            .unwrap()
            .read_to_string(&mut hygiene)
            .unwrap();
        let hygiene: Value = serde_json::from_str(&hygiene).unwrap();
        assert_eq!(hygiene["current_workspace_absent"], true);
        assert_eq!(
            hygiene["final_zip_bytes"].as_u64().unwrap(),
            bytes.len() as u64
        );
        let remaining: Vec<_> = fs::read_dir(output_owner.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != ".anki-decks-owner.json")
            .collect();
        assert_eq!(remaining, vec!["evidence.zip"]);
    }

    #[test]
    fn ordinary_error_and_handled_interrupt_keep_verified_zip_and_cleanup() {
        for code in ["ordinary_failure", "acquisition_interrupted"] {
            let (_output_owner, run, output) = fixture("zip-error-fixture");
            let work = run.path().to_owned();
            let log = run.log_guard().unwrap();
            let error = run
                .finish(Err(format!("{code}: synthetic").into()), log.finish())
                .unwrap_err();
            assert!(error.to_string().contains(code));
            assert!(!work.exists());
            verify(&fs::read(output).unwrap()).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn evidence_failure_never_publishes_final_and_cleans_workspace() {
        let (_output_owner, run, output) = fixture("zip-failure-fixture");
        let work = run.path().to_owned();
        let log = run.log_guard().unwrap();
        std::os::unix::fs::symlink("/etc/passwd", run.report_dir().join("unsafe-link")).unwrap();
        let error = run.finish(Ok(()), log.finish()).unwrap_err();
        assert!(error.to_string().contains("evidence не опубликован"));
        assert!(!output.exists());
        assert!(!work.exists());
    }

    #[test]
    fn no_clobber_publication_preserves_existing_file_and_removes_staging() {
        let (_output_owner, run, output) = fixture("zip-no-clobber-fixture");
        let work = run.path().to_owned();
        let log = run.log_guard().unwrap();
        fs::write(&output, b"existing external file").unwrap();
        assert!(run.finish(Ok(()), log.finish()).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"existing external file");
        assert!(!work.exists());
        assert!(
            fs::read_dir(output.parent().unwrap())
                .unwrap()
                .all(|entry| entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "tmp"))
        );
    }

    #[test]
    fn tampered_payload_does_not_pass_manifest() {
        let (_output_owner, run, output) = fixture("zip-tamper-fixture");
        let log = run.log_guard().unwrap();
        run.finish(Ok(()), log.finish()).unwrap();
        let mut bytes = fs::read(output).unwrap();
        let marker = b"synthetic";
        let position = bytes
            .windows(marker.len())
            .position(|window| window == marker)
            .unwrap();
        bytes[position] = b'X';
        assert!(verify(&bytes).is_err());
    }

    #[test]
    fn redaction_keeps_typed_failure_and_removes_query_credentials_and_dom() {
        let mut value = json!({"failure":{"stage":"search","code":"timeout","message":"failed https://jpdb.io/search?token=secret#cookie","diagnostic":"token=secret"},"cookie":"private","dom":"<body>private</body>","source_url":"https://user:pass@jpdb.io/search?q=secret"});
        redact(&mut value);
        let rendered = value.to_string();
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("private"));
        assert_eq!(value["failure"]["stage"], "search");
        assert_eq!(value["failure"]["code"], "timeout");
        assert_eq!(value["source_url"], "https://jpdb.io/search");
    }
}
