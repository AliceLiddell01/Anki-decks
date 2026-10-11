// Adapted from the MIT licensed shadcn/ui dashboard-01 SiteHeader.
import { useState } from "react";
import { IconMoon, IconSun } from "@tabler/icons-react";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { SidebarTrigger } from "@/components/ui/sidebar";
export function SiteHeader({
  title,
  live = "connecting",
}: {
  title: string;
  live?: string;
}) {
  const [dark, setDark] = useState(
    document.documentElement.classList.contains("dark"),
  );
  return (
    <header className="flex h-(--header-height) shrink-0 items-center gap-2 border-b">
      <div className="flex w-full items-center gap-1 px-4 lg:gap-2 lg:px-6">
        <SidebarTrigger aria-label="Открыть навигацию" className="-ml-1" />
        <Separator
          orientation="vertical"
          className="mx-2 data-[orientation=vertical]:h-4"
        />
        <span className="text-sm font-medium truncate">{title}</span>
        <div className="ml-auto flex items-center gap-2">
          <span className="hidden sm:flex items-center gap-2 text-xs text-muted-foreground">
            <span
              className={
                "size-1.5 rounded-full " +
                (live === "connected" ? "bg-primary" : "bg-muted-foreground")
              }
            />
            {live === "connected"
              ? "Live подключено"
              : live === "connecting"
                ? "Подключение…"
                : "Live отключено"}
          </span>
          <Button
            variant="ghost"
            size="icon"
            aria-label={dark ? "Светлая тема" : "Тёмная тема"}
            onClick={() => {
              document.documentElement.classList.toggle("dark", !dark);
              localStorage.setItem(
                "observatory-theme",
                dark ? "light" : "dark",
              );
              setDark(!dark);
            }}
          >
            {dark ? <IconSun /> : <IconMoon />}
          </Button>
        </div>
      </div>
    </header>
  );
}
