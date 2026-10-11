// Adapted from the MIT licensed dashboard-01 DataTable: header/body render,
// table shell and page controls. Read-only server pages replace demo DnD/editing.
import {
  flexRender,
  getCoreRowModel,
  useReactTable,
  type ColumnDef,
} from "@tanstack/react-table";
import { IconChevronLeft, IconChevronRight } from "@tabler/icons-react";
import {
  Table,
  TableHeader,
  TableBody,
  TableRow,
  TableHead,
  TableCell,
} from "@/components/ui/table";
import { Button } from "@/components/ui/button";
import type { Page } from "@/lib/api";
export function DataTable<T>({
  data,
  columns,
  onPage,
}: {
  data: Page<T>;
  columns: ColumnDef<T>[];
  onPage: (page: number) => void;
}) {
  const table = useReactTable({
    data: data.items,
    columns,
    getCoreRowModel: getCoreRowModel(),
    manualPagination: true,
    rowCount: data.total,
    state: { pagination: { pageIndex: data.page - 1, pageSize: data.limit } },
  });
  const count = Math.max(1, Math.ceil(data.total / data.limit));
  if (data.items.length === 0)
    return (
      <div className="rounded-xl border px-5 py-10 text-center space-y-3">
        <p className="font-medium">Записей не найдено</p>
        <p className="text-sm text-muted-foreground">
          {data.total === 0
            ? "Измените фильтры или выберите другой снимок."
            : "На выбранной странице нет записей."}
        </p>
        {data.total > 0 && (
          <Button variant="outline" onClick={() => onPage(1)}>
            Вернуться на первую страницу
          </Button>
        )}
      </div>
    );
  return (
    <div className="flex min-w-0 flex-col gap-4">
      <p className="text-xs text-muted-foreground sm:hidden">
        Все столбцы доступны при прокрутке таблицы →
      </p>
      <div
        tabIndex={0}
        role="region"
        aria-label="Таблица данных"
        className="max-h-[580px] overflow-auto rounded-lg border"
      >
        <Table className="min-w-[640px]">
          <TableHeader className="sticky top-0 z-10 bg-muted">
            {table.getHeaderGroups().map((group) => (
              <TableRow key={group.id}>
                {group.headers.map((header) => (
                  <TableHead key={header.id}>
                    {header.isPlaceholder
                      ? null
                      : flexRender(
                          header.column.columnDef.header,
                          header.getContext(),
                        )}
                  </TableHead>
                ))}
              </TableRow>
            ))}
          </TableHeader>
          <TableBody>
            {table.getRowModel().rows.length ? (
              table.getRowModel().rows.map((row) => (
                <TableRow key={row.id}>
                  {row.getVisibleCells().map((cell) => (
                    <TableCell
                      key={cell.id}
                      className="py-3 align-top first:min-w-36"
                    >
                      {flexRender(
                        cell.column.columnDef.cell,
                        cell.getContext(),
                      )}
                    </TableCell>
                  ))}
                </TableRow>
              ))
            ) : (
              <TableRow>
                <TableCell
                  colSpan={columns.length}
                  className="h-24 text-center text-muted-foreground"
                >
                  Записей не найдено
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </div>
      <div className="flex flex-wrap items-center justify-between gap-3 text-sm">
        <span className="text-muted-foreground">
          {data.total.toLocaleString("ru")} записей · {data.items.length} на
          странице
        </span>
        <div className="flex items-center gap-3">
          <span>
            Страница {data.page} из {count}
          </span>
          <Button
            variant="outline"
            size="icon"
            className="size-8"
            aria-label="Предыдущая страница"
            disabled={data.page <= 1}
            onClick={() => onPage(data.page - 1)}
          >
            <IconChevronLeft />
          </Button>
          <Button
            variant="outline"
            size="icon"
            className="size-8"
            aria-label="Следующая страница"
            disabled={data.page >= count}
            onClick={() => onPage(data.page + 1)}
          >
            <IconChevronRight />
          </Button>
        </div>
      </div>
    </div>
  );
}
