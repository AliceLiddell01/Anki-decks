//! CLI-контракт `anki-repo`.
//!
//! Здесь только описание аргументов. Предметной логики нет: разбор
//! превращается в запросы для [`crate::run`].

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::code_review::review_queue::{
    CodeRole, DEFAULT_QUEUE_LIST_LIMIT, MAX_QUEUE_LIST_LIMIT, QueueExecutionFilter,
    QueueSurfaceFilter, ReviewPriority, StructuralRole, TextRole,
};

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

/// Предел страницы команд `code-review learning` по умолчанию.
///
/// Значение берётся у владельца страницы очереди: у `learning` нет собственного
/// «разумного» предела, а расходиться с уже принятыми границами набора он не
/// должен.
pub const DEFAULT_LEARNING_LIMIT: u64 = DEFAULT_QUEUE_LIST_LIMIT;
/// Жёсткий максимум страницы команд `code-review learning`.
pub const MAX_LEARNING_LIMIT: u64 = MAX_QUEUE_LIST_LIMIT;
/// Предел числа паттернов в отчёте `learning patterns` по умолчанию.
pub const DEFAULT_LEARNING_PATTERN_LIMIT: u64 = 20;
/// Жёсткий максимум числа паттернов в отчёте `learning patterns`.
pub const MAX_LEARNING_PATTERN_LIMIT: u64 = 200;
/// Предел числа подсказок `learning recommend` по умолчанию.
pub const DEFAULT_LEARNING_RECOMMEND_LIMIT: u64 = 50;
/// Жёсткий максимум числа подсказок `learning recommend`.
pub const MAX_LEARNING_RECOMMEND_LIMIT: u64 = 200;
/// Предел исторических случаев на одну подсказку `learning recommend` по умолчанию.
pub const DEFAULT_LEARNING_CASE_LIMIT: u64 = 3;
/// Жёсткий максимум исторических случаев на одну подсказку.
pub const MAX_LEARNING_CASE_LIMIT: u64 = 10;

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
    about = "Анализ CrowdAnki и свидетельства для ревью кода: inspect, find, stats, validate, qa, review, review-check, code-review queue list/validate/summary/group/candidate, triage, language, edit, models, create, retire, migrate-media, visual-report",
    long_about = "Анализ, проверка и ограниченные изменения одного CrowdAnki-экспорта.\n\
                  inspect, find, stats, validate, qa, review, review-check, models,\n\
                  visual-report, code-review collect/verify/delta и code-review queue\n\
                  list/validate/summary/group/candidate только читают\n\
                  зафиксированное состояние репозитория; code-review сохраняет локальные\n\
                  артефакты с точной идентичностью Git-снимка. Семантический разбор отдельно\n\
                  сохраняет и проверяет решения внешнего семантического ревьюера.\n\
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
                CodeReviewCommand::Execution { command } => match command {
                    ReviewExecutionCommand::Prepare { .. } => "code-review execution prepare",
                    ReviewExecutionCommand::Run { .. } => "code-review execution run",
                    ReviewExecutionCommand::Inspect { .. } => "code-review execution inspect",
                    ReviewExecutionCommand::Cancel { .. } => "code-review execution cancel",
                    ReviewExecutionCommand::Cleanup { .. } => "code-review execution cleanup",
                },
                CodeReviewCommand::Queue { command } => match command {
                    ReviewQueueCommand::List { .. } => "code-review queue list",
                    ReviewQueueCommand::Validate { .. } => "code-review queue validate",
                    ReviewQueueCommand::Summary { .. } => "code-review queue summary",
                    ReviewQueueCommand::Group { .. } => "code-review queue group",
                    ReviewQueueCommand::Candidate { .. } => "code-review queue candidate",
                },
                CodeReviewCommand::Triage { command } => match command {
                    SemanticTriageCommand::Init { .. } => "code-review triage init",
                    SemanticTriageCommand::Validate { .. } => "code-review triage validate",
                    SemanticTriageCommand::Summary { .. } => "code-review triage summary",
                    SemanticTriageCommand::Report { .. } => "code-review triage report",
                },
                CodeReviewCommand::Learning { command, .. } => command.command_name(),
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
        /// Workspace PR/HEAD; по умолчанию `.anki-repo/review/<PR|local>/<FULL_HEAD_SHA>/`, варианты — `snapshot-<32 hex>` внутри него.
        #[arg(long = "out-dir", value_name = "DIR")]
        out_dir: Option<PathBuf>,
        /// Явно разрешить локальный Clippy; сборка может исполнять build.rs и proc-macro.
        #[arg(long)]
        run_clippy: bool,
        /// Номер существующего PR; при ревью PR передавайте его явно.
        #[arg(long, value_name = "NUMBER")]
        pr_number: Option<String>,
    },

    /// Повторно собрать свидетельства для нового HEAD и сравнить с исходным пакетом.
    Verify {
        /// review.json предыдущего прогона.
        #[arg(long, value_name = "PACK")]
        baseline: PathBuf,
        /// Новый HEAD для сравнения; базовый SHA берётся из исходного пакета.
        #[arg(long)]
        head: String,
        /// Workspace нового HEAD: `.anki-repo/review/<PR|local>/<FULL_HEAD_SHA>[/snapshot-<32 hex>]/`; namespace наследуется из исходного пакета.
        #[arg(long = "out-dir", value_name = "DIR")]
        out_dir: Option<PathBuf>,
        /// Явно разрешить локальный Clippy; сборка может исполнять build.rs и proc-macro.
        #[arg(long)]
        run_clippy: bool,
        /// Проверить namespace PR baseline; при отсутствии параметра namespace наследуется.
        #[arg(long, value_name = "NUMBER")]
        pr_number: Option<String>,
    },

    /// Сравнить два ранее сохранённых пакета ревью без повторного анализа.
    Delta {
        /// Исходный файл review.json.
        #[arg(long, value_name = "PACK")]
        before: PathBuf,
        /// Более новый review.json.
        #[arg(long, value_name = "PACK")]
        after: PathBuf,
        /// Только delta.json в workspace `.anki-repo/review/<PR|local>/<FULL_HEAD_SHA>[/snapshot-<32 hex>]/` пакета --after.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },

    /// Проверить, просматривать и фильтровать детерминированную структурную очередь.
    Queue {
        #[command(subcommand)]
        command: ReviewQueueCommand,
    },

    /// Изолированное исполнение проверок на закреплённом снимке ревью.
    Execution {
        #[command(subcommand)]
        command: ReviewExecutionCommand,
    },

    /// Создать, проверить или представить решения семантического разбора пакета ревью.
    Triage {
        #[command(subcommand)]
        command: SemanticTriageCommand,
    },

    /// Локальная адаптивная память ревью: импорт истории, статистика, поиск, рекомендации, feedback и перенос.
    ///
    /// Learning — добавочный слой, а не источник семантической истины: он не
    /// меняет `review.json`, `review-queue.json` и `semantic-triage.json` и не
    /// создаёт семантических решений. Отсутствие истории не мешает обычному
    /// ревью: `collect`, `verify`, `queue validate`, `triage validate` и
    /// `execution` работают без базы learning и не создают её.
    Learning {
        /// Путь к локальной базе learning; по умолчанию .anki-repo/learning/state.sqlite от корня репозитория.
        #[arg(
            long = "db",
            visible_alias = "database",
            global = true,
            value_name = "PATH"
        )]
        db: Option<PathBuf>,
        #[command(subcommand)]
        command: LearningCommand,
    },
}

