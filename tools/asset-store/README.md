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

В store находятся:

- `.owner.json` и versioned `manifest.json` — tracked canonical state;
- `assets/<кандзи>.<format>` — tracked canonical bytes изображения кандзи;
- `.lock` и `.tmp/` — только runtime state, исключённый из Git.

Например, изображение `漢` в GIF-формате хранится в `assets/漢.gif`, а в PNG —
в `assets/漢.png`. Расширение выводится из magic bytes. SHA-256 остаётся в
manifest и проверяется вместе с размером, форматом, identity и semantic
decision; hash не входит в имя kanji asset. Для других namespace общего core
сохраняется hash-suffixed layout.

`.gitignore` разрешает в этом store только `.owner.json`, `manifest.json` и
изображения `.gif`/`.png`; lock и незавершённая публикация не попадают в commit.
Другие каталоги и файлы store не усыновляются. Root с `..`, symlink-компонентом,
неожиданными файлами или пересечением с `decks/` отвергается.

Каждый канонический image связан в manifest с:

- identity `{namespace: "kanji", key: "<символ>"}`;
- фактическим SHA-256, размером и форматом;
- provenance Yarxi и domain metadata с символом, Unicode, номером статьи,
  выбранным source URL и результатом выбора GIF/PNG;
- semantic status, validator id/version, content hash решения и pixel evidence.

Manifest schema сейчас `3`. Замена bytes для существующей identity требует
compare-and-swap по ожидаемому старому hash. Kanji-файл публикуется атомарным
rename из staging; transaction marker и backup в `.tmp/` позволяют при открытии
store восстановить завершённый manifest commit либо откатить незавершённую
замену. Чтение остаётся fail-closed: текущий SHA-256 сверяется с manifest до
использования bytes. У generic hash-suffixed объекта авария до manifest commit
может оставить проверяемый orphan-файл; он не считается asset без manifest
записи.

Generic `AssetStore::ingest` создаёт `pending` candidate. Предметный путь
`AssetStore::ingest_verified` оставляет candidate во временном staging и
публикует его только после ответа `VERIFIED`. `REJECTED`, `UNCERTAIN`, `CORRUPT`
и технический сбой не создают canonical image или manifest entry.

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

Для callers, которым нужен именно browser-rendered PNG, provider поддерживает
явный target `AcquisitionTarget::RenderedFontSamplePng`. Он выбирает rendered
плитку даже при наличии primary GIF и не меняет default policy обычного `ensure`.

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
как сжатый `src/data/kanjivg-r20250816.maskdb.zlib`. KanjiVG распространяется под
CC BY-SA 3.0. Attribution, release и SHA-256 исходного архива зафиксированы рядом
с generated data в `src/data/kanjivg-ATTRIBUTION.txt`.

Для dark tile добавлен дополнительный pixel reference из Noto Serif JP — того
шрифта, который Yarxi реально использует в `.font-sample`. Зафиксированный TTF
имеет SHA-256
`e6cffcd5cae6a298ddfd17173b42d64888913976b2d7053024a9ecf0cf5d3fe8`. Из него
генерируются 6 428 нормализованных масок в
`src/data/yarxi-noto-serif-jp-r1.maskdb.zlib`; исходный TTF не включён в
репозиторий. Его происхождение, upstream license и hash результата описаны в
`src/data/yarxi-noto-serif-jp-ATTRIBUTION.txt`.

Validator сначала оценивает фон по угловым участкам растра, выбирает полярность
по контрасту, удаляет связанные компоненты рамки у края и строит foreground
mask глифа. При приведении candidate raster к маске 64×64 пиксели выбираются из
центров source-ячеек, чтобы не смещать линии к левому верхнему краю. Далее
применяется та же метрика — симметричное Chamfer distance между центрированными
масками: среднее расстояние от каждого набора пикселей до ближайшего пикселя
другого набора, нормированное на размер маски. Для light-on-dark PNG expected
pixels сравниваются с KanjiVG и Noto Serif JP; берётся меньшая из двух дистанций.
Ближайший другой Unicode ищется по объединению всех применимых pinned catalogs
KanjiVG и Noto Serif JP, чтобы обе метрики ожидаемого и конкурирующего символов
учитывали тот же набор шрифтов. Версия validator —
`kanjivg-r20250816-noto-serif-jp-24fc2d26-mask64-v5`. Пороговые значения входят
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

Эти тесты защищают выбранную policy, но не измеряют долю ложных подтверждений на
всём пространстве живых шрифтов. При недостаточном отличии
результат остаётся `UNCERTAIN`, что предпочтительнее неверного подтверждения.

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
  --validator-version kanjivg-r20250816-noto-serif-jp-24fc2d26-mask64-v5

# Вручную импортировать локальный candidate (он останется pending).
cargo run --locked --bin kanji-assets -- --output json ingest \
  --character 漢 --file ./candidate.png
```

`--store <path>` выбирает root, `--repository-root <path>` задаёт checkout для
проверки границы `decks/`. `--allow-insecure-tls` нужен только для сетевого
запроса, когда Yarxi выдаёт указанную ошибку сертификата. `ensure` сохраняет
только `VERIFIED`; повтор уже актуального verified hash не ходит в сеть.

`plan` только показывает выборку `new`/`full` и ничего не меняет. `validate`
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
