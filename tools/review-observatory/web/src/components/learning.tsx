import {
  useApi,
  type Page,
  type LearningSummary,
  type LearningRecord,
  type Json,
} from "@/lib/api";
import { type ColumnDef } from "@tanstack/react-table";
import { Filters, useFilters } from "@/components/filters";
import { DataTable } from "@/components/data-table";
import { SectionCards } from "@/components/section-cards";
import { StateNotice, Status, Provenance } from "@/components/states";
import { JsonFields } from "@/components/evidence";
import { Button } from "@/components/ui/button";
const collectionNames = [
  ["patterns", "Паттерны"],
  ["observations", "История"],
  ["recommendations", "Рекомендации"],
  ["feedback", "Поправки"],
] as const;
const field = (row: LearningRecord, key: string) =>
  typeof row[key] === "string" || typeof row[key] === "number"
    ? String(row[key])
    : "Не наблюдается";
export function Learning({ revision }: { revision: number }) {
  const { params, query, set } = useFilters();
  const collection = collectionNames.some(
    (x) => x[0] === params.get("collection"),
  )
    ? params.get("collection")!
    : "patterns";
  const summary = useApi<LearningSummary>("/learning", revision);
  const list = useApi<Page<LearningRecord>>(
    "/learning/" + collection + "?" + query,
    revision,
  );
  const data = summary.value?.data;
  const columns: ColumnDef<LearningRecord>[] =
    collection === "patterns"
      ? [
          {
            id: "signature",
            header: "Структурный паттерн",
            cell: ({ row }) => (
              <details className="min-w-64 max-w-lg whitespace-normal">
                <summary className="font-mono text-xs text-primary cursor-pointer break-all">
                  {field(row.original, "signature")}
                </summary>
                <JsonFields value={row.original.key} />
              </details>
            ),
          },
          {
            id: "support",
            header: "Независимые наблюдения",
            cell: ({ row }) =>
              row.original.support?.support_units == null
                ? "Не наблюдается"
                : String(row.original.support.support_units),
          },
          {
            id: "level",
            header: "Поддержка / противоречия",
            cell: ({ row }) => (
              <div className="space-y-2">
                <Status
                  value={
                    typeof row.original.support?.level === "string"
                      ? row.original.support.level
                      : null
                  }
                />
                <details className="text-xs">
                  <summary className="cursor-pointer">Полный срез</summary>
                  <JsonFields value={row.original.support} />
                </details>
              </div>
            ),
          },
        ]
      : collection === "observations"
        ? [
            {
              id: "review",
              header: "Исторический случай",
              cell: ({ row }) => (
                <details className="max-w-sm whitespace-normal">
                  <summary className="cursor-pointer text-primary font-mono text-xs break-all">
                    {field(row.original, "review_id")}
                  </summary>
                  <p className="mt-2 text-xs break-all">
                    HEAD {field(row.original, "head_sha")}
                  </p>
                  <JsonFields
                    value={
                      (row.original.observations_json ??
                        row.original.observations) as
                        Record<string, Json> | undefined
                    }
                  />
                  <p className="mt-2 text-xs break-words">
                    {field(row.original, "outcome")}
                  </p>
                </details>
              ),
            },
            {
              accessorKey: "trust",
              header: "Доверие",
              cell: ({ row }) => (
                <Status value={field(row.original, "trust")} />
              ),
            },
            { accessorKey: "revision", header: "Ревизия" },
            {
              accessorKey: "outcome",
              header: "Семантическая полнота",
              cell: ({ row }) => (
                <Status value={field(row.original, "outcome")} />
              ),
            },
          ]
        : collection === "recommendations"
          ? [
              {
                id: "unit",
                header: "Review unit",
                cell: ({ row }) => (
                  <span className="font-mono text-xs block max-w-64 whitespace-normal break-all">
                    {field(row.original, "unit_id")}
                  </span>
                ),
              },
              {
                id: "baseline",
                header: "Baseline priority",
                cell: ({ row }) => (
                  <Status
                    value={
                      typeof row.original.baseline_priority === "string"
                        ? row.original.baseline_priority
                        : null
                    }
                  />
                ),
              },
              {
                id: "rank",
                header: "Рекомендованный порядок",
                cell: ({ row }) => field(row.original, "recommended_rank"),
              },
              {
                id: "reason",
                header: "Обоснование",
                cell: ({ row }) => (
                  <details className="min-w-64 max-w-lg whitespace-normal">
                    <summary className="cursor-pointer text-primary">
                      Доказательства рекомендации
                    </summary>
                    <JsonFields
                      value={
                        Object.fromEntries(
                          Object.entries(row.original).filter(
                            ([, value]) => value !== undefined,
                          ),
                        ) as Record<string, Json>
                      }
                    />
                  </details>
                ),
              },
            ]
          : [
              {
                id: "feedback",
                header: "Поправка",
                cell: ({ row }) => (
                  <details className="min-w-64 max-w-lg whitespace-normal">
                    <summary className="cursor-pointer text-primary font-mono text-xs break-all">
                      {field(row.original, "event_id")}
                    </summary>
                    <p className="text-sm mt-3 whitespace-pre-wrap break-words">
                      {field(row.original, "explanation")}
                    </p>
                    <JsonFields
                      value={
                        Object.fromEntries(
                          Object.entries(row.original).filter(
                            ([, value]) => value !== undefined,
                          ),
                        ) as Record<string, Json>
                      }
                    />
                  </details>
                ),
              },
              {
                accessorKey: "review_id",
                header: "Историческое ревью",
                cell: ({ row }) => (
                  <span className="font-mono text-xs block max-w-48 whitespace-normal break-all">
                    {field(row.original, "review_id")}
                  </span>
                ),
              },
              {
                accessorKey: "unit_id",
                header: "Unit",
                cell: ({ row }) => (
                  <span className="font-mono text-xs block max-w-48 whitespace-normal break-all">
                    {field(row.original, "unit_id")}
                  </span>
                ),
              },
              {
                accessorKey: "kind",
                header: "Тип поправки",
                cell: ({ row }) => field(row.original, "kind"),
              },
            ];
  return (
    <>
      <StateNotice
        result={summary.value}
        loading={summary.loading}
        error={summary.error}
      />
      {data && (
        <>
          <SectionCards
            items={[
              {
                label: "History revision",
                value: data.generation.revision,
                note: "Сохранённая генерация SQLite",
              },
              {
                label: "Trusted reviews",
                value: data.generation.trusted_reviews,
                note: "Доверенные наблюдения",
              },
              {
                label: "Quarantine",
                value: data.generation.quarantined_reviews,
                note: "Не участвуют в доверенной поддержке",
              },
              {
                label: "Независимые units",
                value: data.generation.trusted_units,
                note: "Группа не становится N наблюдениями",
              },
            ]}
          />
          <p className="text-xs text-muted-foreground">
            Schema {data.schema_version} · Policy {data.policy_version} ·{" "}
            {data.counts.imports} imports · {data.counts.feedback} feedback.{" "}
            {data.patterns.abstained
              ? "История пока не поддерживает вывод."
              : data.patterns.policy}
          </p>
          {data.patterns.limitations.map((text, i) => (
            <p className="text-sm text-muted-foreground break-words" key={i}>
              {text}
            </p>
          ))}
        </>
      )}
      <div
        className="flex flex-wrap gap-2"
        role="group"
        aria-label="Раздел learning"
      >
        {collectionNames.map(([key, label]) => (
          <Button
            variant={collection === key ? "secondary" : "ghost"}
            key={key}
            onClick={() => set("collection", key)}
          >
            {label}
          </Button>
        ))}
      </div>
      <Filters />
      <p className="text-xs text-muted-foreground">
        {collection === "recommendations"
          ? "Baseline priority хранится отдельно от рекомендованного порядка. Сохранённые рекомендации не запускаются и не обновляются при просмотре."
          : "Чтение SQLite не выполняет import, миграции, feedback или изменение history_generation."}
      </p>
      {!(
        summary.value &&
        !summary.value.data &&
        list.value &&
        !list.value.data &&
        summary.value.state === list.value.state &&
        JSON.stringify(summary.value.diagnostics) ===
          JSON.stringify(list.value.diagnostics)
      ) && (
        <StateNotice
          result={list.value}
          loading={list.loading}
          error={list.error}
        />
      )}
      {list.value?.data && (
        <DataTable
          data={list.value.data}
          columns={columns}
          onPage={(page) => set("page", String(page))}
        />
      )}
      {summary.value && <Provenance items={summary.value.provenance} />}
      {list.value && <Provenance items={list.value.provenance} />}
    </>
  );
}
