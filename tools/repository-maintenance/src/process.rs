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

/// Ищет same-UID Cargo/rustc процессы рабочего пространства и любые процессы,
/// у которых cwd/exe/open fd указывает внутрь target. Сведения о PID сверяются
/// по start ticks, чтобы повторное использование PID не стало ложным совпадением.
pub fn target_process_check(target: &Path, workspace: &Path) -> ProcessCheck {
    let proc_root = Path::new("/proc");
    let target = match fs::canonicalize(target) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => target.to_path_buf(),
        Err(error) => {
            return ProcessCheck::Unknown {
                reason: format!("не удалось разрешить target: {error}"),
            };
        }
    };
    let workspace = match fs::canonicalize(workspace) {
        Ok(path) => path,
        Err(error) => {
            return ProcessCheck::Unknown {
                reason: format!("не удалось разрешить workspace: {error}"),
            };
        }
    };
    let current_uid = match fs::metadata("/proc/self") {
        Ok(metadata) => metadata.uid(),
        Err(error) => {
            return ProcessCheck::Unknown {
                reason: format!("не удалось определить UID процессов: {error}"),
            };
        }
    };
    let processes = match fs::read_dir(proc_root) {
        Ok(processes) => processes,
        Err(error) => {
            return ProcessCheck::Unknown {
                reason: format!("не удалось прочитать /proc: {error}"),
            };
        }
    };

    for item in processes {
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                return ProcessCheck::Unknown {
                    reason: format!("ошибка перечисления /proc: {error}"),
                };
            }
        };
        let Some(pid) = item
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let process_dir = proc_root.join(pid.to_string());
        let initial_start = match process_start_ticks(&process_dir) {
            Ok(Some(start)) => start,
            Ok(None) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                if fs::metadata(&process_dir).is_ok_and(|metadata| metadata.uid() == current_uid) {
                    return ProcessCheck::Unknown {
                        reason: format!("не удалось проверить identity процесса {pid}: {error}"),
                    };
                }
                continue;
            }
        };
        let process_meta = match fs::metadata(&process_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return ProcessCheck::Unknown {
                    reason: format!("не удалось проверить UID процесса {pid}: {error}"),
                };
            }
        };
        if process_meta.uid() != current_uid {
            continue;
        }
        let comm = match fs::read_to_string(process_dir.join("comm")) {
            Ok(value) => value.trim().to_owned(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return ProcessCheck::Unknown {
                    reason: format!("не удалось прочитать имя процесса {pid}: {error}"),
                };
            }
        };
        let is_build_tool = matches!(comm.as_str(), "cargo" | "rustc" | "rustdoc");
        let cwd = match fs::read_link(process_dir.join("cwd")) {
            Ok(path) => Some(strip_deleted_suffix(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                if is_build_tool {
                    return ProcessCheck::Unknown {
                        reason: format!("нельзя проверить cwd процесса {pid} ({comm})"),
                    };
                }
                None
            }
            Err(error) => {
                return ProcessCheck::Unknown {
                    reason: format!("ошибка чтения cwd процесса {pid}: {error}"),
                };
            }
        };
        let executable = match fs::read_link(process_dir.join("exe")) {
            Ok(path) => Some(strip_deleted_suffix(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                if is_build_tool {
                    return ProcessCheck::Unknown {
                        reason: format!("нельзя проверить executable процесса {pid} ({comm})"),
                    };
                }
                None
            }
            Err(error) => {
                return ProcessCheck::Unknown {
                    reason: format!("ошибка чтения executable процесса {pid}: {error}"),
                };
            }
        };
        let cwd_in_workspace = cwd
            .as_ref()
            .is_some_and(|path| path.starts_with(&workspace));
        let cwd_in_target = cwd.as_ref().is_some_and(|path| path.starts_with(&target));
        let exe_in_target = executable
            .as_ref()
            .is_some_and(|path| path.starts_with(&target));
        if cwd_in_target {
            match same_process(&process_dir, initial_start) {
                Ok(true) => {
                    return ProcessCheck::Busy {
                        pid,
                        process: comm,
                        reference: "cwd процесса расположен в target".into(),
                    };
                }
                Ok(false) => continue,
                Err(error) => {
                    return ProcessCheck::Unknown {
                        reason: format!("не удалось повторно проверить PID {pid}: {error}"),
                    };
                }
            }
        }
        if is_build_tool && cwd_in_workspace {
            match same_process(&process_dir, initial_start) {
                Ok(true) => {
                    return ProcessCheck::Busy {
                        pid,
                        process: comm,
                        reference: "Cargo/Rust процесс работает из workspace".into(),
                    };
                }
                Ok(false) => continue,
                Err(error) => {
                    return ProcessCheck::Unknown {
                        reason: format!("не удалось повторно проверить PID {pid}: {error}"),
                    };
                }
            }
        }
        if exe_in_target {
            match same_process(&process_dir, initial_start) {
                Ok(true) => {
                    return ProcessCheck::Busy {
                        pid,
                        process: comm,
                        reference: "executable процесса расположен в target".into(),
                    };
                }
                Ok(false) => continue,
                Err(error) => {
                    return ProcessCheck::Unknown {
                        reason: format!("не удалось повторно проверить PID {pid}: {error}"),
                    };
                }
            }
        }

        let related_child = if is_build_tool || cwd_in_workspace || cwd_in_target || exe_in_target {
            false
        } else {
            match has_workspace_ancestor(pid, &target, &workspace, current_uid) {
                Ok(value) => value,
                Err(error) => {
                    return ProcessCheck::Unknown {
                        reason: format!("не удалось проверить родителей процесса {pid}: {error}"),
                    };
                }
            }
        };
        // Открытые дескрипторы проверяем у build tools, процессов workspace и
        // их потомков. Несвязанные user daemons могут запрещать чтение fd,
        // поэтому сверяем их отдельно по cwd/exe и не даём им блокировать GC.
        if !is_build_tool && !cwd_in_workspace && !related_child {
            continue;
        }
        match fd_reference(&process_dir, &target) {
            Ok(Some(reference)) => match same_process(&process_dir, initial_start) {
                Ok(true) => {
                    return ProcessCheck::Busy {
                        pid,
                        process: comm,
                        reference,
                    };
                }
                Ok(false) => (),
                Err(error) => {
                    return ProcessCheck::Unknown {
                        reason: format!("не удалось повторно проверить PID {pid}: {error}"),
                    };
                }
            },
            Ok(None) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return ProcessCheck::Unknown {
                    reason: format!("нельзя проверить открытые файлы процесса {pid}"),
                };
            }
            Err(error) => {
                return ProcessCheck::Unknown {
                    reason: format!("ошибка проверки fd процесса {pid}: {error}"),
                };
            }
        }
    }
    ProcessCheck::Clear
}

