import { test, expect } from "@playwright/test";
import {
  fixtureRoute,
  envelope,
  reviews,
  candidates,
  unit,
  coverage,
} from "./fixture.mjs";
const review = reviews[0].id;
const pages = [
  ["/", "Обзор"],
  ["/reviews", "Ревью"],
  ["/evidence", "Доказательства"],
  ["/execution", "Исполнение"],
  ["/triage", "Semantic Triage"],
  ["/learning", "Adaptive Learning"],
  ["/health", "Состояние системы"],
];
test.beforeEach(async ({ page }) => {
  await page.route("**/api/v1/**", fixtureRoute);
});
for (const [path, title] of pages)
  test(`route ${path} reads API and renders ${title}`, async ({ page }) => {
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));
    await page.goto(path + "?review_id=" + review);
    await expect(page.locator("h1")).toHaveText(title);
    await expect(page.getByText("Загрузка источника…")).toHaveCount(0);
    await expect(page.locator("main")).toBeVisible();
    expect(errors).toEqual([]);
  });
test("navigation preserves review and clears unrelated selection/filter state", async ({
  page,
}) => {
  await page.goto(
    "/evidence?review_id=" +
      review +
      "&selected=candidate-0&q=literal&view=units",
  );
  await page.getByRole("link", { name: "Исполнение", exact: true }).click();
  await expect(page).toHaveURL("/execution?review_id=" + review);
  await expect(page.getByLabel("Снимок ревью")).toHaveValue(review);
});
test("review detail displays nullable pack and coverage without crashing", async ({
  page,
}) => {
  await page.route("**/api/v1/reviews/" + review, (route) =>
    route.fulfill({
      json: {
        ...envelope({
          summary: reviews[1],
          target: null,
          queue_summary: null,
          coverage: null,
          tool_runs: null,
          pipeline: [],
          limitations: ["pack missing"],
        }),
        state: "incomplete",
      },
    }),
  );
  await page.goto("/reviews/" + review);
  await expect(page.getByText("pack missing")).toBeVisible();
  await expect(page.locator("h1")).toHaveText("Ревью");
});
test("evidence pagination, search, filters and details are URL backed", async ({
  page,
}) => {
  await page.goto("/evidence?review_id=" + review);
  await page.getByRole("button", { name: "Следующая страница" }).click();
  await expect(page).toHaveURL(/page=2/);
  await page.getByLabel("Поиск", { exact: true }).fill("literal");
  await expect(page).toHaveURL(/q=literal/);
  expect(new URL(page.url()).searchParams.has("page")).toBe(false);
  await page.getByLabel("Приоритет", { exact: true }).selectOption("high");
  await page.getByLabel("Поверхность", { exact: true }).fill("runtime");
  await page.getByLabel("Unknown context").selectOption("true");
  await expect(page).toHaveURL(/unknown=true/);
  await page.getByRole("button", { name: "candidate-0", exact: true }).click();
  await expect(page).toHaveURL(/selected=candidate-0/);
  await expect(
    page.getByRole("heading", { name: "Точное evidence" }),
  ).toBeVisible();
  await expect(
    page.getByText("let outcome = aggregate_all_candidates(&decisions);"),
  ).toBeVisible();
  await page.reload();
  await expect(page.getByLabel("Поверхность")).toHaveValue("runtime");
  await expect(
    page.getByRole("heading", { name: "Точное evidence" }),
  ).toBeVisible();
});
test("unit representatives and members link to exact candidate evidence", async ({
  page,
}) => {
  await page.goto(
    "/evidence?review_id=" + review + "&view=units&selected=unit-1",
  );
  await expect(
    page.getByRole("heading", { name: "Members — отдельная страница" }),
  ).toBeVisible();
  await page
    .getByRole("link", { name: "candidate-1", exact: true })
    .first()
    .click();
  await expect(page).toHaveURL(/selected=candidate-1/);
  expect(new URL(page.url()).searchParams.has("view")).toBe(false);
});
test("missing triage does not become empty findings or unreviewed success", async ({
  page,
}) => {
  await page.route(
    "**/api/v1/reviews/" + review + "/candidates/candidate-0",
    (route) =>
      route.fulfill({
        json: envelope({
          candidate: {
            ...candidates[0],
            signals: [],
            metadata: {},
            snippet: null,
            source: "detector",
          },
          classification: null,
          unit: null,
          decision: null,
          findings: null,
          triage_state: "missing",
          source_url: null,
          source_location: null,
        }),
      }),
  );
  await page.goto("/evidence?review_id=" + review + "&selected=candidate-0");
  await expect(
    page.getByText("Связанные findings не наблюдаются: triage недоступен."),
  ).toBeVisible();
  await expect(page.getByText("Отсутствует", { exact: true })).toBeVisible();
});
test("independent finding has no invented candidate links", async ({
  page,
}) => {
  await page.goto(
    "/triage?review_id=" + review + "&selected=finding-independent",
  );
  await expect(
    page.getByText("Независимое finding · candidate-связь отсутствует").first(),
  ).toBeVisible();
  await page.getByLabel("Поиск", { exact: true }).fill("независимое");
  await expect(page).toHaveURL(/q=/);
});
test("group coverage paginates covered IDs separately from representatives", async ({
  page,
}) => {
  await page.goto("/triage?review_id=" + review + "&triage_view=groups");
  await page.getByRole("button", { name: "group-1", exact: true }).click();
  await expect(page).toHaveURL(/group_id=group-1/);
  const panel = page.getByRole("region", { name: "Покрытие группы" });
  await expect(
    panel.getByRole("heading", { name: "Covered IDs — 50" }),
  ).toBeVisible();
  await panel.getByRole("button", { name: "Следующая страница" }).click();
  await expect(page).toHaveURL(/group_page=2/);
  await panel.getByRole("link", { name: "candidate-0", exact: true }).click();
  await expect(page).toHaveURL(/\/evidence\?review_id=.*selected=candidate-0/);
});
test("unavailable execution result remains unavailable after completion", async ({
  page,
}) => {
  await page.goto("/execution?review_id=" + review);
  await page
    .getByRole("button", { name: "test-job-with-unavailable-result" })
    .click();
  await expect(page.getByText("workspace retained")).toBeVisible();
  await expect(page.getByText("Недоступно", { exact: true })).toHaveCount(2);
  await expect(
    page.getByText("Проверка пройдена", { exact: true }),
  ).toHaveCount(0);
});
test("all learning collections read versioned API; outdated schema is explicit", async ({
  page,
}) => {
  await page.goto("/learning");
  for (const name of ["История", "Рекомендации", "Поправки", "Паттерны"]) {
    await page.getByRole("button", { name, exact: true }).click();
    await expect(
      page.getByText("SQLite schema v1: миграции выполняет только CLI"),
    ).toBeVisible();
  }
  await expect(page.getByText("Learning ready", { exact: true })).toHaveCount(
    0,
  );
});
test("loading, empty and HTTP error states are explicit", async ({ page }) => {
  let release;
  const pending = new Promise((resolve) => {
    release = resolve;
  });
  await page.route("**/api/v1/reviews?*", async (route) => {
    await pending;
    await route.fulfill({
      json: envelope({ items: [], total: 0, page: 1, limit: 25 }),
    });
  });
  await page.goto("/reviews");
  await expect(page.getByText("Загрузка источника…")).toBeVisible();
  release();
  await expect(page.getByText("Записей не найдено")).toBeVisible();
  await page.unroute("**/api/v1/reviews?*");
  await page.route("**/api/v1/reviews?*", (route) =>
    route.fulfill({ status: 503, json: { error: "unavailable" } }),
  );
  await page.reload();
  await expect(page.getByRole("alert")).toContainText("HTTP 503");
});
test("light/dark theme survives reload; mobile menu closes and contains overflow", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/health");
  await page.getByRole("button", { name: "Тёмная тема", exact: true }).click();
  await expect(page.locator("html")).toHaveClass(/dark/);
  await page.reload();
  await expect(page.locator("html")).toHaveClass(/dark/);
  await page.getByRole("button", { name: "Открыть навигацию" }).click();
  await page.getByRole("link", { name: "Доказательства", exact: true }).click();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await page.getByLabel("Снимок ревью").selectOption(review);
  await expect(page.getByRole("table")).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.getByRole("button", { name: "Светлая тема", exact: true }).click();
  await expect(page.locator("html")).not.toHaveClass(/dark/);
});
test("disconnected live stream is shown independently of artifact health", async ({
  page,
}) => {
  await page.goto("/health");
  await expect(page.getByText("Отключено · ожидаем reconnect")).toBeVisible();
  await expect(page.getByText("Источники и целостность")).toBeVisible();
});
test("source permalink requires SHA, known file status and safe repository path", async ({
  page,
}) => {
  const location = {
    snapshot_sha: "a".repeat(40),
    path: "src/日本 語.rs",
    line: 17,
    state: "not_observed",
    limitation: "test source reference",
    file_status: "modified",
  };
  const detail = {
    candidate: {
      ...candidates[0],
      signals: [],
      metadata: {},
      snippet: null,
      source: "detector",
    },
    classification: null,
    unit: null,
    decision: null,
    findings: [],
    triage_state: "available",
    source_url: null,
    source_location: location,
  };
  await page.route(
    "**/api/v1/reviews/" + review + "/candidates/candidate-0",
    (route) => route.fulfill({ json: envelope(detail) }),
  );
  await page.goto("/evidence?review_id=" + review + "&selected=candidate-0");
  await expect(
    page.getByRole("link", { name: "Открыть опубликованный commit" }),
  ).toHaveAttribute(
    "href",
    "https://github.com/AliceLiddell01/Anki-decks/blob/" +
      location.snapshot_sha +
      "/src/" +
      encodeURIComponent("日本 語.rs") +
      "#L17",
  );
  location.path = "../private";
  await page.reload();
  await expect(
    page.getByRole("heading", { name: "Точное evidence" }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Открыть опубликованный commit" }),
  ).toHaveCount(0);
});

test("finding details paginate all linked candidates beyond compact first three", async ({
  page,
}) => {
  await page.goto("/triage?review_id=" + review + "&selected=finding-linked");
  const panel = page.getByRole("region", {
    name: "Связанные evidence finding",
  });
  await expect(
    panel.getByRole("heading", { name: "ТЕСТ: связанное finding" }),
  ).toBeVisible();
  await expect(
    panel.getByRole("link", { name: "candidate-24", exact: true }),
  ).toHaveCount(1);
  await panel.getByRole("button", { name: "Следующая страница" }).click();
  await expect(page).toHaveURL(/finding_page=2/);
  await panel.getByRole("link", { name: "candidate-0", exact: true }).click();
  await expect(page).toHaveURL(/selected=candidate-0/);
});

test("saved report is read lazily, rendered as text and missing report is explicit", async ({
  page,
}) => {
  await page.goto("/reviews/" + review);
  await page.getByRole("button", { name: /Сохранённый отчёт/ }).click();
  await expect(page.locator("pre")).toContainText(
    "<unsafe-html> остаётся текстом.",
  );
  await expect(page.locator("unsafe-html")).toHaveCount(0);
  await page.route("**/api/v1/reviews/" + review + "/report", (route) =>
    route.fulfill({
      json: {
        ...envelope(null),
        state: "missing",
        diagnostics: ["review-report.md отсутствует"],
      },
    }),
  );
  await page.reload();
  await expect(
    page.getByText("review-report.md отсутствует", { exact: true }),
  ).toBeVisible();
  await expect(page.locator("pre")).toHaveCount(0);
});

test("mobile empty data message stays fully inside viewport", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.route("**/api/v1/execution?*", (route) =>
    route.fulfill({
      json: envelope({ items: [], total: 0, page: 1, limit: 25 }),
    }),
  );
  await page.goto("/execution");
  const message = page.getByText("Записей не найдено", { exact: true });
  await expect(message).toBeVisible();
  const bounds = await message.boundingBox();
  expect(bounds.x).toBeGreaterThanOrEqual(0);
  expect(bounds.x + bounds.width).toBeLessThanOrEqual(390);
  await expect(page.getByRole("table")).toHaveCount(0);
});
