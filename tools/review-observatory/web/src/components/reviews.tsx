import { Link, useParams } from "react-router-dom";
import {
  useApi,
  type Page,
  type ReviewSummary,
  type ReviewDetail,
} from "@/lib/api";
import { Filters, useFilters } from "@/components/filters";
import { DataTable } from "@/components/data-table";
import { SectionCards } from "@/components/section-cards";
import { StateNotice, Status, Provenance } from "@/components/states";
export function Reviews({ revision }: { revision: number }) {
  const { query, set } = useFilters();
  const { value, loading, error } = useApi<Page<ReviewSummary>>(
    "/reviews?" + query,
    revision,
  );
  return (
    <>
      <Filters
        sorts={[
          ["updated_desc", "Новые сначала"],
          ["namespace", "По имени"],
        ]}
      />
      <StateNotice result={value} loading={loading} error={error} />
      {value?.data && (
        <DataTable
          data={value.data}
          onPage={(page) => set("page", String(page))}
          columns={[
            {
              accessorKey: "namespace",
              header: "Снимок",
              cell: ({ row }) => (
                <Link
                  className="text-primary font-medium block max-w-72 whitespace-normal break-words"
                  to={
                    "/reviews/" +
                    row.original.id +
                    "?review_id=" +
                    row.original.id
                  }
                >
                  {row.original.namespace}
                  <span className="block text-xs text-muted-foreground font-mono mt-1">
                    {row.original.head_sha?.slice(0, 12) ?? "HEAD неизвестен"}
                  </span>
                </Link>
              ),
            },
            {
              accessorKey: "candidate_count",
              header: "Кандидаты",
              cell: ({ row }) =>
                row.original.candidate_count?.toLocaleString("ru") ?? "—",
            },
            {
              accessorKey: "unit_count",
              header: "Units",
              cell: ({ row }) =>
                row.original.unit_count?.toLocaleString("ru") ?? "—",
            },
            {
              accessorKey: "finding_count",
              header: "Findings",
              cell: ({ row }) => row.original.finding_count ?? "—",
            },
            {
              accessorKey: "state",
              header: "Источники",
              cell: ({ row }) => <Status value={row.original.state} />,
            },
          ]}
        />
      )}
      {value && <Provenance items={value.provenance} />}
    </>
  );
}
export function Review({ revision }: { revision: number }) {
  const { id } = useParams();
  const { value, loading, error } = useApi<ReviewDetail>(
    id ? "/reviews/" + encodeURIComponent(id) : null,
    revision,
  );
  const { params, set } = useFilters();
  const reportOpen = params.get("report") === "open";
  const report = useApi<{ markdown: string }>(
    id && reportOpen ? "/reviews/" + encodeURIComponent(id) + "/report" : null,
    revision,
  );
  const data = value?.data;
  return (
    <>
      <Link to="/reviews" className="text-sm text-primary">
        ← История ревью
      </Link>
      <StateNotice result={value} loading={loading} error={error} />
      {data && (
        <>
          <div>
            <h2 className="text-xl font-semibold break-words">
              {data.summary.namespace}
            </h2>
            <p className="font-mono text-xs text-muted-foreground mt-2 break-all">
              HEAD {data.summary.head_sha ?? "не наблюдается"}
            </p>
          </div>
          <SectionCards
            items={[
              {
                label: "Кандидаты",
                value: data.summary.candidate_count,
                note: "Полный набор исходных signals",
              },
              {
                label: "Review units",
                value: data.summary.unit_count,
                note: "Группы и individual units",
              },
              {
                label: "Рассмотренные IDs",
                value: data.coverage?.reviewed_candidates,
                note: "По решениям semantic triage",
              },
              {
                label: "Не рассмотрено",
                value: data.coverage?.unreviewed_candidates,
                note: "Отсутствующее решение сохраняется",
              },
            ]}
          />
          <section>
            <h2 className="font-semibold mb-4">Наблюдаемые этапы</h2>
            <div className="divide-y divide-border">
              {data.pipeline.map((stage) => (
                <div key={stage.name} className="flex flex-wrap gap-3 py-4">
                  <div className="flex-1 min-w-48">
                    <p className="font-medium">{stage.name}</p>
                    <p className="text-sm text-muted-foreground mt-1">
                      {stage.description}
                    </p>
                  </div>
                  <Status value={stage.state} />
                </div>
              ))}
            </div>
          </section>
          <nav aria-label="Разделы снимка" className="flex flex-wrap gap-3">
            {[
              ["/evidence", "Изучить evidence"],
              ["/triage", "Semantic Triage"],
              ["/execution", "Execution jobs"],
            ].map(([path, label]) => (
              <Link
                className="text-sm text-primary rounded-md border px-3 py-2"
                key={path}
                to={path + "?review_id=" + id}
              >
                {label} →
              </Link>
            ))}
          </nav>
          {(data.tool_runs?.length ?? 0) > 0 && (
            <section>
              <h2 className="font-semibold mb-3">Источники инструментов</h2>
              {data.tool_runs?.map((tool, i) => (
                <div key={i} className="flex flex-wrap gap-3 py-3">
                  <span>{tool.tool}</span>
                  <Status value={tool.status} />
                  <span className="text-sm text-muted-foreground">
                    {tool.message ?? `${tool.diagnostic_count} диагностик`}
                  </span>
                </div>
              ))}
            </section>
          )}
          <section className="rounded-xl bg-card p-5 space-y-4">
            <button
              aria-expanded={reportOpen}
              className="text-left font-semibold text-primary"
              onClick={() => set("report", reportOpen ? "" : "open")}
            >
              Сохранённый отчёт review-report.md {reportOpen ? "▾" : "▸"}
            </button>
            {reportOpen && (
              <>
                <StateNotice
                  result={report.value}
                  loading={report.loading}
                  error={report.error}
                />
                {report.value?.data && (
                  <pre
                    tabIndex={0}
                    className="max-h-[580px] overflow-auto whitespace-pre-wrap break-words font-mono text-xs leading-6"
                  >
                    {report.value.data.markdown}
                  </pre>
                )}
                {report.value && <Provenance items={report.value.provenance} />}
              </>
            )}
          </section>
          {data.limitations.map((text, i) => (
            <p className="text-sm text-muted-foreground" key={i}>
              {text}
            </p>
          ))}
        </>
      )}
      {value && <Provenance items={value.provenance} />}
    </>
  );
}
