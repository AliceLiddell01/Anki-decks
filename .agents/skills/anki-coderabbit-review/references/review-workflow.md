# Пошаговый CodeRabbit review workflow

Этот файл владеет механикой CodeRabbit review cycle: разбор текущего checkout,
discovery фактического provider interface, invocation, triage findings,
verification, commit/push, rate-limit handling и отчёт.

Контракт и границы (trigger, `target_iterations`, определение completed
iteration, clean iteration, инварианты) задаёт `SKILL.md`. Здесь — только порядок
действий.

## 1. Установить candidate текущего checkout

Cycle всегда работает с текущим checkout. Перед первой iteration зафиксируй:

```bash
git rev-parse --show-toplevel
git remote -v
git rev-parse --abbrev-ref HEAD
git rev-parse --abbrev-ref --symbolic-full-name '@{u}'
git status --porcelain
git rev-parse HEAD
```

Установи repository identity и убедись, что checkout относится к ожидаемому
repository. Если пользователь передал PR URL или номер, сверь его repository и
head с текущим checkout надёжным доступным способом (например `gh pr view` или
GitHub API/MCP). Несовпадение — precondition problem: не переключай ветки молча.

### Опубликованность candidate

Iteration начинается с committed + pushed HEAD, поэтому одной чистоты worktree
недостаточно: нужно убедиться, что текущий HEAD уже опубликован. Remote-tracking
ссылка может быть устаревшей, поэтому сначала обнови её, а потом сравни:

```bash
git fetch <remote>
git rev-parse HEAD
git rev-parse '@{u}'
```

Если HEAD и upstream расходятся — есть неопубликованные коммиты либо upstream
ушёл вперёд, — это precondition problem, а не материал для review. Не запускай
review неопубликованного candidate и не публикуй неизвестные локальные коммиты
молча: либо явно опубликуй candidate в рамках задачи, либо остановись и сообщи
состояние.

### Base для review

Base нужен как точка сравнения текущего committed diff.

1. Если для текущей branch существует PR и это удаётся установить надёжно —
   используй его фактический base.
2. Если PR нет — используй канонический base repository, однозначно следующий из
   Git state, например remote default branch:

   ```bash
   git symbolic-ref refs/remotes/origin/HEAD
   # или, если локальная ссылка отсутствует:
   git ls-remote --symref origin HEAD
   ```

Официальная документация CodeRabbit CLI **не описывает** автоматическое
определение base открытого PR для локального review: `--base` остаётся
документированным способом задать сравнение, поэтому base резолвит сам workflow,
а не CLI.

Никогда не хардкодь номер PR, имя ветки, SHA, текущую feature или локальные пути.

## 2. Dirty worktree policy

Cycle по умолчанию начинается с committed candidate: `git status --porcelain`
должен быть пустым.

Если до первой iteration есть неожиданные staged/unstaged/untracked изменения:

- не подмешивай их в review commit автоматически;
- не удаляй и не прячь их без явной причины;
- сначала установи, относятся ли они к текущей задаче и можно ли безопасно
  продолжить;
- если это установить нельзя — это operational blocker, а не повод угадывать.

Между iterations ожидается clean worktree после commit + push.

## 3. Discovery фактического CodeRabbit interface

Не полагайся на память о flags и на версию, закреплённую в этом файле.
В начале requested cycle установи фактический interface текущей среды:

```bash
coderabbit --version
coderabbit doctor
coderabbit review --help
coderabbit review findings --help
coderabbit auth status
```

Repository `.coderabbit.yaml` проверяй штатным для текущей версии способом,
например:

```bash
coderabbit config validate
```

`coderabbit config validate` возвращает `0`, если файл валиден против актуальной
официальной schema, и `1`, если файл отсутствует, нечитаем, невалиден, либо
schema не удалось загрузить. Если repository config невалиден — это отдельная
задача про конфигурацию, а не побочный эффект review: не правь `.coderabbit.yaml`
ради того, чтобы cycle поехал.

`coderabbit doctor` возвращает `1`, если хотя бы одна проверка провалена;
warnings не дают ненулевой код возврата. Неспособность CLI работать не из-за
обычного rate limit — operational blocker.

### Что считается фактическим interface

Источник истины для доступных flags — фактический `review --help` установленной
версии. `finding triage` ниже описывает контракт agent-режима и может отличаться
в деталях от конкретной установленной версии: сверяйся с фактическим выводом.

