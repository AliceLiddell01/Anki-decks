# anki-repo

`anki-repo` — небольшой read-only toolkit для детерминированного анализа одного
CrowdAnki-экспорта репозитория `Anki-decks`. Он даёт агенту быстрый и
предсказуемый доступ к структуре `deck.json`, не требуя читать многомегабайтный
JSON вручную.

Tool **ничего не изменяет**: он не является редактором, не умеет писать
`deck.json`, не копирует и не переименовывает media, не трогает планировщик Anki
и не выполняет никаких мутаций. Все операции — только чтение.

## Зачем

- Один вызов вместо ручного `jq`/скрипта: `inspect`, `find`, `stats`, `validate`.
- Разрешение имён полей через `note_model_uuid` → `note_models[].flds[].ord`
  сосредоточено в одной реализации, поэтому агент не повторяет эту логику.
- Стабильный machine-readable вывод: одинаковый JSON на одинаковых данных.
- Человекочитаемый вывод по умолчанию и отдельный JSON-режим для автоматизации.

## Требования

- Rust toolchain с edition 2024 (проверено на `rustc`/`cargo` 1.98.1).
- Никаких системных сервисов и сети: tool работает только с локальными файлами.

## Сборка и запуск

```bash
cd tools/anki-repo
cargo build --release
./target/release/anki-repo --help

# или без установки
cargo run --quiet -- inspect ../../decks/japanese/words/Words__N3
```

Пути к экспортам можно задавать и относительно текущего каталога, и абсолютно.

## Команды

Во всех примерах ниже `EXPORT` — каталог экспорта, например
`decks/japanese/words/Words__N3`. Указывать нужно именно каталог: путь к самому
`deck.json` отвергается с понятной ошибкой.

### `inspect`

Компактное описание структуры экспорта: корневая колода, число узлов колоды,
общее число заметок, модели заметок с их полями в порядке `ord`, конфигурации
колод, сводка по media и распределение заметок по путям колод.

```bash
anki-repo inspect decks/japanese/words/Words__N1
anki-repo inspect decks/japanese/words/Words__N1 --verbose
```

`--verbose` добавляет UUID, дерево колод в preorder, шаблоны карточек, статистику
по `guid` и bounded sample диагностики (в том числе примеры отсутствующих и
необъявленных media-имён). В JSON-режиме verbose-часть лежит в
`result.verbose` и появляется только при `--verbose`.

### `find`

Поиск заметок по стабильным критериям. Нужно ровно одно из трёх:

```bash
# поиск по guid: identity lookup
anki-repo find decks/japanese/words/Words__N1 --guid 'D!TAYVuHi,'

# сокращение для contains по сырому значению поля «Слово»
anki-repo find decks/japanese/words/Words__N1 --word 偶然

# общий поиск по имени поля
anki-repo find decks/japanese/words/Words__N2 --field 'Часть речи' --value Существительное
anki-repo find decks/japanese/words/Words__N4 --field Значение --value 'снег' --match exact
```

Поведение:

- `--word` работает как `contains` по полю `Слово`.
- `--match` принимает `contains` (по умолчанию) или `exact` и применяется только
  вместе с `--field`.
- `--deck` ограничивает поиск колодой и всеми её вложенными колодами.
- `--limit` (по умолчанию 20, максимум 500) ограничивает размер вывода, но не
  сам поиск: `matched_total` считает все совпадения, а `returned`/`truncated`
  показывают, что именно попало в ответ.
- Сопоставление чувствительно к регистру и выполняется по сырым значениям поля,
  без нормализации HTML.

### `stats`

Структурная агрегированная статистика: число заметок по колодам, по моделям,
использование конфигураций, разбивка полей на пустые и непустые, сводка по
media.

```bash
anki-repo stats decks/japanese/words/Words__N3
anki-repo stats decks/japanese/words/Words__N3 --group-by 'Часть речи' --top 10
```

- `--group-by` считает распределение по сырым значениям указанного поля; `--top`
  (по умолчанию 20, максимум 500) ограничивает размер вывода, а
  `distinct_values` и `truncated` показывают полную картину.
- Столбцы распределения сортируются по убыванию количества, затем по значению.

### `validate`

Детерминированная проверка структурной целостности одного экспорта.

```bash
anki-repo validate decks/japanese/words/Words__N1
anki-repo --json validate decks/japanese/words/Words__N1
```

`valid` равно `true`, если не найдено ни одного issue уровня `ERROR`.
Предупреждения (`WARNING`) и информационные отметки (`INFO`) не делают экспорт
невалидным. Path-проблемы (нет каталога, нет `deck.json`) — это доменная ошибка
с exit code 3, а проблемы содержимого (включая невалидный JSON) — это issues с
exit code 6.

Уровни и коды issue:

| Уровень | Коды |
|---|---|
| `ERROR` | `invalid_json`, `root_not_deck`, `schema_invalid`, `node_not_deck`, `duplicate_note_model_uuid`, `duplicate_deck_config_uuid`, `note_model_identity_missing`, `deck_config_identity_missing`, `note_model_uuid_missing`, `note_model_unresolved`, `deck_config_unresolved`, `note_fields_count_mismatch`, `note_field_value_not_string`, `note_guid_missing`, `duplicate_note_guid`, `field_name_duplicate`, `field_ord_invalid`, `field_ord_negative`, `field_ord_duplicate`, `field_ord_out_of_range`, `field_ord_gap`, `template_field_unresolved` |
| `WARNING` | `media_reference_undeclared`, `media_files_duplicate`, `media_physical_missing`, `media_dir_missing`, `media_name_not_basename`, `template_construct_unchecked`, `deck_config_uuid_missing`, `deck_name_missing`, `conflicting_note_model_definition`, `conflicting_deck_config_definition`, `template_ord_invalid`, `template_ord_duplicate` |
| `INFO` | `export_summary`, `media_physical_unused`, `note_model_unused`, `deck_config_unused`, `empty_field_values`, `issues_truncated` |

