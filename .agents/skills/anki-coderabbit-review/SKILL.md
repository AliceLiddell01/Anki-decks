---
name: anki-coderabbit-review
description: >-
  Явно запрошенный CodeRabbit review cycle текущего checkout: прогоняет
  установленный CodeRabbit CLI против committed HEAD, независимо проверяет
  каждый finding, исправляет подтверждённое, верифицирует и завершает каждую
  iteration отдельным commit + push (default 3 completed iterations, если
  пользователь не задал своё положительное число). Используй только при явном
  CodeRabbit intent: «сделай ревью CodeRabbit», «запусти CodeRabbit», «прогони
  кодрэббит», «сделай 3 итерации CodeRabbit», «повтори CodeRabbit ещё 2 раза»,
  либо продолжение уже начатого CodeRabbit-цикла. Не используй для обычной
  разработки, generic self-review, generic code review без упоминания
  CodeRabbit, подготовки PR, обычного commit/push, проверки тестов, чтения
  CodeRabbit-документации и изменения самого skill.
whenToUse: >-
  Пользователь явно просит CodeRabbit review текущего checkout (в том числе
  задаёт число итераций или просит продолжить начатый CodeRabbit-цикл), а не
  обычное ревью, разработку или подготовку PR.
---

# Anki-decks: явный CodeRabbit review cycle

## Назначение

Skill владеет **только** процедурой CodeRabbit-review текущего checkout в этом
репозитории: получение review от установленного CodeRabbit CLI, независимая
проверка findings, исправление подтверждённого, верификация и завершение каждой
iteration отдельным commit + push.

Skill не владеет:

- обычной разработкой и generic code review;
- generic self-review без внешнего reviewer;
- подготовкой PR и обычным commit/push;
- содержанием `.coderabbit.yaml` (это отдельная конфигурация репозитория);
- репозиторной verification matrix — её владельцы перечислены в
  `AGENTS.md` и `.codex/context/`;
- review чужого репозитория или server-side review без локального checkout;
- облачными возможностями CodeRabbit Coding Agent.

## Ownership публикации

Общую Git/GitHub процедуру этот skill не владеет и не дублирует: branch, index,
commit, push, exact remote verification, PR create/edit, Draft/Ready, merge и
cleanup принадлежат repository skill `anki-git-workflow`.

CodeRabbit cycle **требует** тот terminal state, который нужен конкретной
iteration, и делегирует публикацию этому владельцу, сохраняя собственные
CodeRabbit-specific semantics: provider invocation и завершение review, разбор и
triage findings, iteration semantics, rate limit и clean-pass semantics остаются
здесь.

Это лёгкий repository skill: дисциплина задаётся этим workflow и существующим
Git/verification контекстом, а не отдельным adapter'ом, state machine или
persistent review state. Не создавай их. Он также не является универсальным
frontend'ом над CodeRabbit: новые возможности CLI не расширяют его scope
автоматически.

## Trigger contract

Skill активируется **только при явном CodeRabbit intent** в текущем запросе.
Примеры, которые маршрутизируются сюда:

- «сделай ревью CodeRabbit»;
- «запусти CodeRabbit»;
- «прогони кодрэббит»;
- «сделай 3 итерации CodeRabbit»;
- «сделай 6 итераций кодрэббит»;
- «повтори CodeRabbit ещё 2 раза»;
- «сделай deep CodeRabbit review» — с явным deep intent;
- «прогони CodeRabbit с фокусом на ...» — если пользователь явно задал focus;
- просьба продолжить явно уже начатый CodeRabbit review cycle.

Deep — **opt-in**, а не режим по умолчанию: обычный запрос CodeRabbit cycle
выполняется обычным review, и full pull request review policy включается только
явным запросом пользователя. Если явный deep-запрос выполнить нельзя (нет
early access, capability недоступна, CLI/server incompatible), это состояние
сообщается, а не подменяется обычным review.

Само слово «ревью» без указания CodeRabbit **не** включает внешний reviewer.
Не активируй skill автоматически для обычной разработки, generic self-review,
generic code review без CodeRabbit intent, подготовки PR, обычного commit/push,
проверки тестов, чтения CodeRabbit-документации и задачи по изменению самого
skill. Реализация или правка этого skill не является запросом запустить
CodeRabbit.

## Iteration contract

```text
target_iterations = explicit_user_count ?? 3
```

Явное положительное число в текущем запросе переопределяет default.

Три — это **default, а не maximum**. Если пользователь явно просит 1, 2, 4, 5, 7
или другое положительное число, выполняется ровно столько completed iterations,
сколько позволяют среда и provider.

