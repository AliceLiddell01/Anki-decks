//! Единственная граница внешних приложений редакторской проверки.
//! Диспетчер допускает её только по opt-in и после завершённого frontier.
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use crate::{AssetError, ErrorCode};

pub(super) trait ReviewInteraction {
    fn eligible(&self) -> bool;
    fn ask(&mut self, message: &str) -> Result<bool, AssetError>;
    fn open(&mut self, folder: &Path, decisions: &Path) -> Result<(), AssetError>;
}

pub(super) struct SystemReviewInteraction;

impl ReviewInteraction for SystemReviewInteraction {
    fn eligible(&self) -> bool {
        allows_interaction(
            cfg!(test) || test_ancestor(),
            [
                io::stdin().is_terminal(),
                io::stdout().is_terminal(),
                io::stderr().is_terminal(),
            ],
            ["CI", "GITHUB_ACTIONS", "CODEX_NONINTERACTIVE"]
                .iter()
                .any(|key| std::env::var_os(key).is_some()),
            std::env::var("TERM").ok().as_deref(),
        )
    }

    fn ask(&mut self, message: &str) -> Result<bool, AssetError> {
        let mut stderr = io::stderr().lock();
        write!(stderr, "{message} [y/N] ")
            .and_then(|_| stderr.flush())
            .map_err(|error| AssetError::io("приглашение проверки", error))?;
        let mut line = String::new();
        let count = io::stdin()
            .read_line(&mut line)
            .map_err(|error| AssetError::io("ответ проверки", error))?;
        Ok(count > 0 && line.trim().eq_ignore_ascii_case("y"))
    }

    fn open(&mut self, folder: &Path, decisions: &Path) -> Result<(), AssetError> {
        let wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|text| text.to_ascii_lowercase().contains("microsoft"));
        let editor = editor_setting();
        let args = editor.as_deref().map(editor_arguments).transpose()?;
        // Команды запускаются напрямую, без shell. Текст решений не передаётся
        // ни в имя программы, ни в её аргументы.
        if wsl {
            let path = windows_path(folder)?;
            run("explorer.exe", &[path], true)?;
        } else {
            run("xdg-open", &[folder.as_os_str().to_owned()], false)?;
        }
        if let Some(mut args) = args {
            let executable = args.remove(0);
            let path = if wsl && executable.to_ascii_lowercase().ends_with(".exe") {
                windows_path(decisions)?
            } else {
                decisions.as_os_str().to_owned()
            };
            let executable = if wsl && executable.as_bytes().get(1) == Some(&b':') {
                convert_wsl_path("-u", Path::new(&executable))?
            } else {
                executable.into()
            };
            let mut command = Command::new(executable);
            command.args(args).arg(path);
            checked_status(&mut command, false)
        } else if wsl {
            run("notepad.exe", &[windows_path(decisions)?], false)
        } else {
            run("xdg-open", &[decisions.as_os_str().to_owned()], false)
        }
    }
}

fn allows_interaction(
    test_process: bool,
    terminals: [bool; 3],
    ci: bool,
    term: Option<&str>,
) -> bool {
    !test_process
        && terminals.into_iter().all(|terminal| terminal)
        && !ci
        && term.is_some_and(|value| !value.is_empty() && value != "dumb")
}

// В том числе бинарник, запущенный integration-тестом через PTY: его
// собственный cfg(test) выключен, но родительский Cargo/test harness виден в /proc.
fn test_ancestor() -> bool {
    let mut pid = std::process::id();
    for _ in 0..64 {
        let root = format!("/proc/{pid}");
        let Ok(cmdline) = std::fs::read(format!("{root}/cmdline")) else {
            return true;
        };
        let arguments: Vec<_> = cmdline
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .collect();
        if let Some(program) = arguments.first() {
            let program = std::path::Path::new(
                <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(program),
            );
            if program
                .parent()
                .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "deps"))
                || (program.file_name().is_some_and(|name| name == "cargo")
                    && cargo_test_arguments(&arguments))
            {
                return true;
            }
        }
        let Ok(status) = std::fs::read_to_string(format!("{root}/status")) else {
            return true;
        };
        let Some(parent) = status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|value| value.trim().parse::<u32>().ok())
        }) else {
            return true;
        };
        if parent == 0 {
            return false;
        }
        pid = parent;
    }
    true
}

fn cargo_test_arguments(arguments: &[&[u8]]) -> bool {
    arguments
        .iter()
        .skip(1)
        .take_while(|argument| **argument != b"--")
        .any(|argument| *argument == b"test")
}