/// Операции чтения над очередью, привязанной к точным байтам `review.json`.
#[derive(Debug, Subcommand)]
pub enum ReviewQueueCommand {
    /// Вывести страницу элементов очереди, отобранных по приоритету и классификации.
    #[command(
        long_about = "Отобрать элементы структурной очереди и показать страницу результатов.\n\
                      PACK — исходный review.json; QUEUE — review-queue.json для того же пакета.\n\
                      Значения фильтров классификации задаются точно, в формате snake_case.\n\
                      --surface: production, tests, ci, config, docs, generated, data,\n\
                      agent_context, dependencies, unknown. --execution: production, tests,\n\
                      unknown. --role: text, error_path, security, suppression, test_change,\n\
                      dependency, configuration, generated, repository_context, path,\n\
                      development_reference, unknown. --text-role: human_comment,\n\
                      human_documentation, human_log, human_diagnostic, human_help, human_ui,\n\
                      technical_identifier, machine_contract, external_literal, path, url,\n\
                      cli_flag, code_example, test_fixture, unknown. --code-role: runtime,\n\
                      runtime_boundary, test_setup, test_assertion, test_helper, unknown.\n\
                      --priority принимает high, normal или low."
    )]
    List {
        /// Путь к исходному пакету свидетельств `review.json`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// Путь к структурной очереди `review-queue.json` для указанного пакета.
        #[arg(long, value_name = "QUEUE")]
        queue: PathBuf,
        /// Число элементов на странице (от 1 до 200).
        #[arg(
            long,
            default_value_t = DEFAULT_QUEUE_LIST_LIMIT,
            value_parser = clap::value_parser!(u64).range(1..=MAX_QUEUE_LIST_LIMIT),
        )]
        limit: u64,
        /// Смещение от начала отсортированной очереди.
        #[arg(long, default_value_t = 0)]
        offset: u64,
        /// Оставить элементы с приоритетом high, normal или low.
        #[arg(long, value_name = "PRIORITY")]
        priority: Option<ReviewPriority>,
        /// Оставить только элементы с неизвестной структурной классификацией.
        #[arg(long)]
        unknown: bool,
        /// Имя детектора, сформировавшего исходного кандидата.
        #[arg(long, value_name = "DETECTOR")]
        detector: Option<String>,
        /// Каноническая поверхность файла: например, production, tests или docs.
        #[arg(long, value_name = "SURFACE")]
        surface: Option<QueueSurfaceFilter>,
        /// Исполняемая поверхность: production или tests.
        #[arg(long, value_name = "EXECUTION")]
        execution: Option<QueueExecutionFilter>,
        /// Каноническая структурная роль, например security или error_path.
        #[arg(long, value_name = "ROLE")]
        role: Option<StructuralRole>,
        /// Каноническая роль текста, например human_documentation или cli_flag.
        #[arg(long = "text-role", value_name = "TEXT_ROLE")]
        text_role: Option<TextRole>,
        /// Каноническая роль кода: runtime, runtime_boundary или роль теста.
        #[arg(long = "code-role", value_name = "CODE_ROLE")]
        code_role: Option<CodeRole>,
        /// Пропустить восстановление Git-снимков/AST и явно показать статус structure_only.
        #[arg(long)]
        structure_only: bool,
    },

    /// Проверить источник, полноту покрытия и структуру артефакта очереди.
    Validate {
        /// Путь к исходному пакету свидетельств `review.json`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// Путь к структурной очереди `review-queue.json` для указанного пакета.
        #[arg(long, value_name = "QUEUE")]
        queue: PathBuf,
        /// Проверить только JSON-структуру и digest, не подтверждая классификацию на основе синтаксиса.
        #[arg(long)]
        structure_only: bool,
    },

    /// Показать агрегированную сводку очереди.
    Summary {
        /// Путь к исходному пакету свидетельств `review.json`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// Путь к структурной очереди `review-queue.json` для указанного пакета.
        #[arg(long, value_name = "QUEUE")]
        queue: PathBuf,
        /// Проверить только JSON-структуру и digest, не подтверждая классификацию на основе синтаксиса.
        #[arg(long)]
        structure_only: bool,
    },

    /// Раскрыть одну группу, включая полное множество исходных candidate IDs.
    Group {
        /// Путь к исходному пакету свидетельств `review.json`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// Путь к структурной очереди `review-queue.json` для указанного пакета.
        #[arg(long, value_name = "QUEUE")]
        queue: PathBuf,
        /// Точный ID группы из `review-queue.json`.
        #[arg(long, value_name = "ID")]
        id: String,
        /// Проверить только JSON-структуру и digest, не подтверждая классификацию на основе синтаксиса.
        #[arg(long)]
        structure_only: bool,
    },

    /// Найти candidate и показать его исходное evidence, классификацию и unit.
    Candidate {
        /// Путь к исходному пакету свидетельств `review.json`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// Путь к структурной очереди `review-queue.json` для указанного пакета.
        #[arg(long, value_name = "QUEUE")]
        queue: PathBuf,
        /// Точный ID candidate из исходного `review.json`.
        #[arg(long, value_name = "ID")]
        id: String,
        /// Проверить только JSON-структуру и digest, не подтверждая классификацию на основе синтаксиса.
        #[arg(long)]
        structure_only: bool,
    },
}

