//! Синтаксис и статический рендер Anki-шаблонов — единственный владелец этого
//! понятия в toolkit'е.
//!
//! Это не рендер Anki «как в приложении». Цель скромнее и требует большей
//! честности: дать внешнему агенту-ревьюеру превью карточки, собранное из
//! фактических значений полей, и при этом явно пометить всё, что статически
//! вычислить нельзя. Молчаливая «примерно похожая» отрисовка здесь опаснее
//! отказа: ревьюер принял бы за факт то, что мы домыслили. Поэтому у каждой
//! неподдержанной конструкции есть конкретная причина, а её сырой текст остаётся
//! видимым в `html`.
//!
//! Границы ответственности:
//!
//! * `{{Field}}`, `{{#Field}}…{{/Field}}`, `{{^Field}}…{{/Field}}` и статически
//!   вычислимые special fields (`Tags`, `Type`, `Deck`, `Subdeck`, `Card`,
//!   `FrontSide`) считаются точно;
//! * фильтры (`cloze`, `type`, `tts`, `furigana`, …), рантайм-поля (`CardID`,
//!   `CardFlag`), cloze-модели целиком, структурно битые шаблоны и `<script>`
//!   объявляются unsupported с причиной;
//! * отдельной категории «неизвестный special-looking токен» здесь нет, и это
//!   решение, а не пропуск. Пространство special fields Anki закрыто: `Tags`,
//!   `Type`, `Deck`, `Subdeck`, `Card`, `CardID`, `CardFlag`, `FrontSide`.
//!   Любой другой идентификатор — имя поля модели, потому что `Front`, `Back`,
//!   `Word` — самые частые настоящие имена полей, и эвристика «похоже на
//!   special» пометила бы unsupported именно их. Невычислимы статически ровно
//!   `CardID` и `CardFlag`: они получают unsupported с конкретной причиной, а
//!   неопределённое имя — issue «поле не найдено в модели» без смены статуса;
//! * подстановка одноуровневая: `{{`/`}}` внутри значения поля — это текст, а не
//!   синтаксис, и повторному разбору значение не подлежит. Ровно так ведёт себя
//!   Anki, и обратное поведение позволило бы значению поля подменить шаблон.
//!
//! Генерация карточки повторяет подтверждённую семантику Anki: карточка
//! создаётся тогда и только тогда, когда фронт-шаблон рендерится в непустой
//! результат (`rslib/src/notetype/cardgen.rs`). Поле `req` в этом решении не
//! участвует — это legacy-кэш, и опираться на него нельзя. Cloze-модель не
//! порождает здесь ни одной карточки: cloze-разметку статически не вычислить, и
//! `generated` для неё всегда `false`.

use std::collections::BTreeMap;

/// Вид модели заметок с точки зрения рендера карточек.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// Обычная модель: один шаблон — одна карточка.
    Standard,
    /// Cloze-модель (`type == 1`): один шаблон на все карточки, разметка cloze.
    Cloze,
}

/// Всё, что нужно для статического рендера одной карточки.
///
/// Значения полей передаются сырыми: рендер ничего не нормализует и не
/// экранирует, потому что превью обязано показывать то же, что покажет Anki.
#[derive(Debug, Clone)]
pub struct TemplateContext<'a> {
    /// Имя поля → сырое значение.
    pub fields: &'a BTreeMap<String, String>,
    /// Теги заметки.
    pub tags: &'a [String],
    /// Полное имя колоды; может содержать `::`.
    pub deck_path: &'a str,
    /// Имя модели заметок.
    pub notetype_name: &'a str,
    /// Имя шаблона.
    pub template_name: &'a str,
    /// Вид модели.
    pub model_kind: ModelKind,
    /// Уже отрендеренная лицевая сторона — то, что подставит `{{FrontSide}}`
    /// на обратной стороне. Обычно сюда передаётся `html` из
    /// [`CardPreview::front`] того же вызова [`render_card`].
    pub front_side: &'a str,
}

/// Итог статического рендера одной стороны карточки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderStatus {
    /// Всё, что было в шаблоне, вычислено статически.
    Rendered,
    /// Что-то вычислить статически нельзя; подробности — в `issues`.
    Unsupported,
}

/// Конкретная причина, по которой конструкция не отрисована.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderIssue {
    /// Сырая конструкция, к которой относится причина.
    pub construct: String,
    /// Конкретное объяснение, а не «примерно похоже».
    pub reason: String,
}

/// Одна сторона карточки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSide {
    /// HTML превью: результат рендера шаблона как есть.
    ///
    /// Обёртки карточки здесь нет: классы `card card{N}` зависят от `ord`
    /// шаблона, а `<style>` с CSS — от модели заметок, поэтому и то, и другое
    /// добавляет вызывающий код. Здесь лежит ровно то, что дал шаблон.
    pub html: String,
    /// Статус: `Unsupported`, если хотя бы одна причина попала в `issues`.
    pub status: RenderStatus,
    /// Причины отказа в порядке появления.
    pub issues: Vec<RenderIssue>,
}

/// Превью карточки целиком.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardPreview {
    /// Лицевая сторона из `qfmt`.
    pub front: RenderedSide,
    /// Обратная сторона из `afmt`.
    pub back: RenderedSide,
    /// Фронт-шаблон рендерится в непустой результат → Anki сгенерирует
    /// карточку. Для [`ModelKind::Cloze`] всегда `false`.
    pub generated: bool,
}

/// Рендерит обе стороны карточки статически.
///
/// Функция детерминирована и не паникует ни на каком входе: битый шаблон
/// приводит к [`RenderStatus::Unsupported`] с причиной, а не к панике. Тело
/// секции, чьё условие статически невычислимо, показывается как есть — иначе
/// ревьюер не увидел бы то, о чём именно идёт речь.
#[must_use]
pub fn render_card(qfmt: &str, afmt: &str, ctx: &TemplateContext<'_>) -> CardPreview {
    let front = render_side(qfmt, ctx, Side::Front);
    let back = render_side(afmt, ctx, Side::Back);

    let generated = match ctx.model_kind {
        ModelKind::Standard => !is_field_empty(&front.html),
        ModelKind::Cloze => false,
    };

    CardPreview {
        front,
        back,
        generated,
    }
}

