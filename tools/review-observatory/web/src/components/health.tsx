import { useApi, type Health } from "@/lib/api";
import { StateNotice, Status, Provenance } from "@/components/states";
export function SystemHealth({
  revision,
  live,
}: {
  revision: number;
  live: string;
}) {
  const { value, loading, error } = useApi<Health>("/health", revision);
  const data = value?.data;
  return (
    <>
      <div className="rounded-xl bg-card p-5 sm:p-6">
        <h2 className="font-medium">Live connection</h2>
        <div role="status" className="mt-3 flex items-center gap-2 text-sm">
          <span
            className={
              "size-2 rounded-full " +
              (live === "connected" ? "bg-primary" : "bg-muted-foreground")
            }
          />
          {live === "connected"
            ? "Подключено к SSE"
            : live === "connecting"
              ? "Подключение к серверу…"
              : "Отключено · ожидаем reconnect"}
        </div>
        <p className="mt-3 text-sm text-muted-foreground">
          После reconnect интерфейс получает текущий snapshot. Остановка сервера
          наблюдения не останавливает CLI и review jobs.
        </p>
      </div>
      <StateNotice result={value} loading={loading} error={error} />
      {data && (
        <>
          <section>
            <h2 className="font-semibold mb-3">Источники и целостность</h2>
            <div className="divide-y divide-border">
              {data.components.map((component, i) => (
                <div key={i} className="py-4 flex flex-wrap items-start gap-3">
                  <div className="min-w-0 flex-1">
                    <p className="font-medium break-words">{component.name}</p>
                    {component.diagnostics.length > 0 && (
                      <details className="mt-2 text-sm text-muted-foreground">
                        <summary className="cursor-pointer">
                          Диагностика ({component.diagnostics.length})
                        </summary>
                        {component.diagnostics.map((text, i) => (
                          <p
                            className="mt-3 whitespace-pre-wrap break-words"
                            key={i}
                          >
                            {text}
                          </p>
                        ))}
                      </details>
                    )}
                  </div>
                  <Status value={component.state} />
                </div>
              ))}
            </div>
          </section>
          <section className="rounded-xl bg-muted/40 p-5">
            <h2 className="font-medium mb-3">Условия наблюдения</h2>
            <p className="text-sm text-muted-foreground">
              Доступ:{" "}
              {data.read_only ? "только чтение" : "режим не подтверждён"} ·
              интервал наблюдения {data.poll_interval_ms.toLocaleString("ru")}{" "}
              мс.
            </p>
            {data.limitations.map((text, i) => (
              <p
                className="mt-3 text-sm text-muted-foreground break-words"
                key={i}
              >
                {text}
              </p>
            ))}
          </section>
        </>
      )}
      {value && <Provenance items={value.provenance} />}
    </>
  );
}
