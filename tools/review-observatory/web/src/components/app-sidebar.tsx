// Adapted from the MIT licensed shadcn/ui dashboard-01 AppSidebar.
import { NavLink, useLocation } from "react-router-dom";
import {
  IconDashboard,
  IconListDetails,
  IconSearch,
  IconTerminal2,
  IconChecklist,
  IconDatabase,
  IconHeartbeat,
  IconInnerShadowTop,
} from "@tabler/icons-react";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarGroup,
  SidebarGroupContent,
  useSidebar,
} from "@/components/ui/sidebar";
export const sections = [
  ["/", "Обзор", IconDashboard],
  ["/reviews", "Ревью", IconListDetails],
  ["/evidence", "Доказательства", IconSearch],
  ["/execution", "Исполнение", IconTerminal2],
  ["/triage", "Semantic Triage", IconChecklist],
  ["/learning", "Adaptive Learning", IconDatabase],
  ["/health", "Состояние системы", IconHeartbeat],
] as const;
export function AppSidebar() {
  const { search: locationSearch } = useLocation();
  const review = new URLSearchParams(locationSearch).get("review_id");
  const search = review ? "?review_id=" + encodeURIComponent(review) : "";
  const { setOpenMobile } = useSidebar();
  return (
    <Sidebar collapsible="offcanvas" variant="inset">
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton asChild className="h-auto py-3">
              <NavLink to={"/" + search}>
                <IconInnerShadowTop className="size-6! text-primary" />
                <span className="font-semibold text-base">
                  Review Observatory
                </span>
              </NavLink>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>
      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupContent>
            <SidebarMenu>
              {sections.map(([url, title, Icon]) => (
                <SidebarMenuItem key={url}>
                  <SidebarMenuButton asChild>
                    <NavLink
                      to={url + search}
                      end={url === "/"}
                      onClick={() => setOpenMobile(false)}
                      className={({ isActive }) =>
                        isActive ? "bg-sidebar-accent font-medium" : ""
                      }
                    >
                      <Icon />
                      <span>{title}</span>
                    </NavLink>
                  </SidebarMenuButton>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>
      <SidebarFooter>
        <p className="px-3 py-3 text-xs leading-relaxed text-muted-foreground">
          Локальное наблюдение
          <br />
          Действия выполняются через CLI
        </p>
      </SidebarFooter>
    </Sidebar>
  );
}
