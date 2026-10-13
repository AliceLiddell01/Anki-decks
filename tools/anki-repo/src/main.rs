//! Тонкий CLI/process adapter.
//!
//! Вся доменная работа происходит в библиотеке; здесь только разбор
//! аргументов, печать подготовленного вывода и отображение результата на
//! process exit code.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use clap::Parser;

use anki_repo::cli::{Cli, Command};
use anki_repo::error::ErrorCode;
use anki_repo::output::write_text;
use anki_repo::run::{execute_cli, render_error};
use repository_maintenance::care;

fn main() -> ExitCode {
    let cli = Cli::parse();
    maybe_offer_maintenance(&cli);

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

/// Вступительное предложение — только в обычных командах с настоящим TTY.
/// Никакого чтения stdin у piped запросов и никаких баннеров в JSON.
fn maybe_offer_maintenance(cli: &Cli) {
    if cli.json
        || !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
        || !io::stderr().is_terminal()
        || !matches!(
            &cli.command,
            Command::Inspect { .. }
                | Command::Find { .. }
                | Command::Stats { .. }
                | Command::Validate { .. }
                | Command::Qa { .. }
                | Command::Models { .. }
        )
    {
        return;
    }
    let Ok(root) = repository_maintenance::policy::workspace_root(None, false) else {
        return;
    };
    let Ok(choice) = care::choice() else { return };
    if choice == "yes" {
        return;
    }
    if choice == "no" {
        if care::warning_due().unwrap_or(false) {
            let status = care::inspect(&root);
            if status
                .diagnostics
                .iter()
                .any(|d| d.code == "target_warning" || d.code == "external_cache_limit")
                && care::should_warn().unwrap_or(false)
            {
                eprintln!(
                    "Предупреждение GC: объём управляемых артефактов превысил порог; подробности: anki-repo maintenance status."
                );
            }
        }
        return;
    }
    // Не предлагаем переустановку автоматически при существующем user timer,
    // даже если он связан с другой рабочей копией.
    let timer = repository_maintenance::timer::inspect();
    if timer.installed_root.is_some() || timer.state == "unknown" {
        return;
    }
    let status = care::inspect(&root);
    eprintln!(
        "Рекомендуем включить автоматическую очистку временных файлов и артефактов сборки Anki-decks."
    );
    if let Some(bytes) = status.measurement.project_bytes {
        eprintln!(
            "Измерено управляемых данных: {bytes} байт{}.",
            if status.measurement.complete {
                ""
            } else if status
                .measurement
                .system_tmp
                .as_ref()
                .is_some_and(|tmp| !tmp.complete)
            {
                " (общий обзор системного /tmp неполон; размер системных данных не включён)"
            } else {
                " (полнота проверки не подтверждена)"
            }
        );
    }
    eprintln!("Очистка ограничена проверками владения и безопасности действующего GC.");
    eprint!("Включить автоматическую очистку? [y/N] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return;
    }
    if answer.trim().eq_ignore_ascii_case("y") {
        match care::enable(&root) {
            Ok(()) => eprintln!("Автоочистка подтверждена: таймер установлен и активен."),
            Err(e) => eprintln!(
                "Не удалось включить GC: {e}. Согласие сохранено; проверьте anki-repo maintenance status."
            ),
        }
    } else if let Err(e) = care::set_choice("no") {
        eprintln!("Не удалось сохранить отказ: {e}");
    }
}
