import { useSearchParams } from "react-router-dom";
import { Input } from "@/components/ui/input";
import { useApi, type Page, type ReviewSummary } from "@/lib/api";
export function useFilters() {
  const [params, setParams] = useSearchParams();
  const set = (key: string, value: string) => {
    const next = new URLSearchParams(params);
    if (value) next.set(key, value);
    else next.delete(key);
    if (
      ![
        "page",
        "member_page",
        "group_page",
        "finding_page",
        "selected",
        "group_id",
      ].includes(key)
    )
      next.delete("page");
    if (key === "group_id") next.delete("group_page");
    if (key === "selected") {
      next.delete("member_page");
      next.delete("finding_page");
    }
    if (key === "review_id" || key === "view") {
      next.delete("selected");
      next.delete("unit_id");
      next.delete("member_page");
      next.delete("group_id");
      next.delete("group_page");
      next.delete("finding_page");
    }
    setParams(next);
  };
  const query = new URLSearchParams();
  for (const key of [
    "page",
    "q",
    "sort",
    "priority",
    "detector",
    "role",
    "surface",
    "unknown",
    "unit_id",
  ])
    if (params.get(key)) query.set(key, params.get(key)!);
  query.set("limit", "25");
  return {
    params,
    set,
    query,
    page: Number(params.get("page") || 1),
    review: params.get("review_id"),
  };
}
export function Filters({
  sorts,
  children,
}: {
  sorts?: [string, string][];
  children?: React.ReactNode;
}) {
  const { params, set } = useFilters();
  return (
    <div className="flex flex-wrap items-center gap-3">
      <Input
        aria-label="Поиск"
        placeholder="Поиск по сохранённым данным…"
        value={params.get("q") || ""}
        onChange={(e) => set("q", e.target.value)}
        className="w-full sm:max-w-sm"
      />
      {sorts && (
        <select
          aria-label="Сортировка"
          className="h-9 max-w-full rounded-md border bg-card px-3 text-sm"
          value={params.get("sort") || sorts[0][0]}
          onChange={(e) => set("sort", e.target.value)}
        >
          {sorts.map(([value, label]) => (
            <option key={value} value={value}>
              {label}
            </option>
          ))}
        </select>
      )}
      {children}
    </div>
  );
}
export function ReviewSelector({ revision }: { revision: number }) {
  const { value } = useApi<Page<ReviewSummary>>("/reviews?limit=100", revision);
  const { review, set } = useFilters();
  return (
    <label className="flex flex-col sm:flex-row sm:items-center gap-2 text-sm">
      <span className="text-muted-foreground shrink-0">Снимок ревью</span>
      <select
        aria-label="Снимок ревью"
        className="min-w-0 w-full max-w-xl rounded-md border bg-card px-3 py-2"
        value={review || ""}
        onChange={(e) => set("review_id", e.target.value)}
      >
        <option value="">Выберите сохранённый снимок</option>
        {value?.data?.items.map((x) => (
          <option key={x.id} value={x.id}>
            {x.namespace} · {x.head_sha?.slice(0, 10) ?? "неизвестный HEAD"} ·{" "}
            {x.workspace_variant}
          </option>
        ))}
      </select>
    </label>
  );
}
