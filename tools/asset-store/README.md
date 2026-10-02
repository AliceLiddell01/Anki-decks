# Asset store и `kanji-assets`

`asset-store` — общий Rust core для program-owned assets. Он владеет manifest,
content hash, integrity и lifecycle. Предметный CLI `kanji-assets` добавляет
поиск статьи Yarxi, проверку изображения кандзи и публикацию подтверждённых
файлов в canonical corpus.

Инструмент не читает `decks/**/media/`, не меняет `media_files` и не копирует
изображения в колоды.

## Где лежат данные

Kanji CLI по умолчанию использует `.asset-store/kanji`. Следующий самостоятельный
домен имеет отдельный root `.asset-store/pitch-accent`; два publishable corpus не
делят manifest или каталог `assets/`.

```text
.asset-store/
├── kanji/
│   ├── .owner.json                 # tracked owner marker
│   ├── manifest.json               # tracked; только effective VERIFIED
│   ├── assets/
│   │   ├── gif/漢.gif
│   │   └── png/饅.png
│   ├── .runtime/                   # ignored candidates, quarantine, batches
│   ├── .tmp/                       # ignored staging и transaction markers
│   └── .lock                       # ignored
└── pitch-accent/
    ├── .owner.json
    ├── manifest.json
    ├── assets/png/幽霊.png             # consumer_filename:幽霊.pitch.png
    ├── .runtime/
    ├── .tmp/
    └── .lock
```

Внутренний `storage_path` и имя для конечного consumer — разные значения manifest.
Например, `assets/gif/漢.gif` имеет `consumer_filename: "漢.gif"` и попадает в
`media/漢.gif`. `anki-repo` использует явное consumer-имя и не извлекает его из
структуры asset-store.

`.runtime/manifest.json` использует тот же `store_id`, что и canonical manifest.
`verify_integrity` проверяет обе области. Pending и Quarantined bytes остаются в
runtime, verified-only gate проверяет canonical corpus и отсутствие пересечений
identity с runtime. Отсутствующий corpus допустим.

## Domain policy и manifest

`AssetDomainPolicy` задаёт допустимую identity, canonical storage path,
consumer filename, publishable formats и ограничения размера. `GenericDomainPolicy`
сохраняет hash-addressed layout для непубликуемых generic assets. Предметные
policy не принимают identity другого namespace: `KanjiDomainPolicy` владеет
`kanji`, `PitchAccentDomainPolicy` — `pitch_accent`.

Kanji GIF и PNG физически разделены по `assets/gif/` и `assets/png/`, при этом
имя файла — точный символ и расширение. Для pitch-accent ключом служит точная
словоформа (`surface`), а не чтение (`reading`): PNG хранится в
`assets/png/<surface>.png`, а потребитель получает имя `<surface>.pitch.png`.
Например, запись для `飴` хранится как `assets/png/飴.png` и передаётся Anki под
именем `飴.pitch.png`; имя kanji media `飴.png` остаётся отдельным. Чтение и
положительный идентификатор словарной записи JPDB входят в метаданные и
доказательства.
Сопоставление считает хирагану и катакану эквивалентными при сравнении чтения,
но требует совпадения точной пары `surface` и `reading` с одной из форм выбранной
словарной записи. В `evidence` сохраняется исходное чтение JPDB. Одного
декодирования PNG недостаточно для статуса `VERIFIED`:
`PitchAccentImageValidator` проверяет пару словоформы и
чтения, фактическую страницу словарной записи с тем же ID, наличие
хотя бы одного графика и доказательства тёмной темы.

В `evidence` сохраняются прямоугольники каждого графика в координатах области
просмотра и документа, их геометрическое объединение и `capture_rect`. Снимок
одного графика совпадает с прямоугольником DOM-узла, а снимок нескольких
охватывает их геометрическое объединение. Естественные промежутки между графиками
сохраняются; внешняя рамка или отступ не добавляются. Проверка допускает расхождение
размеров PNG не более чем на 3 пикселя из-за округления границ нативного
CDP-параметра `clip`. Доказательства тёмной темы связывают настройку цветовой схемы
браузера, класс документа и вычисленный стиль фона с пикселями PNG. Для фона
достаточно хотя бы одного
пикселя, у которого разница каждого RGB-канала с наблюдённым цветом не превышает
8; как минимум у восьми пикселей графика хотя бы один канал должен отличаться от
него более чем на 16. Каждый пиксель PNG должен иметь `alpha=255`. Эти проверки
просматривают весь снимок, поэтому искусственный тёмный край не нужен.
Происхождение браузера обязательно. Текущий контракт отрисовки и доказательств
проверяет валидатор v5; доказательства версии 4 не наследуют его доверие, а
неполные доказательства получают `UNCERTAIN`.