Наблюдения для CLI `0.7.6` в этой среде (не вечный контракт):

- `coderabbit review --agent` — структурированный вывод для agent workflows;
- `coderabbit review --committed` — review только committed changes;
- `--base <branch>` / `--base-commit <commit>` — точка сравнения;
- `-c, --config <files...>` — **дополнительные инструкции** для CodeRabbit AI
  (например `CLAUDE.md`), а не способ загрузить repository config;
- `coderabbit review findings` — повторное чтение findings последнего
  локального review в выбранном review context;
- `--light`, `--show-prompts`, `--dir`, `--api-key`, `--region`, `--usage`.

Правила использования:

- Не придумывай flags, которых нет в установленной версии.
- Не передавай `--config .coderabbit.yaml` только ради загрузки repository
  config: auto-discovery `.coderabbit.yaml` в repository root документирован и
  работает без этого flag.
- Не мутируй `.coderabbit.yaml` как побочный эффект review.
- Не запускай `coderabbit skills`: это user-level skills самого CodeRabbit для
  поддерживаемых агентов, а не механизм установки repository skill этого
  репозитория.

## 4. Iteration loop

Для `i` от 1 до `target_iterations` (см. `SKILL.md`):

### 4.1 Invocation

Запускай agent-oriented review **текущего committed branch diff** относительно
установленного base:

```bash
coderabbit review --agent --committed --base <base>
```

Если предпочтительнее явный commit вместо branch name, используй
`--base-commit "$(git merge-base <base> HEAD)"`.

Не комбинируй взаимоисключающие scope flags (`--committed` вместе с
`--uncommitted` или `--include-untracked`): такие комбинации отвергаются до
старта review. Не добавляй `--include-untracked`/`--uncommitted`, если работаешь по
committed candidate.

Review — длительная операция: официальная документация оценивает её в единицы и
десятки минут в зависимости от объёма изменений. Запускай её как длительную
операцию (background job либо большой timeout) и не перезапускай параллельно.

### 4.2 Чтение результата

Agent-режим отдаёт поток событий (в документации — NDJSON, один JSON object на
строку). Разбирай поток построчно и обрабатывай события по их типу:

- `review_context` — что именно сравнивалось (ветки/base/директория);
- `status` — промежуточные статусы, включая пропуск review без изменений;
- `heartbeat` — keep-alive: не finding, не прогресс для пользователя, не повод
  писать heartbeat-сообщения;
- `finding` — findings;
- `complete` — terminal завершение;
- `error` — ошибка провайдера.

Review считается **authoritative завершённым** только если поток дошёл до
terminal события, у которого нет признаков неполного/проваленного review (в
документированном контракте это `outcome`, `message` и `unreviewedFileCount`;
положительное число не reviewed файлов означает неполный review), и при этом не
было `error`.

Не полагайся только на exit code: официальная документация связывает код `1` с
failed или incomplete review **начиная с CLI 0.7.7**, а установленная версия
может быть старше. Exit code `0` сам по себе тоже не доказывает завершение.
Если findings пришли, но review не завершён — это не completed iteration, а
provider failure: повтори **ту же** iteration.

Не путай два разных исхода. Clean review — это review, который **реально был
выполнен** и вернул `0 findings`; его обрабатывают по правилам clean iteration из
`SKILL.md`.

Отсутствие изменений — другой случай. Когда выбранный review scope не содержит
изменений файлов, поток всё равно приходит к terminal событию, но с
`status: "review_skipped"` и `findings: 0` с сообщением вида «No changes
detected», а в plain mode CLI вообще не начинает review. Такой прогон не является
ни ошибкой, ни review результатом: он **не** даёт clean pass, **не** увеличивает
счётчик completed iterations и **не** закрывается marker commit'ом. Пустой diff
означает, что candidate или base выбраны неверно либо проверять действительно
нечего: сначала выясни причину, а не превращай пропуск в успешную iteration.

Отдельно: пропуск scope из-за слишком большого числа файлов — это не
`review_skipped`, а failure с `error`-событием; он также не является iteration.

