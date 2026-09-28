//! Модели заметок: свидетельства о фактической схеме и разрешение модели.
//!
//! Отдельная команда осмотра нужна потому, что агент, создающий заметку, не
//! имеет права выводить семантику поля из одного имени. Имя поля — данные
//! конкретной колоды, а не контракт toolkit'а, поэтому свидетельством служит
//! только фактический экспорт: объявленные `flds[].ord`, порядок полей, число
//! заметок этой модели именно в целевой колоде, использование полей в шаблонах
//! и ограниченные representatives-примеры значений.
//!
//! Здесь же живёт разрешение модели для создания заметки. Автоматический режим
//! опирается только на структурные свидетельства: какие модели действительно
//! используются заметками целевой колоды и чья схема совпадает с набором
//! переданных полей. Частота использования — это ранжирующее свидетельство,
//! которое попадает в отчёт, но не подменяет однозначность: если совместимых
//! моделей несколько, разрешение обязано упасть, а не выбрать «самую частую».
//! Модель по имени поля или по позиции `fields[0]` не выбирается никогда, и
//! ни одна команда здесь не создаёт, не клонирует и не меняет модель.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::ExportIndex;
use crate::loader;
use crate::model::{NoteModel, Ord};
use crate::ops::deck_select::{DeckSelector, ResolvedDeck};
use crate::ops::source::deck_child_paths;
use crate::template::{ConstructKind, ModelKind, scan_constructs};
use crate::text::bounded_sample;

/// Сколько примеров значений поля показывать по умолчанию.
pub const DEFAULT_SAMPLE_LIMIT: usize = 3;
/// Предел числа примеров значений одного поля.
pub const MAX_SAMPLE_LIMIT: usize = 10;

/// Способ, которым была выбрана модель заметок.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMode {
    /// Модель выведена из структуры экспорта.
    #[default]
    Auto,
    /// Модель задана в запросе явно.
    Explicit,
}

impl ModelMode {
    /// Строковое имя режима для машинного вывода.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Explicit => "explicit",
        }
    }
}

/// Селектор модели заметок из запроса создания.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelector {
    /// Режим разрешения модели.
    #[serde(default)]
    pub mode: ModelMode,
    /// Идентичность модели; допустима только при `explicit`.
    #[serde(default)]
    pub crowdanki_uuid: Option<String>,
    /// Точное имя модели; допустимо только при `explicit`.
    #[serde(default)]
    pub name: Option<String>,
}

/// Поле модели в порядке `ord`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelField {
    /// Позиция поля; индекс в массиве `fields` заметки.
    pub ord: usize,
    /// Имя поля.
    pub name: String,
    /// Описание поля из модели, если объявлено.
    pub description: Option<String>,
}

/// Один представитель-пример значения поля.
#[derive(Debug, Clone)]
pub struct FieldSample {
    /// `guid` заметки, из которой взят пример.
    pub guid: Option<String>,
    /// Ограниченный по длине пример значения.
    pub value: String,
}

/// Свидетельство о поле модели.
#[derive(Debug, Clone)]
pub struct FieldEvidence {
    /// Позиция поля.
    pub ord: usize,
    /// Имя поля.
    pub name: String,
    /// Описание поля из модели.
    pub description: Option<String>,
    /// Представители-примеры непустых значений (детерминированно, в порядке заметок).
    pub samples: Vec<FieldSample>,
    /// Сколько заметок этой модели в целевой колоде имеют пустое значение поля.
    pub empty_in_deck: usize,
}

/// Использование шаблона: какие поля и конструкции он требует.
#[derive(Debug, Clone)]
pub struct TemplateEvidence {
    /// Позиция шаблона.
    pub ord: usize,
    /// Имя шаблона, если объявлено.
    pub name: Option<String>,
    /// Имена полей, использованные шаблоном.
    pub fields: Vec<String>,
    /// Статически вычислимые special fields шаблона.
    pub specials: Vec<String>,
    /// Конструкции, которые превью не поддержит, с конкретной причиной.
    pub unsupported: Vec<UnsupportedConstruct>,
}

/// Неподдержанная конструкция шаблона.
#[derive(Debug, Clone)]
pub struct UnsupportedConstruct {
    /// Сырая конструкция.
    pub construct: String,
    /// Конкретная причина, а не «примерно похоже».
    pub reason: String,
}