Одна **завершённая substantive iteration** — это последовательность:

```text
текущий committed + pushed HEAD
→ CodeRabbit review
→ authoritative завершение provider review
→ независимая перепроверка всех findings
→ исправление confirmed / partially confirmed
→ применимая repository verification
→ отдельный commit
→ публикация текущей ветки через `anki-git-workflow` (push + exact remote
  verification)
→ подтверждённый новый clean/synchronized HEAD
```

Следующая iteration начинается только после подтверждённой публикации предыдущей.
Механику commit/push/remote verification владеет `anki-git-workflow`; здесь
остаётся только требование к её результату.

Счётчик completed iterations увеличивается только тогда, когда provider review
фактически завершён **и** вся последовательность iteration дошла до
верифицированного, запушенного HEAD.

Не считается iteration: rate limit до завершения review; auth failure;
network/provider failure; invalid или malformed provider result; запуск в
неправильном repository; любой failure до фактического получения review
результата; failed или incomplete review, то есть прогон, который не дошёл до
authoritative завершения; review scope, отвергнутый как слишком большой;
запрошенный deep, который оказался недоступен или несовместим; пропущенный
review, когда review scope не содержит изменений.

Пришедшие findings сами по себе не доказывают, что review завершён полностью.

Marker commit требует реально выполненного review. Пропуск review без изменений —
не clean pass: он не расходует iteration и означает, что candidate или base
выбраны неверно либо проверять нечего.

## Clean iteration

Отдельный commit + push требуется после **каждой** завершённой iteration, в том
числе когда CodeRabbit завершил review с `0 findings`, все findings оказались
stale/repeated/false positive либо actionable fixes не потребовались. Тогда
iteration закрывается отдельным marker commit (`git commit --allow-empty`) с
содержательным сообщением о clean CodeRabbit pass.

Если iteration содержит реальные fixes, marker commit не создаётся: commit
содержит эти fixes и имеет смысловое сообщение по фактической root cause.

Clean iteration **не является early stop**: workflow заканчивается после
`target_iterations` завершённых iterations, а не после первого clean pass.

## Инварианты

- Target — **текущий checkout**, а не произвольная выбранная ветка. Ветки не
  переключаются молча.
- В skill не хардкодятся номер PR, имя ветки, SHA, текущая feature и локальные
  пути пользователя.
- CodeRabbit — advisory reviewer, а не источник истины. Каждый finding сначала
  проверяется по актуальному reviewed HEAD и только потом исправляется.
- Commit + push каждой completed iteration входит в явно запрошенный cycle и не
  требует отдельного подтверждения. Это разрешение ограничено текущим cycle и
  текущей веткой; сама публикация выполняется по процедуре `anki-git-workflow` и
  подтверждается фактическим удалённым SHA, а не exit code push.
- Skill **не** разрешает merge, force-push, rebase опубликованной истории без
  отдельной причины, создание другой ветки, переключение на другой PR, смену base
  PR, закрытие PR и публикацию несвязанных локальных изменений.
- Неожиданные пользовательские изменения в worktree не подмешиваются в review
  commit автоматически и не уничтожаются.
- Rate limit provider'а не расходует iteration и не маскируется marker commit'ом.
- Настоящий blocker не маскируется marker commit'ом.
- Явно запрошенная пользователем review-возможность (например deep с focus) не
  подменяется молча другой: если её нельзя выполнить, это сообщается как
  состояние, требующее решения.
- Scope review не сужается и не делится молча: урезанный scope — это другой
  review, а не завершённая iteration.
- Отсутствие данных о завершении не принимается за доказательство успеха.
- Версионно-зависимая CLI-механика не хардкодится как вечный контракт:
  фактический interface установленной версии определяется в начале cycle.
- `.coderabbit.yaml` не мутируется как побочный эффект review и не дублируется в
  документации.
- Skill не становится второй копией repository verification matrix и не
  подменяет её владельцев.

## Подробный workflow

Пошаговая механика — setup и candidate, discovery фактического provider interface,
invocation (включая deep), oversized scope, triage, verification, требование к
публикации completed iteration, rate-limit handling, blockers, границы skill и
финальный отчёт — находится в
[`references/review-workflow.md`](references/review-workflow.md).

Читай его перед началом cycle: `SKILL.md` задаёт контракт и границы, reference
владеет порядком действий. Версионно-зависимые детали CLI живут в reference;
наблюдения там привязаны к конкретной проверенной версии и перепроверяются в
начале каждого cycle, а не считаются вечным контрактом.