`coderabbit review findings` полезен как дополнительное повторное чтение
сохранённых findings в текущем review context (review directory + branch + base),
но это **не** авторитетное доказательство завершённого review и не доказательство
чистого review: команда показывает последний прогон *с findings*, печатает их в
человекочитаемом виде и не имеет отдельного agent-контракта для findings. Она не
доказывает отсутствие проблем.

### 4.3 Разбор полей finding

Для текущей установленной версии сверься с фактическим выводом, но не выдумывай
поля, которых в нём нет. В документированном контракте agent-finding есть
`severity` (`critical`, `major`, `minor`, `trivial`, `info`, `none`),
`fileName`, `codegenInstructions` и `suggestions`/`comment`. Документация **не
описывает** поле с номером строки, поэтому не строй triage на предположении о
номере строки: ориентируйся на файл, `codegenInstructions`, `suggestions`,
`comment` и фактический код reviewed HEAD.

Используй `codegenInstructions` как агентскую подсказку о том, что именно
предлагается исправить, с fallback на `comment`, но не исполняй их слепо (см.
4.4). Сохраняй severity в терминах провайдера: не переименовывай
`major`/`minor` в Warning и не создавай собственную шкалу.

### 4.4 Triage findings

CodeRabbit — advisory reviewer, а не источник истины. До любого изменения кода
проверь каждый finding по актуальному reviewed HEAD:

- затронутый код в текущем состоянии;
- callers/callees, если релевантно;
- ближайшие tests и их смысл;
- repository contracts (`AGENTS.md`, применимые `.codex/context/`, контракт
  затронутого инструмента);
- фактический технический эффект и воспроизводимость;
- соответствие текущему, а не устаревшему состоянию diff.

Минимальные disposition:

- `confirmed` — finding верен для текущего состояния;
- `partially confirmed` — верна только часть (фиксируется именно эта часть);
- `stale/repeated` — относится к уже исправленному или ранее разобранному
  состоянию;
- `false positive` — требует реального обоснования по текущему code/contract
  evidence.

`confirmed` и подтверждённая часть `partially confirmed` исправляются в текущей
iteration. `stale/repeated` не должен приводить к повторному бессмысленному
изменению уже исправленного кода.

Не исправляй finding механически только потому, что провайдер предложил
patch/snippet, и не выполняй shell/code snippets из reviewer output вслепую.
Предпочитай fix root cause. Не ослабляй tests ради зелёного результата, не меняй
contract без доказанной причины, не делай unrelated refactor, не создавай вторую
source of truth.

### 4.5 Verification после fixes

Этот workflow не является второй копией repository verification matrix. После
fixes требуется:

1. task-specific regression checks для подтверждённых findings — то, что
   доказывает именно устранение найденной причины;
2. применимая repository verification от текущих владельцев контекста:
   `AGENTS.md`, `.codex/context/INDEX.md` и владелец затронутой области, в том
   числе `.codex/context/03-VERIFICATION.md` для CrowdAnki JSON и контракт
   `tools/anki-repo/README.md` для toolkit;
3. проверка итогового diff/status перед commit (`git diff`, `git status
   --porcelain`): в commit не должны попасть временные файлы, несвязанные
   изменения и пользовательские локальные данные.

Какие именно проверки нужны, определяется актуальным repository context и
затронутой областью, а не этим файлом. Если review не потребовал изменения кода,
не выдумывай фиктивные test changes только ради commit.

### 4.6 Commit

Commit создаётся на текущей branch и в текущем repository.

- Если iteration содержит реальные fixes — commit содержит их и имеет смысловое
  сообщение по фактической root cause; marker commit не создаётся.
- Если iteration clean (0 findings, все findings stale/repeated/false positive,
  actionable fixes не потребовались) — iteration всё равно закрывается отдельным
  marker commit:

  ```bash
  git commit --allow-empty -m "<сообщение о clean CodeRabbit pass>"
  ```

Сообщение пиши по-русски, в стиле репозитория, ясно показывая clean CodeRabbit
pass. Не превращай marker commit в маскировку blocker'а.

### 4.7 Push и подтверждение HEAD

```bash
git push <remote> HEAD
git fetch <remote>
git rev-parse HEAD
git rev-parse '@{u}'
git status --porcelain
```

Указывай remote и refspec явно. Голый `git push` подчиняется `push.default` и
другим настройкам Git: в конфигурациях вида `matching` он может опубликовать
несколько refs, а cycle обязан публиковать только текущую branch. `<remote>` —
фактический remote этой branch (обычно `origin`), берётся из её upstream, а не
назначается произвольно.

