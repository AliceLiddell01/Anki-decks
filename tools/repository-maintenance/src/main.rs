use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

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
    /// Только измерить Cargo target и весь временный каталог.
    Scan(RunArgs),
    /// Показать план; удаление требует явного --apply.
    Clean(CleanArgs),
    /// Управление ежедневным user-level systemd timer.
    Timer(TimerArgs),
    /// Регистрация и использование project-owned внешнего Cargo cache.
    Cache(CacheArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Корень Cargo workspace; по умолчанию ищется от текущего каталога.
    #[arg(long)]
    workspace_root: Option<PathBuf>,
    /// Локальный TOML override единой tracked-политики.
    #[arg(long)]
    policy: Option<PathBuf>,
    /// Инвентаризировать все top-level entries, без top-N ограничения.
    #[arg(long)]
    detail: bool,
    /// Максимальное число крупных entries для обычного вывода.
    #[arg(long)]
    top: Option<usize>,
    /// Корень для тестовой диагностики; production default — /tmp.
    #[arg(long, default_value = "/tmp")]
    temp_root: PathBuf,
    /// Стабильный JSON для автоматизации.
    #[arg(long)]
    json: bool,
    /// Использовать workspace root из install config (systemd unit).
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
    /// Создать private cache directory с marker текущего checkout.
    Init {
        #[arg(long)]
        id: String,
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Запустить команду с CARGO_TARGET_DIR и build-dir в managed cache.
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
    /// Установить idempotent ежедневный user-level systemd timer.
    Install {
        #[arg(long)]
        workspace_root: Option<PathBuf>,
    },
    /// Показать состояние timer.
    Status,
    /// Остановить timer и удалить только принадлежащие инструменту unit-файлы.
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
            i32::from(report.errors > 0)
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
                        println!("Cache: {}", cache_dir.display());
                        println!(
                            "Используйте `cache run --id {id} -- cargo ...` для обновления last-used и GC lifecycle."
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
            if command.iter().skip(1).any(|arg| {
                arg == "--target-dir" || arg.to_string_lossy().starts_with("--target-dir=")
            }) {
                return fail("cache run запрещает переопределять --target-dir", json);
            }
            let cache_root = match repository_maintenance::policy::cache_home() {
                Some(path) => path.join("anki-decks/repository-maintenance"),
                None => return fail("не задан HOME или XDG_CACHE_HOME", json),
            };
            let (cache_dir, target_dir) =
                match repository_maintenance::cache::resolve_build_cache(&root, &cache_root, &id) {
                    Ok(paths) => paths,
                    Err(error) => return fail(&error, json),
                };
            if let Err(error) = repository_maintenance::cache::touch_build_cache(&cache_dir) {
                return fail(&error, json);
            }
            let mut child_command = ProcessCommand::new(&command[0]);
            child_command
                .args(&command[1..])
                .current_dir(&root)
                .env("CARGO_TARGET_DIR", &target_dir)
                .env("CARGO_BUILD_TARGET_DIR", &target_dir)
                .env("CARGO_BUILD_BUILD_DIR", &target_dir);
            let status = child_command.status();
            let touch = repository_maintenance::cache::touch_build_cache(&cache_dir);
            match (status, touch) {
                (Ok(status), Ok(())) => {
                    let code = status.code().unwrap_or(128);
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "schema_version": 1,
                                "cache_id": id,
                                "cache_dir": cache_dir,
                                "target_dir": target_dir,
                                "exit_code": code
                            })
                        );
                    }
                    code
                }
                (Err(error), _) => fail(
                    &format!("не удалось запустить cache command: {error}"),
                    json,
                ),
                (_, Err(error)) => fail(
                    &format!("команда завершилась, но marker не обновлён: {error}"),
                    json,
                ),
            }
        }
    }
}

fn fail(message: &str, json: bool) -> i32 {
    if json {
        match serde_json::to_string_pretty(&ErrorReport {
            schema_version: 1,
            error: message,
        }) {
            Ok(output) => println!("{output}"),
            Err(_) => println!("{{\"schema_version\":1,\"error\":\"serialization failed\"}}"),
        }
    } else {
        eprintln!("Ошибка: {message}");
    }
    2
}
