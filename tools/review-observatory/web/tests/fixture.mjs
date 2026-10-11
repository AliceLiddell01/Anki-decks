// Explicit test data. Never imported by application code or served in normal mode.
export const envelope = (data) => ({
  api_version: 1,
  state: "available",
  data,
  provenance: [
    {
      path: "TEST FIXTURE — synthetic review snapshot",
      sha256: null,
      modified_unix_ms: null,
    },
  ],
  diagnostics: [],
});
export const coverage = {
  total_candidates: 6156,
  reviewed_candidates: 3,
  unreviewed_candidates: 6153,
  individual_review: {
    decision_count: 2,
    by_disposition: { acceptable: 1, confirmed: 1 },
  },
  group_review: {
    decisions: { decision_count: 1 },
    covered_candidate_ids: 1,
    representative_candidate_ids: 1,
  },
  findings: { total_findings: 1, by_severity: { high: 1 } },
};
export const reviews = [
  {
    id: "012345678901234567890123",
    namespace: "ТЕСТОВЫЙ СНИМОК · смешанные решения",
    head_sha: "a".repeat(40),
    base_sha: "b".repeat(40),
    workspace_variant: "root",
    updated_unix_ms: 1780000000000,
    candidate_count: 6156,
    unit_count: 4729,
    finding_count: 1,
    coverage,
    queue_summary: null,
    artifacts: {
      review: "available",
      queue: "available",
      triage: "incomplete",
      report: "missing",
      recommendations: "missing",
    },
    state: "incomplete",
  },
  {
    id: "123456789012345678901234",
    namespace: "ТЕСТОВЫЙ СНИМОК · без triage",
    head_sha: "c".repeat(40),
    base_sha: "b".repeat(40),
    workspace_variant: "root",
    updated_unix_ms: 1780000000000,
    candidate_count: 4198,
    unit_count: 3120,
    finding_count: null,
    coverage: null,
    queue_summary: null,
    artifacts: { review: "available", queue: "available", triage: "missing" },
    state: "incomplete",
  },
];
export const health = {
  components: [
    { name: "Review artifacts", state: "available", diagnostics: [] },
    {
      name: "Learning SQLite",
      state: "incomplete",
      diagnostics: ["Устаревшая схема, просмотр не выполняет миграций"],
    },
  ],
  read_only: true,
  poll_interval_ms: 2000,
  limitations: ["Внешняя работа агента не наблюдается"],
};
export const snapshot = envelope({
  revision: "test",
  observed_unix_ms: 1780000000000,
  reviews,
  learning: {
    ...envelope(null),
    state: "incomplete",
    diagnostics: ["SQLite schema v1: миграции выполняет только CLI"],
  },
  health,
});
export const candidates = Array.from({ length: 25 }, (_, i) => ({
  id: "candidate-" + i,
  detector: "literal",
  path: "tools/anki-repo/src/code_review/learning/very_long_evidence_file_name.rs",
  line: 100 + i,
  origin: "introduced_or_changed",
  unit_id: "unit-1",
  priority: "high",
  role: "runtime",
  unknown: i === 2,
  surfaces: ["runtime"],
  disposition: i === 0 ? "acceptable" : i === 1 ? "confirmed" : null,
}));
export const finding = {
  id: "finding-independent",
  severity: "high",
  title: "ТЕСТ: независимое semantic finding",
  description:
    "Подтверждённое замечание без искусственной связи с candidate. Полное описание сценария, последствия и ограничения.",
  provenance: "independent",
  candidate_ids: [],
};
export const unit = {
  id: "unit-1",
  kind: "group",
  priority: "high",
  detector: "literal",
  role: "runtime",
  candidate_count: 1200,
  representative_candidate_ids: ["candidate-0", "candidate-1"],
  unknown: false,
  classification: {
    surfaces: ["runtime"],
    origin: "introduced_or_changed",
    role: "runtime",
  },
};
export const job = {
  id: "test-job-with-unavailable-result",
  review_id: reviews[0].id,
  mode: "sandboxed",
  scope: "workspace",
  lifecycle: "completed",
  status: "unavailable",
  duration_ms: 1250,
  exit: null,
  workspace_removed: false,
  state: "available",
  limitations: ["ТЕСТ: sandbox недоступен; завершение job не означает passed"],
  enforcement: { memory_mb: 256, process_limit: 32 },
  cleanup: "workspace retained",
  request: {
    argv: ["cargo", "check"],
    cwd: "isolated workspace",
    options: { timeout_seconds: 60 },
  },
};
export async function fixtureRoute(route) {
  const url = new URL(route.request().url());
  let response = snapshot;
  if (url.pathname.endsWith("/events"))
    return route.fulfill({
      contentType: "text/event-stream",
      body: 'event: snapshot\ndata: {"revision":"test"}\n\n',
    });
  if (url.pathname.endsWith("/reviews"))
    response = envelope({ items: reviews, total: 2, page: 1, limit: 25 });
  if (/\/reviews\/[a-f0-9]{24}$/.test(url.pathname))
    response = envelope({
      summary: reviews[0],
      target: null,
      queue_summary: null,
      coverage,
      tool_runs: [],
      pipeline: [
        {
          name: "ReviewPack",
          state: "available",
          description: "Исходные evidence собраны",
        },
        {
          name: "ReviewQueue",
          state: "available",
          description: "Структурная очередь доступна",
        },
        {
          name: "Semantic Triage",
          state: "incomplete",
          description: "3 covered IDs, 6153 unreviewed",
        },
        {
          name: "Execution",
          state: "not_observed",
          description: "Состояние jobs не наблюдается",
        },
      ],
      limitations: ["Просмотр не создаёт semantic decisions"],
    });
  if (url.pathname.endsWith("/candidates"))
    response = envelope({
      items: candidates,
      total: 6156,
      page: Number(url.searchParams.get("page") || 1),
      limit: 25,
    });
  if (/\/candidates\/[^/]+$/.test(url.pathname))
    response = envelope({
      candidate: {
        ...candidates[0],
        column: 1,
        snippet: "let outcome = aggregate_all_candidates(&decisions);",
        signals: ["ТЕСТ: обнаружен литерал"],
        source: "literal detector",
        metadata: { semantic_basis: "individual decision" },
      },
      classification: unit.classification,
      unit,
      decision: {
        candidate_id: "candidate-0",
        disposition: "acceptable",
        reason_code: "supported",
        explanation: "ТЕСТ: решение на точном evidence",
        finding_ids: [],
      },
      findings: [],
    });
  if (url.pathname.endsWith("/units"))
    response = envelope({ items: [unit], total: 4729, page: 1, limit: 25 });
  if (/\/units\/[^/]+$/.test(url.pathname))
    response = envelope({
      unit,
      members: { items: candidates, total: 1200, page: 1, limit: 25 },
      signature: "test-signature",
      priority_signals: [],
    });
  if (url.pathname.endsWith("/triage"))
    response = envelope({
      coverage,
      findings: { items: [finding], total: 1, page: 1, limit: 25 },
      individual_decisions: {
        items: [
          {
            candidate_id: "candidate-0",
            disposition: "acceptable",
            reason_code: "supported",
            explanation: "ТЕСТ: полное обоснование",
            finding_ids: [],
          },
        ],
        total: 1,
        page: 1,
        limit: 25,
      },
      group_decisions: {
        items: [
          {
            id: "group-1",
            disposition: "acceptable",
            reason_code: "supported",
            explanation:
              "ТЕСТ: групповое решение покрывает отдельные IDs; representatives показаны отдельно",
            finding_ids: [],
            candidate_count: 50,
            representative_candidate_ids: ["candidate-0", "candidate-1"],
          },
        ],
        total: 1,
        page: 1,
        limit: 25,
      },
    });
  if (/\/triage\/groups\/[^/]+$/.test(url.pathname))
    response = envelope({
      decision: {
        id: "group-1",
        disposition: "acceptable",
        reason_code: "supported",
        explanation: "ТЕСТ: полное обоснование группового покрытия",
        finding_ids: [],
        candidate_count: 50,
        representative_candidate_ids: ["candidate-0", "candidate-1"],
      },
      covered_candidates: {
        items: candidates,
        total: 50,
        page: Number(url.searchParams.get("page") || 1),
        limit: 25,
      },
    });
  if (/\/triage\/findings\/[^/]+$/.test(url.pathname)) {
    const linked = url.pathname.endsWith("finding-linked");
    response = envelope({
      finding: linked
        ? {
            ...finding,
            id: "finding-linked",
            title: "ТЕСТ: связанное finding",
            candidate_ids: candidates.slice(0, 3).map((x) => x.id),
            candidate_count: 50,
            candidate_ids_truncated: true,
          }
        : finding,
      covered_candidates: {
        items: linked ? candidates : [],
        total: linked ? 50 : 0,
        page: Number(url.searchParams.get("page") || 1),
        limit: 25,
      },
    });
  }
  if (url.pathname.endsWith("/report"))
    response = envelope({
      markdown:
        "# ТЕСТ: отчёт\n\n<unsafe-html> остаётся текстом.\nПолное независимое ревью и ограничения источников.",
    });
  if (url.pathname.endsWith("/execution"))
    response = envelope({ items: [job], total: 1, page: 1, limit: 25 });
  if (/\/execution\/[^/]+$/.test(url.pathname)) response = envelope(job);
  if (url.pathname.endsWith("/health")) response = envelope(health);
  if (url.pathname.endsWith("/learning"))
    response =
      process.env.VISUAL_LEARNING === "available"
        ? envelope({
            generation: {
              revision: 7,
              trusted_reviews: 2,
              quarantined_reviews: 1,
              trusted_units: 3,
            },
            schema_version: 3,
            policy_version: 1,
            counts: { imports: 3, feedback: 1 },
            patterns: {
              abstained: false,
              rules_total: 1,
              policy: "ТЕСТ: разные outcomes представлены отдельно",
              limitations: [
                "Частичные и смешанные units не считаются однородными confirmed",
              ],
              rules: [],
            },
          })
        : snapshot.data.learning;
  if (/\/learning\/[^/]+$/.test(url.pathname))
    response =
      process.env.VISUAL_LEARNING === "available"
        ? envelope({
            items: [
              {
                signature: "test-signature-literal-runtime",
                key: { detector: "literal", role: "runtime" },
                support: {
                  support_units: 3,
                  confirmed_units: 0,
                  acceptable_units: 0,
                  mixed_units: 1,
                  partial_units: 1,
                  unresolved_units: 1,
                  level: "insufficient_evidence",
                },
              },
            ],
            total: 1,
            page: 1,
            limit: 25,
          })
        : snapshot.data.learning;
  return route.fulfill({ json: response });
}