Версия схемы manifest — `5`. Помимо SHA-256, размера, формата, состояния,
происхождения и смыслового решения, каждая запись хранит `consumer_filename`, а
всё хранилище — `domain_id`. Автоматическая запись `ValidationRecord` и
необязательная `HumanAttestation` привязаны к точной идентичности и SHA. Для
pitch-accent решение человека `Approve` не заменяет автоматическую запись
`ValidationRecord`: для тех же байтов нужна запись со статусом `VERIFIED` и
ожидаемыми `validator.id`/`validator.version` для `PitchAccentImageValidator` v5.
Подтверждение человека не делает пригодными старые или неполные доказательства,
`REJECTED` либо `CORRUPT`. Это
требование относится к pitch-accent и не меняет существующие правила обработки
`Approve` для Kanji. Новый SHA не наследует решения предыдущих байтов. Замена
записи требует операции `compare-and-swap` по ожидаемому старому SHA.

Schema v3/v4 и прежний плоский Kanji layout читаются. Изменяющий open выполняет
версионную миграцию под exclusive lock: сохраняет старые bytes через hard link,
сверяет SHA-256, атомарно обновляет manifest, затем удаляет старые flat links.
Transaction marker в `.tmp/` позволяет завершить или откатить прерванный переход.
`ValidationRecord`, `HumanAttestation` и SHA неизменившихся bytes сохраняются;
повторное получение не требуется. После commit старого и нового canonical пути
одновременно не остаётся. CLI JSON сообщает `store.layout_migrated_on_open`.

`AssetStore::read_verified` остаётся read-only compatibility API с
`KanjiDomainPolicy`. Consumers других доменов используют
`read_verified_with_policy`; этот API также читает поддерживаемые legacy v3/v4
файлы через legacy mapping и возвращает явный `consumer_filename`, но не пишет
manifest, не создаёт runtime/lock-файлы, не мигрирует layout и не запускает
recovery. Миграция выполняется только через изменяющий open. Проверяются exact
bytes, checksum, размер, формат, validator identity и human decision.

Общий runtime пакетов владеет безопасной файловой механикой
`.runtime/batches/<batch-id>`: `NOFOLLOW`, блокировкой, revision/CAS, атомарной
записью состояния, ограниченным чтением и blob-файлами, адресуемыми по SHA. Он не
знает о `KanjiBatch`, `KanjiMetrics`, данных Yarxi или HTML-отчёте; эти правила
остаются в предметных адаптерах. Тестовое состояние в обычном текстовом формате
проверяет повторное открытие и чтение blob без правил изображений или кандзи.

Общие сеансы Chromium, поиск исполняемого файла, сведения о запуске, метрики CDP
и сетевая техническая телеметрия находятся в `browser_runtime.rs`. Yarxi сохраняет
собственные селекторы, URLs, TLS policy, фильтры, резервный источник, тёмную тему
и геометрию снимка. Общий runtime позволяет задать отдельный коэффициент
масштабирования `3.0`. Провайдер JPDB использует изолированную сессию; отдельный
`pitch-assets` владеет обработкой пакетов и публикацией подтверждённых PNG.
Интеграция с `anki-repo` остаётся отдельной возможностью.

Whitelist для хранения в Git включает `.owner.json` и `manifest.json` каждого
домена, а также непосредственные пути `assets/gif/*.gif` и `assets/png/*.png`
внутри kanji root и `assets/png/*.png` внутри pitch-accent root. Каталоги с
разрешённым расширением и их содержимое исключены из Git; вложенные каталоги,
другие расширения, `.runtime/`, `.tmp/` и `.lock` тоже исключены. Проверить
правила можно настоящим `git check-ignore` с помощью отдельного регрессионного
скрипта:

```bash
bash tools/asset-store/tests/gitignore_regression.sh
```

