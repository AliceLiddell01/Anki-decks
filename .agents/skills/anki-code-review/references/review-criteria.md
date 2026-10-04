# Критерии независимого code review

## Базовый инженерный review

Применяй по фактически затронутому scope, а не механически ко всему repository.

### Correctness и contracts

Проверяй:

- неверные ветвления, состояния, границы диапазонов и assumptions;
- нарушение существующих API/CLI/JSON/filesystem contracts;
- несовместимость producer/consumer;
- ошибочную семантику partial success, retry, resume, idempotency;
- regressions для существующих call sites;
- несогласованные изменения code/docs/tests/config.

### Error handling и fail semantics

Проверяй:

- потерю или маскирование ошибок;
- fail-open там, где contract требует fail-closed;
- превращение структурированной ошибки в неоднозначную строку;
- cleanup/rollback, который не выполняется на раннем выходе;
- ошибки, после которых durable state врёт о реально завершённой работе.

### Resource lifecycle

Если затронуты файлы, процессы, браузеры, соединения, temp trees или locks,
проверь acquisition, ownership, cleanup на success/error/panic/cancel и
повторный запуск.

### Concurrency

Если затронут параллелизм, проверь минимум:

- shared mutable state и synchronization boundary;
- races между completion/cancel/cleanup;
- ordering assumptions;
- boundedness/backpressure;
- stale results/tokens/generations;
- double-processing и lost work;
- deterministic durable state после interruption.

Не выдумывай concurrency finding, если код однопоточный и изменение этого не
касается.

### Security

Проверяй только применимые риски: секреты в логах, command/path injection,
небезопасную обработку внешних данных, permissions, доверие к network payload.
Не добавляй абстрактный security checklist без attack surface.

### Performance

Finding нужен, если есть доказуемая regression/hot-path проблема или нарушение
явного performance requirement. Не требуй оптимизации холодного кода без
основания.

### Backwards compatibility и observability

Проверь публичные форматы, exit codes, stderr/stdout separation, logs, metrics и
документацию там, где они являются частью изменяемого contract.

## Обязательные специальные критерии

### 1. Необоснованный hardcode

Ищи hardcode путей, имён файлов/директорий, текущих колод, UUID/ID, live payload,
расширений, magic numbers, environment assumptions, фиксированных списков и
конкретной структуры данных там, где код обязан обнаруживать её фактически.

Не считать дефектом автоматически:

- значение внешнего стандарта/protocol;
- стабильный domain invariant;
- явно установленный repository contract;
- именованную константу с доказанной фиксированной семантикой;
- default/limit, для которого есть осмысленная продуктовая или техническая
  причина.

Ключевой вопрос: изменение допустимых данных/окружения, не нарушающее contract,
потребует правки кода или теста только из-за сегодняшнего состояния?

### 2. Overfitting к текущей разработке

Проверяй production code и tests на скрытую привязку к одному текущему feature
instance.

Признаки:

- общий helper на деле знает имя текущего файла/колоды/stage;
- test проверяет конкретную реализацию вместо свойства;
- synthetic test fixture повторяет единственный happy path из PR;
- код special-case'ит текущий testcase;
- generic abstraction работает только при текущем количестве/порядке элементов;
- изменение live payload ломает default tests без изменения contract;
- регрессионный test ловит только одно имя/значение, хотя bug class общий.

Не требуй универсальности за пределами заявленного domain. Специализированный
код допустим, если специализация является частью contract, а не случайностью
текущей задачи.

### 3. Соответствие исходным требованиям и scope

Сопоставь каждый существенный requirement с реализацией и tests.

Отдельно ищи:

- пропущенные требования;
- реализацию, меняющую смысл требования;
- заявленную гарантию без enforcement/test;
- unrelated refactoring или behavior change;
- PR body, который обещает больше, чем реально делает код.

Расширение исходного scope само по себе не defect. Оно допустимо, если
дополнительная работа объективно нужна для correctness, сохранения invariants,
backwards compatibility, тестируемости или завершённой архитектурной границы,
которую исходный prompt не перечислил, но должен был учитывать.

«Раз уж файл открыт» и несвязанный cleanup такой причиной не являются.

### 4. Legacy и dead code

После изменения ищи:

- старый path, полностью заменённый новой реализацией;
- unused helpers/constants/config;
- compatibility branch без оставшегося consumer;
- тесты поведения, которого больше нет;
- документацию/комментарии про удалённую архитектуру;
- временный adapter/shim, который после завершения migration уже не нужен;
- дублирующие abstractions с одним владельцем смысла.

Не объявляй legacy код дефектом только из-за возраста. Совместимость и migration
path могут быть намеренными; finding требует доказательства, что их contract
больше не существует.

### 5. Остаточный английский

Проверь полное содержимое всех файлов, затронутых PR, включая строки вне diff.

Обычные комментарии, diagnostics, logs и пользовательские пояснения должны быть
на русском, если для английского нет контрактной причины.

Разрешены:

- точные имена API, JSON fields, types/functions/flags;
- Rust/Git/GitHub/HTTP/JSON и другая устоявшаяся техническая терминология;
- protocol/domain terms без естественного русского аналога;
- externally-defined literals и machine-readable contract values;
- цитируемые внешние сообщения, если перевод изменил бы contract.

Наличие привычного английского слова не оправдано, если нормальный русский
эквивалент передаёт тот же смысл без потери точности.

Для каждого finding укажи provenance:

- introduced — английский добавлен/изменён текущим PR;
- pre-existing — строка уже была в затронутом файле до PR.

Pre-existing нарушение всё равно входит в запрошенную проверку затронутых
файлов, но не приписывается автору текущего diff.

## Tests как часть production quality

Дополнительно ищи:

- tautological assertions;
- mocks, повторяющие реализацию;
- snapshots без проверки существенного contract;
- flaky timing/order assumptions;
- отсутствие negative/failure-path tests;
- тесты, зависящие от сети или локальной машины без явной integration-причины;
- чрезмерный доступ к internals вместо public behavior;
- production hooks, появившиеся только ради одного теста и ухудшающие API.

Хороший regression test должен ломаться от реалистичного возврата bug и
оставаться устойчивым при безопасном внутреннем refactoring.