/// Управление изолированным заданием без зависимости от системы агентов.
#[derive(Debug, Subcommand)]
pub enum ReviewExecutionCommand {
    /// Подготовить отдельное задание и detached worktree, не запуская проектный код.
    Prepare {
        /// Канонический review.json из `.anki-repo/review/<PR|local>/<FULL_HEAD_SHA>[/snapshot-<32 hex>]/`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// Режим изолированной проверки или эксперимента с отдельной копией исходников.
        #[arg(long, value_enum)]
        mode: crate::code_review::execution::ExecutionMode,
        /// Проверить пространство PR канонического --pack; без параметра пространство наследуется из пути.
        #[arg(long, value_name = "NUMBER")]
        pr_number: Option<String>,
        /// Именованное направление ревью, например tests, runtime, docs или security.
        #[arg(long, value_name = "SCOPE")]
        scope: String,
    },

    /// Запустить явную argv-команду в рабочем каталоге задания.
    Run {
        /// Каталог ранее подготовленного задания.
        job: PathBuf,
        /// Обязательный срок выполнения команды в секундах.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86_400))]
        timeout_seconds: u64,
        /// Рабочий каталог относительно корня закреплённого worktree.
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        /// Общий репозиторный предел одновременных заданий; значения должны совпадать, пока есть активные задания.
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u64).range(1..=64))]
        max_parallel_jobs: u64,
        /// Явно передать переменную окружения; при наличии параметров включается политика explicit.
        #[arg(long = "env", value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
        environment: Vec<String>,
        /// Исполняемый файл и аргументы после `--`; shell не запускается.
        #[arg(last = true, required = true, value_name = "ARGV")]
        argv: Vec<String>,
    },

    /// Прочитать сохранённое состояние задания и результат без изменения файлов.
    Inspect {
        /// Каталог задания.
        job: PathBuf,
    },

    /// Запросить отмену активного задания.
    Cancel {
        /// Каталог задания.
        job: PathBuf,
    },

    /// Очистить только собственный worktree и временные каталоги задания.
    Cleanup {
        /// Каталог задания.
        job: PathBuf,
        /// Подтвердить отсутствие живых потомков после самостоятельной проверки оператором.
        /// Требуется для очистки задания, в котором процесс мог запускаться.
        #[arg(long)]
        confirm_no_live_descendants: bool,
    },
}