`kanji-corpus-gate` и `pitch-corpus-gate` проверяют, можно ли публиковать корпус
соответствующего домена. Они отклоняют неподтверждённые состояния lifecycle,
доказательства от устаревшего валидатора, старую плоскую структуру, пути с
суффиксом SHA, потерянные файлы и незарегистрированные канонические пути.

Проверить корпуса Kanji и pitch-accent из корня рабочего пространства можно командами, которые
только читают данные:

```bash
cargo run --quiet --locked -p asset-store --bin kanji-corpus-gate
cargo run --quiet --locked -p asset-store --bin pitch-corpus-gate
```

Проверка завершается с ненулевым кодом, если запись не имеет действующего
`VERIFIED`, используется неподдерживаемая структура или неверная политика домена,
доказательства валидатора устарели, найдены потерянные байты или повреждено
состояние runtime. Проверка ничего не меняет и не считает данные runtime частью
канонического корпуса.

## Получение pitch accent с JPDB

`JpdbPitchProvider` работает со страницами поиска и словарными страницами JPDB.
URL поиска задаёт `q` через API `URL` и явно устанавливает `lang=english`. После
перехода провайдер принимает страницу `/search` или допустимое перенаправление
сразу на `/vocabulary/<id>/...`. Во втором случае перед получением графика отдельно
проверяются ID записи, соответствие запросу и необязательный явный выбор. Любой
другой маршрут или значение `origin` приводит к отказу. Поиск проверяет связанную
пару написания и чтения среди наблюдённых форм, включая подтверждённые
альтернативные формы. При неоднозначности провайдер не выбирает первый или наиболее
ранжированный результат: он возвращает `ambiguous_vocabulary` и кандидатов с
ID словарной записи, формами и чтениями, частями речи, значениями и фактической
словарной ссылкой. Потребитель может передать явный `JpdbPitchSelection` через
`JpdbPitchRequest`. Провайдер выполняет новый поиск и принимает выбор, только если
ID, `surface` и необязательный `reading` из ссылки совпадают с одним из найденных
кандидатов. Параметры запроса и фрагмент ссылки не входят в идентичность. Допустимая
ссылка использует HTTPS-узел `jpdb.io`, стандартный порт и точный путь из трёх или
четырёх непустых сегментов `/vocabulary/<positive-id>/<surface>[/<reading>]`;
завершающий slash не разрешён. Провайдер не доверяет произвольному URL или ID,
использует фактический `href` результата и не строит словарную ссылку из входных
строк. Без явного выбора неоднозначный запрос остаётся неоднозначным, а
`vocabulary_not_found` отделён от технической ошибки.

Для подтверждённой словарной страницы `no_pitch_accent_on_source` возвращается,
только если распознана ожидаемая структура и в ней нет секции Pitch accent.
Неизвестный или пустой набор графиков в DOM, сетевой или JS-сбой,
неподтверждённая тёмная тема и некорректный снимок остаются техническими
ошибками. Перед фиксацией отсутствия провайдер дожидается завершения критических
запросов к странице и повторно проверяет её структуру; произвольная пауза сама
по себе не считается доказательством. Сбой выполнения или декодирования ответа
`page.evaluate()` сохраняется как `browser_evaluation` с активной стадией;
противоречивые данные DOM остаются `page_contract`, а превышение срока ожидания —
`timeout`.

Все найденные узлы графиков выбранной словарной записи снимаются одним снимком
через CDP при масштабе страницы `1.0` и DSF `3.0`; PNG после захвата не
масштабируется, не обрезается и не перекодируется. Координаты узлов сохраняются
относительно области просмотра и документа с учётом `scrollX`/`scrollY`. Область
снимка совпадает с прямоугольником DOM-узла для одного графика либо охватывает
геометрическое объединение узлов для нескольких;
техническое расхождение ограничено округлением границ параметра `clip` захвата
CDP. В `evidence` входят прямоугольники графиков, итоговая область снимка,
размеры области просмотра и страницы, масштабы, размеры PNG и сведения о фоне.
Валидатор проверяет геометрию и размеры PNG с допуском до 3 пикселей при
округлении границ `clip`, настройку цветовой схемы браузера и класс `dark-mode`,
а также пиксели фона и
графика. Все пиксели PNG должны иметь `alpha=255`. Валидатор не распознаёт сам
рисунок ударения. Естественные промежутки между графиками сохраняются; внешняя
рамка не добавляется.