/// Вид конструкции шаблона, найденной [`scan_constructs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstructKind {
    /// Подстановка значения поля.
    Field,
    /// Открывающая условная секция `{{#Field}}`.
    SectionOpen,
    /// Открывающая негативная секция `{{^Field}}`.
    SectionNegated,
    /// Закрывающая секция `{{/Field}}`.
    SectionClose,
    /// Статически вычислимое special field Anki.
    Special,
    /// Конструкция, которую превью не поддержит.
    Unsupported,
}

/// Одна найденная конструкция шаблона.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Construct {
    /// Сырой текст конструкции вместе со скобками.
    pub raw: String,
    /// Имя поля, special field или фильтра, если оно есть.
    pub name: String,
    /// Вид конструкции.
    pub kind: ConstructKind,
    /// Причина отказа; `Some` только для [`ConstructKind::Unsupported`].
    pub reason: Option<String>,
}

/// Разбирает шаблон и перечисляет все конструкции в порядке появления.
///
/// Порядок — документный: открывающая секция идёт раньше собственного тела,
/// поэтому вложение видно по последовательности без отдельного дерева.
/// Функция ничего не вычисляет: у неё нет значений полей, поэтому признак
/// [`ConstructKind::Field`] здесь означает «похоже на подстановку», а не
/// «поле существует в модели». Признак [`ConstructKind::Special`] точный —
/// реестр special fields Anki закрыт.
#[must_use]
pub fn scan_constructs(template: &str, model_kind: ModelKind) -> Vec<Construct> {
    let mut constructs: Vec<Construct> = Vec::new();
    scan_region(template, model_kind, &mut constructs, 0);
    constructs
}

/// Пустое ли значение поля по правилу Anki.
///
/// Правило: пустая строка или строка, состоящая только из пробельных символов,
/// тегов `<br>` и `<div>` (регистр нечувствителен, закрывающие теги считаются
/// такими же пустыми). Именно это правило решает, войдёт ли тело секции в
/// результат, поэтому оно вынесено в отдельную функцию с собственными тестами,
/// а не растворилось в рендере.
#[must_use]
pub fn is_field_empty(value: &str) -> bool {
    let mut rest = value;
    loop {
        let trimmed = rest.trim_start();
        if trimmed.len() != rest.len() {
            rest = trimmed;
            continue;
        }
        match strip_empty_tag(rest) {
            Some(next) => rest = next,
            None => return rest.is_empty(),
        }
    }
}

/// Максимальная глубина вложенности секций, которую разбирает превью.
///
/// Рекурсия по секциям обязана быть ограниченной: шаблон может быть битым или
/// сгенерированным, а переполнение стека — это не «предсказуемый результат».
/// За пределом тело секции не разбирается вовсе, и это видно как причина отказа.
const MAX_SECTION_DEPTH: usize = 32;

/// Рантайм-поля Anki: статически невычислимы.
const RUNTIME_ONLY_SPECIALS: [&str; 2] = ["CardID", "CardFlag"];

/// Причина отказа для пустого токена `{{}}`.
const EMPTY_TOKEN_REASON: &str = "пустой токен {{}}: в скобках нет имени поля или конструкции";

/// Причина отказа для токена без имени (`{{#}}`, `{{cloze:}}`).
const NO_NAME_REASON: &str = "у конструкции нет имени поля: в скобках есть только служебный символ";

/// Причина отказа для фильтра внутри условия секции.
const SECTION_FILTER_REASON: &str =
    "фильтр внутри условия секции не поддержан: превью вычисляет только имя поля";

/// Причина отказа для слишком глубокой вложенности.
const DEPTH_REASON: &str =
    "вложенность секций превышает предел статического превью: тело секции не разбиралось";

/// Сторона карточки, для которой рендерится шаблон.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Front,
    Back,
}

/// Статически вычислимое special field Anki.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Special {
    Tags,
    Type,
    Deck,
    Subdeck,
    Card,
    FrontSide,
}

impl Special {
    /// Разбирает имя special field; регистр значим, как и в Anki.
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "Tags" => Self::Tags,
            "Type" => Self::Type,
            "Deck" => Self::Deck,
            "Subdeck" => Self::Subdeck,
            "Card" => Self::Card,
            "FrontSide" => Self::FrontSide,
            _ => return None,
        })
    }
}

/// Результат разрешения имени поля или special field.
enum Resolved {
    /// Значение, которое можно подставить.
    Text(String),
    /// Имя распознано, но статически невычислимо.
    Unsupported(String),
    /// Имени нет в модели.
    Missing,
}

/// Разобранная форма одного `{{ … }}`-токена.
enum TokenForm<'a> {
    /// `{{}}`.
    Empty,
    /// `{{#X}}` или `{{^X}}`.
    Section { name: &'a str, negated: bool },
    /// `{{/X}}`.
    Close { name: &'a str },
    /// `{{X}}` или `{{filter:X}}`; пустой `filter` означает подстановку без
    /// фильтра.
    Value { filter: &'a str, name: &'a str },
}

/// Найденный токен вместе с позицией в разбираемом тексте.
struct TokenAt<'a> {
    /// Смещение начала токена.
    start: usize,
    /// Сырой токен со скобками.
    raw: &'a str,
    /// Содержимое между скобками без тримминга.
    inner: &'a str,
}

impl TokenAt<'_> {
    /// Смещение сразу за токеном.
    fn end(&self) -> usize {
        self.start + self.raw.len()
    }
}

/// Найденная пара к открывающей секции.
struct SectionMatch<'a> {
    /// Тело секции.
    inner: &'a str,
    /// Смещение сразу за закрывающим токеном.
    after: usize,
    /// Сырой закрывающий токен.
    close_raw: &'a str,
}

