import { SemanticTriage } from "@/components/triage";
import { Execution } from "@/components/execution";
import { Learning } from "@/components/learning";
import { SystemHealth } from "@/components/health";
import { Evidence } from "@/components/evidence";
import { Overview } from "@/components/overview";
import { useLive } from "@/lib/api";
import { Reviews, Review } from "@/components/reviews";
import { Routes, Route, useLocation } from "react-router-dom";
import { AppSidebar, sections } from "@/components/app-sidebar";
import { SiteHeader } from "@/components/site-header";
import { SidebarProvider, SidebarInset } from "@/components/ui/sidebar";
export function App() {
  const { pathname } = useLocation();
  const live = useLive();
  const title = sections.find((x) => x[0] === pathname)?.[1] ?? "Ревью";
  return (
    <SidebarProvider
      style={
        {
          "--sidebar-width": "17rem",
          "--header-height": "3.5rem",
        } as React.CSSProperties
      }
    >
      <AppSidebar />
      <SidebarInset className="min-w-0">
        <SiteHeader title="Review Observatory" live={live.state} />
        <div className="@container/main flex flex-1 flex-col gap-6 p-4 lg:p-6">
          <div>
            <p className="text-xs text-muted-foreground mb-2">
              ANKI-DECKS / CODE REVIEW
            </p>
            <h1 className="text-2xl font-semibold">{title}</h1>
            <p className="mt-2 text-sm text-muted-foreground">
              История, доказательства и наблюдаемое состояние ревью.
            </p>
          </div>
          <Routes>
            <Route path="/" element={<Overview revision={live.revision} />} />
            <Route
              path="/reviews"
              element={<Reviews revision={live.revision} />}
            />
            <Route
              path="/reviews/:id"
              element={<Review revision={live.revision} />}
            />
            <Route
              path="/evidence"
              element={<Evidence revision={live.revision} />}
            />
            <Route
              path="/triage"
              element={<SemanticTriage revision={live.revision} />}
            />
            <Route
              path="/execution"
              element={<Execution revision={live.revision} />}
            />
            <Route
              path="/learning"
              element={<Learning revision={live.revision} />}
            />
            <Route
              path="/health"
              element={
                <SystemHealth revision={live.revision} live={live.state} />
              }
            />
          </Routes>
        </div>
      </SidebarInset>
    </SidebarProvider>
  );
}