Соглашения, важные для интерпретации результата:

- Отсутствующий физически media-файл — это `WARNING`, а не `ERROR`: политика
  хранения media в репозитории отдельно не определена.
- Имена media сравниваются только по basename; пути из `media_files` никогда не
  используются для доступа к файловой системе.
- Повторное объявление одной и той же модели/конфигурации с тем же определением
  (обычная ситуация во вложенных узлах CrowdAnki) не считается ошибкой; конфликт
  разных определений под одним UUID — это `WARNING`.
- Ограниченный парсер шаблонов проверяет только простые ссылки вида
  `{{Поле}}`. Конструкции с фильтрами (`{{tts ja_JP:Поле}}`), незакрытые блоки и
  прочие сложные случаи помечаются как `template_construct_unchecked`
  (`WARNING`), чтобы не выдавать ложных ошибок.
- Число issues одного кода ограничено; при усечении добавляется
  `issues_truncated` с числом опущенных записей.

## Режимы вывода

- Обычный режим: человекочитаемый текст на русском языке в `stdout`. Длинные
  значения полей приводятся к одной строке и ограничиваются по длине.
- `--json` (можно указывать до или после подкоманды): ровно один JSON-документ
  в `stdout`, ничего в `stderr`.

Успешный ответ:

```json
{
  "schema_version": 1,
  "command": "inspect",
  "result": { }
}
```

Ошибка:

```json
{
  "schema_version": 1,
  "command": "find",
  "error": {
    "code": "not_found",
    "message": "по заданному критерию не найдено ни одной заметки",
    "details": { }
  }
}
```

`schema_version` описывает контракт вывода; `code` — стабильный snake_case
идентификатор, не зависящий от имён внутренних типов Rust.

В JSON-режиме поля заметки представлены объектом, ключи которого идут в порядке
`ord` модели. Если модель повреждена и содержит повторяющиеся имена полей, к
повторному ключу добавляется суффикс `#N`, чтобы объект оставался однозначным.

## Exit codes

| Код | Значение |
|---|---|
| `0` | Успех |
| `1` | Зарезервировано |
| `2` | Ошибка использования CLI (неизвестный аргумент, несовместимая комбинация, значение вне диапазона) |
| `3` | Неверный ввод: нет каталога, нет `deck.json`, невалидный JSON, корень не является `Deck`, неизвестное поле или колода |
| `4` | `find` не нашёл ни одного совпадения |
| `5` | `find --guid` нашёл несколько заметок с одним `guid` |
| `6` | `validate` нашёл хотя бы один `ERROR` |
| `70` | Неожиданная внутренняя ошибка |

Коды ошибок (`error.code`) и их exit codes:

| Код | Exit | Когда |
|---|---|---|
| `usage` | 2 | Несовместимые аргументы, обнаруженные вне clap |
| `input_unreadable` | 3 | Каталог экспорта недоступен или передан путь к файлу |
| `deck_json_missing` | 3 | В каталоге нет `deck.json` |
| `invalid_json` | 3 | `deck.json` не является валидным JSON |
| `root_not_deck` | 3 | Корень не является CrowdAnki `Deck` |
| `schema_invalid` | 3 | JSON валиден, но не соответствует типизированному ядру |
| `unknown_field` | 3 | Указанного поля нет ни в одной модели экспорта |
| `unknown_deck` | 3 | Указанная колода не найдена |
| `not_found` | 4 | Нет совпадений |
| `ambiguous` | 5 | Неоднозначный `guid` |
| `internal_error` | 70 | Внутренняя ошибка |

## Детерминированность

Одинаковые входные данные дают побайтово одинаковый вывод:

- узлы колод обходятся в preorder;
- заметки сохраняют порядок экспорта;
- поля модели выводятся в порядке `ord`;
- распределения сортируются по количеству (по убыванию), затем по значению;
- issues в `validate` сортируются по уровню, коду, пути колоды, месту и тексту.

## Ограничения

- Tool намеренно умеет работать только с одним экспортом за вызов.
- Media не копируются и не сравниваются по содержимому: сравниваются только имена.
- Поиск не поддерживает регулярные выражения и не нормализует HTML.
- Культура `--word` привязана к конкретному имени поля `Слово`, используемому в
  словарных колодах; для других полей используйте `--field`.
- Культура `--deck` требует точного пути колоды, как он собран из `name` узлов.
- Tool не проверяет семантику японского содержимого и не сравнивает колоды между
  собой.

## Проверки

```bash
cd tools/anki-repo
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo nextest run
```

Набор тестов состоит из:

- unit-тестов доменной логики (разрешение полей через `ord`, обход дерева,
  проверки `find`/`stats`, сканер шаблонов, контракт exit codes);
- `tests/synthetic.rs` — синтетические fixtures на каждую ветку validation и
  каждую доменную ошибку;
- `tests/real_decks.rs` — канонические колоды `Words__N1..N5`, где ожидаемые
  значения пересчитываются из сырого JSON и из содержимого `media/`, а не
  зашиты в код;
- `tests/cli_contract.rs` — аргументы CLI, exit codes и разделение
  `stdout`/`stderr` через реальный binary.
