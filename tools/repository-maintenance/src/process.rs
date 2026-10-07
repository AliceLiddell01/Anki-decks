use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessCheck {
    Clear,
    Busy {
        pid: u32,
        process: String,
        reference: String,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    start_ticks: u64,
    parent: u32,
    state: char,
}

impl ProcessIdentity {
    fn is_live(self) -> bool {
        !matches!(self.state, 'Z' | 'X' | 'x')
    }
}

// Небольшая граница ввода /proc позволяет проверять гонки и ошибки доступа
// детерминированно, не создавая zombie или изменяя права реальных процессов.
trait ProcessReader {
    fn pids(&self) -> io::Result<Vec<u32>>;
    fn uid(&self, pid: u32) -> io::Result<u32>;
    fn identity(&self, pid: u32) -> io::Result<Option<ProcessIdentity>>;
    fn name(&self, pid: u32) -> io::Result<String>;
    fn path(&self, pid: u32, name: &str) -> io::Result<Option<PathBuf>>;
    fn fd_reference(&self, pid: u32, target: &Path) -> io::Result<Option<String>>;
}

struct ProcFs;

impl ProcessReader for ProcFs {
    fn pids(&self) -> io::Result<Vec<u32>> {
        fs::read_dir("/proc")?
            .filter_map(|item| match item {
                Ok(item) => item
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse().ok())
                    .map(Ok),
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    fn uid(&self, pid: u32) -> io::Result<u32> {
        Ok(fs::metadata(process_dir(pid))?.uid())
    }

    fn identity(&self, pid: u32) -> io::Result<Option<ProcessIdentity>> {
        match fs::read(process_dir(pid).join("stat")) {
            Ok(stat) => parse_identity(&stat).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn name(&self, pid: u32) -> io::Result<String> {
        Ok(fs::read_to_string(process_dir(pid).join("comm"))?
            .trim()
            .to_owned())
    }

    fn path(&self, pid: u32, name: &str) -> io::Result<Option<PathBuf>> {
        match fs::read_link(process_dir(pid).join(name)) {
            Ok(path) => Ok(Some(strip_deleted_suffix(path))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn fd_reference(&self, pid: u32, target: &Path) -> io::Result<Option<String>> {
        for descriptor in fs::read_dir(process_dir(pid).join("fd"))? {
            let descriptor = descriptor?;
            let path = match fs::read_link(descriptor.path()) {
                Ok(path) => strip_deleted_suffix(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if path.starts_with(target) {
                return Ok(Some(format!(
                    "открытый fd {}",
                    descriptor.file_name().to_string_lossy()
                )));
            }
        }
        Ok(None)
    }
}

fn process_dir(pid: u32) -> PathBuf {
    Path::new("/proc").join(pid.to_string())
}

/// Ищет процессы с UID текущего пользователя, использующие `target`, а также
/// процессы Cargo/Rust, работающие из этой рабочей области. Собственный PID
/// исключается только после сверки времени запуска. Результаты `Busy` и `Unknown`
/// перепроверяются: исчезнувший, заменённый или завершившийся PID уже не владеет
/// рабочими дескрипторами и не блокирует очистку.
pub fn target_process_check(target: &Path, workspace: &Path) -> ProcessCheck {
    let target = match fs::canonicalize(target) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => target.to_path_buf(),
        Err(error) => {
            return unknown(format!(
                "не удалось определить путь к каталогу `target`: {error}"
            ));
        }
    };
    let workspace = match fs::canonicalize(workspace) {
        Ok(path) => path,
        Err(error) => {
            return unknown(format!(
                "не удалось определить путь к рабочей области: {error}"
            ));
        }
    };
    let reader = ProcFs;
    let self_pid = std::process::id();
    let uid = match reader.uid(self_pid) {
        Ok(uid) => uid,
        Err(error) => return unknown(format!("не удалось определить UID процессов: {error}")),
    };
    let self_identity = match reader.identity(self_pid) {
        Ok(Some(identity)) => identity,
        Ok(None) => return unknown("не удалось определить identity собственного процесса".into()),
        Err(error) => return unknown(format!("не удалось определить self identity: {error}")),
    };
    check_processes(&reader, &target, &workspace, uid, self_pid, self_identity)
}

fn unknown(reason: String) -> ProcessCheck {
    ProcessCheck::Unknown { reason }
}

fn check_processes(
    reader: &impl ProcessReader,
    target: &Path,
    workspace: &Path,
    uid: u32,
    self_pid: u32,
    self_identity: ProcessIdentity,
) -> ProcessCheck {
    let pids = match reader.pids() {
        Ok(pids) => pids,
        Err(error) => return unknown(format!("не удалось перечислить /proc: {error}")),
    };
    for pid in pids {
        match reader.uid(pid) {
            Ok(process_uid) if process_uid != uid => continue,
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return unknown(format!("не удалось проверить UID процесса {pid}: {error}"));
            }
        }
        let initial = match reader.identity(pid) {
            Ok(Some(identity)) if identity.is_live() => identity,
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return unknown(format!(
                    "не удалось проверить идентичность процесса {pid}: {error}"
                ));
            }
        };
        if pid == self_pid && initial.start_ticks == self_identity.start_ticks {
            continue;
        }
        let candidate = inspect_process(reader, pid, initial, target, workspace, uid);
        if matches!(candidate, Ok(None)) {
            continue;
        }
        match reader.identity(pid) {
            Ok(Some(current))
                if current.is_live() && current.start_ticks == initial.start_ticks => {}
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return unknown(format!("не удалось повторно проверить PID {pid}: {error}"));
            }
        }
        match candidate {
            Ok(Some((process, reference))) => {
                return ProcessCheck::Busy {
                    pid,
                    process,
                    reference,
                };
            }
            Err(error) => return unknown(format!("не удалось проверить процесс {pid}: {error}")),
            Ok(None) => (),
        }
    }
    ProcessCheck::Clear
}

fn inspect_process(
    reader: &impl ProcessReader,
    pid: u32,
    identity: ProcessIdentity,
    target: &Path,
    workspace: &Path,
    uid: u32,
) -> io::Result<Option<(String, String)>> {
    let process = reader.name(pid)?;
    let is_build_tool = matches!(process.as_str(), "cargo" | "rustc" | "rustdoc");
    let cwd = reader.path(pid, "cwd");
    let executable = reader.path(pid, "exe");
    let cwd_in_workspace = path_within(&cwd, workspace);
    let cwd_in_target = path_within(&cwd, target);
    let exe_in_target = path_within(&executable, target);
    let reference = if cwd_in_target {
        Some("текущий каталог процесса находится внутри `target`")
    } else if exe_in_target {
        Some("исполняемый файл процесса находится внутри `target`")
    } else if is_build_tool && cwd_in_workspace {
        Some("процесс Cargo/Rust запущен из рабочей области")
    } else {
        None
    };
    if let Some(reference) = reference {
        return Ok(Some((process, reference.into())));
    }
    // Одно имя `cargo` само по себе не доказывает связь с этой рабочей областью.
    let related = cwd_in_workspace
        || has_workspace_ancestor(reader, identity.parent, pid, target, workspace, uid)?;
    let inaccessible_cwd = cwd.is_err();
    for result in [cwd, executable] {
        match result {
            Err(error) if error.kind() != io::ErrorKind::PermissionDenied || related => {
                return Err(error);
            }
            _ => (),
        }
    }
    match reader.fd_reference(pid, target) {
        Ok(Some(reference)) => Ok(Some((process, reference))),
        Ok(None) => {
            // Если текущий каталог сборщика недоступен, нельзя доказать, что он
            // относится к другой рабочей области.
            if is_build_tool && inaccessible_cwd {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "нельзя определить рабочую область процесса сборки",
                ))
            } else {
                Ok(None)
            }
        }
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied
                && !related
                && !(is_build_tool && inaccessible_cwd) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn path_within(path: &io::Result<Option<PathBuf>>, root: &Path) -> bool {
    path.as_ref()
        .ok()
        .and_then(|path| path.as_ref())
        .is_some_and(|path| path.starts_with(root))
}

fn has_workspace_ancestor(
    reader: &impl ProcessReader,
    mut parent: u32,
    pid: u32,
    target: &Path,
    workspace: &Path,
    uid: u32,
) -> io::Result<bool> {
    for _ in 0..64 {
        if parent <= 1 || parent == pid {
            return Ok(false);
        }
        match reader.uid(parent) {
            Ok(parent_uid) if parent_uid != uid => return Ok(false),
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
        let Some(initial) = reader.identity(parent)? else {
            return Ok(false);
        };
        if !initial.is_live() {
            return Ok(false);
        }
        let process = match reader.name(parent) {
            Ok(process) => process,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let cwd = ancestor_path(reader, parent, "cwd")?;
        let executable = ancestor_path(reader, parent, "exe")?;
        let related = cwd.as_ref().is_some_and(|path| path.starts_with(target))
            || executable
                .as_ref()
                .is_some_and(|path| path.starts_with(target))
            || (matches!(process.as_str(), "cargo" | "rustc" | "rustdoc")
                && cwd.as_ref().is_some_and(|path| path.starts_with(workspace)));
        let Some(current) = reader.identity(parent)? else {
            return Ok(false);
        };
        if !current.is_live() || current.start_ticks != initial.start_ticks {
            return Ok(false);
        }
        if related {
            return Ok(true);
        }
        parent = current.parent;
    }
    Ok(false)
}

fn ancestor_path(reader: &impl ProcessReader, pid: u32, name: &str) -> io::Result<Option<PathBuf>> {
    match reader.path(pid, name) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(None),
        result => result,
    }
}

fn parse_identity(stat: &[u8]) -> io::Result<ProcessIdentity> {
    let text = String::from_utf8_lossy(stat);
    let close = text.rfind(')').ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "повреждённый /proc/<pid>/stat")
    })?;
    let fields: Vec<_> = text[close + 1..].split_ascii_whitespace().collect();
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "неверная identity /proc/<pid>/stat",
        )
    };
    Ok(ProcessIdentity {
        state: fields
            .first()
            .and_then(|state| state.chars().next())
            .ok_or_else(invalid)?,
        parent: fields
            .get(1)
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?,
        start_ticks: fields
            .get(19)
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?,
    })
}

fn strip_deleted_suffix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    PathBuf::from(text.strip_suffix(" (deleted)").unwrap_or(&text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::BTreeMap;

    const SELF_PID: u32 = 42;
    const UID: u32 = 1000;

    #[derive(Default)]
    struct FakeProc {
        processes: BTreeMap<u32, FakeProcess>,
    }

    struct FakeProcess {
        identity: ProcessIdentity,
        name: String,
        cwd: PathBuf,
        exe: PathBuf,
        fd: Option<PathBuf>,
        fd_error: Option<io::ErrorKind>,
        // После первого чтения идентичности моделирует исчезновение PID, повторное
        // использование PID или переход процесса в состояние «зомби».
        next_identity: Option<Option<ProcessIdentity>>,
        identity_reads: Cell<usize>,
    }

    impl FakeProcess {
        fn new(name: &str, cwd: &str) -> Self {
            Self {
                identity: ProcessIdentity {
                    start_ticks: 123,
                    parent: 1,
                    state: 'S',
                },
                name: name.into(),
                cwd: cwd.into(),
                exe: PathBuf::from("/usr/bin").join(name),
                fd: None,
                fd_error: None,
                next_identity: None,
                identity_reads: Cell::new(0),
            }
        }
    }

    impl ProcessReader for FakeProc {
        fn pids(&self) -> io::Result<Vec<u32>> {
            Ok(self.processes.keys().copied().collect())
        }
        fn uid(&self, _pid: u32) -> io::Result<u32> {
            Ok(UID)
        }
        fn identity(&self, pid: u32) -> io::Result<Option<ProcessIdentity>> {
            let process = &self.processes[&pid];
            let reads = process.identity_reads.get();
            process.identity_reads.set(reads + 1);
            Ok(if reads > 0 {
                process.next_identity.unwrap_or(Some(process.identity))
            } else {
                Some(process.identity)
            })
        }
        fn name(&self, pid: u32) -> io::Result<String> {
            Ok(self.processes[&pid].name.clone())
        }
        fn path(&self, pid: u32, name: &str) -> io::Result<Option<PathBuf>> {
            let process = &self.processes[&pid];
            Ok(Some(
                if name == "cwd" {
                    &process.cwd
                } else {
                    &process.exe
                }
                .clone(),
            ))
        }
        fn fd_reference(&self, pid: u32, target: &Path) -> io::Result<Option<String>> {
            let process = &self.processes[&pid];
            if let Some(kind) = process.fd_error {
                return Err(io::Error::from(kind));
            }
            Ok(process
                .fd
                .as_ref()
                .filter(|path| path.starts_with(target))
                .map(|_| "открытый fd 9".into()))
        }
    }

    fn check(processes: impl IntoIterator<Item = (u32, FakeProcess)>) -> ProcessCheck {
        check_processes(
            &FakeProc {
                processes: processes.into_iter().collect(),
            },
            Path::new("/workspace/target"),
            Path::new("/workspace"),
            UID,
            SELF_PID,
            ProcessIdentity {
                start_ticks: 123,
                parent: 1,
                state: 'S',
            },
        )
    }

    #[test]
    fn own_pid_with_executable_in_target_does_not_block() {
        let mut own = FakeProcess::new("anki-repository-maintenance", "/workspace");
        own.exe = "/workspace/target/release/anki-repository-maintenance".into();
        own.fd_error = Some(io::ErrorKind::PermissionDenied);
        assert!(matches!(check([(SELF_PID, own)]), ProcessCheck::Clear));
    }

    #[test]
    fn another_process_with_the_same_name_and_executable_still_blocks() {
        let mut other = FakeProcess::new("anki-repository-maintenance", "/other");
        other.exe = "/workspace/target/release/anki-repository-maintenance".into();
        assert!(matches!(
            check([(43, other)]),
            ProcessCheck::Busy { pid: 43, .. }
        ));
    }

    #[test]
    fn own_pid_with_different_start_ticks_is_not_exempt() {
        let mut reused = FakeProcess::new("anki-repository-maintenance", "/workspace/target");
        reused.identity.start_ticks += 1;
        assert!(matches!(
            check([(SELF_PID, reused)]),
            ProcessCheck::Busy { .. }
        ));
    }

    #[test]
    fn related_live_cargo_blocks_even_with_unreadable_fd() {
        let mut cargo = FakeProcess::new("cargo", "/workspace");
        cargo.fd_error = Some(io::ErrorKind::PermissionDenied);
        assert!(matches!(check([(43, cargo)]), ProcessCheck::Busy { .. }));
    }

    #[test]
    fn unrelated_live_cargo_with_unreadable_fd_does_not_block() {
        let mut cargo = FakeProcess::new("cargo", "/other-workspace");
        cargo.fd_error = Some(io::ErrorKind::PermissionDenied);
        assert!(matches!(check([(43, cargo)]), ProcessCheck::Clear));
    }

    #[test]
    fn zombie_and_dead_cargo_do_not_block() {
        for state in ['Z', 'X', 'x'] {
            let mut cargo = FakeProcess::new("cargo", "/workspace");
            cargo.identity.state = state;
            cargo.fd_error = Some(io::ErrorKind::PermissionDenied);
            assert!(matches!(check([(43, cargo)]), ProcessCheck::Clear));
        }
    }

    #[test]
    fn disappeared_pid_is_discarded_after_busy_or_unreadable_fd() {
        for cwd in ["/workspace/target", "/workspace"] {
            let mut process = FakeProcess::new("worker", cwd);
            process.fd_error = Some(io::ErrorKind::PermissionDenied);
            process.next_identity = Some(None);
            assert!(matches!(check([(43, process)]), ProcessCheck::Clear));
        }
    }

    #[test]
    fn pid_that_becomes_zombie_or_is_reused_is_discarded() {
        for identity in [
            ProcessIdentity {
                start_ticks: 123,
                parent: 1,
                state: 'Z',
            },
            ProcessIdentity {
                start_ticks: 124,
                parent: 1,
                state: 'S',
            },
        ] {
            let mut process = FakeProcess::new("worker", "/workspace/target");
            process.next_identity = Some(Some(identity));
            assert!(matches!(check([(43, process)]), ProcessCheck::Clear));
        }
    }

    #[test]
    fn related_live_worker_with_unreadable_fd_remains_unknown() {
        let mut worker = FakeProcess::new("worker", "/workspace");
        worker.fd_error = Some(io::ErrorKind::PermissionDenied);
        assert!(matches!(
            check([(43, worker)]),
            ProcessCheck::Unknown { .. }
        ));
    }

    #[test]
    fn unrelated_process_with_target_fd_still_blocks() {
        let mut worker = FakeProcess::new("worker", "/other");
        worker.fd = Some("/workspace/target/artifact".into());
        assert!(matches!(check([(43, worker)]), ProcessCheck::Busy { .. }));
    }

    #[test]
    fn related_child_with_unreadable_fd_remains_unknown() {
        let mut worker = FakeProcess::new("worker", "/other");
        worker.identity.parent = 44;
        worker.fd_error = Some(io::ErrorKind::PermissionDenied);
        // Родитель записан позже рабочего процесса: результат зависит от проверки
        // цепочки родителей рабочего процесса.
        let cargo = FakeProcess::new("cargo", "/workspace");
        assert!(matches!(
            check([(43, worker), (44, cargo)]),
            ProcessCheck::Unknown { .. }
        ));
    }

    #[test]
    fn parses_stat_with_spaces_and_parentheses_in_comm() {
        let stat = format!("43 (cargo (worker)) S 1 {}123", "0 ".repeat(17));
        assert_eq!(
            parse_identity(stat.as_bytes()).unwrap(),
            ProcessIdentity {
                start_ticks: 123,
                parent: 1,
                state: 'S'
            }
        );
    }

    #[test]
    fn detects_a_live_process_with_cwd_in_target() {
        use asset_store::temp_workspace::TempWorkspace;
        use std::process::{Command, Stdio};

        let owner = TempWorkspace::create("repository-maintenance-process-test")
            .expect("временный каталог проекта создаётся");
        let root = owner.path().join("fixture");
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .current_dir(&target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Рабочий каталог наследуется при запуске: не нужно ждать, пока дочерний
        // процесс `sleep` перейдёт в режим ожидания.
        let result = target_process_check(&target, &root);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            matches!(result, ProcessCheck::Busy { pid, .. } if pid == child.id()),
            "живой процесс, использующий каталог `target`, должен блокировать очистку: {result:?}"
        );
    }
}