fn editor_setting() -> Option<String> {
    if let Ok(editor) = std::env::var("GIT_EDITOR")
        && !editor.trim().is_empty()
    {
        return Some(editor);
    }
    if let Ok(output) = Command::new("git")
        .args(["config", "--get", "core.editor"])
        .output()
        && output.status.success()
        && let Ok(editor) = String::from_utf8(output.stdout)
        && !editor.trim().is_empty()
    {
        return Some(editor.trim().to_owned());
    }
    ["VISUAL", "EDITOR"].iter().find_map(|key| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

fn windows_path(path: &Path) -> Result<std::ffi::OsString, AssetError> {
    convert_wsl_path("-w", path)
}

fn convert_wsl_path(direction: &str, path: &Path) -> Result<std::ffi::OsString, AssetError> {
    let output = Command::new("wslpath")
        .arg(direction)
        .arg(path)
        .output()
        .map_err(|error| AssetError::io("wslpath", error))?;
    if !output.status.success() {
        return Err(invalid("wslpath не преобразовал путь"));
    }
    let value = String::from_utf8(output.stdout).map_err(|_| invalid("wslpath вернул не UTF-8"))?;
    Ok(value.trim_end_matches(['\r', '\n']).into())
}

fn run(program: &str, args: &[std::ffi::OsString], explorer: bool) -> Result<(), AssetError> {
    checked_status(Command::new(program).args(args), explorer)
}

fn checked_status(command: &mut Command, explorer: bool) -> Result<(), AssetError> {
    let status = command
        .status()
        .map_err(|error| AssetError::io("запуск приложения проверки", error))?;
    // Explorer при передаче папки работающему процессу возвращает также 1.
    if status.success() || (explorer && status.code() == Some(1)) {
        Ok(())
    } else {
        Err(invalid(format!(
            "приложение проверки завершилось с {status}"
        )))
    }
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

/// Поддерживает кавычки и пробелы в пути/аргументах редактора, но намеренно
/// не реализует shell, подстановки, pipeline или выполнение составных команд.
fn editor_arguments(text: &str) -> Result<Vec<String>, AssetError> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            if characters.peek().is_some_and(|next| {
                matches!(next, '\\' | '\'' | '"') || (quote.is_none() && next.is_whitespace())
            }) {
                escaped = true;
            } else {
                word.push(character);
            }
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                word.push(character);
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character.is_whitespace() {
            if !word.is_empty() {
                result.push(std::mem::take(&mut word));
            }
        } else if matches!(
            character,
            ';' | '|' | '&' | '<' | '>' | '`' | '$' | '\n' | '\r'
        ) {
            return Err(invalid(
                "редактор должен быть программой с аргументами без shell-операторов",
            ));
        } else {
            word.push(character);
        }
    }
    if quote.is_some() || escaped {
        return Err(invalid("незакрытые кавычки или escape редактора"));
    }
    if !word.is_empty() {
        result.push(word);
    }
    if result.is_empty() {
        return Err(invalid("пустая команда редактора"));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tests_cannot_enable_system_apps_even_with_inherited_tty() {
        assert!(!SystemReviewInteraction.eligible());
        assert!(test_ancestor());
    }
    #[test]
    fn every_stream_ci_terminal_and_test_guard_is_required() {
        assert!(allows_interaction(false, [true; 3], false, Some("xterm")));
        for stream in 0..3 {
            let mut terminals = [true; 3];
            terminals[stream] = false;
            assert!(!allows_interaction(false, terminals, false, Some("xterm")));
        }
        assert!(!allows_interaction(true, [true; 3], false, Some("xterm")));
        assert!(!allows_interaction(false, [true; 3], true, Some("xterm")));
        for term in [None, Some(""), Some("dumb")] {
            assert!(!allows_interaction(false, [true; 3], false, term));
        }
    }
    #[test]
    fn cargo_run_user_arguments_are_not_test_harness_markers() {
        assert!(!cargo_test_arguments(&[
            b"cargo",
            b"run",
            b"--",
            b"batch",
            b"run",
            b"--batch-id",
            b"test"
        ]));
        assert!(cargo_test_arguments(&[b"cargo", b"test", b"--", b"filter"]));
    }
    #[test]
    fn editor_paths_arguments_and_shell_operators() {
        assert_eq!(
            editor_arguments("'/mnt/c/Program Files/Editor/edit.exe' --wait").unwrap(),
            ["/mnt/c/Program Files/Editor/edit.exe", "--wait"]
        );
        assert_eq!(
            editor_arguments(r#""C:\Program Files\Editor\edit.exe" --wait"#).unwrap(),
            [r"C:\Program Files\Editor\edit.exe", "--wait"]
        );
        for command in [
            "",
            "editor; other",
            "editor $(other)",
            "editor | other",
            "'editor",
        ] {
            assert!(editor_arguments(command).is_err());
        }
    }
}
