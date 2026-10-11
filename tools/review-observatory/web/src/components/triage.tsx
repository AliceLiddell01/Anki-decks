import { Link } from "react-router-dom";
import {
  useApi,
  type Triage,
  type Finding,
  type Decision,
  type GroupDetail,
  type CandidateRow,
  type FindingDetail,
} from "@/lib/api";
import { Filters, ReviewSelector, useFilters } from "@/components/filters";
import { SectionCards } from "@/components/section-cards";
import { DataTable } from "@/components/data-table";
import { StateNotice, Status, Provenance, Empty } from "@/components/states";
import { Button } from "@/components/ui/button";
export function SemanticTriage({ revision }: { revision: number }) {
  const { review, params, query, set } = useFilters();
  const { value, loading, error } = useApi<Triage>(
    review
      ? "/reviews/" + encodeURIComponent(review) + "/triage?" + query
      : null,
    revision,
  );
  const group = useApi<GroupDetail>(
    review && params.get("group_id")
      ? "/reviews/" +
          encodeURIComponent(review) +
          "/triage/groups/" +
          encodeURIComponent(params.get("group_id")!) +
          "?limit=25&page=" +
          (params.get("group_page") || "1")
      : null,
    revision,
  );
  const selectedFinding = useApi<FindingDetail>(
    review && params.get("selected")
      ? "/reviews/" +
          encodeURIComponent(review) +
          "/triage/findings/" +
          encodeURIComponent(params.get("selected")!) +
          "?limit=25&page=" +
          (params.get("finding_page") || "1")
      : null,
    revision,
  );
  const data = value?.data;
  const tab = params.get("triage_view") || "findings";
  return (
    <>
      <ReviewSelector revision={revision} />
      {!review ? (
        <Empty text="Выберите снимок для просмотра semantic triage" />
      ) : (
        <>
          <StateNotice result={value} loading={loading} error={error} />
          {data && (
            <>
              <SectionCards
                items={[
                  {
                    label: "Individual decisions",
                    value: data.coverage.individual_review.decision_count,
                    note: "Каждое решение одного кандидата",
                  },
                  {
                    label: "Group decisions",
                    value: data.coverage.group_review.decisions.decision_count,
                    note: "Одно решение — одна группа",
                  },
                  {
                    label: "Covered IDs",
                    value: data.coverage.reviewed_candidates,
                    note: "Покрытие исходных кандидатов",
                  },
                  {
                    label: "Unreviewed IDs",
                    value: data.coverage.unreviewed_candidates,
                    note: "Не рассмотрены семантически",
                  },
                ]}
              />
              <div className="rounded-xl bg-muted/40 p-4 text-sm text-muted-foreground">
                Группами покрыто{" "}
                {data.coverage.group_review.covered_candidate_ids.toLocaleString(
                  "ru",
                )}{" "}
                IDs; рассмотрено представителей:{" "}
                {data.coverage.group_review.representative_candidate_ids.toLocaleString(
                  "ru",
                )}
                . Независимые findings могут существовать без candidate-связи.
              </div>
              <div
                className="flex flex-wrap gap-2"
                role="group"
                aria-label="Раздел triage"
              >
                {[
                  ["findings", "Findings"],
                  ["individual", "Individual decisions"],
                  ["groups", "Group decisions"],
                ].map(([key, label]) => (
                  <Button
                    key={key}
                    variant={key === tab ? "secondary" : "ghost"}
                    onClick={() => set("triage_view", key)}
                  >
                    {label}
                  </Button>
                ))}
              </div>
              <Filters />
              {tab === "findings" ? (
                <DataTable<Finding>
                  data={data.findings}
                  onPage={(page) => set("page", String(page))}
                  columns={[
                    {
                      accessorKey: "title",
                      header: "Finding",
                      cell: ({ row }) => (
                        <details
                          open={
                            params.get("selected") === row.original.id ||
                            undefined
                          }
                          className="min-w-72 max-w-xl whitespace-normal"
                        >
                          <summary className="cursor-pointer text-primary font-medium">
                            {row.original.title}
                          </summary>
                          <button
                            className="block font-mono text-xs mt-3 break-all text-primary"
                            onClick={() => set("selected", row.original.id)}
                          >
                            Все связанные evidence:{" "}
                            {row.original.candidate_count ??
                              row.original.candidate_ids.length}{" "}
                            →
                          </button>
                          <p className="mt-3 text-sm whitespace-pre-wrap break-words">
                            {row.original.description}
                          </p>
                          {row.original.candidate_ids.length ? (
                            <ul className="mt-4 space-y-2">
                              {row.original.candidate_ids.map((id) => (
                                <li key={id}>
                                  <Link
                                    className="font-mono text-xs text-primary break-all"
                                    to={
                                      "/evidence?review_id=" +
                                      review +
                                      "&selected=" +
                                      encodeURIComponent(id)
                                    }
                                  >
                                    {id} →
                                  </Link>
                                </li>
                              ))}
                            </ul>
                          ) : (
                            <p className="mt-3 text-xs text-muted-foreground">
                              Независимое finding · candidate-связь отсутствует
                            </p>
                          )}
                        </details>
                      ),
                    },
                    {
                      accessorKey: "severity",
                      header: "Severity",
                      cell: ({ row }) => (
                        <Status value={row.original.severity} />
                      ),
                    },
                    {
                      accessorKey: "provenance",
                      header: "Provenance",
                      cell: ({ row }) => (
                        <span className="text-xs">
                          {row.original.provenance}
                        </span>
                      ),
                    },
                    {
                      id: "links",
                      header: "Evidence IDs",
                      cell: ({ row }) =>
                        row.original.candidate_ids.length || "Независимое",
                    },
                  ]}
                />
              ) : (
                <DataTable<Decision>
                  data={
                    tab === "groups"
                      ? data.group_decisions
                      : data.individual_decisions
                  }
                  onPage={(page) => set("page", String(page))}
                  columns={[
                    {
                      id: "identity",
                      header: tab === "groups" ? "Группа" : "Кандидат",
                      cell: ({ row }) =>
                        row.original.candidate_id ? (
                          <Link
                            className="font-mono text-xs text-primary break-all"
                            to={
                              "/evidence?review_id=" +
                              review +
                              "&selected=" +
                              encodeURIComponent(row.original.candidate_id)
                            }
                          >
                            {row.original.candidate_id}
                          </Link>
                        ) : (
                          <button
                            className="font-mono text-xs break-all text-primary text-left"
                            onClick={() =>
                              set("group_id", row.original.id || "")
                            }
                          >
                            {row.original.id}
                          </button>
                        ),
                    },
                    {
                      accessorKey: "disposition",
                      header: "Disposition",
                      cell: ({ row }) => (
                        <Status value={row.original.disposition} />
                      ),
                    },
                    { accessorKey: "reason_code", header: "Reason" },
                    {
                      id: "explanation",
                      header: "Обоснование",
                      cell: ({ row }) => (
                        <details className="min-w-64 max-w-lg whitespace-normal">
                          <summary className="cursor-pointer">
                            Показать обоснование
                          </summary>
                          <p className="mt-3 whitespace-pre-wrap break-words text-sm">
                            {row.original.explanation}
                          </p>
                          {row.original.candidate_count != null && (
                            <p className="text-xs mt-3">
                              {row.original.candidate_count} покрытых IDs
                            </p>
                          )}
                          {row.original.representative_candidate_ids?.map(
                            (id) => (
                              <Link
                                key={id}
                                className="block mt-2 font-mono text-xs text-primary break-all"
                                to={
                                  "/evidence?review_id=" +
                                  review +
                                  "&selected=" +
                                  encodeURIComponent(id)
                                }
                              >
                                Representative: {id}
                              </Link>
                            ),
                          )}
                          {row.original.finding_ids.map((id) => (
                            <Link
                              key={id}
                              to={
                                "/triage?review_id=" +
                                review +
                                "&selected=" +
                                encodeURIComponent(id)
                              }
                              className="block text-primary text-xs mt-2"
                            >
                              Finding {id}
                            </Link>
                          ))}
                        </details>
                      ),
                    },
                  ]}
                />
              )}
              {params.get("selected") && (
                <section
                  aria-label="Связанные evidence finding"
                  className="rounded-xl bg-card p-5 space-y-4 min-w-0"
                >
                  <div className="flex justify-between gap-3">
                    <h2 className="font-semibold">
                      Связанные evidence finding
                    </h2>
                    <Button variant="ghost" onClick={() => set("selected", "")}>
                      Закрыть
                    </Button>
                  </div>
                  <StateNotice
                    result={selectedFinding.value}
                    loading={selectedFinding.loading}
                    error={selectedFinding.error}
                  />
                  {selectedFinding.value?.data && (
                    <>
                      <h3 className="font-medium">
                        {selectedFinding.value.data.finding.title}
                      </h3>
                      <p className="whitespace-pre-wrap text-sm break-words">
                        {selectedFinding.value.data.finding.description}
                      </p>
                      {selectedFinding.value.data.covered_candidates.total ===
                      0 ? (
                        <p className="text-sm text-muted-foreground">
                          Независимое finding · candidate-связь отсутствует
                        </p>
                      ) : (
                        <DataTable<CandidateRow>
                          data={selectedFinding.value.data.covered_candidates}
                          onPage={(page) => set("finding_page", String(page))}
                          columns={[
                            {
                              accessorKey: "id",
                              header: "Связанный кандидат",
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
                                <span className="block min-w-72 max-w-72 whitespace-normal break-all font-mono text-xs">
                                  {row.original.path}
                                </span>
                              ),
                            },
                          ]}
                        />
                      )}
                      <Provenance items={selectedFinding.value.provenance} />
                    </>
                  )}
                </section>
              )}
              {params.get("group_id") && (
                <section
                  aria-label="Покрытие группы"
                  className="rounded-xl bg-card p-5 space-y-4 min-w-0"
                >
                  <div className="flex justify-between gap-3">
                    <h2 className="font-semibold">Покрытие группы</h2>
                    <Button variant="ghost" onClick={() => set("group_id", "")}>
                      Закрыть
                    </Button>
                  </div>
                  <StateNotice
                    result={group.value}
                    loading={group.loading}
                    error={group.error}
                  />
                  {group.value?.data && (
                    <>
                      <p className="font-mono text-xs break-all">
                        {group.value.data.decision.id}
                      </p>
                      <Status value={group.value.data.decision.disposition} />
                      <p className="text-sm whitespace-pre-wrap">
                        {group.value.data.decision.explanation}
                      </p>
                      <h3 className="font-medium text-sm">Representatives</h3>
                      {group.value.data.decision.representative_candidate_ids?.map(
                        (id) => (
                          <Link
                            key={id}
                            className="block text-primary font-mono text-xs break-all"
                            to={
                              "/evidence?review_id=" +
                              review +
                              "&selected=" +
                              encodeURIComponent(id)
                            }
                          >
                            {id} →
                          </Link>
                        ),
                      )}
                      <h3 className="font-medium text-sm">
                        Covered IDs —{" "}
                        {group.value.data.covered_candidates.total}
                      </h3>
                      <DataTable<CandidateRow>
                        data={group.value.data.covered_candidates}
                        onPage={(page) => set("group_page", String(page))}
                        columns={[
                          {
                            accessorKey: "id",
                            header: "Покрытый кандидат",
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
                              <span className="block min-w-72 max-w-72 whitespace-normal break-all font-mono text-xs">
                                {row.original.path}
                              </span>
                            ),
                          },
                          {
                            accessorKey: "disposition",
                            header: "Решение",
                            cell: ({ row }) => (
                              <Status value={row.original.disposition} />
                            ),
                          },
                        ]}
                      />
                      <Provenance items={group.value.provenance} />
                    </>
                  )}
                </section>
              )}
            </>
          )}
          {value && <Provenance items={value.provenance} />}
        </>
      )}
    </>
  );
}
