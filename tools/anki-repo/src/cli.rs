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
/// Предел findings на один код для `qa` по умолчанию.
pub const DEFAULT_QA_MAX_PER_CODE: u64 = 20;
/// Жёсткий максимум findings на один код для `qa`.
pub const MAX_QA_MAX_PER_CODE: u64 = 200;
/// Предел страницы `review` по умолчанию.
pub const DEFAULT_REVIEW_LIMIT: u64 = 25;
/// Жёсткий максимум страницы `review`.
pub const MAX_REVIEW_LIMIT: u64 = 200;
/// Смещение страницы `review` по умолчанию.
pub const DEFAULT_REVIEW_OFFSET: u64 = 0;

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
    about = "Анализ, QA-review и точечная правка CrowdAnki-экспортов: inspect, find, stats, validate, qa, review, review-check, edit",
    long_about = "Анализ, QA-review и точечная правка одного CrowdAnki-экспорта.\n\
                  inspect, find, stats, validate, qa, review и review-check только читают.\n\
                  edit меняет значения существующих полей существующих заметок и\n\
                  пишет только по явному --apply, только для канонического deck.json\n\
                  и только после проверок предусловий.\n\
                  Toolkit не вызывает LLM API: разбор содержимого делает внешний агент.",
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
            Command::Qa { .. } => "qa",
            Command::Review { .. } => "review",
            Command::ReviewCheck { .. } => "review-check",
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

    /// Детерминированные QA-findings по содержимому карточек.
    Qa {
        /// Каталог экспорта, например decks/japanese/words/Words__N3.
        export_dir: PathBuf,

        /// Ограничить вывод одним кодом правила; флаг можно повторять.
        #[arg(long = "code", value_name = "CODE")]
        codes: Vec<String>,

        /// Предел числа findings на один код; остальные только считаются.
        #[arg(
            long,
            default_value_t = DEFAULT_QA_MAX_PER_CODE,
            value_parser = clap::value_parser!(u64).range(1..=MAX_QA_MAX_PER_CODE),
        )]
        max_per_code: u64,
    },

    /// Компактный bounded batch карточек для внешнего review.
    #[command(group(
        clap::ArgGroup::new("criteria")
            .required(true)
            .multiple(false)
            .args(["all", "guid", "word", "field", "qa_code"])
    ))]
    Review {
        /// Каталог экспорта, например decks/japanese/words/Words__N3.
        export_dir: PathBuf,

        /// Выбрать все заметки области.
        #[arg(long)]
        all: bool,

        /// Точный выбор заметки по guid.
        #[arg(long)]
        guid: Option<String>,

        /// Сокращение для contains по сырому значению поля «Слово».
        #[arg(long)]
        word: Option<String>,

        /// Имя поля модели для отбора.
        #[arg(long, requires = "value")]
        field: Option<String>,

        /// Искомое значение поля.
        #[arg(long, requires = "field", conflicts_with = "word")]
        value: Option<String>,

        /// Режим сопоставления для --field (только вместе с --field).
        #[arg(long = "match", value_enum)]
        match_mode: Option<MatchArg>,

        /// Отобрать заметки с finding указанного кода QA.
        #[arg(long = "qa-code", value_name = "CODE")]
        qa_code: Option<String>,

        /// Ограничить выборку колодой и её вложенными колодами.
        #[arg(long)]
        deck: Option<String>,

        /// Сколько выбранных заметок пропустить.
        #[arg(long, default_value_t = DEFAULT_REVIEW_OFFSET)]
        offset: u64,

        /// Предел числа заметок в одной странице.
        #[arg(
            long,
            default_value_t = DEFAULT_REVIEW_LIMIT,
            value_parser = clap::value_parser!(u64).range(1..=MAX_REVIEW_LIMIT),
        )]
        limit: u64,
    },

    /// Проверка предложений агента против текущего экспорта.
    #[command(name = "review-check")]
    ReviewCheck {
        /// Каталог экспорта, например decks/japanese/words/Words__N3.
        export_dir: PathBuf,

        /// JSON-документ предложений (schema_version 1, `proposals`); `-` читает stdin.
        #[arg(long = "proposals", value_name = "PATH")]
        proposals_file: PathBuf,
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