Каждый исход получения содержит исходные байты и `PitchAccentDomainMetadata`,
пригодные для `PitchAccentImageValidator`. `JpdbPitchProvider::acquire_requests`
запускает отдельную сессию для набора запросов;
`acquire_requests_in_session` принимает сессию вызывающего кода. В обоих случаях
запросы обрабатываются по очереди, но для каждого выполняется новый поиск.
Обычная ошибка одного элемента не останавливает пакет. Если наблюдатель CDP
теряет данные и сессию больше нельзя считать надёжной, текущий элемент получает
ошибку на своей стадии, а следующие — явный `session_failure`.

Для ручной приёмки используйте внешний JSON-план. Для каждого элемента обязательно
задайте `expected_outcome` одним из точных значений:
`acquired`, `no_pitch_accent_on_source`, `ambiguous_vocabulary` или
`vocabulary_not_found`. Дополнительно разрешены поля:

- `expected_vocabulary_id` и `min_graph_count` только для `acquired`;
- `expected_candidate_ids` только для `ambiguous_vocabulary`; значения должны
  точно совпасть с набором ID фактических кандидатов без учёта порядка;
- `selection` только для `acquired`. В нём обязательны `vocabulary_id` и
  `detail_url`; ID должен совпасть с `expected_vocabulary_id`, а ссылка должна
  соответствовать описанному выше маршруту JPDB с тем же ID.

До создания отчёта и запуска браузера программа проверяет запрос, идентичность домена
и имя для потребителя, числовые ограничения, повторы запроса и ссылку выбора.
Краевые пробелы в `surface` и `reading` обрезаются при поиске, а исходные значения
плана сохраняются в отчёте для диагностики. Повтор словоформы и чтения допустим
для обычного неоднозначного запроса и отдельного явного выбора; точный повтор
одной пары запроса и выбора отклоняется. После поиска провайдер повторно сверяет
выбор с фактически найденными кандидатами.

Синтетический пример показывает поля плана. Для реального прогона замените ID и
`detail_url` в `selection` на значения из `evidence` выбранного кандидата:

```json
{
  "items": [
    {
      "surface": "合成語",
      "reading": "ごうせいご",
      "expected_outcome": "acquired",
      "expected_vocabulary_id": 42,
      "min_graph_count": 1
    },
    { "surface": "別の語", "expected_outcome": "no_pitch_accent_on_source" },
    {
      "surface": "重複語",
      "expected_outcome": "ambiguous_vocabulary",
      "expected_candidate_ids": [101, 102]
    },
    {
      "surface": "重複語",
      "expected_outcome": "acquired",
      "expected_vocabulary_id": 101,
      "selection": {
        "vocabulary_id": 101,
        "detail_url": "https://jpdb.io/vocabulary/101/重複語/ちょうふくご"
      }
    },
    { "surface": "欠落語", "expected_outcome": "vocabulary_not_found" }
  ]
}
```

Запуск из корня рабочего пространства:

```bash
cargo run --locked -p asset-store --bin jpdb_pitch_acceptance -- \
  --plan ../jpdb-pitch-plan.json
```

Без `--output` JSON/HTML-отчёт и полученные PNG помещаются в системный временный
каталог вне рабочей копии. С `--output <DIR>` укажите новый каталог вне рабочей
копии. Родительский каталог `<DIR>` должен уже существовать; каталог отчёта
создаётся только если его ещё нет. Отчёт сохраняет полный план и для каждого
элемента ожидаемый и фактический исход, результат сравнения, сведения о
кандидатах, фактические ID словарных записей и ссылки, формы и чтения,
доказательства тёмной темы, геометрию графиков и снимка, DSF, размеры PNG,
SHA-256 и решение основного валидатора. Исход `acquired` считается успешным,
только если статус валидатора — `VERIFIED` и соблюдены ограничения плана.
Ожидаемые неоднозначность, отсутствие записи и отсутствие pitch могут быть
успешными исходами; неожиданно не найденная словарная запись означает ошибку
прогона. Отсутствующий результат провайдера и локальная ошибка обработки также
считаются несовпадением. Ошибка одного элемента не удаляет уже собранные данные:
программа записывает итоговые `evidence.json` и `index.html`, включая доступные
частичные результаты. Исходные байты результата `acquired` сохраняются без
преобразования; PNG с неверной сигнатурой или ошибкой декодирования доступен как
`.bin` и не показывается как изображение. Инструмент не меняет каноническое
хранилище, пользовательские media или `decks/**`; план и отчёт остаются локальными.