/// Ищет следующий токен в `text`, начиная со смещения `position`.
///
/// `None` означает, что конструкции больше нет: остаток — обычный текст. Это же
/// относится к незакрытому `{{` — он не становится конструкцией и попадает в
/// превью как текст, потому что у него нет ни имени, ни конца.
fn next_token(text: &str, position: usize) -> Option<TokenAt<'_>> {
    let tail = text.get(position..)?;
    let start = position + tail.find("{{")?;
    let after_open = &text[start + 2..];
    let close = after_open.find("}}")?;
    Some(TokenAt {
        start,
        raw: &text[start..start + 2 + close + 2],
        inner: &after_open[..close],
    })
}

/// Разбирает содержимое токена без предположений о допустимых символах имени.
///
/// Имя поля может содержать пробелы, кириллицу, CJK, цифры и дефисы, поэтому
/// здесь нет ни `\w`, ни предположения об ASCII: единственное преобразование —
/// тримминг краёв, как и у самого Anki.
fn token_form(inner: &str) -> TokenForm<'_> {
    let token = inner.trim();

    if token.is_empty() {
        return TokenForm::Empty;
    }
    if let Some(rest) = token.strip_prefix('#') {
        return TokenForm::Section {
            name: rest.trim(),
            negated: false,
        };
    }
    if let Some(rest) = token.strip_prefix('^') {
        return TokenForm::Section {
            name: rest.trim(),
            negated: true,
        };
    }
    if let Some(rest) = token.strip_prefix('/') {
        return TokenForm::Close { name: rest.trim() };
    }
    match token.split_once(':') {
        Some((filter, name)) => TokenForm::Value {
            filter: filter.trim(),
            name: name.trim(),
        },
        None => TokenForm::Value {
            filter: "",
            name: token,
        },
    }
}

/// Ищет парный `{{/name}}`, считая вложенность только по тому же имени.
///
/// Совпадение по имени, а не по строгому стеку, повторяет поведение Anki: в её
/// шаблонах закрывающий тег обязан называть своё поле, поэтому `{{/Other}}`
/// внутри секции — это не «закрытие глубже», а отдельная ошибка шаблона, и она
/// всплывает при разборе тела.
fn find_matching_close<'a>(text: &'a str, position: usize, name: &str) -> Option<SectionMatch<'a>> {
    let mut depth = 0_usize;
    let mut cursor = position;

    while let Some(token) = next_token(text, cursor) {
        match token_form(token.inner) {
            TokenForm::Close { name: close_name } if close_name == name => {
                if depth == 0 {
                    return Some(SectionMatch {
                        inner: &text[position..token.start],
                        after: token.end(),
                        close_raw: token.raw,
                    });
                }
                depth -= 1;
            }
            TokenForm::Section {
                name: open_name, ..
            } if open_name == name => depth += 1,
            _ => {}
        }
        cursor = token.end();
    }

    None
}

/// Причина, по которой фильтр нельзя вычислить статически.
///
/// `None` — только для пустого фильтра: `{{Field}}` и `{{:Field}}` в Anki
/// означают одно и то же, подстановку без фильтра. Всё остальное, включая
/// неизвестные фильтры (их Anki передаёт Python-аддонам), статически
/// невоспроизводимо, поэтому у каждого есть своя конкретная причина.
fn filter_issue(filter: &str, model_kind: ModelKind) -> Option<String> {
    let filter = filter.trim();
    if filter.is_empty() {
        return None;
    }
    // `tts` принимает параметры до двоеточия (`{{tts ja_JP:Field}}`), поэтому
    // имя фильтра — первое слово, а не вся левая часть.
    let head = filter.split_whitespace().next().unwrap_or(filter);

    let reason = match head {
        "cloze" | "cloze-only" => match model_kind {
            ModelKind::Cloze => {
                "фильтр «cloze» не поддержан: cloze-разметка вычисляется движком Anki для каждой cloze-карточки"
            }
            ModelKind::Standard => {
                "фильтр «cloze» не поддержан: он требует cloze-модели, статически не вычисляется"
            }
        },
        "type" | "type-cloze" | "type-nc" => {
            "фильтр «type» не поддержан: ввод ответа воспроизводится только при показе карточки"
        }
        "tts" => "фильтр «tts» не поддержан: синтез речи не воспроизводится статически",
        "hint" => "фильтр «hint» не поддержан: подсказка раскрывается только интерактивно",
        "furigana" => {
            "фильтр «furigana» не поддержан: разметка чтения строится движком Anki поверх значений полей"
        }
        "kanji" => "фильтр «kanji» не поддержан: разбор иероглифов выполняет движок Anki",
        "kana" => "фильтр «kana» не поддержан: разбор каны выполняет движок Anki",
        "text" => "фильтр «text» не поддержан: извлечение текста из HTML выполняет движок Anki",
        unknown => {
            return Some(format!(
                "неизвестный фильтр «{unknown}»: Anki передаёт такие фильтры аддонам"
            ));
        }
    };

    Some(reason.to_string())
}

/// Причина отказа для рантайм-поля Anki.
fn runtime_reason(name: &str) -> String {
    format!("рантайм-поле Anki «{name}»: значение известно только при показе карточки")
}

/// Причина отказа для незакрытой секции.
fn unclosed_reason(name: &str) -> String {
    format!("секция не закрыта: в шаблоне нет парного {{{{/{name}}}}}")
}

/// Причина отказа для лишнего закрывающего токена.
fn stray_close_reason(name: &str) -> String {
    format!("лишний закрывающий токен: для {{{{/{name}}}}} нет открывающей секции")
}

/// Срезает один пустой тег в начале строки: `<br>`, `<br/>`, `</br>`, `<div>`,
/// `</div>`.
fn strip_empty_tag(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('<')?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);

    let name_len = rest
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .map(char::len_utf8)
        .sum::<usize>();
    let name = &rest[..name_len];
    if !name.eq_ignore_ascii_case("br") && !name.eq_ignore_ascii_case("div") {
        return None;
    }

    let rest = &rest[name_len..];
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    rest.strip_prefix('>')
}

