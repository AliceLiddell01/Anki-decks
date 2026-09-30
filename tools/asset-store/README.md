# Asset store и `kanji-assets`

`asset-store` — общий Rust core для program-owned assets. Он владеет manifest,
content hash, integrity и lifecycle. Предметный CLI `kanji-assets` добавляет
поиск статьи Yarxi, проверку изображения кандзи и публикацию подтверждённых
файлов в canonical corpus.

Инструмент не читает `decks/**/media/`, не меняет `media_files` и не копирует
изображения в колоды.

## Где лежат данные

Корень по умолчанию — `.asset-store/kanji` в корне workspace. Если он уже
существует, `list`, `plan` и `validate` работают только с ним. `ensure` создаёт
store при первом вызове.

В store разделены публикуемое состояние и локальные записи проверки:

```text
.asset-store/kanji/
├── .owner.json             # tracked marker владельца
├── manifest.json           # tracked: только VERIFIED
├── assets/                 # tracked: bytes VERIFIED изображений
├── .runtime/               # ignored: локальные кандидаты и карантин
│   ├── .owner.json         # тот же store_id
│   ├── manifest.json       # Pending и Quarantined
│   ├── assets/             # bytes локальных кандидатов и карантина
│   └── .tmp/               # ignored staging runtime-состояния
├── .lock                    # ignored lock
└── .tmp/                    # ignored staging и транзакционные файлы
```

`.runtime/manifest.json` использует тот же `store_id`, что и публикуемый
manifest. `verify_integrity` проверяет оба состояния и возвращает их записи.
Read-only publishability gate подтверждает canonical corpus и проверяет, что
локальный runtime store, если он есть, структурно исправен и не пересекается с
опубликованными identity. Runtime assets при этом не становятся публикуемыми.
Отсутствующий `.asset-store/kanji` не считается ошибкой: репозиторий может пока
не содержать corpus. При открытии legacy store с каноническим manifest
`schema_version: 3` старые `Pending` и `Quarantined` записи переносятся из
корневого manifest в `.runtime/` под exclusive lock. Store с `schema_version: 1`
и каталогом `objects/` нужно создать заново: он не мигрируется.
Если в `.runtime/` уже есть запись с той же identity, при восстановлении она
сохраняется, а старая canonical запись и bytes удаляются.

Например, изображение `漢` в GIF-формате хранится в `assets/漢.gif`, а в PNG —
в `assets/漢.png`. Расширение выводится из magic bytes. SHA-256 остаётся в
manifest и проверяется вместе с размером, форматом, identity и semantic
decision; hash не входит в имя kanji asset. Для других namespace общего core
сохраняется hash-suffixed layout.

`.gitignore` разрешает публиковать только `.owner.json`, `manifest.json` и
изображения `.gif`/`.png` в `assets/`; `.runtime/`, lock и незавершённая
публикация остаются локальными. Read-only gate отклоняет non-VERIFIED записи,
orphan-файлы и незарегистрированные bytes в tracked corpus. Другие каталоги и
файлы store не усыновляются. Root с `..`, symlink-компонентом, неожиданными
файлами или пересечением с `decks/` отвергается.

Каждый канонический image связан в manifest с:

- identity `{namespace: "kanji", key: "<символ>"}`;
- фактическим SHA-256, размером и форматом;
- provenance Yarxi и domain metadata с символом, Unicode, номером статьи,
  выбранным source URL и результатом выбора GIF/PNG;
- semantic status, validator id/version, content hash решения и pixel evidence.

Acquisition evidence содержит фактические сведения browser runtime, важные для
воспроизведения rendered PNG: product, protocol version, revision, user agent,
JavaScript version и источник выбора исполняемого файла (`CHROME_BIN`,
`CHROMIUM_BIN`, `PATH`, Playwright cache или default `chromiumoxide`). Абсолютный
путь, профиль браузера и cookies не сохраняются.