/// Операции над отдельным версионируемым документом семантического разбора.
#[derive(Debug, Subcommand)]
pub enum SemanticTriageCommand {
    /// Инициализировать документ с явным списком нерассмотренных кандидатов.
    ///
    /// Требует запуска из Git-репозитория для проверки безопасного пути записи.
    Init {
        /// Неизменяемый пакет свидетельств `review.json`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// Только semantic-triage.input.json в workspace
        /// `.anki-repo/review/<PR|local>/<FULL_HEAD_SHA>[/snapshot-<32 hex>]/` пакета --pack.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
    },

    /// Проверить заполненный разбор по точному исходному пакету `review.json`.
    ///
    /// Без `--canonical-out` команда не зависит от Git-каталога. Запись
    /// канонического JSON требует запуска из Git-репозитория.
    Validate {
        /// Неизменяемый пакет свидетельств `review.json`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// Заполненный JSON-документ семантического разбора.
        #[arg(long, value_name = "PATH")]
        triage: PathBuf,
        /// Только semantic-triage.json в workspace
        /// `.anki-repo/review/<PR|local>/<FULL_HEAD_SHA>[/snapshot-<32 hex>]/` пакета --pack.
        /// Валидный собственный документ можно атомарно обновить.
        #[arg(long = "canonical-out", value_name = "PATH")]
        canonical_out: Option<PathBuf>,
    },

    /// Показать компактную воспроизводимую сводку проверенного разбора.
    Summary {
        /// Неизменяемый пакет свидетельств `review.json`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// JSON-документ семантического разбора.
        #[arg(long, value_name = "PATH")]
        triage: PathBuf,
    },

    /// Создать компактный Markdown-отчёт по проверенному каноническому разбору.
    ///
    /// Требует запуска из Git-репозитория для проверки безопасного пути записи.
    Report {
        /// Неизменяемый пакет свидетельств `review.json`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// JSON-документ семантического разбора.
        #[arg(long, value_name = "PATH")]
        triage: PathBuf,
        /// Только review-report.md в workspace
        /// `.anki-repo/review/<PR|local>/<FULL_HEAD_SHA>[/snapshot-<32 hex>]/` пакета --pack.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
    },
}