/// Рендерит одну сторону карточки.
fn render_side(template: &str, ctx: &TemplateContext<'_>, side: Side) -> RenderedSide {
    let mut renderer = Renderer {
        ctx,
        side,
        html: String::new(),
        issues: Vec::new(),
        unsupported: false,
    };

    renderer.render_region(template, 0);

    if template.to_ascii_lowercase().contains("<script") {
        renderer.mark_unsupported(
            "<script",
            "в шаблоне есть <script: в v1 JavaScript превью не исполняется, поэтому сторона не считается отрисованной",
        );
    }
    if ctx.model_kind == ModelKind::Cloze {
        renderer.mark_unsupported(
            "cloze",
            "cloze-модель: статическое превью не вычисляет cloze-разметку ни для одной карточки модели",
        );
    }

    RenderedSide {
        html: renderer.html,
        status: if renderer.unsupported {
            RenderStatus::Unsupported
        } else {
            RenderStatus::Rendered
        },
        issues: renderer.issues,
    }
}

/// Состояние рендера одной стороны.
struct Renderer<'a, 'ctx> {
    ctx: &'ctx TemplateContext<'a>,
    side: Side,
    html: String,
    issues: Vec<RenderIssue>,
    unsupported: bool,
}

impl Renderer<'_, '_> {
    /// Добавляет сырой текст в превью без экранирования.
    fn push_text(&mut self, text: &str) {
        self.html.push_str(text);
    }

    /// Помечает сторону как unsupported и фиксирует причину.
    fn mark_unsupported(&mut self, construct: &str, reason: &str) {
        self.unsupported = true;
        self.issues.push(RenderIssue {
            construct: construct.to_string(),
            reason: reason.to_string(),
        });
    }

    /// Фиксирует отсутствующее в модели имя поля, не меняя статус стороны.
    fn mark_missing(&mut self, raw: &str, name: &str) {
        self.issues.push(RenderIssue {
            construct: raw.to_string(),
            reason: format!(
                "поле не найдено в модели: «{name}» не объявлено среди полей, подставлена пустая строка"
            ),
        });
    }

    /// Рендерит область шаблона: обычный текст и токены верхнего уровня.
    fn render_region(&mut self, region: &str, depth: usize) {
        let mut position = 0;

        while position < region.len() {
            let Some(token) = next_token(region, position) else {
                self.push_text(&region[position..]);
                return;
            };
            self.push_text(&region[position..token.start]);

            match token_form(token.inner) {
                TokenForm::Empty => {
                    self.mark_unsupported(token.raw, EMPTY_TOKEN_REASON);
                    self.push_text(token.raw);
                    position = token.end();
                }
                TokenForm::Value { filter, name } => {
                    self.render_value(token.raw, filter, name);
                    position = token.end();
                }
                TokenForm::Close { name } => {
                    self.mark_unsupported(token.raw, &stray_close_reason(name));
                    self.push_text(token.raw);
                    position = token.end();
                }
                TokenForm::Section { name, negated } => {
                    position = self.render_section(region, &token, name, negated, depth);
                }
            }
        }
    }

    /// Рендерит секцию и возвращает смещение сразу за ней.
    fn render_section(
        &mut self,
        region: &str,
        token: &TokenAt<'_>,
        name: &str,
        negated: bool,
        depth: usize,
    ) -> usize {
        if name.is_empty() {
            self.mark_unsupported(token.raw, NO_NAME_REASON);
            self.push_text(token.raw);
            return token.end();
        }
        if name.contains(':') {
            self.mark_unsupported(token.raw, SECTION_FILTER_REASON);
            self.push_text(token.raw);
            return token.end();
        }

        let Some(found) = find_matching_close(region, token.end(), name) else {
            self.mark_unsupported(token.raw, &unclosed_reason(name));
            self.push_text(token.raw);
            return token.end();
        };

        if depth >= MAX_SECTION_DEPTH {
            self.mark_unsupported(token.raw, DEPTH_REASON);
            self.push_text(token.raw);
            self.push_text(found.close_raw);
            return found.after;
        }

        match self.resolve(name) {
            // Условие статически невычислимо: показываем секцию как есть, чтобы
            // ревьюер видел и скобки, и то, что внутри.
            Resolved::Unsupported(reason) => {
                self.mark_unsupported(token.raw, &reason);
                self.push_text(token.raw);
                self.render_region(found.inner, depth + 1);
                self.push_text(found.close_raw);
            }
            Resolved::Missing => {
                self.mark_missing(token.raw, name);
                self.apply_section(&found, negated, true, depth);
            }
            Resolved::Text(value) => {
                let empty = is_field_empty(&value);
                self.apply_section(&found, negated, empty, depth);
            }
        }

        found.after
    }

    /// Включает тело секции, если условие пустоты её пропускает.
    fn apply_section(
        &mut self,
        found: &SectionMatch<'_>,
        negated: bool,
        empty: bool,
        depth: usize,
    ) {
        let include = if negated { empty } else { !empty };
        if include {
            self.render_region(found.inner, depth + 1);
        }
    }

    /// Рендерит подстановку значения: `{{Field}}` или `{{filter:Field}}`.
    fn render_value(&mut self, raw: &str, filter: &str, name: &str) {
        if let Some(reason) = filter_issue(filter, self.ctx.model_kind) {
            self.mark_unsupported(raw, &reason);
            self.push_text(raw);
            return;
        }
        if name.is_empty() {
            self.mark_unsupported(raw, NO_NAME_REASON);
            self.push_text(raw);
            return;
        }

        match self.resolve(name) {
            // Подстановка одноуровневая: значение не разбирается повторно.
            Resolved::Text(value) => self.push_text(&value),
            Resolved::Unsupported(reason) => {
                self.mark_unsupported(raw, &reason);
                self.push_text(raw);
            }
            Resolved::Missing => self.mark_missing(raw, name),
        }
    }