fn fd_reference(process_dir: &Path, target: &Path) -> io::Result<Option<String>> {
    let descriptors = fs::read_dir(process_dir.join("fd"))?;
    for descriptor in descriptors {
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

fn has_workspace_ancestor(pid: u32, target: &Path, workspace: &Path, uid: u32) -> io::Result<bool> {
    let proc_root = Path::new("/proc");
    let Some(mut parent) = process_parent_pid(&proc_root.join(pid.to_string()))? else {
        return Ok(false);
    };
    for _ in 0..64 {
        if parent <= 1 || parent == pid {
            return Ok(false);
        }
        let process_dir = proc_root.join(parent.to_string());
        let metadata = match fs::metadata(&process_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if metadata.uid() != uid {
            return Ok(false);
        }
        let comm = match fs::read_to_string(process_dir.join("comm")) {
            Ok(value) => value.trim().to_owned(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let cwd = read_process_path(&process_dir.join("cwd"))?;
        let executable = read_process_path(&process_dir.join("exe"))?;
        let cwd_in_workspace = cwd.as_ref().is_some_and(|path| path.starts_with(workspace));
        let cwd_in_target = cwd.as_ref().is_some_and(|path| path.starts_with(target));
        let exe_in_target = executable
            .as_ref()
            .is_some_and(|path| path.starts_with(target));
        if cwd_in_target
            || exe_in_target
            || (matches!(comm.as_str(), "cargo" | "rustc" | "rustdoc") && cwd_in_workspace)
        {
            return Ok(true);
        }
        let Some(next_parent) = process_parent_pid(&process_dir)? else {
            return Ok(false);
        };
        parent = next_parent;
    }
    Ok(false)
}

fn read_process_path(path: &Path) -> io::Result<Option<PathBuf>> {
    match fs::read_link(path) {
        Ok(path) => Ok(Some(strip_deleted_suffix(path))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(None),
        Err(error) => Err(error),
    }
}

fn process_parent_pid(process_dir: &Path) -> io::Result<Option<u32>> {
    let stat = match fs::read(process_dir.join("stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let text = String::from_utf8_lossy(&stat);
    let close = text.rfind(')').ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "повреждённый /proc/<pid>/stat")
    })?;
    text[close + 1..]
        .split_ascii_whitespace()
        .nth(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "нет PPID в /proc/<pid>/stat"))?
        .parse()
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "неверный PPID"))
}

fn process_start_ticks(process_dir: &Path) -> io::Result<Option<u64>> {
    let stat = match fs::read(process_dir.join("stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let text = String::from_utf8_lossy(&stat);
    let Some(close) = text.rfind(')') else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "повреждённый /proc/<pid>/stat",
        ));
    };
    let start = text[close + 1..]
        .split_ascii_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "нет start ticks"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "неверные start ticks"))?;
    Ok(Some(start))
}

fn same_process(process_dir: &Path, expected_start: u64) -> io::Result<bool> {
    Ok(process_start_ticks(process_dir)? == Some(expected_start))
}

fn strip_deleted_suffix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    PathBuf::from(text.strip_suffix(" (deleted)").unwrap_or(&text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asset_store::temp_workspace::TempWorkspace;
    use std::fs;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn detects_a_live_process_with_cwd_in_target() {
        let owner = TempWorkspace::create("repository-maintenance-process-test")
            .expect("project-owned temp workspace создаётся");
        let root = owner.path().join("fixture");
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        let mut child = Command::new("sleep")
            .arg("5")
            .current_dir(&target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(40));
        let result = target_process_check(&target, &root);
        assert!(
            !matches!(result, ProcessCheck::Clear),
            "active process must be reported busy or make the result unprovable: {result:?}"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