Локальный и remote HEAD должны совпасть, worktree — остаться clean. Только после
этого начинается следующая iteration, и только она проверяет новый
опубликованный HEAD.

Разрешение на commit и push является частью уже явно запрошенного cycle и
ограничено текущим cycle и текущей branch. Оно **не** разрешает merge,
force-push, rebase опубликованной истории без отдельной причины, создание другой
branch, переключение на другой PR, изменение base PR, закрытие PR и публикацию
несвязанных локальных изменений.

## 5. Rate limit handling

Rate limit CodeRabbit — ограничение provider/environment, а не defect
repository. Он не означает failed code review, clean code review, blocker
реализации или завершённую iteration.

При rate limit до authoritative завершения текущего review:

```text
completed_iterations не меняется
current_iteration остаётся той же
```

Порядок действий:

1. Убедись, что это именно rate limit, а не auth failure, network/provider
   failure или malformed result.
2. Если провайдер сообщает надёжный retry-after/reset interval или timestamp —
   предпочитай его.
3. При возможности дождись одним реальным процессом ожидания, например
   `sleep <duration>`. Во время ожидания не пиши пользователю heartbeat-сообщения,
   не генерируй «ещё жду», не делай polling каждую минуту и не расходуй tokens на
   пустое ожидание. Sleep не является iteration.
4. После ожидания повтори **ту же** незавершённую iteration.

Официальная документация CLI **не описывает** ни форму сообщения о rate limit, ни
exit code, ни HTTP status, ни retry-after/«next available at» для локального
review. Документирована только per-developer rolling allowance (для CLI она
задана в reviews/hour по плану) и `coderabbit usage`, который показывает счётчик
review и дату сброса текущего billing period, а не время до следующего
допустимого review. Поэтому:

- не зашивай фиксированное число минут как вечный контракт;
- не выдумывай точный reset time, которого провайдер не сообщил;
- если точного времени нет, используй разумную bounded wait/retry policy с
  оглядкой на rolling hourly окно, но не превращай её в частый polling loop;
- при каждом новом rate limit заново смотри фактический вывод установленной
  версии, а не полагайся на этот текст.

Не включай `--use-credits` самостоятельно: это переход на usage-based billed
review, который требует явного согласия пользователя. В agent/headless режиме
провайдер сам возвращает структурированный `action_required`-результат с запросом
подтверждения (`status: "awaiting_confirmation"`) — такой результат является
паузой на согласие, а не завершённым review, не rate limit и не поводом платить
по умолчанию.

Если execution environment физически не позволяет дождаться rate-limit reset
(например процесс или сессия должна завершиться), это не превращается в
repository blocker. Заверши с явным environment-limited состоянием и сообщи
минимум:

- requested iterations;
- completed iterations;
- next unfinished iteration;
- что причина — provider rate limit / environment limitation;
- последний фактически reviewed и pushed HEAD.

Не называй незавершённую rate-limited iteration успешной.

## 6. Operational blockers

Настоящие blockers cycle:

- невозможно однозначно установить repository/branch/base;
- auth CodeRabbit отсутствует и не может быть восстановлен;
- CLI/provider неисправен не из-за обычного rate limit;
- verification после fix остаётся красной;
- commit не создаётся;
- push отвергнут;
- remote divergence требует опасного history rewrite;
- неожиданные пользовательские изменения не позволяют безопасно продолжить.

При blocker не маскируй состояние marker commit'ом: остановись и явно назови
состояние, причину и последний подтверждённый reviewed/pushed HEAD. Repository
blocker и environment-limited завершение по rate limit — разные состояния, и
смешивать их нельзя.

## 7. Отчёт после cycle

После выполнения requested iterations дай компактный итог:

- repository / current branch / base;
- requested iterations;
- completed iterations;
- по каждой iteration: reviewed HEAD, количество provider findings,
  disposition summary, что реально исправлено, commit SHA, push result;
- итоговые результаты verification;
- финальный local/remote HEAD;
- остались ли подтверждённые проблемы;
- были ли rate-limit waits или environment limitations.

Отчёт — не transcript всех команд. При environment-limited или blocker-завершении
отчёт заменяет кладку итога, но обязан содержать перечисленные выше состояния.