Manifest schema сейчас `3`. Замена bytes для существующей identity требует
compare-and-swap по ожидаемому старому hash. Kanji-файл публикуется атомарным
rename из staging; transaction marker и backup в `.tmp/` позволяют при открытии
store восстановить завершённый manifest commit либо откатить незавершённую
замену. Чтение остаётся fail-closed: текущий SHA-256 сверяется с manifest до
использования bytes. У generic hash-suffixed объекта авария до manifest commit
может оставить проверяемый orphan-файл; он не считается asset без manifest
записи.

Generic `AssetStore::ingest` сохраняет `Pending` candidate в `.runtime/`; такой
файл не попадает в tracked corpus. `AssetStore::ingest_verified` держит bytes во
временном staging и публикует их в canonical `assets/` только после ответа
`VERIFIED`. `REJECTED`, `UNCERTAIN`, `CORRUPT` и технический сбой не публикуют
bytes в canonical corpus. При повторной проверке уже опубликованного asset,
который больше не получает `VERIFIED`, его bytes и запись сначала сохраняются в
локальном runtime quarantine, затем запись и bytes атомарно убираются из
tracked corpus.

Публикуемость можно проверить из корня workspace отдельной read-only командой:

```bash
cargo run --quiet --locked -p asset-store --bin kanji-corpus-gate
```

Gate возвращает ненулевой код, если в canonical manifest есть состояние кроме
`VERIFIED` или решение другого валидатора, если в canonical `assets/` обнаружены
orphan или незарегистрированные bytes, либо если `.runtime/` не проходит
структурную проверку или пересекается с публикуемой identity. Проверка ничего не
меняет и не считает runtime bytes частью
canonical corpus.

## Получение с Yarxi

`ensure` открывает отдельную headless Chromium-сессию и работает с фактическим
динамическим интерфейсом `https://www.yarxi.su/`. Поиск ограничен контейнером
поиска иероглифов, его полем «Чтение» и кнопкой «Найти». После открытия вкладки
«Информация» provider требует точный Unicode code point запрошенного символа;
несоответствующая статья отвергается. Номер статьи и source metadata входят в
provenance.

Обычный `ensure` сохраняет исходный приоритет primary `img.kakijun-gif` и
читает её GIF bytes из browser resource layer. Fallback разрешён только после
доказанного отсутствия GIF: вкладка и media area готовы, точный Unicode
подтверждён, относящиеся к странице запросы завершились без network/HTTP/JS
ошибок, DOM стабилен две секунды и элемента GIF нет. Pending или failed GIF,
таймаут, незавершённая картинка, ошибка сети или JavaScript дают acquisition
failure; эти состояния никогда не считаются `gif_absent`.

Обычный `ensure` использует `AcquisitionTarget::PreferredSource`: сначала
загружает primary GIF. К изображению `.font-sample` можно перейти только после
подтверждённого отсутствия GIF. Если у крайней слева допустимой плитки есть
подходящий исходный PNG, provider читает его bytes; если изображение нарисовано
шрифтом, provider делает element screenshot плитки. Явный target
`AcquisitionTarget::RenderedFontSamplePng` запрашивает именно rendered PNG даже
при наличии GIF; обычный `ensure` сохраняет приоритет GIF.

При подтверждённом отсутствии provider выбирает крайнюю слева видимую
допустимую плитку `.font-sample`; классы `stroke-order` исключаются. Если у
плитки есть допустимый исходный PNG, default fallback читает его bytes. Если
нужный образец отрисован текстом, provider устанавливает воспроизводимую тёмную
схему страницы: `prefers-color-scheme: dark` и `data-theme="dark"` на корневом
элементе Yarxi. На уровне страницы `#app` получает цвет из текущей dark palette,
поскольку его обычное CSS-правило задаёт светлый текст независимо от темы. Сам
`.font-sample` provider не перекрашивает и не перестраивает. Он снимает только
DOM-элемент плитки, сохраняя фон, рамку, отступы и rendered glyph; полностраничный
screenshot не используется. В provenance включены выбранная плитка, CSS rect,
computed цвета, параметры шрифта, viewport, device scale и фактические размеры
PNG.

