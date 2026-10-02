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

/// Предел выборки свидетельств `models` по умолчанию.
///
/// Значение берётся у владельца операции, чтобы CLI-умолчание не разошлось с
/// фактическим поведением команды.
pub const DEFAULT_MODELS_SAMPLE_LIMIT: u64 = crate::ops::models::DEFAULT_SAMPLE_LIMIT as u64;
/// Жёсткий максимум выборки свидетельств `models`.
pub const MAX_MODELS_SAMPLE_LIMIT: u64 = crate::ops::models::MAX_SAMPLE_LIMIT as u64;
/// Предел подробных заметок `visual-report` по умолчанию.
pub const DEFAULT_VISUAL_PREVIEW_LIMIT: u64 =
    crate::ops::visual_report::DEFAULT_PREVIEW_LIMIT as u64;
/// Жёсткий максимум подробных заметок `visual-report`.
pub const MAX_VISUAL_PREVIEW_LIMIT: u64 = crate::ops::visual_report::MAX_PREVIEW_LIMIT as u64;

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
    about = "Анализ, QA-review, создание и вывод из обращения заметок CrowdAnki: inspect, find, stats, validate, qa, review, review-check, models, create, retire, visual-report",
    long_about = "Анализ, QA-review и правка одного CrowdAnki-экспорта.\n\
                  inspect, find, stats, validate, qa, review, review-check, models и\n\
                  visual-report только читают.\n\
                  edit меняет значения существующих полей существующих заметок.\n\
                  create добавляет заметки в существующие колоды существующих моделей.\n\
                  retire помечает заметки тегом вместо физического удаления.\n\
                  Все три мутирующие команды пишут только по явному --apply, только\n\
                  для канонического deck.json и только после проверок предусловий.\n\
                  Имена полей берутся из фактической модели экспорта: models показывает\n\
                  схему полей и свидетельства по значениям.\n\
                  Toolkit не вызывает LLM API и не ходит в сеть.",
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
            Command::Models { .. } => "models",
            Command::Create { .. } => "create",
            Command::MigrateMedia { .. } => "migrate-media",
            Command::Retire { .. } => "retire",
            Command::VisualReport { .. } => "visual-report",
        }
    }
}