## `pitch-assets`: канонический корпус и обработка пакетов

CLI получает pitch-accent, хранит состояние пакетов и публикует подтверждённые
PNG в `.asset-store/pitch-accent`. Он не читает экспорты CrowdAnki и не знает
имён моделей карточек или полей заметки. Без `--store` используется хранилище
pitch-accent в текущей рабочей копии. `--output json` возвращает машиночитаемый
результат с версией схемы.

```bash
# Просмотр и проверка корпуса без сетевого получения.
cargo run --locked -p asset-store --bin pitch-assets -- corpus list
cargo run --locked -p asset-store --bin pitch-assets -- corpus check

# Одно получение или пакет по строгому JSON-плану.
cargo run --locked -p asset-store --bin pitch-assets -- ensure \
  --surface '幽霊' --reading 'ゆうれい'
cargo run --locked -p asset-store --bin pitch-assets -- batch start \
  --plan ./pitch-plan.json
cargo run --locked -p asset-store --bin pitch-assets -- batch run \
  --batch-id my-pitch-batch

# Состояние, отчёт проверки и продолжение после решения.
cargo run --locked -p asset-store --bin pitch-assets -- batch status \
  --batch-id my-pitch-batch
cargo run --locked -p asset-store --bin pitch-assets -- batch review \
  --batch-id my-pitch-batch
cargo run --locked -p asset-store --bin pitch-assets -- batch resume \
  --batch-id my-pitch-batch

# Явный выбор кандидата, адресный повтор или отказ для точного SHA.
cargo run --locked -p asset-store --bin pitch-assets -- batch select \
  --batch-id my-pitch-batch --surface '重複語' --vocabulary-id 101 \
  --detail-url 'https://jpdb.io/vocabulary/101/重複語/ちょうふくご'
cargo run --locked -p asset-store --bin pitch-assets -- batch retry \
  --batch-id my-pitch-batch --surface '幽霊' --reason 'повтор после временной ошибки'
cargo run --locked -p asset-store --bin pitch-assets -- batch reacquire \
  --batch-id my-pitch-batch --surface '幽霊' --reason 'явное обновление записи'
cargo run --locked -p asset-store --bin pitch-assets -- batch reject \
  --batch-id my-pitch-batch --surface '幽霊' \
  --sha256 "$CANDIDATE_SHA256" --reason 'визуально отклонён'
```

Внешний JSON-план `PitchPlan` имеет `schema_version: 1`; неизвестные поля и
версии отклоняются. Сохранённое состояние пакета имеет отдельную версию схемы 2.
Старое состояние схемы 1 завершается ошибкой: CLI не пытается восстановить по
истории текущую попытку кандидата или исходный план. Каждый элемент плана
содержит точную `surface`, необязательный `reading` и необязательный `selection`
с `vocabulary_id` и `detail_url`. Одинаковые запросы схлопываются, а несовместимые
запросы одной surface приводят к `identity_conflict` до запуска браузера.

При создании пакета CLI нормализует запросы и сохраняет их как неизменяемый
`original_plan` вместе с SHA-256. Команды выбора, повтора и повторного получения
меняют активный запрос элемента, но не исходный план. Повторное использование того
же `batch_id` разрешено только при совпадении исходного плана и его дайджеста.
Для выбора из `ambiguous_vocabulary` укажите точные ID и `detail_url` кандидата из
отчёта проверки. Следующий `batch run` выполняет новый поиск и повторно
подтверждает выбранную запись в JPDB.

```json
{
  "schema_version": 1,
  "items": [
    { "surface": "幽霊", "reading": "ゆうれい" },
    {
      "surface": "重複語",
      "reading": "ちょうふくご",
      "selection": {
        "vocabulary_id": 101,
        "detail_url": "https://jpdb.io/vocabulary/101/重複語/ちょうふくご"
      }
    }
  ]
}
```

`batch status` проверяет целостность хранилища и сверяет сохранённый пакет со
свежим полным снимком канонических записей. Эта команда не запускает браузер и
не обращается к сети; при изменении данных хранилища она сохраняет обновлённое
состояние пакета. В JSON поле `changed` равно `true`, если команда изменила
сохранённые данные канонического корпуса или пакета, включая новое состояние,
кандидатные байты и файл `review.html`. Создание служебной блокировки или пустых
каталогов границы runtime само по себе не считается изменением данных. В остальных
случаях `changed` равно `false`.

