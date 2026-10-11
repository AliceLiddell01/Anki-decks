// Adapted from shadcn/ui dashboard-01 SectionCards (MIT).
import {
  Card,
  CardHeader,
  CardDescription,
  CardTitle,
  CardFooter,
} from "@/components/ui/card";
export interface Metric {
  label: string;
  value: number | null | undefined;
  note: string;
}
export function SectionCards({ items }: { items: Metric[] }) {
  return (
    <div className="grid grid-cols-1 gap-4 @xl/main:grid-cols-2 @5xl/main:grid-cols-4 *:data-[slot=card]:shadow-none">
      {items.map((x) => (
        <Card key={x.label} className="@container/card border-0">
          <CardHeader>
            <CardDescription>{x.label}</CardDescription>
            <CardTitle className="text-3xl font-semibold tabular-nums">
              {x.value == null
                ? "—"
                : new Intl.NumberFormat("ru").format(x.value)}
            </CardTitle>
          </CardHeader>
          <CardFooter className="text-sm text-muted-foreground">
            {x.note}
          </CardFooter>
        </Card>
      ))}
    </div>
  );
}
