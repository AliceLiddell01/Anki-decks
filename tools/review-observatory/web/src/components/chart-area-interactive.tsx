// Adapted from shadcn/ui dashboard-01 ChartAreaInteractive (MIT).
import { useIsMobile } from "@/hooks/use-mobile";
import { useState } from "react";
import { Bar, BarChart, CartesianGrid, XAxis, YAxis } from "recharts";
import {
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardAction,
  CardContent,
} from "@/components/ui/card";
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
} from "@/components/ui/chart";
import type { ReviewSummary } from "@/lib/api";
export function ChartAreaInteractive({
  reviews,
}: {
  reviews: ReviewSummary[];
}) {
  const isMobile = useIsMobile();
  const [metric, setMetric] = useState("candidates");
  const data = reviews
    .filter((x) =>
      metric === "candidates"
        ? x.candidate_count !== null
        : x.unit_count !== null,
    )
    .slice(0, 12)
    .map((x) => ({
      name:
        (x.namespace.length > 24
          ? x.namespace.slice(0, 21) + "…"
          : x.namespace) +
        " · " +
        (x.head_sha?.slice(0, 7) ?? "HEAD"),
      value: metric === "candidates" ? x.candidate_count : x.unit_count,
    }));
  if (!data.length) return null;
  return (
    <Card className="@container/card border-0 shadow-none">
      <CardHeader>
        <CardTitle>Объём снимков</CardTitle>
        <CardDescription>
          Детерминированные сигналы — без оценки семантического качества.
        </CardDescription>
        <CardAction>
          <select
            aria-label="Метрика графика"
            value={metric}
            onChange={(e) => setMetric(e.target.value)}
            className="max-w-32 rounded-md bg-muted px-2 py-1 text-xs"
          >
            <option value="candidates">Кандидаты</option>
            <option value="units">Review units</option>
          </select>
        </CardAction>
      </CardHeader>
      <CardContent className="px-2 sm:px-6">
        <ChartContainer
          config={{
            value: {
              label: metric === "candidates" ? "Кандидаты" : "Review units",
              color: "var(--primary)",
            },
          }}
          className="aspect-auto h-[240px] w-full"
        >
          <BarChart
            accessibilityLayer
            data={data}
            margin={{ left: 0, right: 12, bottom: 8 }}
          >
            <CartesianGrid vertical={false} />
            <XAxis
              hide={isMobile}
              tick={{ fill: "var(--muted-foreground)" }}
              dataKey="name"
              tickLine={false}
              axisLine={false}
              tickMargin={10}
              minTickGap={30}
            />
            <YAxis
              tick={{ fill: "var(--muted-foreground)" }}
              tickLine={false}
              axisLine={false}
              width={42}
            />
            <ChartTooltip content={<ChartTooltipContent />} />
            <Bar
              dataKey="value"
              fill="var(--color-value)"
              radius={[4, 4, 0, 0]}
            />
          </BarChart>
        </ChartContainer>
      </CardContent>
    </Card>
  );
}
