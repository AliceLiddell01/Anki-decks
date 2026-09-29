# Asset store и `kanji-assets`

`asset-store` — общий Rust core для program-owned assets. Он не зависит от
CrowdAnki, Anki, Yarxi или формата карточек. Бинарник `kanji-assets` задаёт
kanji-specific identity и использует этот core как единственного владельца
manifest, content hash, integrity и lifecycle.

## Граница владения

Инструмент работает только в явно выбранном store root. По умолчанию это
`.asset-store/kanji` в найденном корне Cargo workspace; если workspace не найден,
путь берётся относительно текущего каталога. Содержимое исключено из Git.
Для kanji CLI store проверяет, что root не пересекается с `decks/`: учитываются
заданный `--repository-root`, текущий каталог и прямые `decks/` у предков пути
store. Проверка смотрит только границы путей и не обходит `decks/`.

Root с `..`, symlink-компонентом или чужими файлами не принимается. Core не
сканирует соседние каталоги, не следует symlink из store и не удаляет неизвестные
файлы. Kanji CLI отклоняет `--file`, если он находится внутри обнаруженного
`decks/` дерева, включая путь через symlink. Explicit ingest читает ровно один
переданный файл вне этих деревьев; source должен быть обычным файлом без symlink.
Пользовательские `decks/**/media/` и CrowdAnki `media_files` не являются
источниками или частью asset store.

На Linux store root открывается по компонентам через directory handles без
следования symlink; последующие операции привязаны к открытому root handle, а не
повторно разрешают исходный pathname. Для инициализации принимается только новый
или существующий пустой root. Любой непустой root без согласованных `.owner.json`
и `manifest.json` отклоняется без записи; одинокий manifest не усыновляется.
Прерванная инициализация без обоих файлов ownership не восстанавливается
автоматически и требует отдельного ручного решения.

`kanji-assets ingest` открывает source один раз без follow конечного symlink,
проверяет protected boundary для фактически открытого объекта и копирует bytes из
того же file descriptor. Замена pathname после проверки не меняет прочитанный
объект.

Внутри root лежат `.owner.json`, `.lock`, versioned `manifest.json`, каталог
`objects/` с неизменяемыми объектами `objects/<sha256>.blob` и каталог `.tmp/` для
временной публикации. Objects адресуются фактическим SHA-256 bytes, а не именем
исходного файла; после публикации они помечаются read-only. Классификация формата
берётся из сигнатуры bytes (`png`, `jpeg`, `gif`, `webp`, `bmp`, `tiff` или
`unknown`), а не из расширения.

Объекты хранятся в одном content-addressed каталоге для всех lifecycle-состояний.
Доверенный набор определяется только записями manifest с `lifecycle: verified`;
сам факт наличия файла в `objects/` не означает доверия.

`manifest.json` — единственный canonical индекс assets и lifecycle. Его верхний
уровень содержит `schema_version`, `store_id`, `revision` и отсортированный список
`assets`. Запись сохраняет:

- identity `{namespace, key}`;
- `storage_path`, SHA-256, `byte_length` и обнаруженный `format`;
- provenance (`source_kind`, исходное имя без абсолютного host path);
- lifecycle `pending`, `verified` или `quarantined`;
- semantic decision с `status`, `validator.id`, `validator.version`, hash
  проверенных bytes и `evidence`;
- необязательный `domain_metadata`, который generic core не интерпретирует.

Для kanji в `domain_metadata` сохраняются точная строка `character` и список
`unicode_codepoints` вида `U+6F22`. Строка не нормализуется и не выводится из имени
файла. Unicode representation здесь фиксирует identity, но не доказывает, что
изображение содержит этот символ; последовательности с variation selector также
сохраняются посимвольно.

Каждый read и mutation проверяет schema, связи lifecycle/decision и соответствие
файлов manifest по hash, размеру и формату. Изменённый или отсутствующий object,
повреждённый/неподдерживаемый manifest, path traversal и symlink дают fail-closed
ошибку. Записи manifest публикуются через синхронизированный временный файл и
атомарную замену; `.tmp/` не считается canonical state. Межпроцессные readers и
writers сериализуются shared/exclusive directory lock и `.lock`; открытый directory
handle удерживает операции в том же store root при изменении его pathname.

