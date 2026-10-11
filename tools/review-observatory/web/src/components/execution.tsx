import { Link } from "react-router-dom";
import { useApi, type Page, type Job } from "@/lib/api";
import { Filters, ReviewSelector, useFilters } from "@/components/filters";
import { DataTable } from "@/components/data-table";
import { StateNotice, Status, Provenance } from "@/components/states";
import { JsonFields } from "@/components/evidence";
import { Button } from "@/components/ui/button";
export function Execution({ revision }: { revision: number }) {
  const { review, params, set, page } = useFilters();
  const selected = params.get("selected");
  const query = new URLSearchParams({ limit: "25", page: String(page) });
  if (review) query.set("review_id", review);
  if (params.get("q")) query.set("q", params.get("q")!);
  const list = useApi<Page<Job>>("/execution?" + query, revision);
  const detail = useApi<Job>(
    selected ? "/execution/" + encodeURIComponent(selected) : null,
    revision,
  );
  const job = detail.value?.data;
  return (
    <>
      <ReviewSelector revision={revision} />
      <Filters />
      <p className="text-xs text-muted-foreground">
        Показаны только собственные execution jobs с сохранённым состоянием.
        Работа внешнего агента не наблюдается.
      </p>
      <StateNotice
        result={list.value}
        loading={list.loading}
        error={list.error}
      />
      {list.value?.data && (
        <DataTable
          data={list.value.data}
          onPage={(page) => set("page", String(page))}
          columns={[
            {
              accessorKey: "id",
              header: "Job",
              cell: ({ row }) => (
                <button
                  className="font-mono text-xs text-primary break-all whitespace-normal text-left max-w-64"
                  onClick={() => set("selected", row.original.id)}
                >
                  {row.original.id}
                </button>
              ),
            },
            {
              accessorKey: "mode",
              header: "Mode",
              cell: ({ row }) => row.original.mode ?? "Не наблюдается",
            },
            {
              accessorKey: "lifecycle",
              header: "Lifecycle",
              cell: ({ row }) => <Status value={row.original.lifecycle} />,
            },
            {
              accessorKey: "status",
              header: "Результат",
              cell: ({ row }) => <Status value={row.original.status} />,
            },
            {
              accessorKey: "duration_ms",
              header: "Время",
              cell: ({ row }) =>
                row.original.duration_ms == null
                  ? "Не наблюдается"
                  : (row.original.duration_ms / 1000).toLocaleString("ru") +
                    " с",
            },
            {
              accessorKey: "state",
              header: "Источник",
              cell: ({ row }) => <Status value={row.original.state} />,
            },
          ]}
        />
      )}
      {selected && (
        <section
          aria-label="Подробности исполнения"
          className="space-y-5 rounded-xl bg-card p-5 sm:p-6"
        >
          <div className="flex justify-between items-center gap-3">
            <h2 className="font-semibold">Сохранённое исполнение</h2>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => set("selected", "")}
            >
              Закрыть
            </Button>
          </div>
          <StateNotice
            result={detail.value}
            loading={detail.loading}
            error={detail.error}
          />
          {job && (
            <>
              <p className="font-mono text-xs break-all">{job.id}</p>
              <div className="flex flex-wrap gap-2">
                <Status value={job.lifecycle} />
                <Status value={job.status} />
                <Status value={job.state} />
              </div>
              <dl className="grid grid-cols-1 sm:grid-cols-2 gap-4 text-sm">
                <div>
                  <dt className="text-muted-foreground">Mode / scope</dt>
                  <dd className="mt-1 break-words">
                    {job.mode ?? "Не наблюдается"} /{" "}
                    {job.scope ?? "Не наблюдается"}
                  </dd>
                </div>
                <div>
                  <dt className="text-muted-foreground">Продолжительность</dt>
                  <dd className="mt-1">
                    {job.duration_ms == null
                      ? "Не наблюдается"
                      : (job.duration_ms / 1000).toLocaleString("ru") + " с"}
                  </dd>
                </div>
                <div>
                  <dt className="text-muted-foreground">Workspace cleanup</dt>
                  <dd className="mt-1">
                    {job.workspace_removed === true
                      ? "Workspace удалён"
                      : job.workspace_removed === false
                        ? "Workspace сохранён"
                        : "Не наблюдается"}
                  </dd>
                </div>
                <div>
                  <dt className="text-muted-foreground">Process exit</dt>
                  <dd>
                    {job.exit ? (
                      <JsonFields value={job.exit} />
                    ) : (
                      <p className="mt-1">Не наблюдается</p>
                    )}
                  </dd>
                </div>
              </dl>
              <Link
                to={"/reviews/" + job.review_id + "?review_id=" + job.review_id}
                className="text-sm text-primary"
              >
                Связанный снимок ревью →
              </Link>
              {job.request && (
                <details>
                  <summary className="cursor-pointer font-medium text-sm">
                    Параметры сохранённого запроса
                  </summary>
                  <JsonFields
                    value={{
                      argv: job.request.argv,
                      cwd: job.request.cwd,
                      options: job.request.options,
                    }}
                  />
                </details>
              )}
              {job.enforcement && (
                <details>
                  <summary className="cursor-pointer font-medium text-sm">
                    Наблюдаемые ограничения ресурсов
                  </summary>
                  <JsonFields value={job.enforcement} />
                </details>
              )}
              {job.cleanup && (
                <p className="text-sm text-muted-foreground break-words">
                  Cleanup: {job.cleanup}
                </p>
              )}
              <section>
                <h3 className="font-medium text-sm mb-2">
                  Ограничения наблюдения
                </h3>
                {job.limitations.length ? (
                  job.limitations.map((text, i) => (
                    <p
                      key={i}
                      className="mt-2 text-sm text-muted-foreground break-words"
                    >
                      {text}
                    </p>
                  ))
                ) : (
                  <p className="text-sm text-muted-foreground">
                    Дополнительные ограничения в сохранённых данных не указаны.
                  </p>
                )}
              </section>
            </>
          )}
          {detail.value && <Provenance items={detail.value.provenance} />}
        </section>
      )}
      {list.value && <Provenance items={list.value.provenance} />}
    </>
  );
}