/// Свидетельство об одной модели заметок.
#[derive(Debug, Clone)]
pub struct ModelEvidence {
    /// Идентичность CrowdAnki.
    pub crowdanki_uuid: String,
    /// Имя модели.
    pub name: String,
    /// Значение ключа `type` модели, если оно целое.
    pub model_type: Option<i64>,
    /// Вид модели с точки зрения рендера карточек.
    pub model_kind: ModelKind,
    /// Объявлена ли модель в поддереве целевой колоды.
    pub declared_in_deck: bool,
    /// Заметки этой модели именно в целевом узле.
    pub notes_in_deck: usize,
    /// Заметки этой модели во всём поддереве целевой колоды.
    pub notes_in_subtree: usize,
    /// Поля в порядке `ord`.
    pub fields: Vec<FieldEvidence>,
    /// Шаблоны модели.
    pub templates: Vec<TemplateEvidence>,
    /// Причины, по которым схему полей нельзя использовать для сборки значений.
    pub schema_problems: Vec<String>,
    /// Значение ключа `req` как есть (legacy-кэш, не участвует в генерации карт).
    pub req: Option<Value>,
}

/// Описание целевой колоды в отчёте осмотра.
#[derive(Debug, Clone)]
pub struct DeckIdentity {
    /// Полное имя колоды.
    pub path: String,
    /// Идентичность CrowdAnki.
    pub crowdanki_uuid: Option<String>,
    /// Позиция в preorder-обходе.
    pub preorder: usize,
    /// Заметки, объявленные непосредственно в узле.
    pub notes_in_deck: usize,
}

/// Запрос осмотра моделей.
#[derive(Debug, Clone)]
pub struct ModelsQuery {
    /// Селектор целевой колоды.
    pub deck: DeckSelector,
    /// Сколько примеров значений поля показывать.
    pub sample_limit: usize,
}

/// Результат осмотра моделей.
#[derive(Debug, Clone)]
pub struct ModelsResult {
    /// Каталог экспорта.
    pub export_dir: PathBuf,
    /// Целевая колода.
    pub deck: DeckIdentity,
    /// Запрошенный предел примеров.
    pub sample_limit: usize,
    /// Модели в детерминированном порядке: по имени, затем по `crowdanki_uuid`.
    pub models: Vec<ModelEvidence>,
}

/// Разрешённая модель заметок для создания заметки.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// Способ разрешения.
    pub mode: ModelMode,
    /// Идентичность CrowdAnki.
    pub crowdanki_uuid: String,
    /// Имя модели.
    pub name: String,
    /// Вид модели с точки зрения рендера карточек.
    pub model_kind: ModelKind,
    /// Поля в порядке `ord`: именно в этом порядке собирается `fields` заметки.
    pub fields: Vec<ModelField>,
    /// Заметки этой модели в целевом узле.
    pub notes_in_deck: usize,
    /// Заметки этой модели во всём поддереве.
    pub notes_in_subtree: usize,
    /// Какими свидетельствами модель была выбрана.
    pub evidence: String,
}

/// Осматривает модели, применимые к целевой колоде.
///
/// # Errors
///
/// Ошибки чтения экспорта и разрешения селектора колоды.
pub fn inspect(export_dir: &Path, query: &ModelsQuery) -> Result<ModelsResult, DomainError> {
    let (deck_json, raw) = loader::read_deck_json_bytes(export_dir)?;
    let value = loader::parse_deck_json_bytes(&raw, &deck_json)?;
    let tree = loader::typed_root(loader::parse_deck_json_bytes(&raw, &deck_json)?, &deck_json)?;

    inspect_document(&tree, &value, export_dir, query)
}