Правая ссылка `kakijun.jp` не читается как изображение и запрещена как источник.
Для `blob:`/`data:` разрешённых default fallback изображений bytes читаются из
browser context. Перед semantic validation сверяются GIF/PNG magic bytes; затем
декодер проверяет полный файл, размеры, непустую область рисунка и GIF frame
stream. Browser-rendered tile остаётся в исходном масштабе; фактические размеры,
viewport и device scale сохраняются в provenance.

### Ручная приёмка тёмных PNG

`yarxi_dark_acceptance` — ручной acceptance harness для проверки реального
rendered PNG и HTML-просмотра результата. Он получает символы из внешнего JSON
плана; число элементов и групп не фиксировано. План должен содержать непустой
`items`, где у каждого элемента есть уникальный `character`, принятый
`KanjiCharacter` как поддерживаемый Han/CJK scalar. Поле `group` необязательное
и должно быть непустым, если задано. Поля
`unicode_codepoint`, `yarxi_article_number` и `frequency_index` необязательны.
Провайдер всегда сверяет точный Unicode символа со статьёй Yarxi; если указать
`unicode_codepoint`, он также должен совпадать с символом. Номер статьи и индекс
частотности сравниваются с evidence Yarxi, если они заданы.

Например, план можно сохранить вне checkout:

```json
{
  "items": [
    {
      "character": "漢",
      "group": "пример",
      "unicode_codepoint": "U+6F22",
      "yarxi_article_number": 123,
      "frequency_index": 45
    },
    { "character": "字" }
  ]
}
```

Запуск из корня workspace:

```bash
cargo run --locked -p asset-store --bin yarxi_dark_acceptance -- \
  --plan ../yarxi-plan.json
```

При необходимости можно явно разрешить обработку точного Yarxi TLS
interstitial флагом `--allow-insecure-tls`. По умолчанию TLS exception выключен.
Без `--output` отчёт создаётся в системном temp-каталоге вне checkout. Для
постоянного пути передайте новый каталог вне checkout через `--output <DIR>`;
родительский каталог должен существовать.
Отчёт содержит полученные PNG, `evidence.json` и `index.html`; он не добавляет
кандидаты в asset store и не публикует их. Live plan и результаты ручного
прогона остаются локальными и не коммитятся. До отдельной публикации владелец
репозитория вручную просматривает реальные изображения, даже если автоматическая
семантическая проверка вернула `VERIFIED`.

Provider обрабатывает batch последовательно. Для запуска нужен Chrome/Chromium;
путь можно явно задать через `CHROME_BIN` или `CHROMIUM_BIN`.
Также проверяются `PATH` и каталог Playwright cache. Browser profile, cookies и
session state не сохраняются в store.

### Сертификат Yarxi

Сейчас Chrome может показать `ERR_CERT_AUTHORITY_INVALID` для Yarxi. По умолчанию
provider прекращает работу. Флаг `--allow-insecure-tls` разрешает только нажать
Chrome interstitial «Перейти на сайт» для исходного URL на точном host
`www.yarxi.su`, при точном коде `ERR_CERT_AUTHORITY_INVALID` без учёта регистра
и наличии ссылки перехода. Глобальный
`--ignore-certificate-errors` и отключение TLS-проверки в процессе не включаются.
Флаг не следует переносить в другие команды или источники.

## Semantic validator и benchmark

`KanjiImageValidator` анализирует пиксели PNG/GIF, а не имя файла, URL, DOM text
или provenance. PNG декодируется целиком. `GifDecoder::into_frames()` выдаёт
композитные кадры с учётом GIF disposal; validator проверяет все кадры и берёт
кадр с наибольшей площадью foreground.

Основной reference catalog построен из KanjiVG release `r20250816`, commit
`bd13ffbcc9d85cb86ae98bbbf001d9069220b901`. Исходные Unicode-addressed SVG
нормализованы в 64×64 бинарные маски; catalog содержит 6 430 символов и включён
как сжатый `src/data/kanjivg-r20250816.maskdb.zlib` с SHA-256
`5581e65d5a681cdf8e441be04c1cea5f6ec61342ce8830ed083ba417d83a88ff`. KanjiVG
распространяется под CC BY-SA 3.0. Attribution, release и SHA-256 исходного
архива зафиксированы рядом с generated data в
`src/data/kanjivg-ATTRIBUTION.txt`.