    /// Разрешает имя: special field, невычислимое рантайм-поле или поле модели.
    fn resolve(&self, name: &str) -> Resolved {
        // Special fields Anki имеют приоритет над полем с тем же именем: они
        // добавляются движком поверх полей модели.
        if let Some(special) = Special::parse(name) {
            return Resolved::Text(self.special_value(special));
        }
        if RUNTIME_ONLY_SPECIALS.contains(&name) {
            return Resolved::Unsupported(runtime_reason(name));
        }
        match self.ctx.fields.get(name) {
            Some(value) => Resolved::Text(value.clone()),
            None => Resolved::Missing,
        }
    }

    /// Значение статически вычислимого special field.
    fn special_value(&self, special: Special) -> String {
        match special {
            Special::Tags => self.ctx.tags.join(" "),
            Special::Type => self.ctx.notetype_name.to_string(),
            Special::Deck => deck_name(self.ctx.deck_path).to_string(),
            Special::Subdeck => subdeck_name(self.ctx.deck_path).to_string(),
            Special::Card => self.ctx.template_name.to_string(),
            Special::FrontSide => match self.side {
                // На лицевой стороне обращаться к самой себе нечему.
                Side::Front => String::new(),
                Side::Back => self.ctx.front_side.to_string(),
            },
        }
    }
}

/// Полное имя колоды с fallback `(Deck)`, когда имени нет.
fn deck_name(deck_path: &str) -> &str {
    if deck_path.is_empty() {
        "(Deck)"
    } else {
        deck_path
    }
}

/// Последняя компонента имени колоды после `::`.
fn subdeck_name(deck_path: &str) -> &str {
    let name = deck_name(deck_path);
    match name.rsplit_once("::") {
        Some((_, subdeck)) => subdeck,
        None => name,
    }
}

/// Рекурсивно перечисляет конструкции области.
fn scan_region(region: &str, model_kind: ModelKind, out: &mut Vec<Construct>, depth: usize) {
    let mut position = 0;

    while position < region.len() {
        let Some(token) = next_token(region, position) else {
            return;
        };

        match token_form(token.inner) {
            TokenForm::Empty => {
                out.push(unsupported_construct(token.raw, "", EMPTY_TOKEN_REASON));
                position = token.end();
            }
            TokenForm::Close { name } => {
                out.push(unsupported_construct(
                    token.raw,
                    name,
                    &stray_close_reason(name),
                ));
                position = token.end();
            }
            TokenForm::Value { filter, name } => {
                out.push(value_construct(token.raw, filter, name, model_kind));
                position = token.end();
            }
            TokenForm::Section { name, negated } => {
                let kind = if negated {
                    ConstructKind::SectionNegated
                } else {
                    ConstructKind::SectionOpen
                };

                if name.is_empty() {
                    out.push(unsupported_construct(token.raw, name, NO_NAME_REASON));
                    position = token.end();
                    continue;
                }
                if name.contains(':') {
                    out.push(unsupported_construct(
                        token.raw,
                        name,
                        SECTION_FILTER_REASON,
                    ));
                    position = token.end();
                    continue;
                }

                match find_matching_close(region, token.end(), name) {
                    None => {
                        out.push(unsupported_construct(
                            token.raw,
                            name,
                            &unclosed_reason(name),
                        ));
                        position = token.end();
                    }
                    Some(found) if depth >= MAX_SECTION_DEPTH => {
                        out.push(unsupported_construct(token.raw, name, DEPTH_REASON));
                        position = found.after;
                    }
                    Some(found) => {
                        out.push(Construct {
                            raw: token.raw.to_string(),
                            name: name.to_string(),
                            kind,
                            reason: None,
                        });
                        scan_region(found.inner, model_kind, out, depth + 1);
                        out.push(Construct {
                            raw: found.close_raw.to_string(),
                            name: name.to_string(),
                            kind: ConstructKind::SectionClose,
                            reason: None,
                        });
                        position = found.after;
                    }
                }
            }
        }
    }
}

/// Собирает конструкцию подстановки для [`scan_constructs`].
fn value_construct(raw: &str, filter: &str, name: &str, model_kind: ModelKind) -> Construct {
    if let Some(reason) = filter_issue(filter, model_kind) {
        return unsupported_construct(raw, name, &reason);
    }
    if name.is_empty() {
        return unsupported_construct(raw, name, NO_NAME_REASON);
    }
    if Special::parse(name).is_some() {
        return Construct {
            raw: raw.to_string(),
            name: name.to_string(),
            kind: ConstructKind::Special,
            reason: None,
        };
    }
    if RUNTIME_ONLY_SPECIALS.contains(&name) {
        return unsupported_construct(raw, name, &runtime_reason(name));
    }

    Construct {
        raw: raw.to_string(),
        name: name.to_string(),
        kind: ConstructKind::Field,
        reason: None,
    }
}