/// Операции локальной адаптивной памяти ревью.
///
/// Ни одна операция не создаёт семантических решений, не меняет исходные
/// артефакты ревью и не обращается в сеть. Читающие операции не создают базу:
/// отсутствие истории — это состояние, а не ошибка обычного ревью.
#[derive(Debug, Subcommand)]
pub enum LearningCommand {
    /// Импортировать проверенную завершённую историю одного ревью.
    #[command(
        visible_alias = "ingest",
        long_about = "Проверяет review.json, review-queue.json и необязательный semantic-triage.json \
                      теми же валидаторами, что обычное ревью, и сохраняет историю локально.\n\
                      Повторный импорт того же точного набора свидетельств не удваивает статистику; \
                      другие байты при той же идентичности снимка оформляются как аудируемая ревизия. \
                      --structure-only — явно маркированный ослабленный режим: запись помечается \
                      карантином и исключается из обучения."
    )]
    Import {
        /// Путь к исходному пакету свидетельств `review.json`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// Путь к структурной очереди `review-queue.json` для того же пакета.
        #[arg(long, value_name = "QUEUE")]
        queue: PathBuf,
        /// Проверенный документ семантического разбора `semantic-triage.json`.
        #[arg(long, value_name = "PATH")]
        triage: Option<PathBuf>,
        /// Результат завершённого изолированного задания; допускается не более одного файла.
        #[arg(long = "execution", value_name = "PATH", action = clap::ArgAction::Append)]
        execution: Vec<PathBuf>,
        /// Ослабленный импорт по структурной проверке: запись помечается карантином.
        #[arg(long)]
        structure_only: bool,
        /// Явный вариант источника: `root` либо `snapshot-<32 hex>`.
        #[arg(long, default_value = "root", value_name = "VARIANT")]
        variant: String,
        /// Метка каталога ревью для аудита; путь клона в историю не попадает.
        #[arg(long, value_name = "LABEL")]
        label: Option<String>,
    },

    /// Показать состояние локальной истории learning, ничего не создавая.
    Status,

    /// Проверить схему, целостность и доступность поиска в локальной истории.
    ///
    /// Отсутствующая база — это `not_found`, более новая схема —
    /// `learning_schema_unsupported`, повреждённый файл — `learning_corrupt`.
    Validate,

    /// Показать агрегированную статистику накопленной истории.
    Stats {
        /// Включать карантинные записи (по умолчанию исключены).
        #[arg(long)]
        include_quarantine: bool,
        /// Предел числа записей истории в выводе.
        #[arg(
            long,
            default_value_t = DEFAULT_LEARNING_LIMIT,
            value_parser = clap::value_parser!(u64).range(1..=MAX_LEARNING_LIMIT),
        )]
        limit: u64,
    },

    /// Показать объяснимые структурные паттерны с фильтрами и лимитом.
    Patterns {
        /// Ограничение по детектору исходного сигнала.
        #[arg(long, value_name = "DETECTOR")]
        detector: Option<String>,
        /// Ограничение по структурной роли.
        #[arg(long, value_name = "ROLE")]
        role: Option<StructuralRole>,
        /// Ограничение по роли кода.
        #[arg(long = "code-role", value_name = "CODE_ROLE")]
        code_role: Option<CodeRole>,
        /// Ограничение по происхождению сигнала.
        #[arg(long, value_name = "ORIGIN")]
        origin: Option<String>,
        /// Включать карантинные записи (по умолчанию исключены).
        #[arg(long)]
        include_quarantine: bool,
        /// Предел числа паттернов в отчёте.
        #[arg(
            long,
            default_value_t = DEFAULT_LEARNING_PATTERN_LIMIT,
            value_parser = clap::value_parser!(u64).range(1..=MAX_LEARNING_PATTERN_LIMIT),
        )]
        limit: u64,
        /// Отказать с `insufficient_evidence`, если ни один паттерн не подтверждён поддержкой.
        #[arg(long)]
        require_supported: bool,
    },

    /// Найти аналогичные исторические случаи локальным поиском.
    #[command(
        visible_alias = "similar",
        long_about = "Ищет исторические случаи структурными фильтрами и локальной текстовой подстрокой.\n\
                      FTS5 используется, если он собран и проиндексирован; иначе выполняется \
                      воспроизводимый подстрочный поиск по тому же тексту. Карантинные записи \
                      пониженного доверия по умолчанию исключены и включаются только явным \
                      --include-quarantine: найденная аналогия — вспомогательный контекст, а не \
                      основание, поэтому доверие возвращённого случая видно в выводе. Вывод всегда \
                      ограничен страницей и не печатает полные списки candidate_ids."
    )]
    Search {
        /// Текстовая подстрока (не менее двух символов).
        #[arg(long, value_name = "TEXT")]
        text: Option<String>,
        /// Ограничение по детектору исходного сигнала.
        #[arg(long, value_name = "DETECTOR")]
        detector: Option<String>,
        /// Ограничение по исполняемой поверхности: production, tests или unknown.
        #[arg(long, value_name = "SURFACE")]
        surface: Option<QueueExecutionFilter>,
        /// Ограничение по происхождению сигнала.
        #[arg(long, value_name = "ORIGIN")]
        origin: Option<String>,
        /// Ограничение по структурной роли.
        #[arg(long, value_name = "ROLE")]
        role: Option<StructuralRole>,
        /// Ограничение по роли кода.
        #[arg(long = "code-role", value_name = "CODE_ROLE")]
        code_role: Option<CodeRole>,
        /// Ограничение по решению ревьюера.
        #[arg(long, value_name = "DISPOSITION")]
        disposition: Option<LearningDispositionArg>,
        /// Ограничение по происхождению замечания.
        #[arg(long, value_name = "PROVENANCE")]
        provenance: Option<String>,
        /// Ограничение по серьёзности замечания.
        #[arg(long, value_name = "SEVERITY")]
        severity: Option<String>,
        /// Ограничение по `repository_id`.
        #[arg(long = "repository", value_name = "ID")]
        repository: Option<String>,
        /// Включать карантинные записи (по умолчанию исключены).
        #[arg(long)]
        include_quarantine: bool,
        /// Предел числа случаев на странице.
        #[arg(
            long,
            default_value_t = DEFAULT_LEARNING_LIMIT,
            value_parser = clap::value_parser!(u64).range(1..=MAX_LEARNING_LIMIT),
        )]
        limit: u64,
        /// Смещение от начала отсортированных совпадений.
        #[arg(long, default_value_t = 0)]
        offset: u64,
    },

    /// Построить версионируемый документ рекомендаций для конкретного снимка.
    #[command(
        long_about = "Строит производный recommendations artifact по текущему review.json + \
                      review-queue.json и явно выбранной версии истории и политики.\n\
                      Baseline priority и review-queue.json не изменяются; ни одна единица не \
                      исключается из обязательного просмотра. Без доступной истории документ \
                      явно помечается как режим без learning, а не выдаёт прошлую статистику за \
                      доказанную безопасность. --out публикует только recommendations.json в \
                      рабочей области пакета."
    )]
    Recommend {
        /// Путь к исходному пакету свидетельств `review.json`.
        #[arg(long, value_name = "PACK")]
        pack: PathBuf,
        /// Путь к структурной очереди `review-queue.json` для того же пакета.
        #[arg(long, value_name = "QUEUE")]
        queue: PathBuf,
        /// Проверенный документ семантического разбора `semantic-triage.json`.
        #[arg(long, value_name = "PATH")]
        triage: Option<PathBuf>,
        /// Ослабленная структурная проверка входа: подсказки не опираются на историю.
        #[arg(long)]
        structure_only: bool,
        /// Явный вариант источника: `root` либо `snapshot-<32 hex>`.
        #[arg(long, default_value = "root", value_name = "VARIANT")]
        variant: String,
        /// Метка каталога ревью для аудита.
        #[arg(long, value_name = "LABEL")]
        label: Option<String>,
        /// Предел числа подсказок в документе.
        #[arg(
            long,
            default_value_t = DEFAULT_LEARNING_RECOMMEND_LIMIT,
            value_parser = clap::value_parser!(u64).range(1..=MAX_LEARNING_RECOMMEND_LIMIT),
        )]
        limit: u64,
        /// Предел исторических случаев на одну подсказку.
        #[arg(
            long = "case-limit",
            default_value_t = DEFAULT_LEARNING_CASE_LIMIT,
            value_parser = clap::value_parser!(u64).range(1..=MAX_LEARNING_CASE_LIMIT),
        )]
        case_limit: u64,
        /// Ожидаемая ревизия истории; несовпадение — `source_changed`.
        #[arg(long = "history-revision", value_name = "N")]
        history_revision: Option<u64>,
        /// Ожидаемая версия политики learning; несовпадение — `learning_schema_unsupported`.
        #[arg(long = "policy-version", value_name = "N")]
        policy_version: Option<u32>,
        /// Штатный режим без истории: рекомендации не опираются на learning.
        #[arg(long = "without-learning")]
        without_learning: bool,
        /// Требовать непустую проверенную историю; иначе — различимый отказ.
        #[arg(long = "require-history")]
        require_history: bool,
        /// Только recommendations.json в рабочей области `.anki-repo/review/<PR|local>/<HEAD>[/snapshot-<32 hex>]/` пакета --pack.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },

    /// Обратная связь: оценка полезности рекомендации или аудируемая правка исхода.
    Feedback {
        #[command(subcommand)]
        command: LearningFeedbackCommand,
    },

    /// Собрать переносимый архив проверенной истории.
    Export {
        /// Путь архива learning; существующий чужой файл не перезаписывается.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
    },

    /// Создать транзакционный снимок базы (`VACUUM INTO`).
    Backup {
        /// Путь файла backup; существующий файл не перезаписывается.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
    },

    /// Восстановить историю из переносимого архива с повторной проверкой digest.
    Restore {
        /// Архив, созданный `code-review learning export`.
        #[arg(long = "archive", visible_alias = "from", value_name = "PATH")]
        archive: PathBuf,
    },

    /// Удалить одну запись истории вместе с её производными данными.
    #[command(
        long_about = "Удаляет один review run и его дочерние записи из локальной SQLite-транзакции.\n\
                      Операция необратима и требует явного --confirm. Review artifacts, исходники \
                      и утверждённые файлы политики на диске не удаляются."
    )]
    Forget {
        /// Идентификатор записи из `learning stats`.
        #[arg(long = "review-id", value_name = "ID")]
        review_id: String,
        /// Подтвердить необратимое удаление указанного review run.
        #[arg(long)]
        confirm: bool,
    },

    /// Предложить, показать и утвердить версионируемую политику.
    Policy {
        #[command(subcommand)]
        command: LearningPolicyCommand,
    },
}

