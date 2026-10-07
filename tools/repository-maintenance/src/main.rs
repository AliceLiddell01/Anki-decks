use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, ExitStatus, Stdio};

use clap::{Args, Parser, Subcommand};
use repository_maintenance::policy::{Policy, workspace_root};
use repository_maintenance::{RunOptions, render_json, render_text, run};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "anki-repository-maintenance",
    version,
    about = "Инвентаризация и безопасная очистка артефактов Anki-decks"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Только измерить каталог Cargo `target` и весь временный каталог.
    Scan(RunArgs),
    /// Показать план; удаление требует явного --apply.
    Clean(CleanArgs),
    /// Управление ежедневным пользовательским таймером systemd.
    Timer(TimerArgs),
    /// Учёт и использование принадлежащего проекту внешнего кэша Cargo.
    Cache(CacheArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Корень рабочей области Cargo; по умолчанию определяется от текущего каталога.
    #[arg(long)]
    workspace_root: Option<PathBuf>,
    /// Локальное переопределение общей политики в TOML.
    #[arg(long)]
    policy: Option<PathBuf>,
    /// Включить в инвентаризацию все записи верхнего уровня, а не только N крупнейших.
    #[arg(long)]
    detail: bool,
    /// Максимальное число крупнейших записей в обычном выводе.
    #[arg(long)]
    top: Option<usize>,
    /// Корень для тестовой диагностики; в обычном режиме используется `/tmp`.
    #[arg(long, default_value = "/tmp")]
    temp_root: PathBuf,
    /// Стабильный JSON для автоматизации.
    #[arg(long)]
    json: bool,
    /// Использовать корень рабочей области из конфигурации установки службы systemd.
    #[arg(long, hide = true)]
    installed_root: bool,
}

#[derive(Debug, Args)]
struct CleanArgs {
    #[command(flatten)]
    common: RunArgs,
    /// Выполнить очистку подтверждённых кандидатов.
    #[arg(long)]
    apply: bool,
}

#[derive(Debug, Args)]
struct TimerArgs {
    #[command(subcommand)]
    action: TimerAction,
    /// Стабильный JSON для автоматизации.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CacheArgs {
    #[command(subcommand)]
    action: CacheAction,
}

#[derive(Debug, Subcommand)]
enum CacheAction {
    /// Создать приватный каталог кэша с маркером текущей рабочей области.
    Init {
        #[arg(long)]
        id: String,
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Запустить Cargo с каталогами сборки в управляемом кэше.
    Run {
        #[arg(long)]
        id: String,
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        #[arg(last = true, required = true, num_args = 1..)]
        command: Vec<OsString>,
    },
}

#[derive(Debug, Subcommand)]
enum TimerAction {
    /// Установить ежедневный пользовательский таймер systemd; повторная установка безопасна.
    Install {
        #[arg(long)]
        workspace_root: Option<PathBuf>,
    },
    /// Показать состояние таймера.
    Status,
    /// Остановить таймер и удалить только принадлежащие инструменту unit-файлы.
    Uninstall,
}

#[derive(Serialize)]
struct ErrorReport<'a> {
    schema_version: u32,
    error: &'a str,
}