/// Осматривает модели по уже разобранному экспорту.
///
/// # Errors
///
/// Ошибки разрешения селектора колоды и внутренней рассинхронизации проекций.
pub fn inspect_document(
    tree: &crate::model::DeckNode,
    value: &Value,
    export_dir: &Path,
    query: &ModelsQuery,
) -> Result<ModelsResult, DomainError> {
    let index = ExportIndex::build(tree);
    let child_paths = deck_child_paths(value)?;
    let deck = query.deck.resolve(&index, &child_paths)?;
    let subtree = index.subtree_range(deck.preorder);
    let sample_limit = query.sample_limit.clamp(1, MAX_SAMPLE_LIMIT);

    let mut models: Vec<ModelEvidence> = Vec::new();
    for model in applicable_models(&index, &subtree) {
        let Some(uuid) = model.crowdanki_uuid.clone() else {
            continue;
        };
        models.push(evidence_of(
            &index,
            &subtree,
            deck.preorder,
            model,
            uuid,
            sample_limit,
        ));
    }

    models.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.crowdanki_uuid.cmp(&right.crowdanki_uuid))
    });

    Ok(ModelsResult {
        export_dir: export_dir.to_path_buf(),
        deck: DeckIdentity {
            path: deck.path,
            crowdanki_uuid: deck.crowdanki_uuid,
            preorder: deck.preorder,
            notes_in_deck: deck.notes_in_deck,
        },
        sample_limit,
        models,
    })
}

/// Модели, применимые к поддереву колоды: объявленные в нём или использованные
/// его заметками.
fn applicable_models<'a>(
    index: &'a ExportIndex<'a>,
    subtree: &std::ops::Range<usize>,
) -> Vec<&'a NoteModel> {
    let mut uuids: BTreeSet<&str> = BTreeSet::new();
    for node in index
        .nodes
        .iter()
        .filter(|node| subtree.contains(&node.preorder))
    {
        for model in &node.node.note_models {
            if let Some(uuid) = model.crowdanki_uuid.as_deref() {
                uuids.insert(uuid);
            }
        }
    }
    for note in index
        .notes
        .iter()
        .filter(|note| subtree.contains(&note.node))
    {
        if let Some(uuid) = note.note.note_model_uuid.as_deref() {
            uuids.insert(uuid);
        }
    }

    uuids
        .into_iter()
        .filter_map(|uuid| index.model_by_uuid(uuid))
        .collect()
}

/// Собирает свидетельство об одной модели.
fn evidence_of(
    index: &ExportIndex<'_>,
    subtree: &std::ops::Range<usize>,
    deck: usize,
    model: &NoteModel,
    uuid: String,
    sample_limit: usize,
) -> ModelEvidence {
    let notes: Vec<&crate::model::Note> = index
        .notes
        .iter()
        .filter(|note| subtree.contains(&note.node))
        .filter(|note| note.note.note_model_uuid.as_deref() == Some(uuid.as_str()))
        .map(|note| note.note)
        .collect();

    // Свидетельство обязано идти в порядке `ord`, а не в порядке объявления:
    // `ord` — это и есть индекс значения поля в `fields` заметки, поэтому и
    // примеры значений, и подсчёт пустых берутся по `ord`. Иначе агент увидел бы
    // значение чужого поля как образец семантики этого.
    //
    // Непригодная схема полей — не повод молча потерять свидетельство: модель
    // остаётся в ответе, поля перечисляются в порядке объявления, а причина
    // непригодности лежит рядом в `schema_problems`.
    let ordered = ordered_fields(model).unwrap_or_else(|_| {
        model
            .flds
            .iter()
            .enumerate()
            .map(|(position, field)| ModelField {
                ord: position,
                name: field.name.clone(),
                description: field.description.clone(),
            })
            .collect()
    });

    let fields = ordered
        .iter()
        .map(|field| FieldEvidence {
            ord: field.ord,
            name: field.name.clone(),
            description: field.description.clone(),
            samples: sample_values(&notes, field.ord, sample_limit),
            empty_in_deck: notes
                .iter()
                .filter(|note| {
                    note.fields
                        .get(field.ord)
                        .and_then(crate::model::FieldValue::as_text)
                        .is_none_or(str::is_empty)
                })
                .count(),
        })
        .collect();

    ModelEvidence {
        crowdanki_uuid: uuid.clone(),
        name: model.name.clone().unwrap_or_default(),
        model_type: model.model_type.value(),
        model_kind: model_kind(model),
        declared_in_deck: index.subtree_range(deck).contains(&deck)
            && model_owner_declares(index, subtree, &model.crowdanki_uuid),
        notes_in_deck: index
            .notes
            .iter()
            .filter(|note| note.node == deck)
            .filter(|note| note.note.note_model_uuid.as_deref() == Some(uuid.as_str()))
            .count(),
        notes_in_subtree: notes.len(),
        fields,
        templates: model
            .tmpls
            .iter()
            .enumerate()
            .map(|(position, template)| {
                template_evidence(
                    template
                        .ord
                        .value()
                        .and_then(|ord| usize::try_from(ord).ok())
                        .unwrap_or(position),
                    template.name.clone(),
                    &template.qfmt,
                    &template.afmt,
                    model_kind(model),
                )
            })
            .collect(),
        schema_problems: schema_problems(model),
        req: model.req.clone(),
    }
}

