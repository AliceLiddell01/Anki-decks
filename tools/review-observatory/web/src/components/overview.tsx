import { Link } from "react-router-dom";
import { useApi, type Snapshot } from "@/lib/api";
import { SectionCards } from "@/components/section-cards";
import { ChartAreaInteractive } from "@/components/chart-area-interactive";
import { StateNotice, Status, Provenance, Empty } from "@/components/states";
export function Overview({ revision }: { revision: number }) {
  const { value, loading, error } = useApi<Snapshot>("/snapshot", revision);
  const data = value?.data;
  const latest = data?.reviews[0];
  return (
    <>
      <StateNotice result={value} loading={loading} error={error} />
      {data && (
        <>
          <SectionCards
            items={[
              {
                label: "Известные снимки",
                value: data.reviews.length,
                note: "Сохранённые локальные артефакты",
              },
              {
                label: "Кандидаты последнего снимка",
                value: latest?.candidate_count,
                note: "Сигналы детекторов, не findings",
              },
              {
                label: "Review units",
                value: latest?.unit_count,
                note: "Структурные единицы очереди",
              },
              {
                label: "Semantic findings",
                value: latest?.finding_count,
                note: "Только подтверждённый triage",
              },
            ]}
          />
          <ChartAreaInteractive reviews={data.reviews} />
          <section>
            <div className="flex items-center justify-between mb-4">
              <h2 className="font-semibold">Последние ревью</h2>
              <Link to="/reviews" className="text-sm text-primary">
                Вся история →
              </Link>
            </div>
            {!data.reviews.length ? (
              <Empty text="Сохранённых ревью пока нет" />
            ) : (
              <div className="divide-y divide-border">
                {data.reviews.slice(0, 5).map((review) => (
                  <Link
                    key={review.id}
                    to={"/reviews/" + review.id + "?review_id=" + review.id}
                    className="flex flex-wrap items-center gap-3 py-4"
                  >
                    <div className="min-w-0 basis-full sm:basis-auto sm:flex-1">
                      <p className="font-medium break-words">
                        {review.namespace}
                      </p>
                      <p className="text-xs text-muted-foreground mt-1 font-mono truncate">
                        {review.head_sha ?? "HEAD не наблюдается"}
                      </p>
                    </div>
                    <span className="text-sm text-muted-foreground">
                      {review.candidate_count ?? "—"} кандидатов
                    </span>
                    <Status value={review.state} />
                  </Link>
                ))}
              </div>
            )}
          </section>
          <p className="text-xs text-muted-foreground">
            Просмотр таблиц и страниц не означает semantic review. Работа
            внешнего агента и расход токенов не наблюдаются.
          </p>
        </>
      )}
      {value && <Provenance items={value.provenance} />}
    </>
  );
}
