//! CLI-контракт `anki-repo`.
//!
//! Здесь только описание аргументов. Никакой доменной логики: разбор
//! превращается в domain-запросы в [`crate::run`].

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Предел результата `find` по умолчанию.
pub const DEFAULT_LIMIT: u64 = 20;
/// Жёсткий максимум результата `find`.
pub const MAX_LIMIT: u64 = 500;
/// Предел distribution output `stats` по умолчанию.
pub const DEFAULT_TOP: u64 = 20;
/// Жёсткий максимум distribution output `stats`.
pub const MAX_TOP: u64 = 500;

/// Режим сопоставления значения поля на уровне CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MatchArg {
    /// Подстрока в сыром значении поля.
    Contains,
    /// Полное совпадение с сырым значением поля.
    Exact,
}

/// Toolkit для CrowdAnki-экспортов репозитория Anki-decks.
#[derive(Debug, Parser)]
#[command(
    name = "anki-repo",
    version,
    about = "Анализ и точечная правка CrowdAnki-экспортов: inspect, find, stats, validate, edit",
    long_about = "Анализ и точечная правка одного CrowdAnki-экспорта.\n\
                  inspect, find, stats и validate только читают.\n\
                  edit меняет значения существующих полей существующих заметок и\n\
                  пишет только по явному --apply, только для канонического deck.json\n\
                  и только после проверок предусловий.",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Вывести стабильный machine-readable JSON вместо human-readable текста.
    #[arg(long, global = true)]
    pub json: bool,

    /// Команда.
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Стабильное имя выбранной команды для JSON envelope.
    pub const fn command_name(&self) -> &'static str {
        match &self.command {
            Command::Inspect { .. } => "inspect",
            Command::Find { .. } => "find",
            Command::Stats { .. } => "stats",
            Command::Validate { .. } => "validate",
            Command::Edit { .. } => "edit",
        }
    }
}

/// Поддерживаемые команды.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Компактное описание структуры экспорта.
    #[command(visible_alias = "describe")]
    Inspect {
        /// Каталог экспорта, например decks/japanese/words/Words__N3.
        export_dir: PathBuf,
        /// Добавить UUID, deck paths, шаблоны и bounded sample диагностики.
        #[arg(long)]
        verbose: bool,
    },

    /// Поиск заметок по стабильным критериям.
    #[command(group(
        clap::ArgGroup::new("criteria")
            .required(true)
            .multiple(false)
            .args(["guid", "word", "field"])
    ))]
    Find {
        /// Каталог экспорта, например decks/japanese/words/Words__N3.
        export_dir: PathBuf,

        /// Точный поиск заметки по guid.
        #[arg(long)]
        guid: Option<String>,

        /// Сокращение для contains по сырому значению поля «Слово».
        #[arg(long)]
        word: Option<String>,

        /// Имя поля модели для поиска.
        #[arg(long, requires = "value")]
        field: Option<String>,

        /// Искомое значение поля.
        #[arg(long, requires = "field", conflicts_with = "word")]
        value: Option<String>,

        /// Режим сопоставления для --field (только вместе с --field).
        #[arg(long = "match", value_enum)]
        match_mode: Option<MatchArg>,

        /// Ограничить поиск колодой и её вложенными колодами.
        #[arg(long)]
        deck: Option<String>,

        /// Предел числа возвращаемых заметок.
        #[arg(
            long,
            default_value_t = DEFAULT_LIMIT,
            value_parser = clap::value_parser!(u64).range(1..=MAX_LIMIT),
        )]
        limit: u64,
    },

    /// Структурная агрегированная статистика.
    Stats {
        /// Каталог экспорта, например decks/japanese/words/Words__N3.
        export_dir: PathBuf,

        /// Агрегация по сырым значениям указанного поля.
        #[arg(long)]
        group_by: Option<String>,

        /// Предел размера distribution output (только вместе с --group-by).
        #[arg(
            long,
            requires = "group_by",
            value_parser = clap::value_parser!(u64).range(1..=MAX_TOP),
        )]
        top: Option<u64>,
    },

    /// Детерминированная проверка структурной целостности.
    Validate {
        /// Каталог экспорта, например decks/japanese/words/Words__N3.
        export_dir: PathBuf,
    },

    /// Точечная правка значений существующих полей существующих заметок.
    #[command(group(
        clap::ArgGroup::new("request")
            .required(true)
            .multiple(false)
            .args(["request_file", "guid"])
    ))]
    Edit {
        /// Каталог экспорта, например decks/japanese/words/Words__N3.
        export_dir: PathBuf,

        /// JSON-файл с запросом на правки; `-` читает запрос со stdin.
        #[arg(long = "request", value_name = "PATH")]
        request_file: Option<PathBuf>,

        /// `guid` заметки для одиночной правки.
        #[arg(long, requires_all = ["field", "set", "expect"])]
        guid: Option<String>,

        /// Имя поля модели заметки.
        #[arg(long, requires = "guid")]
        field: Option<String>,

        /// Новое значение поля.
        #[arg(long, requires = "guid")]
        set: Option<String>,

        /// Ожидаемое текущее значение поля: без совпадения правка отклоняется.
        #[arg(long = "expect", requires = "guid")]
        expect: Option<String>,

        /// Записать изменения в deck.json. Без флага выполняется только dry-run.
        #[arg(long)]
        apply: bool,
    },
}