/// Объявлена ли модель в поддереве колоды.
fn model_owner_declares(
    index: &ExportIndex<'_>,
    subtree: &std::ops::Range<usize>,
    uuid: &Option<String>,
) -> bool {
    let Some(uuid) = uuid.as_deref() else {
        return false;
    };
    index
        .nodes
        .iter()
        .filter(|node| subtree.contains(&node.preorder))
        .any(|node| {
            node.node
                .note_models
                .iter()
                .any(|model| model.crowdanki_uuid.as_deref() == Some(uuid))
        })
}

/// Ограниченные representatives-примеры непустых значений поля.
fn sample_values(notes: &[&crate::model::Note], position: usize, limit: usize) -> Vec<FieldSample> {
    let mut samples: Vec<FieldSample> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for note in notes {
        if samples.len() >= limit {
            break;
        }
        let Some(value) = note
            .fields
            .get(position)
            .and_then(crate::model::FieldValue::as_text)
        else {
            continue;
        };
        if value.trim().is_empty() || !seen.insert(value.to_string()) {
            continue;
        }
        samples.push(FieldSample {
            guid: note.guid.clone(),
            value: bounded_sample(value),
        });
    }

    samples
}

/// Собирает использование полей и конструкций одного шаблона.
fn template_evidence(
    ord: usize,
    name: Option<String>,
    qfmt: &str,
    afmt: &str,
    kind: ModelKind,
) -> TemplateEvidence {
    let mut fields: BTreeSet<String> = BTreeSet::new();
    let mut specials: BTreeSet<String> = BTreeSet::new();
    let mut unsupported: Vec<UnsupportedConstruct> = Vec::new();

    for text in [qfmt, afmt] {
        for construct in scan_constructs(text, kind) {
            match construct.kind {
                ConstructKind::Field
                | ConstructKind::SectionOpen
                | ConstructKind::SectionNegated
                | ConstructKind::SectionClose => {
                    fields.insert(construct.name);
                }
                ConstructKind::Special => {
                    specials.insert(construct.name);
                }
                ConstructKind::Unsupported => {
                    let reason = construct
                        .reason
                        .unwrap_or_else(|| "конструкция не поддержана превью".to_string());
                    if !unsupported
                        .iter()
                        .any(|known| known.construct == construct.raw)
                    {
                        unsupported.push(UnsupportedConstruct {
                            construct: construct.raw,
                            reason,
                        });
                    }
                }
            }
        }
    }

    TemplateEvidence {
        ord,
        name,
        fields: fields.into_iter().collect(),
        specials: specials.into_iter().collect(),
        unsupported,
    }
}

/// Вид модели с точки зрения рендера карточек.
#[must_use]
pub fn model_kind(model: &NoteModel) -> ModelKind {
    match model.model_type {
        Ord::Int(1) => ModelKind::Cloze,
        _ => ModelKind::Standard,
    }
}

