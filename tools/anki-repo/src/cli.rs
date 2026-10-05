//! CLI-контракт `anki-repo`.
//!
//! Здесь только описание аргументов. Предметной логики нет: разбор
//! превращается в запросы для [`crate::run`].

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Предел результата `find` по умолчанию.
pub const DEFAULT_LIMIT: u64 = 20;
/// Жёсткий максимум результата `find`.
pub const MAX_LIMIT: u64 = 500;
/// Предел числа элементов распределения `stats` по умолчанию.
pub const DEFAULT_TOP: u64 = 20;
/// Максимально допустимое число элементов распределения `stats`.
pub const MAX_TOP: u64 = 500;
/// Предел результатов на один код для `qa` по умолчанию.
pub const DEFAULT_QA_MAX_PER_CODE: u64 = 20;
/// Жёсткий максимум результатов на один код для `qa`.
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

/// Инструменты для CrowdAnki-экспортов репозитория Anki-decks.
#[derive(Debug, Parser)]
#[command(
    name = "anki-repo",
    version,
    about = "Анализ CrowdAnki и свидетельства для ревью кода: inspect, find, stats, validate, qa, review, review-check, code-review, language, edit, models, create, retire, migrate-media, visual-report",
    long_about = "Анализ, проверка и ограниченные изменения одного CrowdAnki-экспорта.\n\
                  inspect, find, stats, validate, qa, review, review-check, models,\n\
                  visual-report и code-review collect/verify/delta только читают\n\
                  зафиксированное состояние репозитория; code-review сохраняет локальные\n\
                  артефакты с точной идентичностью Git-снимка.\n\
                  language scan/check только читают. language apply требует\n\
                  явного --apply и решений replace, подтверждённых человеком.\n\
                  edit меняет значения существующих полей существующих заметок.\n\
                  create добавляет заметки в существующие колоды и модели.\n\
                  retire помечает заметки тегом вместо физического удаления.\n\
                  migrate-media переводит только доказанные ссылки с legacy-имени домена.\n\
                  Все изменяющие команды по умолчанию выполняют dry-run; запись возможна\n\
                  только с явным --apply и после проверок предусловий.\n\
                  edit и retire меняют только канонический deck.json. create меняет\n\
                  deck.json и размещает только проверенные файлы, разрешённые явными\n\
                  правилами домена. migrate-media меняет deck.json, размещает проверенный\n\
                  канонический файл и удаляет legacy-файл только после доказательства\n\
                  пары identity—имя и проверки всех потребителей.\n\
                  Схему полей показывает models. Инструменты не вызывают LLM API и\n\
                  не ходят в сеть.",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Вывести стабильный машиночитаемый JSON вместо текста для человека.
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
            Command::CodeReview { command } => match command {
                CodeReviewCommand::Collect { .. } => "code-review collect",
                CodeReviewCommand::Verify { .. } => "code-review verify",
                CodeReviewCommand::Delta { .. } => "code-review delta",
            },
            Command::Language { command } => match command {
                LanguageCommand::Scan { .. } => "language scan",
                LanguageCommand::Check { .. } => "language check",
                LanguageCommand::Apply { .. } => "language apply",
            },
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
        /// Добавить UUID, пути подколод, шаблоны и ограниченную выборку диагностики.
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

        /// Предел размера распределения (только вместе с --group-by).
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

    /// Детерминированные результаты QA по содержимому карточек.
    Qa {
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
        export_dir: PathBuf,

        /// Ограничить вывод одним кодом правила; флаг можно повторять.
        #[arg(long = "code", value_name = "CODE")]
        codes: Vec<String>,

        /// Предел числа результатов на один код; остальные только считаются.
        #[arg(
            long,
            default_value_t = DEFAULT_QA_MAX_PER_CODE,
            value_parser = clap::value_parser!(u64).range(1..=MAX_QA_MAX_PER_CODE),
        )]
        max_per_code: u64,
    },

    /// Компактный ограниченный пакет карточек для внешней проверки.
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

        /// Отобрать заметки с замечанием указанного кода QA.
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

    /// Создание заметок в существующих колодах и моделях.
    ///
    /// Ссылки на media по умолчанию запрещены. Явные правила для точной модели,
    /// поля и домена могут разрешить только проверенные файлы; с `--apply`
    /// файлы размещаются в `media/`, а их имена добавляются в `media_files`.
    Create {
        /// Каталог CrowdAnki-экспорта: каталог, в котором лежит deck.json.
        export_dir: PathBuf,

        /// JSON-документ запроса (schema_version 1, `notes`); `-` читает stdin.
        #[arg(long = "request", value_name = "PATH")]
        request_file: PathBuf,

        /// Записать заметки и разрешённые объявления `media_files`, разместить проверенные файлы. Без флага — только dry-run.
        #[arg(long)]
        apply: bool,

        /// Записать разрешённый запрос в файл: его повторный прогон идемпотентен.
        #[arg(long = "emit-resolved", value_name = "PATH")]
        emit_resolved: Option<PathBuf>,

        /// Правила создания заметок; по умолчанию .anki-repo/create.yaml из репозитория экспорта.
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

    /// Контролируемая миграция доказанных ссылок с legacy-имени домена.
    ///
    /// Политика домена должна доказать точную пару `AssetIdentity` и имени файла.
    /// Команда проверяет всех потребителей в полях заметок, `qfmt`, `afmt` и CSS;
    /// неподтверждённый потребитель блокирует миграцию целиком. По умолчанию
    /// выполняется dry-run; запись возможна только с явным `--apply`.
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

        /// Записать проверенный deck.json и канонический файл, затем удалить legacy-файл. Без флага — только dry-run.
        #[arg(long)]
        apply: bool,

        /// Правила привязки доменных обработчиков; по умолчанию .anki-repo/create.yaml из репозитория экспорта.
        #[arg(long)]
        create_config: Option<PathBuf>,

        /// Проверенное хранилище изображений кандзи; по умолчанию .asset-store/kanji из репозитория экспорта.
        #[arg(long)]
        asset_store: Option<PathBuf>,

        /// Проверенное хранилище pitch-accent; по умолчанию .asset-store/pitch-accent из репозитория экспорта.
        #[arg(long)]
        pitch_asset_store: Option<PathBuf>,
    },

    /// Детерминированные свидетельства для независимого ревью кода.
    CodeReview {
        #[command(subcommand)]
        command: CodeReviewCommand,
    },

    /// Проверка и утверждённая правка человеческого текста.
    Language {
        #[command(subcommand)]
        command: LanguageCommand,
    },
}