impl LearningCommand {
    /// Стабильное имя подкоманды для JSON envelope.
    #[must_use]
    pub const fn command_name(&self) -> &'static str {
        match self {
            Self::Import { .. } => "code-review learning import",
            Self::Status => "code-review learning status",
            Self::Validate => "code-review learning validate",
            Self::Stats { .. } => "code-review learning stats",
            Self::Patterns { .. } => "code-review learning patterns",
            Self::Search { .. } => "code-review learning search",
            Self::Recommend { .. } => "code-review learning recommend",
            Self::Feedback { command } => match command {
                LearningFeedbackCommand::Record { .. } => "code-review learning feedback record",
                LearningFeedbackCommand::List { .. } => "code-review learning feedback list",
                LearningFeedbackCommand::Show { .. } => "code-review learning feedback show",
            },
            Self::Export { .. } => "code-review learning export",
            Self::Backup { .. } => "code-review learning backup",
            Self::Restore { .. } => "code-review learning restore",
            Self::Forget { .. } => "code-review learning forget",
            Self::Policy { command } => match command {
                LearningPolicyCommand::Propose { .. } => "code-review learning policy propose",
                LearningPolicyCommand::List { .. } => "code-review learning policy list",
                LearningPolicyCommand::Show { .. } => "code-review learning policy show",
                LearningPolicyCommand::Approve { .. } => "code-review learning policy approve",
            },
        }
    }
}