/// Поддерживаемые команды.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Компактное описание структуры экспорта.
    #[command(visible_alias = "describe")]
    Inspect {
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
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
            .args(["guid", "field"])
    ))]
    Find {
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
        export_dir: PathBuf,

        /// Точный поиск заметки по guid.
        #[arg(long)]
        guid: Option<String>,

        /// Имя поля модели для поиска.
        #[arg(long, requires = "value")]
        field: Option<String>,

        /// Искомое значение поля.
        #[arg(long, requires = "field")]
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
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
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
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
        export_dir: PathBuf,
    },

    /// Детерминированные QA-findings по содержимому карточек.
    Qa {
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
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
            .args(["all", "guid", "field", "qa_code"])
    ))]
    Review {
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
        export_dir: PathBuf,

        /// Выбрать все заметки области.
        #[arg(long)]
        all: bool,

        /// Точный выбор заметки по guid.
        #[arg(long)]
        guid: Option<String>,

        /// Имя поля модели для отбора.
        #[arg(long, requires = "value")]
        field: Option<String>,

        /// Искомое значение поля.
        #[arg(long, requires = "field")]
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
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
        export_dir: PathBuf,

        /// JSON-документ предложений (schema_version 1, `proposals`); `-` читает stdin.
        #[arg(long = "proposals", value_name = "PATH")]
        proposals_file: PathBuf,
    },

    /// Схема полей моделей области и свидетельства по фактическим значениям.
    ///
    /// Селекторы колоды можно комбинировать: их согласованность проверяет домен
    /// (`deck_identity_mismatch`), а не clap, — та же проверка работает и для
    /// селекторов внутри JSON-запроса `create`.
    #[command(group(
        clap::ArgGroup::new("deck_selector")
            .required(false)
            .multiple(true)
            .args(["deck", "deck_uuid", "deck_preorder"])
    ))]
    Models {
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
        export_dir: PathBuf,

        /// Колода по полному имени (`::`-путь из deck.json).
        #[arg(long)]
        deck: Option<String>,

        /// Колода по её crowdanki_uuid.
        #[arg(long = "deck-uuid", value_name = "UUID")]
        deck_uuid: Option<String>,

        /// Колода по позиции в порядке обхода; `0` — корневая колода экспорта.
        #[arg(long = "deck-preorder", value_name = "N")]
        deck_preorder: Option<usize>,

        /// Сколько примеров значений показывать на поле.
        #[arg(
            long = "sample-limit",
            default_value_t = DEFAULT_MODELS_SAMPLE_LIMIT,
            value_parser = clap::value_parser!(u64).range(1..=MAX_MODELS_SAMPLE_LIMIT),
        )]
        sample_limit: u64,
    },

    /// Создание заметок в существующих колодах существующих моделей.
    Create {
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
        export_dir: PathBuf,

        /// JSON-документ запроса (schema_version 1, `notes`); `-` читает stdin.
        #[arg(long = "request", value_name = "PATH")]
        request_file: PathBuf,

        /// Записать заметки в deck.json. Без флага выполняется только dry-run.
        #[arg(long)]
        apply: bool,

        /// Записать разрешённый запрос в файл: его повторный прогон идемпотентен.
        #[arg(long = "emit-resolved", value_name = "PATH")]
        emit_resolved: Option<PathBuf>,

        /// Правила создания; по умолчанию .anki-repo/create.yaml из репозитория экспорта.
        #[arg(long)]
        create_config: Option<PathBuf>,

        /// Проверенное хранилище изображений кандзи; по умолчанию .asset-store/kanji из репозитория экспорта.
        #[arg(long)]
        asset_store: Option<PathBuf>,

        /// Проверенное хранилище pitch-accent; по умолчанию .asset-store/pitch-accent из репозитория экспорта.
        #[arg(long)]
        pitch_asset_store: Option<PathBuf>,
    },

    /// Вывод заметок из обращения: тег вместо физического удаления.
    #[command(group(
        clap::ArgGroup::new("targets")
            .required(true)
            .multiple(false)
            .args(["request_file", "guid"])
    ))]
    Retire {
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
        export_dir: PathBuf,

        /// JSON-документ запроса (schema_version 1, `tag`, `notes`); `-` читает stdin.
        #[arg(long = "request", value_name = "PATH")]
        request_file: Option<PathBuf>,

        /// `guid` выводимой из обращения заметки; флаг можно повторить.
        #[arg(long = "guid", value_name = "GUID", requires = "tag")]
        guid: Vec<String>,

        /// Тег вывода из обращения: чем именно помечать, решает колода.
        #[arg(long = "tag", value_name = "TAG", requires = "guid")]
        tag: Option<String>,

        /// Записать теги в deck.json. Без флага выполняется только dry-run.
        #[arg(long)]
        apply: bool,
    },

    /// Статический визуальный отчёт об изменениях двух состояний экспорта.
    #[command(name = "visual-report")]
    VisualReport {
        /// Каталог экспорта в состоянии «до».
        #[arg(long = "before", value_name = "DIR")]
        before: PathBuf,

        /// Каталог экспорта в состоянии «после».
        #[arg(long = "after", value_name = "DIR")]
        after: PathBuf,

        /// Каталог, в который пишется отчёт; создаётся, если его нет.
        #[arg(long = "out", value_name = "DIR")]
        out: PathBuf,

        /// Тег вывода из обращения: по нему заметка попадает в раздел выведенных.
        #[arg(long = "retire-tag", value_name = "TAG")]
        retire_tag: Option<String>,

        /// Сколько заметок показать подробно в каждом разделе отчёта.
        ///
        /// Предел применяется к каждому разделу отдельно (`created`, `changed`,
        /// `retired`, `removed`, неоднозначные `guid`), а общее число файлов
        /// превью дополнительно ограничено `MAX_CARD_FILES`.
        #[arg(
            long = "preview-limit",
            default_value_t = DEFAULT_VISUAL_PREVIEW_LIMIT,
            value_parser = clap::value_parser!(u64).range(1..=MAX_VISUAL_PREVIEW_LIMIT),
        )]
        preview_limit: u64,
    },

    /// Точечная правка значений существующих полей существующих заметок.
    #[command(group(
        clap::ArgGroup::new("request")
            .required(true)
            .multiple(false)
            .args(["request_file", "guid"])
    ))]
    Edit {
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
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

    /// Контролируемая миграция legacy consumer filename на каноническое имя домена.
    ///
    /// Существует ровно для одного случая: прежний конвейер называл pitch-картинку
    /// `<surface>.png`, и для односложного слова с kanji-fallback это имя совпадает
    /// с каноническим именем изображения символа. Команда доказывает семантику
    /// каждой ссылки, переводит их на каноническое имя домена и освобождает
    /// legacy-имя. Ни одна ссылка с иной семантикой не переписывается: команда
    /// останавливается и показывает свидетельства.
    #[command(name = "migrate-media")]
    MigrateMedia {
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
        export_dir: PathBuf,

        /// Пространство имён домена, которому принадлежит каноническое имя.
        #[arg(long, value_name = "NAMESPACE")]
        namespace: String,

        /// Ключ идентичности внутри домена, например поверхность слова.
        #[arg(long, value_name = "KEY")]
        key: String,

        /// Legacy-имя файла, ссылки на которое переводятся на каноническое.
        #[arg(long = "from", value_name = "FILENAME")]
        from: String,

        /// Записать изменения в deck.json и освободить legacy-имя. Без флага — dry-run.
        #[arg(long)]
        apply: bool,

        /// Правила создания; по умолчанию .anki-repo/create.yaml из репозитория экспорта.
        #[arg(long)]
        create_config: Option<PathBuf>,

        /// Проверенное хранилище изображений кандзи; по умолчанию .asset-store/kanji из репозитория экспорта.
        #[arg(long)]
        asset_store: Option<PathBuf>,

        /// Проверенное хранилище pitch-accent; по умолчанию .asset-store/pitch-accent из репозитория экспорта.
        #[arg(long)]
        pitch_asset_store: Option<PathBuf>,
    },
}
