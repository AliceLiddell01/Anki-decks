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
  `AGENTS.md` и `.codex/context/`.

Это лёгкий repository skill: дисциплина задаётся этим workflow и существующим
Git/verification контекстом, а не отдельным adapter'ом, state machine или
persistent review state. Не создавай их.

## Trigger contract

Skill активируется **только при явном CodeRabbit intent** в текущем запросе.
Примеры, которые маршрутизируются сюда:

- «сделай ревью CodeRabbit»;
- «запусти CodeRabbit»;
- «прогони кодрэббит»;
- «сделай 3 итерации CodeRabbit»;
- «сделай 6 итераций кодрэббит»;
- «повтори CodeRabbit ещё 2 раза»;
- просьба продолжить явно уже начатый CodeRabbit review cycle.

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
→ push текущей ветки
→ подтверждение нового clean/synchronized HEAD
```

Следующая iteration начинается только после успешного commit + push предыдущей.

Счётчик completed iterations увеличивается только тогда, когда provider review
фактически завершён **и** вся последовательность iteration дошла до
верифицированного, запушенного HEAD.

Не считается iteration: rate limit до завершения review; auth failure;
network/provider failure; invalid или malformed provider result; запуск в
неправильном repository; любой failure до фактического получения review
результата; пропущенный review, когда review scope не содержит изменений.

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
  текущей веткой.
- Skill **не** разрешает merge, force-push, rebase опубликованной истории без
  отдельной причины, создание другой ветки, переключение на другой PR, смену base
  PR, закрытие PR и публикацию несвязанных локальных изменений.
- Неожиданные пользовательские изменения в worktree не подмешиваются в review
  commit автоматически и не уничтожаются.
- Rate limit provider'а не расходует iteration и не маскируется marker commit'ом.
- Настоящий blocker не маскируется marker commit'ом.
- `.coderabbit.yaml` не мутируется как побочный эффект review и не дублируется в
  документации.
- Skill не становится второй копией repository verification matrix и не
  подменяет её владельцев.

## Подробный workflow

Пошаговая механика — setup и candidate, discovery фактического provider interface,
invocation, triage, verification, commit/push, rate-limit handling, blockers и
финальный отчёт — находится в
[`references/review-workflow.md`](references/review-workflow.md).

Читай его перед началом cycle: `SKILL.md` задаёт контракт и границы, reference
владеет порядком действий.