## Lifecycle и validation

Новый ingest создаёт только `pending` candidate. Повтор тех же identity и bytes
даёт `already_present` без новой записи. Другой hash для существующей identity
возвращает `identity_conflict`; явная замена требует
`--replace-expected-sha256 <текущий-hash>` и сбрасывает asset в `pending` без
старого semantic decision.

Generic `SemanticValidator` принимает явные id/version, фактические bytes и
возвращает один из результатов `verified`, `rejected`, `uncertain`, `corrupt` с
непустым evidence. Core сохраняет decision только после успешного вызова
validator'а и привязывает его к текущему content hash и версии validator'а.
Только `verified` переводит запись в trusted lifecycle; остальные три результата
попадают в `quarantined`. Техническая ошибка validator'а сохраняется как blocker,
не создаёт decision и не повышает доверие.

`new` выбирает записи без актуального decision для пары текущий content hash +
validator id/version. `full` выбирает все активные записи manifest независимо от
предыдущего решения. Оба режима работают по manifest этого store; filesystem
`mtime` и каталоги вне root не участвуют. Повторное решение с тем же содержимым
может быть no-op для persistent state.

## CLI

Команды запускаются из корня workspace:

```bash
cargo run --quiet --bin kanji-assets -- --output json init
cargo run --quiet --bin kanji-assets -- --output json ingest \
  --character 漢 --file ./candidate.png
cargo run --quiet --bin kanji-assets -- --output json list
cargo run --quiet --bin kanji-assets -- --output json plan \
  --mode new --validator-id kanji-cv --validator-version v1
cargo run --quiet --bin kanji-assets -- --output json plan \
  --mode full --validator-id kanji-cv --validator-version v1
```

`--store <path>` выбирает root, `--repository-root <path>` задаёт checkout,
относительно которого дополнительно защищается `decks/`. `ingest` импортирует
ровно один явный файл. Формат и hash определяются по bytes. Для явной замены
добавьте `--replace-expected-sha256 <текущий-hash>`.

`init` сообщает `initialized` и `changed: true` при создании state либо
`already_initialized` и `changed: false` при повторе. Все команды отражают
инициализацию нового store в поле `changed`.

`list` проверяет integrity и выводит manifest. `plan` выполняет общий `new` или
`full` selection и возвращает выбранные записи, текущие состояния, hashes,
validation/evidence, store и режим. Сейчас `plan` только планирует: он включает
blocker `semantic_validator_unavailable`, не меняет state и не утверждает
`VERIFIED`. Для будущего backend библиотека предоставляет `SemanticValidator` и
`AssetStore::validate`; production always-true validator отсутствует.

JSON имеет `schema_version`, `operation`, `store`, `mode`, `assets`, `changed`,
`conflicts`, `blockers` и `outcome`; ошибка добавляет стабильные `error.code`,
`category`, `message` и `details`. Ключевые error codes: `identity_conflict`,
`boundary_violation`, `path_traversal`, `store_not_owned`,
`unsupported_schema_version`, `manifest_corrupt`, `missing_asset_file`,
`integrity_mismatch`, `source_file_missing`, `invalid_validation_evidence` и
`io_failure`. Exit categories:
`0` — успешная операция или no-op, `3` — invalid input/conflict, `4` — boundary,
schema или integrity blocker, `5` — I/O failure. Для точного контракта используйте
JSON `error.code`, а не prose или exit code отдельно.

## Ограничения текущего этапа

- сетевого acquisition и браузерной логики нет;
- Yarxi integration ещё нет;
- kanji semantic CV/OCR/visual validator ещё нет;
- наличие candidate asset не означает `verified`;
- инструмент не подтверждает соответствие изображения character identity;
- пользовательские Anki media не входят в store;
- assets не копируются в `decks/**/media/`, `media_files` и `anki-repo create` не
  изменяются;
- реальные third-party fixtures и решения о распространении будущего корпуса не
  добавлены.

Тесты используют только синтетические bytes и временные каталоги. Общие проверки
запускаются из workspace по контракту корневого `AGENTS.md`.
