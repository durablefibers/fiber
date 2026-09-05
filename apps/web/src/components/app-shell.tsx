import { Link, useRouter, useRouterState } from "@tanstack/react-router"
import {
  Activity,
  Box,
  FolderKanban,
  LogOut,
  Orbit,
  Settings,
  Workflow,
} from "lucide-react"
import { useEffect, useState } from "react"
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarRail,
  SidebarTrigger,
} from "@/components/ui/sidebar"
import { TooltipProvider } from "@/components/ui/tooltip"
import { api, setToken } from "@/lib/api"

const SIDEBAR_COOKIE = "sidebar_state"
const SIDEBAR_COOKIE_MAX_AGE = 60 * 60 * 24 * 7

function readSidebarOpen(fallback = true): boolean {
  if (typeof document === "undefined") return fallback
  try {
    const stored = localStorage.getItem(SIDEBAR_COOKIE)
    if (stored === "true") return true
    if (stored === "false") return false
  } catch {
    /* ignore */
  }
  const match = document.cookie.match(/(?:^|; )sidebar_state=([^;]*)/)
  if (match?.[1] === "true") return true
  if (match?.[1] === "false") return false
  return fallback
}

function writeSidebarOpen(open: boolean) {
  document.cookie = `${SIDEBAR_COOKIE}=${open}; path=/; max-age=${SIDEBAR_COOKIE_MAX_AGE}`
  try {
    localStorage.setItem(SIDEBAR_COOKIE, String(open))
  } catch {
    /* ignore */
  }
}

const links = [
  { to: "/", label: "Projects", icon: FolderKanban },
  { to: "/agents", label: "Agents", icon: Box },
  { to: "/settings", label: "Settings", icon: Settings },
] as const

export function AppShell({
  children,
  projectId,
  projectName,
}: {
  children: React.ReactNode
  projectId?: string
  projectName?: string
}) {
  const router = useRouter()
  const pathname = useRouterState({ select: (s) => s.location.pathname })
  const [sidebarOpen, setSidebarOpen] = useState(true)

  useEffect(() => {
    setSidebarOpen(readSidebarOpen(true))
  }, [])

  const onSidebarOpenChange = (open: boolean) => {
    setSidebarOpen(open)
    writeSidebarOpen(open)
  }

  const logout = async () => {
    try {
      await api.logout()
    } catch {
      /* ignore */
    }
    setToken(null)
    await router.navigate({ to: "/login" })
  }

  return (
    <SidebarProvider
      open={sidebarOpen}
      onOpenChange={onSidebarOpenChange}
      className="h-svh! min-h-0!"
    >
      <TooltipProvider>
        <Sidebar collapsible="icon" className="border-sidebar-border">
          <SidebarHeader>
            <SidebarMenu>
              <SidebarMenuItem>
                <SidebarMenuButton
                  size="lg"
                  tooltip="Fiber"
                  className="data-active:bg-transparent"
                  render={<Link to="/" />}
                >
                  <div className="flex aspect-square size-8 items-center justify-center rounded-lg bg-sidebar-primary/15 text-sky-400">
                    <Workflow className="size-4" />
                  </div>
                  <div className="grid flex-1 text-left text-sm leading-tight">
                    <span className="truncate font-semibold tracking-tight">
                      Fiber
                    </span>
                    <span className="truncate text-[10px] text-sidebar-foreground/50 uppercase tracking-[0.14em]">
                      Durable CI
                    </span>
                  </div>
                </SidebarMenuButton>
              </SidebarMenuItem>
            </SidebarMenu>
          </SidebarHeader>

          <SidebarContent>
            <SidebarGroup>
              <SidebarGroupLabel>Navigate</SidebarGroupLabel>
              <SidebarGroupContent>
                <SidebarMenu>
                  {links.map((l) => {
                    const isActive =
                      l.to === "/"
                        ? pathname === "/"
                        : pathname === l.to || pathname.startsWith(`${l.to}/`)
                    return (
                      <SidebarMenuItem key={l.to}>
                        <SidebarMenuButton
                          tooltip={l.label}
                          isActive={isActive}
                          render={<Link to={l.to} />}
                        >
                          <l.icon />
                          <span>{l.label}</span>
                        </SidebarMenuButton>
                      </SidebarMenuItem>
                    )
                  })}
                </SidebarMenu>
              </SidebarGroupContent>
            </SidebarGroup>

            {projectId ? (
              <SidebarGroup>
                <SidebarGroupLabel>
                  {projectName ?? "Project"}
                </SidebarGroupLabel>
                <SidebarGroupContent>
                  <SidebarMenu>
                    <SidebarMenuItem>
                      <SidebarMenuButton
                        tooltip="Overview"
                        isActive={
                          pathname === `/p/${projectId}` ||
                          pathname === `/p/${projectId}/`
                        }
                        render={
                          <Link to="/p/$projectId" params={{ projectId }} />
                        }
                      >
                        <Activity />
                        <span>Overview</span>
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                    <SidebarMenuItem>
                      <SidebarMenuButton
                        tooltip="Durable fibers"
                        isActive={pathname.startsWith(`/p/${projectId}/fibers`)}
                        render={
                          <Link
                            to="/p/$projectId/fibers"
                            params={{ projectId }}
                          />
                        }
                      >
                        <Orbit />
                        <span>Fibers</span>
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                  </SidebarMenu>
                </SidebarGroupContent>
              </SidebarGroup>
            ) : null}
          </SidebarContent>

          <SidebarFooter>
            <SidebarMenu>
              <SidebarMenuItem>
                <SidebarMenuButton
                  tooltip="Sign out"
                  onClick={() => void logout()}
                >
                  <LogOut />
                  <span>Sign out</span>
                </SidebarMenuButton>
              </SidebarMenuItem>
            </SidebarMenu>
          </SidebarFooter>
          <SidebarRail />
        </Sidebar>

        <SidebarInset className="min-h-0 overflow-hidden">
          <div className="flex h-11 shrink-0 items-center gap-2 border-sidebar-border border-b px-3">
            <SidebarTrigger />
            {projectName ? (
              <span className="truncate text-muted-foreground text-sm">
                {projectName}
              </span>
            ) : null}
            <kbd className="ml-auto hidden rounded-md border border-border/70 bg-muted/40 px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground sm:inline-block">
              ⌘B
            </kbd>
          </div>
          <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
            {children}
          </div>
        </SidebarInset>
      </TooltipProvider>
    </SidebarProvider>
  )
}