/// Собирает неподдержанную конструкцию.
fn unsupported_construct(raw: &str, name: &str, reason: &str) -> Construct {
    Construct {
        raw: raw.to_string(),
        name: name.to_string(),
        kind: ConstructKind::Unsupported,
        reason: Some(reason.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Синтетический контекст: нейтральные имена, никаких реальных колод.
    struct Fixture {
        fields: BTreeMap<String, String>,
        tags: Vec<String>,
        deck_path: String,
        notetype_name: String,
        template_name: String,
        front_side: String,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                fields: BTreeMap::new(),
                tags: Vec::new(),
                deck_path: "Alpha".to_string(),
                notetype_name: "Alpha".to_string(),
                template_name: "Alpha card".to_string(),
                front_side: String::new(),
            }
        }

        fn field(mut self, name: &str, value: &str) -> Self {
            self.fields.insert(name.to_string(), value.to_string());
            self
        }

        fn tags(mut self, tags: &[&str]) -> Self {
            self.tags = tags.iter().map(|tag| (*tag).to_string()).collect();
            self
        }

        fn deck(mut self, deck_path: &str) -> Self {
            self.deck_path = deck_path.to_string();
            self
        }

        fn front_side(mut self, front_side: &str) -> Self {
            self.front_side = front_side.to_string();
            self
        }

        fn context(&self, model_kind: ModelKind) -> TemplateContext<'_> {
            TemplateContext {
                fields: &self.fields,
                tags: &self.tags,
                deck_path: &self.deck_path,
                notetype_name: &self.notetype_name,
                template_name: &self.template_name,
                model_kind,
                front_side: &self.front_side,
            }
        }

        fn render(&self, qfmt: &str, afmt: &str) -> CardPreview {
            render_card(qfmt, afmt, &self.context(ModelKind::Standard))
        }
    }

    #[test]
    fn field_substitution_uses_raw_value() {
        let fixture = Fixture::new().field("field-a", "<b>значение</b>");
        let preview = fixture.render("{{field-a}}", "");

        assert_eq!(preview.front.html, "<b>значение</b>");
        assert_eq!(preview.front.status, RenderStatus::Rendered);
        assert!(preview.front.issues.is_empty());
        assert!(preview.generated);
    }

    #[test]
    fn section_is_included_only_for_non_empty_field() {
        let template = "{{#field-a}}[{{field-a}}]{{/field-a}}";

        let filled = Fixture::new()
            .field("field-a", "значение")
            .render(template, "");
        assert_eq!(filled.front.html, "[значение]");

        let empty = Fixture::new().field("field-a", "").render(template, "");
        assert_eq!(empty.front.html, "");
        assert!(!empty.generated);

        // Тег из правила пустоты: секция не должна раскрываться.
        let markup_only = Fixture::new()
            .field("field-a", "<br> <div></div>")
            .render(template, "");
        assert_eq!(markup_only.front.html, "");
        assert!(!markup_only.generated);
    }

    #[test]
    fn negated_section_is_included_only_for_empty_field() {
        let template = "{{^field-a}}нет значения{{/field-a}}";

        let empty = Fixture::new().field("field-a", "   ").render(template, "");
        assert_eq!(empty.front.html, "нет значения");

        let filled = Fixture::new()
            .field("field-a", "значение")
            .render(template, "");
        assert_eq!(filled.front.html, "");
    }

    #[test]
    fn nested_sections_follow_field_emptiness() {
        let template = "{{#outer}}A{{#inner}}B{{/inner}}C{{^inner}}D{{/inner}}E{{/outer}}";

        let both = Fixture::new()
            .field("outer", "x")
            .field("inner", "y")
            .render(template, "");
        assert_eq!(both.front.html, "ABCE");

        let only_outer = Fixture::new()
            .field("outer", "x")
            .field("inner", "")
            .render(template, "");
        assert_eq!(only_outer.front.html, "ACDE");

        let neither = Fixture::new().render(template, "");
        assert_eq!(neither.front.html, "");
    }

    #[test]
    fn front_side_is_empty_on_front_and_substituted_on_back() {
        let fixture = Fixture::new().front_side("<b>лицевая</b>");
        let preview = fixture.render("{{FrontSide}}", "{{FrontSide}}");

        assert_eq!(preview.front.html, "");
        assert_eq!(preview.back.html, "<b>лицевая</b>");
        assert_eq!(preview.back.status, RenderStatus::Rendered);
    }

    #[test]
    fn field_names_may_contain_spaces_cyrillic_and_non_ascii() {
        let fixture = Fixture::new()
            .field("模型 A", "значение")
            .field("часть речи", "существительное");

        let preview = fixture.render("{{ 模型 A }}/{{часть речи}}", "");
        assert_eq!(preview.front.html, "значение/существительное");
        assert_eq!(preview.front.status, RenderStatus::Rendered);

        // Внутренний пробел значим: «模型A» — другое имя.
        let other = fixture.render("{{模型A}}", "");
        assert_eq!(other.front.html, "");
        assert_eq!(other.front.issues.len(), 1);
        assert!(other.front.issues[0].reason.contains("не найдено в модели"));
    }

    #[test]
    fn substitution_is_single_level() {
        let fixture = Fixture::new()
            .field("field-a", "{{field-b}}")
            .field("field-b", "секрет");
        let preview = fixture.render("{{field-a}}", "");

        assert_eq!(preview.front.html, "{{field-b}}");
        assert_eq!(preview.front.status, RenderStatus::Rendered);
        assert!(preview.front.issues.is_empty());
    }

    #[test]
    fn special_fields_resolve_statically() {
        let fixture = Fixture::new()
            .tags(&["alpha", "beta"])
            .deck("Parent::Child")
            .front_side("лицевая");

        let preview = fixture.render(
            "{{Tags}}|{{Type}}|{{Deck}}|{{Subdeck}}|{{Card}}",
            "{{FrontSide}}",
        );

        assert_eq!(
            preview.front.html,
            "alpha beta|Alpha|Parent::Child|Child|Alpha card"
        );
        assert_eq!(preview.back.html, "лицевая");
    }

    #[test]
    fn deck_and_subdeck_have_documented_fallbacks() {
        let fixture = Fixture::new().deck("");
        let preview = fixture.render("{{Deck}}|{{Subdeck}}", "");
        assert_eq!(preview.front.html, "(Deck)|(Deck)");

        let flat = Fixture::new().deck("Alpha");
        let preview = flat.render("{{Deck}}|{{Subdeck}}", "");
        assert_eq!(preview.front.html, "Alpha|Alpha");

        // Пустые теги дают пустое значение special field.
        let no_tags = Fixture::new().render("{{Tags}}", "");
        assert_eq!(no_tags.front.html, "");
    }

    #[test]
    fn tags_drive_sections_like_regular_fields() {
        let template = "{{#Tags}}есть теги{{/Tags}}{{^Tags}}нет тегов{{/Tags}}";

        let tagged = Fixture::new().tags(&["alpha"]).render(template, "");
        assert_eq!(tagged.front.html, "есть теги");

        let untagged = Fixture::new().render(template, "");
        assert_eq!(untagged.front.html, "нет тегов");
    }

    #[test]
    fn cloze_model_is_unsupported_as_a_whole() {
        let fixture = Fixture::new().field("field-a", "значение");
        let preview = render_card(
            "{{field-a}}",
            "{{field-a}}",
            &fixture.context(ModelKind::Cloze),
        );

        assert_eq!(preview.front.status, RenderStatus::Unsupported);
        assert_eq!(preview.back.status, RenderStatus::Unsupported);
        assert!(!preview.generated);
        assert!(
            preview
                .front
                .issues
                .iter()
                .any(|issue| issue.reason.contains("cloze-модель"))
        );
        // Поддержанные конструкции всё равно подставлены: превью остаётся полезным.
        assert_eq!(preview.front.html, "значение");
    }

    #[test]
    fn unsupported_filters_keep_raw_text_visible() {
        let cases = [
            "{{cloze:field-a}}",
            "{{cloze-only:field-a}}",
            "{{type:field-a}}",
            "{{type-cloze:field-a}}",
            "{{type-nc:field-a}}",
            "{{tts ja_JP:field-a}}",
            "{{hint:field-a}}",
            "{{furigana:field-a}}",
            "{{kanji:field-a}}",
            "{{kana:field-a}}",
            "{{text:field-a}}",
            "{{unknown-addon-filter:field-a}}",
        ];

        for template in cases {
            let preview = Fixture::new()
                .field("field-a", "значение")
                .render(template, "");
            assert_eq!(
                preview.front.status,
                RenderStatus::Unsupported,
                "{template} должен быть unsupported"
            );
            assert_eq!(
                preview.front.html, template,
                "{template} должен остаться сырым"
            );
            assert_eq!(preview.front.issues.len(), 1, "{template} без причины");
            assert!(
                !preview.front.issues[0].reason.is_empty(),
                "{template} без причины"
            );
        }
    }

    #[test]
    fn empty_filter_is_plain_substitution() {
        let preview = Fixture::new()
            .field("field-a", "значение")
            .render("{{:field-a}}", "");
        assert_eq!(preview.front.html, "значение");
        assert_eq!(preview.front.status, RenderStatus::Rendered);
    }

    #[test]
    fn runtime_only_specials_are_unsupported() {
        for template in ["{{CardID}}", "{{CardFlag}}"] {
            let preview = Fixture::new().render(template, "");
            assert_eq!(preview.front.status, RenderStatus::Unsupported);
            assert_eq!(preview.front.html, template);
            assert!(preview.front.issues[0].reason.contains("рантайм-поле"));
        }

        // Условие по рантайм-полю тоже невычислимо, но тело видно.
        let preview = Fixture::new().render("{{#CardID}}тело{{/CardID}}", "");
        assert_eq!(preview.front.status, RenderStatus::Unsupported);
        assert_eq!(preview.front.html, "{{#CardID}}тело{{/CardID}}");
    }

    #[test]
    fn script_in_template_is_unsupported() {
        let preview = Fixture::new().render("<div>a</div><SCRIPT>x</SCRIPT>", "");
        assert_eq!(preview.front.status, RenderStatus::Unsupported);
        assert!(preview.front.issues[0].reason.contains("<script"));
        // Текст не вырезан: именно поэтому сторона и помечена unsupported.
        assert_eq!(preview.front.html, "<div>a</div><SCRIPT>x</SCRIPT>");
    }

    #[test]
    fn missing_field_keeps_status_and_reports_issue() {
        let preview = Fixture::new().render("до {{нет такого}} после", "");
        assert_eq!(preview.front.html, "до  после");
        assert_eq!(preview.front.status, RenderStatus::Rendered);
        assert_eq!(preview.front.issues.len(), 1);
        assert_eq!(preview.front.issues[0].construct, "{{нет такого}}");
        assert!(
            preview.front.issues[0]
                .reason
                .contains("поле не найдено в модели")
        );
    }

    /// Ограничение реестра special fields Anki закреплено тестом: имя в форме
    /// special, которого нет в реестре, — это поле модели, а не «неизвестный
    /// special», и его отсутствие не делает сторону unsupported.
    #[test]
    fn name_shaped_like_a_special_but_unknown_stays_a_field() {
        let preview = Fixture::new().render("{{CardKind}}", "");
        assert_eq!(preview.front.html, "");
        assert_eq!(preview.front.status, RenderStatus::Rendered);
        assert_eq!(preview.front.issues.len(), 1);
        assert!(
            preview.front.issues[0]
                .reason
                .contains("поле не найдено в модели")
        );

        // То же на этапе разбора: без значений полей имя остаётся подстановкой.
        let constructs = scan_constructs("{{CardKind}}", ModelKind::Standard);
        assert_eq!(constructs[0].kind, ConstructKind::Field);
        assert_eq!(constructs[0].name, "CardKind");
    }

    #[test]
    fn structural_breakage_is_reported_without_panicking() {
        let unclosed = Fixture::new()
            .field("field-a", "x")
            .render("{{#field-a}}тело", "");
        assert_eq!(unclosed.front.status, RenderStatus::Unsupported);
        assert_eq!(unclosed.front.html, "{{#field-a}}тело");
        assert!(unclosed.front.issues[0].reason.contains("не закрыта"));

        let stray = Fixture::new().render("тело{{/field-a}}", "");
        assert_eq!(stray.front.status, RenderStatus::Unsupported);
        assert_eq!(stray.front.html, "тело{{/field-a}}");
        assert!(stray.front.issues[0].reason.contains("лишний закрывающий"));

        // Секция с чужим закрывающим токеном: имя не совпало.
        let mismatched = Fixture::new()
            .field("field-a", "x")
            .field("field-b", "y")
            .render("{{#field-a}}тело{{/field-b}}", "");
        assert_eq!(mismatched.front.status, RenderStatus::Unsupported);

        for template in ["{{}}", "{{#}}", "{{/}}", "{{ : }}", "{{^}}"] {
            let preview = Fixture::new().render(template, "");
            assert_eq!(
                preview.front.status,
                RenderStatus::Unsupported,
                "{template} должен быть unsupported"
            );
            assert!(!preview.front.issues.is_empty(), "{template} без причины");
        }

        // Одиночные скобки не образуют токен: у него нет ни имени, ни конца.
        for template in ["{{", "}}", "a {{ b"] {
            let preview = Fixture::new().render(template, "");
            assert_eq!(preview.front.status, RenderStatus::Rendered);
            assert_eq!(preview.front.html, template);
        }
    }

    #[test]
    fn unterminated_open_token_is_plain_text() {
        // Незакрытый `{{` не имеет ни имени, ни конца, поэтому это текст.
        let preview = Fixture::new()
            .field("field-a", "x")
            .render("{{#field-a}}a{{", "");
        assert_eq!(preview.front.html, "{{#field-a}}a{{");
        assert_eq!(preview.front.status, RenderStatus::Unsupported);
    }

    #[test]
    fn generated_follows_front_emptiness_rule() {
        let whitespace = Fixture::new().render(" \n\t ", "");
        assert_eq!(whitespace.front.html, " \n\t ");
        assert!(!whitespace.generated);

        let markup = Fixture::new().render("<BR><div></div>", "");
        assert!(!markup.generated);

        let content = Fixture::new().render("<div>x</div>", "");
        assert!(content.generated);

        // Состояние back на генерацию не влияет.
        let back_only = Fixture::new().render("", "контент обратной стороны");
        assert!(!back_only.generated);
    }

    #[test]
    fn deep_nesting_does_not_overflow_stack() {
        let template = format!(
            "{}{}",
            "{{#field-a}}".repeat(200),
            "тело".to_string() + &"{{/field-a}}".repeat(200)
        );
        let preview = Fixture::new().field("field-a", "x").render(&template, "");
        assert_eq!(preview.front.status, RenderStatus::Unsupported);
        assert!(
            preview
                .front
                .issues
                .iter()
                .any(|issue| issue.reason.contains("вложенность"))
        );
    }

    #[test]
    fn render_is_deterministic_and_panic_free_on_garbage() {
        let garbage = "{{#a}}{{b}}{{/c}}{{}}{{:x}}{{#d}}{{^e}}{{/e}}}}{{ii";
        let fixture = Fixture::new().field("a", "1").field("b", "2");
        let first = fixture.render(garbage, "");
        let second = fixture.render(garbage, "");
        assert_eq!(first, second);
        assert_eq!(first.front.status, RenderStatus::Unsupported);
    }

    #[test]
    fn scan_lists_nested_constructs_in_document_order() {
        let constructs = scan_constructs(
            "{{field-a}}{{#field-b}}{{field-c}}{{/field-b}}{{^field-d}}{{/field-d}}",
            ModelKind::Standard,
        );

        let listed: Vec<(&str, ConstructKind)> = constructs
            .iter()
            .map(|construct| (construct.name.as_str(), construct.kind))
            .collect();

        assert_eq!(
            listed,
            vec![
                ("field-a", ConstructKind::Field),
                ("field-b", ConstructKind::SectionOpen),
                ("field-c", ConstructKind::Field),
                ("field-b", ConstructKind::SectionClose),
                ("field-d", ConstructKind::SectionNegated),
                ("field-d", ConstructKind::SectionClose),
            ]
        );
        assert!(
            constructs
                .iter()
                .all(|construct| construct.reason.is_none())
        );
    }

    #[test]
    fn scan_classifies_specials_and_unsupported_constructs() {
        let constructs = scan_constructs(
            "{{Tags}}{{Type}}{{Deck}}{{Subdeck}}{{Card}}{{FrontSide}}{{CardID}}{{cloze:field-a}}",
            ModelKind::Standard,
        );

        let kinds: Vec<ConstructKind> = constructs.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ConstructKind::Special,
                ConstructKind::Special,
                ConstructKind::Special,
                ConstructKind::Special,
                ConstructKind::Special,
                ConstructKind::Special,
                ConstructKind::Unsupported,
                ConstructKind::Unsupported,
            ]
        );
        assert!(
            constructs[6]
                .reason
                .as_deref()
                .unwrap()
                .contains("рантайм-поле")
        );
        assert!(constructs[7].reason.as_deref().unwrap().contains("cloze"));
        assert_eq!(constructs[7].name, "field-a");
    }

    #[test]
    fn scan_reports_broken_structure() {
        let constructs = scan_constructs("{{#field-a}}тело", ModelKind::Standard);
        assert_eq!(constructs.len(), 1);
        assert_eq!(constructs[0].kind, ConstructKind::Unsupported);
        assert!(
            constructs[0]
                .reason
                .as_deref()
                .unwrap()
                .contains("не закрыта")
        );

        let constructs = scan_constructs("{{/field-a}}{{}}", ModelKind::Standard);
        assert_eq!(constructs.len(), 2);
        assert_eq!(constructs[0].kind, ConstructKind::Unsupported);
        assert_eq!(constructs[1].kind, ConstructKind::Unsupported);
        assert_eq!(constructs[1].raw, "{{}}");

        for template in ["{{", "}}", "{{{{", "{{:}}"] {
            let constructs = scan_constructs(template, ModelKind::Standard);
            assert!(
                constructs
                    .iter()
                    .all(|construct| construct.kind == ConstructKind::Unsupported)
            );
        }
    }

    #[test]
    fn empty_field_rule_matches_anki() {
        for empty in [
            "",
            " ",
            "\n\t",
            "<br>",
            "<BR>",
            "<br/>",
            "</br>",
            "<div>",
            "</div>",
            "<div></div>",
            " <br> <div> </DIV>",
        ] {
            assert!(is_field_empty(empty), "{empty:?} должен считаться пустым");
        }

        for filled in ["x", "<br>x", "<div>a</div>", "<span></span>", "&nbsp;", "0"] {
            assert!(
                !is_field_empty(filled),
                "{filled:?} не должен считаться пустым"
            );
        }
    }
}
