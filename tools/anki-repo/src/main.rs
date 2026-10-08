//! Тонкий CLI/process adapter.
//!
//! Вся доменная работа происходит в библиотеке; здесь только разбор
//! аргументов, печать подготовленного вывода и отображение результата на
//! process exit code.

use std::io;
use std::process::ExitCode;

use clap::Parser;

use anki_repo::cli::Cli;
use anki_repo::error::ErrorCode;
use anki_repo::output::write_text;
use anki_repo::run::{execute_cli, render_error};

fn main() -> ExitCode {
    let cli = Cli::parse();

    match execute_cli(&cli) {
        Ok(rendered) => {
            if stdout_write_failed(&rendered.stdout) {
                return internal_failure();
            }
            ExitCode::from(rendered.exit)
        }
        Err(error) => {
            let command = cli.command_name();
            let (stdout, stderr) = render_error(command, cli.json, &error);
            let failed = stdout_write_failed(&stdout);
            // Диагностика вторична: если её не удалось записать, основной
            // результат и exit code всё равно должны быть отданы.
            let _ = write_text(&mut io::stderr().lock(), &stderr);
            if failed {
                return internal_failure();
            }
            ExitCode::from(error.exit_code())
        }
    }
}

/// Пишет вывод команды в stdout и сообщает, была ли запись внутренней ошибкой.
///
/// Закрытый читателем pipe (`anki-repo … | head`) внутренней ошибкой не
/// считается: получатель сам перестал читать, а результат команды уже вычислен,
/// поэтому сохраняется её собственный exit code. Любой другой отказ записи
/// (нет места на диске, негодный дескриптор) — это внутренняя ошибка.
fn stdout_write_failed(text: &str) -> bool {
    match write_text(&mut io::stdout().lock(), text) {
        Ok(()) => false,
        Err(error) => error.kind() != io::ErrorKind::BrokenPipe,
    }
}

/// Exit code внутренней ошибки: вывод не удалось записать.
fn internal_failure() -> ExitCode {
    ExitCode::from(ErrorCode::Internal.exit_code())
}