Для dark tile добавлен дополнительный pixel reference из Noto Serif JP — того
шрифта, который Yarxi реально использует в `.font-sample`. Зафиксированный TTF
имеет SHA-256
`e6cffcd5cae6a298ddfd17173b42d64888913976b2d7053024a9ecf0cf5d3fe8`. Из него
генерируются 6 428 нормализованных масок в
`src/data/yarxi-noto-serif-jp-r1.maskdb.zlib` с SHA-256
`ea6231b13f23ed16705839b8c3e911f89fea67b91b6b315f3f562f75c7b0531c`; исходный
TTF не включён в репозиторий. Его происхождение и upstream license описаны в
`src/data/yarxi-noto-serif-jp-ATTRIBUTION.txt`.

Validator сначала оценивает фон по угловым участкам растра, выбирает полярность
по контрасту, удаляет связанные компоненты рамки у края и строит foreground
mask глифа. Reference builders и runtime validator используют общую функцию
`kanji_mask::normalize_binary_mask`: она обрезает растр по непустым foreground
bounds, вписывает его в область 56×56 и центрирует в маске 64×64. Каждый выходной
пиксель выбирает ближайший к центру пиксель source-cell по формуле
`(2*i+1)*source_size/(2*output_size)`. Так один алгоритм преобразует исходные
KanjiVG/Noto маски и candidate raster. Далее применяется симметричное Chamfer
distance между центрированными масками: среднее расстояние от каждого набора
пикселей до ближайшего пикселя другого набора, нормированное на размер маски.
Для light-on-dark PNG expected pixels сравниваются с KanjiVG и Noto Serif JP;
берётся меньшая из двух дистанций.
Ближайший другой Unicode ищется по объединению всех применимых pinned catalogs
KanjiVG и Noto Serif JP, чтобы обе метрики ожидаемого и конкурирующего символов
учитывали тот же набор шрифтов. Версия validator —
`kanjivg-r20250816-5581e65d-noto-serif-jp-24fc2d26-ea6231b1-mask64-v6`. Пороговые значения входят
в validator contract:

- `VERIFIED`: expected distance ≤ `0.020` и отрыв от ближайшего другого Unicode
  reference ≥ `0.004`;
- `REJECTED`: expected distance хуже ближайшего reference не менее чем на
  `0.015`;
- между этими условиями — `UNCERTAIN`;
- невалидные/битые/пустые bytes — `CORRUPT`; отсутствие reference — `UNCERTAIN`.

Изменение порогов требует новой версии validator и соответствующей regression
coverage. Неуверенное совпадение остаётся `UNCERTAIN`, а неподходящие пиксели не
подтверждаются по имени файла, статье или provenance.

Пороговую policy можно воспроизводимо проверить офлайн на закреплённых каталогах:

```bash
cargo test -p asset-store offline_threshold_calibration -- --nocapture
```

Benchmark берёт 16 glyph из pinned Noto Serif JP catalog, равномерно выбранных по
Unicode-порядку, строит для них синтетические rendered-scale плитки 142 px и
считает положительные expected distance/margin и статусы. Для каждого образца
он также проверяет ближайшую конкурирующую Unicode identity и запрещает ей статус
`VERIFIED`. Запуск печатает количество положительных результатов по статусам,
максимальное expected distance, минимальный margin, максимальный gap для чужой
identity и результаты её проверки. Набор фиксирован embedded reference DB и не
зависит от сети, ручной live-приёмки или текущего плана символов.

