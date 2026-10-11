import { Link } from "react-router-dom";
import { type ColumnDef } from "@tanstack/react-table";
import {
  useApi,
  type Page,
  type CandidateRow,
  type CandidateDetail,
  type Unit,
  type Json,
} from "@/lib/api";
import { Filters, ReviewSelector, useFilters } from "@/components/filters";
import { DataTable } from "@/components/data-table";
import { StateNotice, Status, Provenance, Empty } from "@/components/states";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
export function Evidence({ revision }: { revision: number }) {
  const { params, query, review, set } = useFilters();
  const units = params.get("view") === "units";
  const selected = params.get("selected");
  const root = review ? "/reviews/" + encodeURIComponent(review) : null;
  const list = useApi<Page<CandidateRow | Unit>>(
    root ? root + (units ? "/units?" : "/candidates?") + query : null,
    revision,
  );
  const detail = useApi<CandidateDetail>(
    root && selected && !units
      ? root + "/candidates/" + encodeURIComponent(selected)
      : null,
    revision,
  );
  const unit = useApi<{
    unit: Unit;
    members: Page<CandidateRow>;
    signature: string | null;
    priority_signals: Json[];
  }>(
    root && selected && units
      ? root +
          "/units/" +
          encodeURIComponent(selected) +
          "?limit=25&page=" +
          (params.get("member_page") || "1")
      : null,
    revision,
  );
  const columns: ColumnDef<CandidateRow | Unit>[] = [
    {
      accessorKey: "id",
      header: units ? "Review unit" : "Кандидат",
      cell: ({ row }) => (
        <button
          className="text-left text-primary font-mono text-xs max-w-64 whitespace-normal break-all"
          onClick={() => set("selected", row.original.id)}
        >
          {row.original.id}
        </button>
      ),
    },
    {
      accessorKey: "priority",
      header: "Baseline",
      cell: ({ row }) => <Status value={row.original.priority} />,
    },
    { accessorKey: "detector", header: "Детектор" },
    {
      accessorKey: "role",
      header: "Роль",
      cell: ({ row }) => (
        <span className="max-w-40 block whitespace-normal">
          {row.original.role ?? "—"}
        </span>
      ),
    },
    units
      ? {
          id: "size",
          header: "Размер группы",
          cell: ({ row }) => (
            <span>
              {"candidate_count" in row.original
                ? row.original.candidate_count
                : "—"}
            </span>
          ),
        }
      : {
          id: "source",
          header: "Место",
          cell: ({ row }) =>
            "path" in row.original ? (
              <span className="block font-mono text-xs min-w-72 max-w-80 whitespace-normal break-all">
                {row.original.path}
                {row.original.line ? ":" + row.original.line : ""}
              </span>
            ) : null,
        },
    {
      id: "surface",
      header: "Поверхность",
      cell: ({ row }) => (
        <span className="block max-w-40 whitespace-normal text-xs">
          {("surfaces" in row.original
            ? row.original.surfaces
            : Array.isArray(row.original.classification?.surfaces)
              ? row.original.classification.surfaces
              : null
          )?.join(", ") || "Не наблюдается"}
        </span>
      ),
    },
    {
      accessorKey: "unknown",
      header: "Контекст",
      cell: ({ row }) => (
        <span className="text-xs">
          {row.original.unknown
            ? "Unknown"
            : row.original.unknown === false
              ? "Классифицирован"
              : "Не наблюдается"}
        </span>
      ),
    },
  ];
  return (
    <>
      <ReviewSelector revision={revision} />
      {!review ? (
        <Empty text="Выберите снимок для исследования evidence" />
      ) : (
        <>
          <div
            className="flex flex-wrap gap-2"
            role="group"
            aria-label="Тип evidence"
          >
            <Button
              variant={units ? "ghost" : "secondary"}
              onClick={() => {
                set("view", "");
              }}
            >
              Candidates
            </Button>
            <Button
              variant={units ? "secondary" : "ghost"}
              onClick={() => {
                set("view", "units");
              }}
            >
              Review units
            </Button>
          </div>
          <Filters
            sorts={
              units
                ? [
                    ["priority", "По baseline priority"],
                    ["size", "По размеру"],
                  ]
                : [
                    ["priority", "По baseline priority"],
                    ["path", "По пути"],
                    ["detector", "По детектору"],
                  ]
            }
          >
            <>
              <select
                aria-label="Приоритет"
                value={params.get("priority") || ""}
                onChange={(e) => set("priority", e.target.value)}
                className="h-9 rounded-md border bg-card px-2 text-sm"
              >
                <option value="">Все приоритеты</option>
                {["high", "normal", "low"].map((x) => (
                  <option key={x}>{x}</option>
                ))}
              </select>
              <Input
                className="sm:w-44"
                aria-label="Детектор"
                placeholder="Детектор"
                value={params.get("detector") || ""}
                onChange={(e) => set("detector", e.target.value)}
              />
              <Input
                className="sm:w-44"
                aria-label="Структурная роль"
                placeholder="Структурная роль"
                value={params.get("role") || ""}
                onChange={(e) => set("role", e.target.value)}
              />
              <Input
                className="sm:w-40"
                aria-label="Поверхность"
                placeholder="File surface"
                value={params.get("surface") || ""}
                onChange={(e) => set("surface", e.target.value)}
              />
              <select
                aria-label="Unknown context"
                value={params.get("unknown") || ""}
                onChange={(e) => set("unknown", e.target.value)}
                className="h-9 rounded-md border bg-card px-2 text-sm"
              >
                <option value="">Любой контекст</option>
                <option value="true">Unknown</option>
                <option value="false">Классифицирован</option>
              </select>
            </>
          </Filters>
          {params.get("unit_id") && (
            <Button
              variant="ghost"
              className="self-start"
              onClick={() => set("unit_id", "")}
            >
              Сбросить фильтр unit_id
            </Button>
          )}
          <p className="text-xs text-muted-foreground">
            Baseline priority задаёт структурная очередь. Просмотр страниц не
            меняет semantic coverage.
          </p>
          <StateNotice
            result={list.value}
            loading={list.loading}
            error={list.error}
          />
          {list.value?.data && (
            <DataTable
              data={list.value.data}
              columns={columns}
              onPage={(page) => set("page", String(page))}
            />
          )}
          {selected && (
            <section
              aria-label="Подробности evidence"
              className="rounded-xl bg-card p-5 sm:p-6 space-y-5 min-w-0"
            >
              <div className="flex items-center justify-between gap-3">
                <h2 className="font-semibold">Точное evidence</h2>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => set("selected", "")}
                >
                  Закрыть
                </Button>
              </div>
              <StateNotice
                result={units ? unit.value : detail.value}
                loading={units ? unit.loading : detail.loading}
                error={units ? unit.error : detail.error}
              />
              {detail.value?.data && !units && (
                <CandidateView data={detail.value.data} review={review} />
              )}
              {unit.value?.data && units && (
                <>
                  <p className="text-sm break-all font-mono">
                    {unit.value.data.unit.id}
                  </p>
                  <div className="flex flex-wrap gap-2">
                    <Status value={unit.value.data.unit.priority} />
                    <span className="text-sm">
                      {unit.value.data.unit.candidate_count} исходных кандидатов
                    </span>
                  </div>
                  <h3 className="text-sm font-medium">Representatives</h3>
                  <ul className="space-y-2">
                    {unit.value.data.unit.representative_candidate_ids.map(
                      (id) => (
                        <li key={id}>
                          <Link
                            className="text-primary text-xs font-mono break-all"
                            to={
                              "/evidence?review_id=" +
                              review +
                              "&selected=" +
                              encodeURIComponent(id)
                            }
                          >
                            {id}
                          </Link>
                        </li>
                      ),
                    )}
                  </ul>
                  <p className="text-sm text-muted-foreground break-all">
                    Signature: {unit.value.data.signature ?? "не наблюдается"}
                  </p>
                  <JsonFields value={unit.value.data.unit.classification} />
                  <h3 className="text-sm font-medium">
                    Members — отдельная страница
                  </h3>
                  <DataTable
                    data={unit.value.data.members}
                    onPage={(page) => set("member_page", String(page))}
                    columns={[
                      {
                        accessorKey: "id",
                        header: "Кандидат",
                        cell: ({ row }) => (
                          <Link
                            className="font-mono text-xs text-primary break-all"
                            to={
                              "/evidence?review_id=" +
                              review +
                              "&selected=" +
                              encodeURIComponent(row.original.id)
                            }
                          >
                            {row.original.id}
                          </Link>
                        ),
                      },
                      {
                        accessorKey: "path",
                        header: "Путь",
                        cell: ({ row }) => (
                          <span className="block min-w-72 max-w-72 break-all whitespace-normal font-mono text-xs">
                            {row.original.path}
                          </span>
                        ),
                      },
                      {
                        accessorKey: "disposition",
                        header: "Semantic outcome",
                        cell: ({ row }) => (
                          <Status
                            value={row.original.disposition ?? "unreviewed"}
                          />
                        ),
                      },
                    ]}
                  />
                </>
              )}
              {(units ? unit.value : detail.value) && (
                <Provenance
                  items={(units ? unit.value : detail.value)!.provenance}
                />
              )}
            </section>
          )}
          {list.value && <Provenance items={list.value.provenance} />}
        </>
      )}
    </>
  );
}
function CandidateView({
  data,
  review,
}: {
  data: CandidateDetail;
  review: string;
}) {
  return (
    <>
      <div>
        <p className="font-mono text-xs break-all">{data.candidate.id}</p>
        <p className="mt-2 font-mono text-sm break-all">
          {data.candidate.path}
          {data.candidate.line ? ":" + data.candidate.line : ""}
        </p>
        <p className="mt-2 text-sm text-muted-foreground">
          {data.candidate.detector} · {data.candidate.origin} ·{" "}
          {data.candidate.source}
        </p>
      </div>
      {data.candidate.snippet && (
        <pre
          className="overflow-x-auto rounded-lg bg-muted p-4 text-xs leading-6"
          tabIndex={0}
        >
          {data.candidate.snippet}
        </pre>
      )}
      <ul className="space-y-2 text-sm">
        {data.candidate.signals.map((x, i) => (
          <li key={i} className="break-words">
            {x}
          </li>
        ))}
      </ul>
      {data.classification && (
        <details>
          <summary className="cursor-pointer text-sm font-medium">
            Structural classification
          </summary>
          <JsonFields value={data.classification} />
        </details>
      )}
      {Object.keys(data.candidate.metadata).length > 0 && (
        <details>
          <summary className="cursor-pointer text-sm font-medium">
            Полное detector evidence
          </summary>
          <JsonFields value={data.candidate.metadata} />
        </details>
      )}
      {data.unit && (
        <Link
          className="text-primary text-sm break-all"
          to={
            "/evidence?review_id=" +
            review +
            "&view=units&selected=" +
            encodeURIComponent(data.unit.id)
          }
        >
          Review unit: {data.unit.id} →
        </Link>
      )}
      {data.source_location && (
        <div className="rounded-lg bg-muted p-4 text-xs space-y-2">
          <p className="font-mono break-all">
            Git snapshot:{" "}
            {data.source_location.snapshot_sha ?? "не наблюдается"} ·{" "}
            {data.source_location.path}
            {data.source_location.line ? ":" + data.source_location.line : ""}
          </p>
          <p className="text-muted-foreground">
            {data.source_location.limitation}
          </p>
          <SourceLink
            location={data.source_location}
            sourceUrl={data.source_url}
          />
        </div>
      )}
      <section>
        <h3 className="font-medium text-sm mb-3">Semantic decision</h3>
        <Status
          value={
            data.decision?.disposition ??
            (data.triage_state === "available"
              ? "unreviewed"
              : data.triage_state)
          }
        />
        {data.decision && (
          <>
            <p className="text-xs text-muted-foreground mt-2">
              Reason: {data.decision.reason_code}
            </p>
            <p className="text-sm mt-2 whitespace-pre-wrap break-words">
              {data.decision.explanation}
            </p>
          </>
        )}
      </section>
      {data.findings === null && (
        <p className="text-sm text-muted-foreground">
          Связанные findings не наблюдаются: triage недоступен.
        </p>
      )}
      {data.findings?.map((finding) => (
        <div key={finding.id}>
          <p className="font-medium">{finding.title}</p>
          <p className="text-sm whitespace-pre-wrap break-words mt-2">
            {finding.description}
          </p>
          <Link
            className="text-sm text-primary"
            to={
              "/triage?review_id=" +
              encodeURIComponent(review) +
              "&selected=" +
              encodeURIComponent(finding.id)
            }
          >
            Finding {finding.id} →
          </Link>
        </div>
      ))}
    </>
  );
}
export function JsonFields({
  value,
}: {
  value: Record<string, Json> | null | undefined;
}) {
  if (!value) return null;
  return (
    <dl className="mt-3 space-y-3 text-xs">
      {Object.entries(value).map(([key, value]) => (
        <div
          key={key}
          className="grid grid-cols-1 sm:grid-cols-[180px_minmax(0,1fr)] gap-1"
        >
          <dt className="text-muted-foreground break-all">{key}</dt>
          <dd className="font-mono whitespace-pre-wrap break-all">
            {typeof value === "string" ? value : JSON.stringify(value, null, 2)}
          </dd>
        </div>
      ))}
    </dl>
  );
}

function SourceLink({
  location,
  sourceUrl,
}: {
  location: NonNullable<CandidateDetail["source_location"]>;
  sourceUrl: CandidateDetail["source_url"];
}) {
  const sha = location.snapshot_sha;
  const parts = location.path.split("/");
  if (
    !sha ||
    !/^[a-f0-9]{40,64}$/i.test(sha) ||
    !location.file_status ||
    ![
      "added",
      "modified",
      "deleted",
      "renamed",
      "copied",
      "type_changed",
    ].includes(location.file_status) ||
    parts.some((x) => !x || x === "." || x === "..") ||
    location.path.includes("\\")
  )
    return null;
  if (!sourceUrl) return null;
  let url: URL;
  try {
    url = new URL(sourceUrl);
  } catch {
    return null;
  }
  if (url.protocol !== "https:" || !url.hostname || url.username || url.password)
    return null;
  return (
    <>
      <a
        href={url.toString()}
        target="_blank"
        rel="noreferrer"
        className="text-primary underline"
      >
        Открыть опубликованный commit ↗
      </a>
      <p className="text-muted-foreground">
        Локальный или неопубликованный commit может быть недоступен на GitHub.
      </p>
    </>
  );
}