/// Причины, по которым схему полей нельзя использовать для сборки значений.
#[must_use]
pub fn schema_problems(model: &NoteModel) -> Vec<String> {
    let mut problems = Vec::new();

    if model.flds.is_empty() {
        problems.push("модель не объявляет ни одного поля".to_string());
        return problems;
    }

    let mut names: BTreeSet<&str> = BTreeSet::new();
    for (position, field) in model.flds.iter().enumerate() {
        let Some(ord) = field.ord.value() else {
            problems.push(format!("поле #{position}: ord отсутствует или не целое"));
            continue;
        };
        let Ok(ord) = usize::try_from(ord) else {
            problems.push(format!("поле #{position}: отрицательный ord {ord}"));
            continue;
        };
        if ord >= model.flds.len() {
            problems.push(format!(
                "поле #{position}: ord {ord} вне диапазона полей модели ({})",
                model.flds.len()
            ));
        }
        if field.name.is_empty() {
            problems.push(format!("поле #{position}: пустое имя"));
        } else if !names.insert(field.name.as_str()) {
            problems.push(format!("имя поля {:?} объявлено дважды", field.name));
        }
    }

    let mut ords: Vec<Option<usize>> = model
        .flds
        .iter()
        .map(|field| field.ord.value().and_then(|ord| usize::try_from(ord).ok()))
        .collect();
    ords.sort_unstable();
    let expected: Vec<Option<usize>> = (0..model.flds.len()).map(Some).collect();
    if problems.is_empty() && ords != expected {
        problems.push(format!(
            "порядок полей модели не является непрерывным 0..{}: {ords:?}",
            model.flds.len() - 1
        ));
    }

    problems
}

/// Поля модели в порядке `ord`.
///
/// # Errors
///
/// [`ErrorCode::ModelSchemaUnusable`], если порядок полей непригоден.
pub fn ordered_fields(model: &NoteModel) -> Result<Vec<ModelField>, DomainError> {
    let problems = schema_problems(model);
    if !problems.is_empty() {
        return Err(DomainError::with_details(
            ErrorCode::ModelSchemaUnusable,
            format!(
                "схема полей модели «{}» непригодна для сборки значений: {}",
                model.name.as_deref().unwrap_or("(без имени)"),
                problems.join("; ")
            ),
            details! {
                "crowdanki_uuid" => model.crowdanki_uuid.clone(),
                "model_name" => model.name.clone(),
                "problems" => problems,
            },
        ));
    }

    let mut fields: Vec<ModelField> = model
        .flds
        .iter()
        .map(|field| ModelField {
            ord: field
                .ord
                .value()
                .and_then(|ord| usize::try_from(ord).ok())
                .unwrap_or(0),
            name: field.name.clone(),
            description: field.description.clone(),
        })
        .collect();
    fields.sort_by_key(|field| field.ord);
    Ok(fields)
}