Для текущей версии validator v6 12 из 16 положительных синтетических случаев
получили `VERIFIED`, 4 — `UNCERTAIN`, 0 — `REJECTED`; максимум expected distance
составил `0.002642`, минимум margin — `0.001648`. Для ближайшей конкурирующей
identity все 16 случаев остались `UNCERTAIN` (0 `VERIFIED`, 0 `REJECTED`), а
максимальный gap составил `0.012586`. Эти результаты не меняют пороги.

Детерминированные benchmark/regression тесты используют встроенные Unicode
catalogs и синтетические raster bytes. Они проверяют:

- exact KanjiVG mask в PNG и animated GIF;
- тёмную Yarxi-style PNG-плитку с рамкой по Noto Serif JP и отказ для неверной
  Unicode identity;
- rendered-scale dark tile и отказ для близкой, но неверной Unicode identity;
- реальный поздний полный кадр против частичного раннего кадра;
- неверные близкие identity;
- увеличение толщины и обрезанный frame;
- битые signature/frame stream, blank image и отсутствующий reference;
- границы distance/margin/rejection threshold.

Calibration corpus покрывает закреплённые reference glyph и nearest-confusable
identity, но не оценивает долю ложных подтверждений для реальных изображений
Yarxi или на всём пространстве живых источников. Результат benchmark не заменяет
ручной просмотр live PNG; при недостаточном отличии validator оставляет решение
`UNCERTAIN`.

## Команды

Запускайте из корня workspace:

```bash
# Идемпотентно получить и проверить один или несколько кандзи.
cargo run --locked --bin kanji-assets -- \
  --allow-insecure-tls --output json ensure 漢 字

# Проверить только новые/устаревшие semantic decisions.
cargo run --locked --bin kanji-assets -- --output json validate --mode new

# Перепроверить все assets.
cargo run --locked --bin kanji-assets -- --output json validate --mode full

# Просмотреть состояние и детерминированный план проверки.
cargo run --locked --bin kanji-assets -- --output json list
cargo run --locked --bin kanji-assets -- --output json plan \
  --mode new --validator-id kanjivg-pixel-chamfer \
  --validator-version kanjivg-r20250816-5581e65d-noto-serif-jp-24fc2d26-ea6231b1-mask64-v6

# Вручную импортировать локальный candidate (он останется pending).
cargo run --locked --bin kanji-assets -- --output json ingest \
  --character 漢 --file ./candidate.png
```

`--store <path>` выбирает root, `--repository-root <path>` задаёт checkout для
проверки границы `decks/`. `--allow-insecure-tls` нужен только для сетевого
запроса, когда Yarxi выдаёт указанную ошибку сертификата. `ensure` сохраняет
только `VERIFIED`; повтор уже актуального verified hash не ходит в сеть.

`list`, `plan` и `validate` открывают store; открытие может создать runtime-файлы,
восстановить незавершённую операцию или перенести legacy-записи через runtime
boundary. Сама выборка `plan` не записывает данные. `changed: false` не означает,
что открытие было только для чтения. `validate`
запускает production validator: `new` выбирает отсутствующие/устаревшие решения,
`full` перепроверяет весь manifest. Любой неуспешный semantic status попадает в
quarantine; общий exit code ненулевой, если запрошенная цель не `VERIFIED`.

JSON-ответ содержит `schema_version`, `operation`, `store`, `mode`, `assets`,
`changed`, `conflicts`, `blockers` и `outcome`; ошибка добавляет стабильные
`error.code`, `category`, `message` и `details`. В `ensure` у каждого asset есть
отдельный `item_outcome`, поэтому успешные независимые результаты не скрывают
ошибки соседних символов.

`ingest` читает ровно один явный source file вне `decks/` и создаёт pending
candidate. Формат/hash определяются по bytes. Для compare-and-swap замены
добавьте `--replace-expected-sha256 <текущий-hash>`.

## Проверки

Локальные provider/validator tests не обращаются к сети; они используют только
синтетические данные и временные каталоги. Live browser acquisition выполняется
отдельно против сайта и не является частью default suite. Проверки workspace
запускаются из корня:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Корневой Cargo workspace остаётся единственной точкой сборки. `decks/**/media/`
не используются как fixtures или источники.