fn main() {
    let cli = Cli::parse();
    let exit_code = match cli.command {
        Command::Scan(args) => execute(args, false, "scan"),
        Command::Clean(args) => {
            let mode = if args.apply { "apply" } else { "dry-run" };
            execute(args.common, args.apply, mode)
        }
        Command::Timer(args) => timer(args),
        Command::Cache(args) => cache(args),
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

fn execute(args: RunArgs, apply: bool, mode: &'static str) -> i32 {
    let policy = match args.policy.as_deref() {
        Some(path) => Policy::from_path(path),
        None => Policy::defaults(),
    };
    let policy = match policy {
        Ok(policy) => policy,
        Err(error) => return fail(&error, args.json),
    };
    if args.top.is_some_and(|top| top == 0) && !args.detail {
        return fail("--top должен быть больше нуля", args.json);
    }
    let root = match workspace_root(args.workspace_root.as_deref(), args.installed_root) {
        Ok(root) => root,
        Err(error) => return fail(&error, args.json),
    };
    let options = RunOptions {
        apply,
        detail: args.detail,
        mode,
        temp_root: args.temp_root,
        top_entries: args.top.unwrap_or(policy.tmp_top_entries),
    };
    match run(&root, &policy, &options) {
        Ok(report) => {
            if args.json {
                match render_json(&report) {
                    Ok(json) => println!("{json}"),
                    Err(error) => return fail(&error, true),
                }
            } else {
                print!("{}", render_text(&report));
            }
            i32::from(report.fatal_errors > 0)
        }
        Err(error) => fail(&error, args.json),
    }
}

fn timer(args: TimerArgs) -> i32 {
    let result = match args.action {
        TimerAction::Install {
            workspace_root: root,
        } => {
            let root = match workspace_root(root.as_deref(), false) {
                Ok(root) => root,
                Err(error) => return fail(&error, args.json),
            };
            let executable = match std::env::current_exe() {
                Ok(path) => path,
                Err(error) => {
                    return fail(&format!("не удалось определить binary: {error}"), args.json);
                }
            };
            repository_maintenance::timer::install(&root, &executable)
        }
        TimerAction::Status => repository_maintenance::timer::status(),
        TimerAction::Uninstall => repository_maintenance::timer::uninstall(),
    };
    match result {
        Ok(report) => {
            if args.json {
                match serde_json::to_string_pretty(&report) {
                    Ok(json) => println!("{json}"),
                    Err(error) => return fail(&error.to_string(), true),
                }
            } else {
                println!("{}: {}", report.state, report.message);
                if let Some(output) = report.command_output
                    && !output.is_empty()
                {
                    println!("{output}");
                }
            }
            0
        }
        Err(error) => fail(&error, args.json),
    }
}

fn cache(args: CacheArgs) -> i32 {
    match args.action {
        CacheAction::Init {
            id,
            workspace_root: explicit_root,
            json,
        } => {
            let root = match workspace_root(explicit_root.as_deref(), false) {
                Ok(root) => root,
                Err(error) => return fail(&error, json),
            };
            let cache_root = match repository_maintenance::policy::cache_home() {
                Some(path) => path.join("anki-decks/repository-maintenance"),
                None => return fail("не задан HOME или XDG_CACHE_HOME", json),
            };
            match repository_maintenance::cache::initialize_build_cache(&root, &cache_root, &id) {
                Ok(cache_dir) => {
                    let target_dir = cache_dir.join("target");
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "schema_version": 1,
                                "workspace_root": root,
                                "cache_id": id,
                                "cache_dir": cache_dir,
                                "target_dir": target_dir,
                                "ownership": "project-owned"
                            })
                        );
                    } else {
                        println!("Кэш: {}", cache_dir.display());
                        println!(
                            "Используйте `cache run --id {id} -- cargo ...`, чтобы обновлять время последнего использования и учитывать жизненный цикл очистки."
                        );
                    }
                    0
                }
                Err(error) => fail(&error, json),
            }
        }
        CacheAction::Run {
            id,
            workspace_root: explicit_root,
            json,
            command,
        } => {
            let root = match workspace_root(explicit_root.as_deref(), false) {
                Ok(root) => root,
                Err(error) => return fail(&error, json),
            };
            if let Err(error) = validate_cache_cargo_invocation(&command, &root) {
                return fail(&error, json);
            }
            let cache_root = match repository_maintenance::policy::cache_home() {
                Some(path) => path.join("anki-decks/repository-maintenance"),
                None => return fail("не задан HOME или XDG_CACHE_HOME", json),
            };
            let lock = match repository_maintenance::guard::MaintenanceLock::acquire() {
                Ok(lock) => lock,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return fail(
                        "уже выполняется управляемая сборка или очистка; повторите команду позже",
                        json,
                    );
                }
                Err(error) => {
                    return fail(
                        &format!("не удалось получить общую блокировку обслуживания: {error}"),
                        json,
                    );
                }
            };
            let cache_guard = match repository_maintenance::cache::CacheRunGuard::begin(
                &root,
                &cache_root,
                &id,
                lock,
            ) {
                Ok(guard) => guard,
                Err(error) => return fail(&error, json),
            };
            let cache_dir = cache_guard.cache_dir().to_path_buf();
            let target_dir = cache_guard.target_dir().to_path_buf();
            let managed_command = match managed_cargo_command(&command, &target_dir) {
                Ok(command) => command,
                Err(error) => {
                    let finish = cache_guard.finish();
                    return fail(&append_cache_finish_error(error, finish.err()), json);
                }
            };
            let mut child_command = ProcessCommand::new(&command[0]);
            child_command
                .args(&managed_command[1..])
                .current_dir(&root)
                .env("CARGO_TARGET_DIR", &target_dir)
                .env("CARGO_BUILD_TARGET_DIR", &target_dir)
                .env("CARGO_BUILD_BUILD_DIR", &target_dir);
            let status = run_cache_child(&mut child_command, json);
            let finish = cache_guard.finish();
            match status {
                Ok(status) => {
                    let code = child_exit_code(status);
                    let marker_error = finish.err();
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "schema_version": 1,
                                "cache_id": id,
                                "cache_dir": cache_dir,
                                "target_dir": target_dir,
                                "exit_code": code,
                                "error": marker_error
                            })
                        );
                    } else if let Some(error) = &marker_error {
                        eprintln!(
                            "Ошибка: не удалось обновить маркер жизненного цикла кэша: {error}"
                        );
                    }
                    if code == 0 && marker_error.is_some() {
                        2
                    } else {
                        code
                    }
                }
                Err(error) => {
                    let message = append_cache_finish_error(error, finish.err());
                    fail(&message, json)
                }
            }
        }
    }
}