Состояние пакета хранит типизированные исходы провайдера, историю попыток,
неоднозначности, решения валидатора и намерения публикации. Текущий кандидат
связан с точным номером попытки, её поколением и SHA-256 содержимого. Поэтому
одинаковый SHA в разных попытках не смешивает их запросы и доказательства. Сами
байты дедуплицируются по SHA и сохраняются до публикации в
`.runtime/batches/<id>/candidates/`. После каждого исхода элемента состояние
записывается. На время браузерного запроса блокировка runtime снимается; после
него CLI сверяет поколение и ревизию элемента и отбрасывает устаревший результат.
После перезапуска `resume` повторно проверяет кандидата текущим валидатором и
может продолжить публикацию без повторного JPDB запроса.

Публикация читает метаданные и `evidence` только из указанной попытки и требует её
текущий `VERIFIED` результат от `PitchAccentImageValidator` v5. Перед записью
`AssetStore::ingest_verified` повторно проверяет байты и сверяет ожидаемый SHA
предыдущей записи операцией `compare-and-swap`. Публикуемая запись хранится как
`assets/png/<surface>.png`, а имя для потребителя — `<surface>.pitch.png`. Если
свежая сверка хранилища подтверждает существующую каноническую запись, проверенную
текущей версией валидатора, пакет может использовать её без нового сетевого получения.

Явный `reacquire` начинает новое поколение и фиксирует SHA текущей записи
хранилища как основу `compare-and-swap`. Если SHA владельца меняется после
фиксации этого основания, свежая сверка `batch status` покажет конфликт
`refresh_owner_drift` и заблокирует публикацию старого кандидата. `batch reacquire`
с обязательным `--reason` архивирует конфликтное намерение публикации, принимает
актуальное состояние владельца как новое CAS-основание, начинает следующее
поколение и снимает ссылку на старую попытку, поэтому она не будет опубликована.
Если повторное получение даёт тот же SHA,
метаданные и `evidence` обновляются только после повторной проверки байтов
валидатором v5 и при совпадении CAS с тем же SHA записи хранилища. В публикацию
попадают метаданные точной текущей попытки. Визуальный отказ привязан к SHA
конкретных байтов; другие SHA не наследуют его. `retry` разрешён для повторяемых
технических ошибок и не сбрасывает
соседние завершённые элементы. `no_pitch_accent_on_source` завершает только
текущий пакет, не создавая пустую запись или постоянный кэш отсутствия;
`vocabulary_not_found` остаётся отдельным нерешённым исходом.

`batch review` сохраняет `review.html` в игнорируемом каталоге `.runtime` пакета.
Отчёт показывает полученные и канонические PNG, словоформу и чтение, словарную
ссылку JPDB, число графиков, SHA-256, размер, статус валидатора и доказательства
источника и отрисовки. При неоднозначности он перечисляет всех кандидатов, а
отсутствие pitch, ненайденную словарную запись и техническую ошибку показывает
как типизированные исходы без фиктивных изображений. Строки экранируются;
HTML не меняет состояние пакета и не является источником истины. Решения
задаются адресными командами `select`, `reject`, `retry` и `reacquire`; ручного
способа опубликовать байты без текущей автоматической проверки нет.

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

`KanjiImageValidator` и Kanji batch adapter ограничивают изображения 8 МиБ.
Общий batch runtime задаёт верхний предел 64 МиБ для state и blob, а предметный
adapter может установить меньший предел. Human approval использует лимит
текущей domain policy, а при его отсутствии — 8 МиБ. При подтверждении человеком
читается не более выбранного предела плюс один байт, чтобы обнаружить превышение
без неограниченного выделения памяти, затем проверяется целостность и полностью
декодируется изображение.

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
- GIF формы Yarxi `полный кадр → постепенная прорисовка → полный кадр`; проверяется
  итоговая полная семантическая маска, а не номер выбранного кадра;
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

# Создать batch (ID генерируется автоматически) и пройти до пяти breadth-first раундов.
cargo run --locked --bin kanji-assets -- --output json batch start 漢 字
cargo run --locked --bin kanji-assets -- --output json batch run --batch-id <id>