/// Обратная связь по конкретному случаю истории.
#[derive(Debug, Subcommand)]
pub enum LearningFeedbackCommand {
    /// Записать событие обратной связи.
    #[command(visible_alias = "revise")]
    Record {
        /// Запись ревью, к которой относится утверждение.
        #[arg(long = "review-id", value_name = "ID")]
        review_id: String,
        /// Единица очереди, к которой относится утверждение.
        #[arg(long = "unit-id", value_name = "ID")]
        unit_id: String,
        /// Кандидат, если утверждение относится к нему.
        #[arg(long = "candidate-id", value_name = "ID")]
        candidate_id: Option<String>,
        /// Вид утверждения: оценка полезности либо содержательная правка исхода.
        #[arg(long, value_enum)]
        kind: LearningFeedbackKindArg,
        /// Действие по отношению к прежним утверждениям.
        #[arg(long, value_enum, default_value_t = LearningFeedbackActionArg::Append)]
        action: LearningFeedbackActionArg,
        /// Новый действующий исход для содержательной правки.
        #[arg(long, value_enum, value_name = "DISPOSITION")]
        disposition: Option<LearningDispositionArg>,
        /// Оценка полезности рекомендации.
        #[arg(long, value_enum, value_name = "USEFULNESS")]
        usefulness: Option<LearningUsefulnessArg>,
        /// Заменяемое или отзываемое утверждение; обязательно для `supersede` и `retract`.
        #[arg(long = "supersedes-event-id", value_name = "ID")]
        supersedes_event_id: Option<String>,
        /// Объяснение утверждения.
        #[arg(long, value_name = "TEXT")]
        explanation: String,
        /// Источник утверждения: reviewer, operator, automation.
        #[arg(long, default_value = "reviewer", value_name = "SOURCE")]
        provenance: String,
        /// Явный идентификатор события; без него идентификатор выводится из содержания.
        #[arg(long = "event-id", value_name = "ID")]
        event_id: Option<String>,
    },

    /// Показать аудит утверждений по одному случаю.
    List {
        /// Запись ревью.
        #[arg(long = "review-id", value_name = "ID")]
        review_id: String,
        /// Единица очереди.
        #[arg(long = "unit-id", value_name = "ID")]
        unit_id: String,
    },