fn validate_cache_cargo_invocation(
    command: &[OsString],
    workspace_root: &std::path::Path,
) -> Result<(), String> {
    let Some(program) = command.first() else {
        return Err("`cache run` требует прямого вызова Cargo".into());
    };
    if std::path::Path::new(program)
        .file_name()
        .is_none_or(|name| name != "cargo")
    {
        return Err(
            "`cache run` принимает только прямой вызов Cargo; запуск через оболочку или обёртку `env` запрещён"
                .into(),
        );
    }
    let mut index = 1;
    if command
        .get(index)
        .is_some_and(|argument| argument.to_string_lossy().starts_with('+'))
    {
        index += 1;
    }
    let subcommand_index = loop {
        let Some(argument) = command.get(index) else {
            return Err("`cache run` требует поддерживаемую встроенную команду Cargo".into());
        };
        let argument_text = argument.to_string_lossy();
        match argument_text.as_ref() {
            "--config" => {
                let value = command
                    .get(index + 1)
                    .ok_or("после `--config` ожидается значение")?;
                validate_cargo_config(value, workspace_root)?;
                index += 2;
            }
            "--target-dir" => {
                return Err("`cache run` запрещает переопределять `--target-dir`".into());
            }
            "-C" => {
                return Err(
                    "`cache run` запрещает Cargo `-C`, меняющий каталог проекта и поиск конфигурации"
                        .into(),
                );
            }
            "--locked" | "--offline" | "--frozen" | "--quiet" | "-q" | "--verbose" | "-v"
            | "--help" | "-h" | "--version" | "-V" => index += 1,
            "--color" | "-Z" => {
                if command.get(index + 1).is_none() {
                    return Err(format!("после `{argument_text}` ожидается значение"));
                }
                index += 2;
            }
            _ if argument_text.starts_with("--config=") => {
                validate_cargo_config(
                    &OsString::from(argument_text.strip_prefix("--config=").unwrap()),
                    workspace_root,
                )?;
                index += 1;
            }
            _ if argument_text.starts_with("--color=") => index += 1,
            _ if argument_text.starts_with("--target-dir=") => {
                return Err("`cache run` запрещает переопределять `--target-dir`".into());
            }
            _ if argument_text.starts_with("-C") => {
                return Err(
                    "`cache run` запрещает Cargo `-C`, меняющий каталог проекта и поиск конфигурации"
                        .into(),
                );
            }
            _ if argument_text.starts_with("-Z") || argument_text.starts_with("-v") => {
                index += 1;
            }
            _ if argument_text.starts_with('-') => {
                return Err(format!(
                    "неподдерживаемый глобальный аргумент Cargo до подкоманды: {argument_text}"
                ));
            }
            _ => break index,
        }
    };
    let subcommand = command[subcommand_index].to_string_lossy();
    if !matches!(
        subcommand.as_ref(),
        "build" | "check" | "test" | "bench" | "doc" | "run"
    ) {
        return Err(format!(
            "`cache run` разрешает только встроенные команды Cargo для сборки; `{subcommand}` может быть псевдонимом или внешней командой"
        ));
    }

    let mut index = subcommand_index + 1;
    while index < command.len() {
        let argument = command[index].to_string_lossy();
        if argument == "--" {
            break;
        }
        if argument == "--target-dir" || argument.starts_with("--target-dir=") {
            return Err("`cache run` запрещает переопределять `--target-dir`".into());
        }
        if argument == "-C" || argument.starts_with("-C") {
            return Err(
                "`cache run` запрещает Cargo `-C`, меняющий каталог проекта и поиск конфигурации"
                    .into(),
            );
        }
        if argument == "--config" {
            let value = command
                .get(index + 1)
                .ok_or("после `--config` ожидается значение")?;
            validate_cargo_config(value, workspace_root)?;
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--config=") {
            validate_cargo_config(&OsString::from(value), workspace_root)?;
        }
        index += 1;
    }
    Ok(())
}