# Продолжить batch позже, посмотреть JSON state или записать локальный review HTML.
cargo run --locked --bin kanji-assets -- --output json batch status --batch-id <id>
cargo run --locked --bin kanji-assets -- --output json batch review --batch-id <id>

# Применить явное exact-hash решение пользователя.
cargo run --locked --bin kanji-assets -- --output json batch decide \
  --batch-id <id> --character 漢 --sha256 <sha256> \
  --action confirm --reason "пользователь подтвердил текущий candidate"

# После технической серии failures разрешить новый acquisition generation.
cargo run --locked --bin kanji-assets -- --output json batch retry \
  --batch-id <id> --character 字 --reason "источник снова доступен"
```

`--store <path>` выбирает root, `--repository-root <path>` задаёт checkout для
проверки границы `decks/`. `--allow-insecure-tls` нужен только для сетевого
запроса, когда Yarxi выдаёт указанную ошибку сертификата. `ensure` сохраняет
только effective `VERIFIED`; повтор уже доверенного exact hash не ходит в сеть.

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

### Kanji batch

`batch start [--batch-id ID] CHARACTER...` дедуплицирует identities, создаёт
versioned state под `.runtime/batches/<id>/` и до acquisition отмечает текущие
assets, которые owner уже подтверждает как effective verified. Если передать
существующий ID, requested identity set и validator должны совпасть; команда
возобновит state, а не начнёт новый batch.

`batch run --batch-id ID [--rounds 1..5]` по умолчанию запускает до пяти
acquisition rounds. Каждый round целиком проходит текущий frontier до следующего
retry; опубликованные или уже effective verified identities повторно не
acquire'ятся. Независимый item failure остаётся в `issues` и не отменяет соседние
исходы. Browser acquisition проходит без batch lock; после него CLI открывает
state заново и отбрасывает результат, если item уже вышел из frontier.

До автоматической публикации attempt и candidate bytes записываются в runtime.
Exact single `VERIFIED` candidate проходит owner validator и atomic canonical
commit. Distinct technically valid hashes дают aggregate по mean/min/max/count
для `expected_distance` и nearest-reference margin; exact duplicate SHA не
увеличивает sample count. `kanji-distinct-mean-v2` сохраняет исходные пороги
`0.020` и `0.004`, требует, чтобы выбранный кандидат сам проходил порог
nearest-reference margin, и фиксирует deterministic exact selected SHA.
Aggregate trust не подменяет automated status самого selected candidate:
versioned aggregate evidence сохраняется отдельным решением.

После исчерпания пяти раундов unresolved items получают `awaiting_human` state.
`batch review` пишет ignored локальный HTML с анимированными GIF, всеми distinct
candidate images, exact SHA, automated outcomes, per-attempt evidence и
aggregate metrics. Кандидаты остаются по hash-derived путям в `candidates/`;
HTML не является источником истины.

`batch decide` принимает только структурированные `confirm`, `reject` или
`reacquire` для текущих `--character` и `--sha256`. `confirm` требует технически
корректные GIF/PNG bytes, exact source SHA compare-and-swap, automated evidence
через owner boundary и полный decode перед human approval. `reject` сохраняет
human decision для exact candidate и отправляет только эту identity в новое
поколение acquisition. `reacquire` планирует targeted повтор без semantic
approval. Для item после пяти технических failures без candidate используй
`batch retry` без `--sha256`; с candidate CLI требует `--sha256`.

`batch status` возвращает state; `batch review` ещё и создаёт artifact. Batch
JSON содержит `batch_id`, `counts`, per-item `items`, item-level `issues`,
`blockers`, optional `review_artifact` и полный `batch` state/evidence.
`run` возвращает exit `0` только при полном canonical resolution и `3` при
awaiting-human или частичном прогрессе; отдельные ошибки остаются привязаны к
identity. `start`, `status`, `review`, `decide` и `retry` дают `0` при успешном
исходе; системные boundary/schema/integrity ошибки используют стандартные exit
codes asset owner.

После process restart продолжай с тем же ID по `status` → `run`/`review` /
`decide`; CLI возобновляет exact owner publication из сохранённых candidate и
human-decision intent без повторного acquisition. Runtime state, HTML и candidate
bytes не входят в canonical corpus и не предназначены для Git.

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
