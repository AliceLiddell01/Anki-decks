# Review Observatory

Отдельный локальный сервер наблюдает существующие review artifacts и learning.
Его выключение не влияет на команды `anki-repo`, execution jobs и SQLite.
Сервер не запускается при сборке, открытии проекта или вызовах CLI.

Сборка frontend и явный запуск из корня workspace:

```bash
npm ci --prefix tools/review-observatory/web
npm run build --prefix tools/review-observatory/web
cargo run --release --bin review-observatory -- serve
```

Открыть `http://127.0.0.1:3085`. Остановить — `Ctrl+C` или `SIGTERM`.
Сервер корректно закрывает SSE и собственное наблюдение; задания review
продолжают исполняться. Node.js нужен для сборки и разработки frontend.
В обычном просмотре собранные файлы раздаёт Rust.

Явные параметры:

```bash
cargo run --release --bin review-observatory -- serve --port 3086
cargo run --release --bin review-observatory -- serve --repository /path/to/repository
cargo run --release --bin review-observatory -- serve --host ::1 --port 3085
```

`--repository` должен указывать на корень workspace. Поддерживаются только
loopback IP. Занятый порт вызывает ошибку, автоматического переключения нет.
Отсутствующий `web/dist/index.html` вызывает сообщение с командами сборки;
сервер не устанавливает npm dependencies и не открывает браузер.

## Интерфейс и локальные проверки

Разделы доступны по адресам `/` (обзор), `/reviews`, `/evidence`, `/execution`,
`/triage`, `/learning` и `/health`. Для разработки можно отдельно запустить
Vite после явного запуска Rust-сервера:

```bash
npm run dev --prefix tools/review-observatory/web
```

Проверки frontend:

```bash
npm run typecheck --prefix tools/review-observatory/web
npm run build --prefix tools/review-observatory/web
npx playwright install chromium
npm run test:browser --prefix tools/review-observatory/web
```

Chromium нужен только для browser tests и создания снимков; обычный просмотр
использует собранную статику без Node.js. Локальные PNG и журнал визуальных
проверок находятся в `.anki-repo/observatory/visual-evidence/`; фикстуры и
результаты Playwright — только в явных тестах.

## Источники и состояния

Наблюдаются фиксированные файлы в
`.anki-repo/review/<PR|local>/<HEAD>[/snapshot-<32 hex>]/`:

- `review.json`, `review-queue.json`, `semantic-triage.json`;
- `review-report.md`, `recommendations.json`;
- непосредственные `runs/<job-id>/job.json`, `state.json`, `result.json`,
  `source-review.json` и состояние собственной execution lock.

`worktree`, `target`, runtime, сырые stdout/stderr и logs не перечисляются
рекурсивно и не публикуются. Очередь и triage проверяются существующими core
моделями по точным digest источника. HTTP не пересобирает AST и не запускает
анализаторы. Pipeline показывает состояние сохранённых артефактов;
действия агента и токены вне этих источников имеют состояние `not_observed`.
Просмотр очередной страницы не меняет семантическое покрытие.

Learning читается через `LearningStore::open_read_only`, исключительно
`READ_ONLY`, без миграций, изменения journal mode, feedback, индекса или
`history_generation`. Сводка паттернов использует core `pattern_report`;
история импортов использует нормализующий core read API. Транзакции чтения
дают согласованный снимок; составная сводка также проверяет неизменность
монотонной генерации между чтениями и повторяет наблюдение при гонке.

Для WAL требуется уже существующий пригодный WAL/SHM. `readonly_shm=1`
bundled SQLite VFS исключает создание SHM и writable mmap наблюдателем.
Закрытая checkpointed WAL-база без этих файлов показывается как
`unavailable`, старый формат — как `incomplete`: dashboard не меняет их.
`immutable` не используется, потому что он игнорирует WAL и concurrent writers.

`available` означает доступность источника; оно не означает успешную
проверку или завершённое semantic review. `missing`, `incomplete`, `invalid`,
`unavailable` и `not_observed` сохраняются явно. Неизвестные значения — `null`.
Независимые findings остаются без выдуманных связей с candidate.
Commit, path и line взяты из evidence; remote URL в этих артефактах отсутствует,
поэтому API показывает ограничение для web permalink.

## API v1

Все маршруты только на чтение, с envelope
`{api_version, state, data, provenance, diagnostics}`.
Страница имеет форму `{items, total, page, limit}`; `page` начинается с 1,
`limit` — 1–100, по умолчанию 50. `q` задаёт подстрочный поиск.

| Маршрут `/api/v1/...` | Назначение |
| --- | --- |
| `snapshot`, `health` | Текущий снимок, компонентные состояния и ограничения |
| `reviews` | История известных snapshots |
| `reviews/{id}` | Источники, pipeline, coverage и summary |
| `reviews/{id}/candidates` | Фильтры `priority`, `detector`, `role`, `surface`, `unknown`, `unit_id`, `disposition`; сортировка `priority`, `id`, `path`, `detector` |
| `reviews/{id}/candidates/{candidate}` | Точный evidence, classification, compact unit, decision и связанные findings |
| `reviews/{id}/units` | Компактные units, фильтры и сортировка `priority`/`size` |
| `reviews/{id}/units/{unit}` | Подробности и отдельная страница members |
| `reviews/{id}/triage` | Coverage, страницы findings и decisions |
| `reviews/{id}/triage/groups/{group}` | Полное covered множество через страницы candidates |
| `reviews/{id}/triage/findings/{finding}` | Finding и страницы связанных candidates |
| `reviews/{id}/report` | Markdown как текст, с состоянием исходного файла |
| `execution`, `execution/{job}` | Собственные jobs, lifecycle, безопасные сведения результата |
| `learning` | Генерация, counts и core patterns |
| `learning/{collection}` | `patterns`, `observations`, `feedback`, `decisions`, `findings`, `recommendations` |
| `events` | SSE `snapshot`/`snapshot_changed`, revision и время наблюдения |

Baseline priorities сохраняют порядок `high`, `normal`, `low`.
Рекомендации имеют отдельные `baseline_priority`, `suggested_position` и
`recommended_rank`; learning не переписывает baseline.

SSE хранит последние 64 уведомления. `Last-Event-ID` восстанавливает доступный
отрезок; пропущенные события вызывают `snapshot`, после которого клиент повторно
получает текущее состояние. Источники проверяются каждые 2 секунды. Неизменные
review documents и jobs используют cache по идентичности файла, размеру и mtime;
активность execution lock проверяется отдельно.

## Границы чтения

Host и Origin должны совпадать с адресом локального сервера. Cross-site запросы,
CORS, тела запросов и мутирующие HTTP methods не поддерживаются. Пути пользователя
не передаются в файловый read API. Файлы и каталоги открываются через дескрипторы
с `O_NOFOLLOW`; symlink, FIFO и другие неожиданные типы отвергаются. JSON ограничен
64 MiB, малые документы — 4 MiB, статические assets — 16 MiB. Глобальные пределы
индекса: 256 snapshots, 256 MiB исходных артефактов и 1000 jobs; превышение
отражается как неполнота с диагностикой. Каталоги jobs сканируются только на
непосредственном уровне. Ошибка отдельного источника не останавливает HTTP.