/// Подкоманды сбора и сравнения свидетельств для ревью кода.
#[derive(Debug, Subcommand)]
pub enum CodeReviewCommand {
    /// Собрать пакет свидетельств для явного диапазона Git.
    Collect {
        /// Базовая ссылка Git, например main или origin/main.
        #[arg(long)]
        base: String,
        /// Верхняя ссылка Git; её SHA фиксируется в пакете.
        #[arg(long)]
        head: String,
        /// Каталог локальных артефактов; по умолчанию вычисляется по SHA.
        #[arg(long = "out-dir", value_name = "DIR")]
        out_dir: Option<PathBuf>,
        /// Явно разрешить локальный Clippy; сборка может исполнять build.rs и proc-macro.
        #[arg(long)]
        run_clippy: bool,
    },

    /// Повторно собрать свидетельства для нового HEAD и сравнить с исходным пакетом.
    Verify {
        /// review.json предыдущего прогона.
        #[arg(long, value_name = "PACK")]
        baseline: PathBuf,
        /// Новый HEAD для сравнения; базовый SHA берётся из исходного пакета.
        #[arg(long)]
        head: String,
        /// Каталог нового снимка ревью; имя по умолчанию вычисляется по SHA.
        #[arg(long = "out-dir", value_name = "DIR")]
        out_dir: Option<PathBuf>,
        /// Явно разрешить локальный Clippy; сборка может исполнять build.rs и proc-macro.
        #[arg(long)]
        run_clippy: bool,
    },

    /// Сравнить два ранее сохранённых пакета ревью без повторного анализа.
    Delta {
        /// Исходный файл review.json.
        #[arg(long, value_name = "PACK")]
        before: PathBuf,
        /// Более новый review.json.
        #[arg(long, value_name = "PACK")]
        after: PathBuf,
        /// Необязательный путь JSON-отчёта delta.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

/// Подкоманды языковой проверки.
#[derive(Debug, Subcommand)]
pub enum LanguageCommand {
    /// Найти человекочитаемые фрагменты в заданных файлах или полных версиях файлов
    /// из пакета ревью.
    #[command(group(
        clap::ArgGroup::new("scan_source")
            .required(true)
            .multiple(false)
            .args(["paths", "pack"])
    ))]
    Scan {
        /// Корень репозитория, относительно которого задаются пути.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Путь внутри корня репозитория; флаг можно повторять.
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<PathBuf>,
        /// Использовать область изменений и версии файлов после изменений из пакета ревью.
        #[arg(long, value_name = "PACK")]
        pack: Option<PathBuf>,
        /// Сохранить полный артефакт сканирования для принятия решений.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
    },

    /// Повторно проверить пути из артефакта сканирования после ручных правок.
    Check {
        /// Исходный артефакт команды `language scan`.
        #[arg(long, value_name = "SCAN")]
        scan: PathBuf,
        /// Корень репозитория, в котором перечитываются текущие файлы.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Необязательный путь для обновлённого артефакта сканирования.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },

    /// Проверить и (только с --apply) применить явно утверждённые замены.
    Apply {
        /// Артефакт решений, полученный после смысловой проверки кандидатов.
        #[arg(long, value_name = "PATH")]
        decisions: PathBuf,
        /// Корень репозитория.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Записать все замены после общей проверки предусловий.
        #[arg(long)]
        apply: bool,
    },
}