/// Разрешает модель заметок для создания заметки.
///
/// `required_fields` — имена полей, для которых запрос передал значения.
///
/// # Errors
///
/// [`ErrorCode::InvalidRequest`] — режим и ссылка на модель противоречат друг
/// другу; [`ErrorCode::UnknownModel`] — подходящей модели нет;
/// [`ErrorCode::AmbiguousModel`] — подходящих моделей несколько;
/// [`ErrorCode::ModelSchemaUnusable`] — схема полей выбранной модели непригодна;
/// [`ErrorCode::MissingFieldValue`] — модель требует поле, которого нет в запросе;
/// [`ErrorCode::UnknownField`] — запрос передаёт имя, которого нет в модели.
pub fn resolve(
    index: &ExportIndex<'_>,
    deck: &ResolvedDeck,
    selector: &ModelSelector,
    required_fields: &BTreeSet<String>,
) -> Result<ResolvedModel, DomainError> {
    let subtree = index.subtree_range(deck.preorder);

    match selector.mode {
        ModelMode::Explicit => {
            let model = explicit_model(index, selector)?;
            let model = usable_model(model)?;

            let known: BTreeSet<&str> = model.flds.iter().map(|f| f.name.as_str()).collect();
            let missing: Vec<String> = known
                .iter()
                .filter(|name| !required_fields.contains(**name))
                .map(|name| (*name).to_string())
                .collect();
            if !missing.is_empty() {
                return Err(DomainError::with_details(
                    ErrorCode::MissingFieldValue,
                    format!(
                        "запрос не задаёт значения полей модели «{}»: {}",
                        model.name.as_deref().unwrap_or("(без имени)"),
                        missing.join(", ")
                    ),
                    details! {
                        "crowdanki_uuid" => model.crowdanki_uuid.clone(),
                        "missing_fields" => missing,
                    },
                ));
            }

            let extra: Vec<String> = required_fields
                .iter()
                .filter(|name| !known.contains(name.as_str()))
                .cloned()
                .collect();
            if !extra.is_empty() {
                return Err(DomainError::with_details(
                    ErrorCode::UnknownField,
                    format!(
                        "запрос задаёт поля, которых нет в модели «{}»: {}",
                        model.name.as_deref().unwrap_or("(без имени)"),
                        extra.join(", ")
                    ),
                    details! {
                        "crowdanki_uuid" => model.crowdanki_uuid.clone(),
                        "unknown_fields" => extra,
                        "known_fields" => known.iter().map(|name| (*name).to_string()).collect::<Vec<_>>(),
                    },
                ));
            }

            finish(index, &subtree, deck, ModelMode::Explicit, model)
        }
        ModelMode::Auto => {
            if selector.crowdanki_uuid.is_some() || selector.name.is_some() {
                return Err(DomainError::with_details(
                    ErrorCode::InvalidRequest,
                    "режим auto не принимает crowdanki_uuid или name: укажи mode = explicit",
                    details! { "field" => "model" },
                ));
            }

            let usable: Vec<&NoteModel> = index
                .models
                .iter()
                .copied()
                .filter(|model| schema_problems(model).is_empty())
                .collect();

            let compatible: Vec<&NoteModel> = usable
                .into_iter()
                .filter(|model| {
                    model.flds.len() == required_fields.len()
                        && model
                            .flds
                            .iter()
                            .all(|field| required_fields.contains(&field.name))
                })
                .collect();

            let used: Vec<&NoteModel> = compatible
                .iter()
                .copied()
                .filter(|model| notes_in_subtree(index, &subtree, model) > 0)
                .collect();

            let candidates = if used.is_empty() { &compatible } else { &used };

            let evidence = if !used.is_empty() {
                "единственная модель, использованная заметками целевой колоды и совместимая с набором полей запроса"
            } else if compatible.len() == 1 {
                "в поддереве целевой колоды нет заметок совместимых моделей; совместимая модель экспорта единственная"
            } else {
                "совместимых моделей экспорта несколько"
            };

            match candidates.as_slice() {
                [only] => {
                    let model = usable_model(only)?;
                    finish(index, &subtree, deck, ModelMode::Auto, model).map(|resolved| {
                        ResolvedModel {
                            evidence: evidence.to_string(),
                            ..resolved
                        }
                    })
                }
                [] => Err(DomainError::with_details(
                    ErrorCode::UnknownModel,
                    format!(
                        "в экспорте нет модели, совместимой с набором полей запроса ({})",
                        required_fields
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    details! {
                        "field" => "model",
                        "model_mode" => "auto",
                        "rejected" => index
                            .models
                            .iter()
                            .map(|model| {
                                let problems = schema_problems(model);
                                let reason = if problems.is_empty() {
                                    format!(
                                        "поля модели: {}",
                                        model.flds.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")
                                    )
                                } else {
                                    problems.join("; ")
                                };
                                details! {
                                    "crowdanki_uuid" => model.crowdanki_uuid.clone(),
                                    "model_name" => model.name.clone(),
                                    "reason" => reason,
                                }
                            })
                            .collect::<Vec<_>>(),
                    },
                )),
                many => Err(DomainError::with_details(
                    ErrorCode::AmbiguousModel,
                    format!(
                        "набор полей запроса совместим с {} моделями; укажи mode = explicit и crowdanki_uuid",
                        many.len()
                    ),
                    details! {
                        "field" => "model",
                        "model_mode" => "auto",
                        "candidates" => many
                            .iter()
                            .map(|model| details! {
                                "crowdanki_uuid" => model.crowdanki_uuid.clone(),
                                "model_name" => model.name.clone(),
                                "notes_in_subtree" => notes_in_subtree(index, &subtree, model),
                                "field_count" => model.flds.len(),
                            })
                            .collect::<Vec<_>>(),
                    },
                )),
            }
        }
    }
}

/// Достраивает разрешённую модель: поля в порядке `ord` и свидетельства.
fn finish(
    index: &ExportIndex<'_>,
    subtree: &std::ops::Range<usize>,
    deck: &ResolvedDeck,
    mode: ModelMode,
    model: &NoteModel,
) -> Result<ResolvedModel, DomainError> {
    Ok(ResolvedModel {
        mode,
        crowdanki_uuid: model.crowdanki_uuid.clone().unwrap_or_default(),
        name: model.name.clone().unwrap_or_default(),
        model_kind: model_kind(model),
        fields: ordered_fields(model)?,
        notes_in_deck: index
            .notes
            .iter()
            .filter(|note| note.node == deck.preorder)
            .filter(|note| note.note.note_model_uuid == model.crowdanki_uuid)
            .count(),
        notes_in_subtree: notes_in_subtree(index, subtree, model),
        evidence: match mode {
            ModelMode::Explicit => "модель задана в запросе явно".to_string(),
            ModelMode::Auto => String::new(),
        },
    })
}

/// Модель, выбранная явно.
fn explicit_model<'a>(
    index: &'a ExportIndex<'a>,
    selector: &ModelSelector,
) -> Result<&'a NoteModel, DomainError> {
    let by_uuid = selector.crowdanki_uuid.as_deref();
    let by_name = selector.name.as_deref();

    match (by_uuid, by_name) {
        (Some(_), Some(_)) => Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            "mode = explicit принимает ровно одну ссылку на модель: crowdanki_uuid или name",
            details! { "field" => "model" },
        )),
        (None, None) => Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            "mode = explicit требует crowdanki_uuid или name",
            details! { "field" => "model" },
        )),
        (Some(uuid), None) => index.model_by_uuid(uuid).ok_or_else(|| {
            DomainError::with_details(
                ErrorCode::UnknownModel,
                format!("в экспорте нет модели с crowdanki_uuid {uuid}"),
                details! {
                    "field" => "model.crowdanki_uuid",
                    "crowdanki_uuid" => uuid,
                    "known_models" => index
                        .models
                        .iter()
                        .map(|model| model.crowdanki_uuid.clone())
                        .collect::<Vec<_>>(),
                },
            )
        }),
        (None, Some(name)) => {
            let matches: Vec<&NoteModel> = index
                .models
                .iter()
                .copied()
                .filter(|model| model.name.as_deref() == Some(name))
                .collect();
            match matches.as_slice() {
                [only] => Ok(only),
                [] => Err(DomainError::with_details(
                    ErrorCode::UnknownModel,
                    format!("в экспорте нет модели с именем {name:?}"),
                    details! {
                        "field" => "model.name",
                        "model_name" => name,
                        "known_models" => index
                            .models
                            .iter()
                            .filter_map(|model| model.name.clone())
                            .collect::<Vec<_>>(),
                    },
                )),
                many => Err(DomainError::with_details(
                    ErrorCode::AmbiguousModel,
                    format!(
                        "имя модели {name:?} носят {} модели; укажи crowdanki_uuid",
                        many.len()
                    ),
                    details! {
                        "field" => "model.name",
                        "model_name" => name,
                        "candidates" => many
                            .iter()
                            .map(|model| details! {
                                "crowdanki_uuid" => model.crowdanki_uuid.clone(),
                                "model_name" => model.name.clone(),
                            })
                            .collect::<Vec<_>>(),
                    },
                )),
            }
        }
    }
}

/// Проверяет, что схема модели пригодна, и возвращает её же.
fn usable_model(model: &NoteModel) -> Result<&NoteModel, DomainError> {
    let problems = schema_problems(model);
    if problems.is_empty() {
        Ok(model)
    } else {
        Err(DomainError::with_details(
            ErrorCode::ModelSchemaUnusable,
            format!(
                "схема полей модели «{}» непригодна: {}",
                model.name.as_deref().unwrap_or("(без имени)"),
                problems.join("; ")
            ),
            details! {
                "crowdanki_uuid" => model.crowdanki_uuid.clone(),
                "model_name" => model.name.clone(),
                "problems" => problems,
            },
        ))
    }
}

/// Число заметок модели в поддереве колоды.
fn notes_in_subtree(
    index: &ExportIndex<'_>,
    subtree: &std::ops::Range<usize>,
    model: &NoteModel,
) -> usize {
    index
        .notes
        .iter()
        .filter(|note| subtree.contains(&note.node))
        .filter(|note| note.note.note_model_uuid == model.crowdanki_uuid)
        .count()
}
