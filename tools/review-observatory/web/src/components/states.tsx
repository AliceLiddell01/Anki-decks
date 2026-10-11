import type { Envelope } from "@/lib/api";
import { Badge } from "@/components/ui/badge";
export const stateLabels: Record<string, string> = {
  available: "Доступно",
  missing: "Отсутствует",
  incomplete: "Неполные данные",
  invalid: "Невалидно",
  unavailable: "Недоступно",
  not_observed: "Не наблюдается",
  confirmed: "Подтверждено",
  acceptable: "Допустимо",
  unreviewed: "Не рассмотрено",
  mixed: "Смешанное",
  partial: "Частичное",
  unresolved: "Не разрешено",
  running: "Выполняется",
  completed: "Завершено",
  failed: "Ошибка",
  passed: "Проверка пройдена",
  unknown: "Неизвестно",
};
export function Status({ value }: { value: string | null | undefined }) {
  return (
    <Badge
      variant="outline"
      className="max-w-full self-start font-normal whitespace-normal"
    >
      {value ? (stateLabels[value] ?? value) : "Не наблюдается"}
    </Badge>
  );
}
export function StateNotice({
  result,
  loading,
  error,
}: {
  result: Envelope<unknown> | null;
  loading: boolean;
  error: string | null;
}) {
  if (loading)
    return (
      <div role="status" className="rounded-xl bg-muted/40 px-6 py-10">
        <p className="font-medium">Загрузка источника…</p>
        <p className="mt-2 text-sm text-muted-foreground">
          Читаем сохранённый снимок.
        </p>
      </div>
    );
  if (error)
    return (
      <div role="alert" className="rounded-xl bg-muted/40 p-6">
        <p className="font-medium">Не удалось получить данные</p>
        <p className="mt-2 text-sm text-muted-foreground">{error}</p>
        <p className="mt-2 text-sm text-muted-foreground">
          Запустите Review Observatory явно и обновите страницу.
        </p>
      </div>
    );
  if (!result) return null;
  if (result.state === "available" && !result.diagnostics.length) return null;
  return (
    <div role="status" className="rounded-xl bg-muted/40 p-5">
      <Status value={result.state} />
      {result.diagnostics.map((text, i) => (
        <p className="mt-2 text-sm text-muted-foreground break-words" key={i}>
          {text}
        </p>
      ))}
      {!result.data && !result.diagnostics.length && (
        <p className="mt-2 text-sm text-muted-foreground">
          Источник этого этапа ещё не появился. Отсутствие данных не означает
          успешную проверку.
        </p>
      )}
    </div>
  );
}
export function Empty({ text = "Записей не найдено" }: { text?: string }) {
  return (
    <div className="py-12 text-center">
      <p className="font-medium">{text}</p>
      <p className="mt-2 text-sm text-muted-foreground">
        Измените фильтры или выберите другой снимок.
      </p>
    </div>
  );
}
export function Provenance({
  items,
}: {
  items: Envelope<unknown>["provenance"];
}) {
  return items.length ? (
    <details className="text-xs text-muted-foreground">
      <summary className="cursor-pointer">
        Источники и provenance ({items.length})
      </summary>
      <ul className="mt-3 space-y-2">
        {items.map((x) => (
          <li key={x.path} className="break-all">
            <p>{x.path}</p>
            {x.sha256 && <p className="font-mono">SHA256 {x.sha256}</p>}
          </li>
        ))}
      </ul>
    </details>
  ) : null;
}