fn validate_cargo_config(
    value: &std::ffi::OsStr,
    workspace_root: &std::path::Path,
) -> Result<(), String> {
    let value = value
        .to_str()
        .ok_or("значение Cargo `--config` должно быть в UTF-8, чтобы проверить `cache run`")?;
    let parsed = if value.contains('=') {
        toml::from_str::<toml::Value>(value)
            .map_err(|error| format!("не удалось разобрать Cargo `--config`: {error}"))?
    } else {
        let path = std::path::Path::new(value);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace_root.join(path)
        };
        let text = std::fs::read_to_string(&path).map_err(|error| {
            format!(
                "не удалось прочитать файл конфигурации Cargo {}: {error}",
                path.display()
            )
        })?;
        toml::from_str::<toml::Value>(&text).map_err(|error| {
            format!(
                "не удалось разобрать файл конфигурации Cargo {}: {error}",
                path.display()
            )
        })?
    };
    let overrides_managed_directory = parsed
        .get("build")
        .and_then(toml::Value::as_table)
        .is_some_and(|build| build.contains_key("target-dir") || build.contains_key("build-dir"));
    if overrides_managed_directory {
        return Err("Cargo `--config` не может переопределять `build.target-dir` или `build.build-dir` при запуске `cache run`".into());
    }
    Ok(())
}

fn managed_cargo_command(
    command: &[OsString],
    target_dir: &std::path::Path,
) -> Result<Vec<OsString>, String> {
    let target_dir = target_dir
        .to_str()
        .ok_or("путь к управляемому кэшу нельзя передать Cargo: он не является UTF-8")?;
    let target_value = toml::Value::String(target_dir.to_owned()).to_string();
    let mut managed = command.to_vec();
    let mut insert_at = usize::from(
        managed
            .get(1)
            .is_some_and(|argument| argument.to_string_lossy().starts_with('+')),
    ) + 1;
    while insert_at < managed.len() {
        match managed[insert_at].to_string_lossy().as_ref() {
            "--" => break,
            "--config" => insert_at += 2,
            _ => insert_at += 1,
        }
    }
    let overrides = [
        OsString::from("--config"),
        OsString::from(format!("build.target-dir={target_value}")),
        OsString::from("--config"),
        OsString::from(format!("build.build-dir={target_value}")),
    ];
    managed.splice(insert_at..insert_at, overrides);
    Ok(managed)
}

fn run_cache_child(command: &mut ProcessCommand, json: bool) -> Result<ExitStatus, String> {
    if !json {
        return command
            .status()
            .map_err(|error| format!("не удалось запустить Cargo: {error}"));
    }
    command.stdout(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("не удалось запустить Cargo: {error}"))?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("не удалось получить поток stdout Cargo".into());
    };
    let output_thread = match std::thread::Builder::new()
        .name("cache-run-stdout".into())
        .spawn(move || {
            let mut stderr = io::stderr().lock();
            let mut buffer = [0_u8; 8192];
            let mut write_failed = false;
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(size) => {
                        if !write_failed && stderr.write_all(&buffer[..size]).is_err() {
                            write_failed = true;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        return Err(format!("не удалось прочитать stdout Cargo: {error}"));
                    }
                }
            }
        }) {
        Ok(thread) => thread,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "не удалось запустить потоковую передачу stdout Cargo: {error}"
            ));
        }
    };
    let waited = match child.wait() {
        Ok(status) => Ok(status),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("не удалось дождаться завершения Cargo: {error}"))
        }
    };
    let forwarded = output_thread
        .join()
        .map_err(|_| "потоковая передача stdout Cargo аварийно завершилась".to_owned())?;
    let status = waited?;
    forwarded?;
    Ok(status)
}

fn child_exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(128)
}

fn append_cache_finish_error(mut message: String, finish_error: Option<String>) -> String {
    if let Some(error) = finish_error {
        message.push_str(&format!(
            "; маркер жизненного цикла также не обновлён: {error}"
        ));
    }
    message
}

fn fail(message: &str, json: bool) -> i32 {
    if json {
        match serde_json::to_string_pretty(&ErrorReport {
            schema_version: 1,
            error: message,
        }) {
            Ok(output) => println!("{output}"),
            Err(_) => println!(
                "{{\"schema_version\":1,\"error\":\"не удалось сформировать JSON-отчёт\"}}"
            ),
        }
    } else {
        eprintln!("Ошибка: {message}");
    }
    2
}