    /// Показать одно сохранённое событие обратной связи.
    Show {
        /// Идентификатор события.
        #[arg(long = "event-id", value_name = "ID")]
        event_id: String,
    },
}

/// Жизненный цикл предложенного постоянного правила.
#[derive(Debug, Subcommand)]
pub enum LearningPolicyCommand {
    /// Создать объяснимое предложение постоянного правила.
    ///
    /// Предложение никогда не применяется автоматически: оно перечисляет
    /// подтверждающие и противоречащие случаи и требует явного утверждения
    /// человеком через обычный контроль изменений.
    Propose {
        /// Точная подпись ключа признаков из `learning patterns`.
        #[arg(long, value_name = "SIGNATURE")]
        signature: String,
        /// Идентификатор предлагаемого правила.
        #[arg(long = "rule-id", value_name = "ID")]
        rule_id: String,
    },

    /// Перечислить сохранённые предложения политики.
    List {
        /// Предел числа предложений в выводе.
        #[arg(
            long,
            default_value_t = DEFAULT_LEARNING_LIMIT,
            value_parser = clap::value_parser!(u64).range(1..=MAX_LEARNING_LIMIT),
        )]
        limit: u64,
    },

    /// Показать сохранённое предложение политики.
    Show {
        /// Идентификатор предложения.
        #[arg(long = "id", value_name = "ID")]
        id: String,
    },

    /// Материализовать утверждённый артефакт политики для ручного коммита.
    ///
    /// Команда не меняет базу как владельца политики и не выполняет
    /// автопромоцию: утверждённый JSON пишется по явному пути и попадает в Git
    /// только решением человека.
    Approve {
        /// Идентификатор утверждаемого предложения.
        #[arg(long = "id", value_name = "ID")]
        id: String,
        /// Путь утверждённого артефакта политики внутри репозитория.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
        /// Пояснение к утверждению, сохраняемое в артефакте.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
    },
}

/// Вид утверждения обратной связи на уровне CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LearningFeedbackKindArg {
    /// Оценка полезности сгенерированной рекомендации.
    Usefulness,
    /// Содержательная правка исхода семантического рассмотрения.
    SemanticOutcomeRevision,
}

impl LearningFeedbackKindArg {
    /// Значение для истории learning.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usefulness => "recommendation_usefulness",
            Self::SemanticOutcomeRevision => "semantic_outcome_revision",
        }
    }
}

/// Действие обратной связи на уровне CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LearningFeedbackActionArg {
    /// Новое утверждение; при конфликте запись отклоняется.
    Append,
    /// Отзыв ранее сохранённого утверждения.
    Retract,
    /// Явная замена ранее сохранённого утверждения.
    Supersede,
}

/// Содержательное решение ревьюера на уровне CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LearningDispositionArg {
    /// Подтверждённый дефект.
    Confirmed,
    /// Допустимое поведение.
    Acceptable,
    /// Ложное срабатывание.
    FalsePositive,
    /// Неприменимое замечание.
    NotApplicable,
    /// Неопределённый исход.
    Uncertain,
}

impl LearningDispositionArg {
    /// Значение, принятое историей learning.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Acceptable => "acceptable",
            Self::FalsePositive => "false_positive",
            Self::NotApplicable => "not_applicable",
            Self::Uncertain => "uncertain",
        }
    }
}

/// Оценка полезности рекомендации на уровне CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LearningUsefulnessArg {
    /// Подсказка помогла.
    Useful,
    /// Подсказка не помогла.
    NotUseful,
    /// Подсказка помогла частично.
    PartiallyUseful,
}

impl LearningUsefulnessArg {
    /// Значение, принятое историей learning.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Useful => "useful",
            Self::NotUseful => "not_useful",
            Self::PartiallyUseful => "partially_useful",
        }
    }
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

#[cfg(test)]
mod execution_cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn execution_cleanup_descendant_confirmation_is_explicit() {
        for (extra, expected) in [(None, false), (Some("--confirm-no-live-descendants"), true)] {
            let mut argv = vec![
                "anki-repo",
                "code-review",
                "execution",
                "cleanup",
                "job-fixture",
            ];
            if let Some(flag) = extra {
                argv.push(flag);
            }
            let cli = Cli::try_parse_from(argv).unwrap();
            let Command::CodeReview {
                command:
                    CodeReviewCommand::Execution {
                        command:
                            ReviewExecutionCommand::Cleanup {
                                confirm_no_live_descendants,
                                ..
                            },
                    },
            } = cli.command
            else {
                panic!("ожидалась команда очистки задания")
            };
            assert_eq!(confirm_no_live_descendants, expected);
        }
    }
}
