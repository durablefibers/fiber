import { Link, useRouter, useRouterState } from "@tanstack/react-router"
import {
  Activity,
  Box,
  Boxes,
  FolderKanban,
  ListChecks,
  LogOut,
  Orbit,
  Settings,
  Sliders,
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

/**
 * The rail is the resting state. Fiber is canvas-first, and 16rem of chrome is 16rem the
 * DAG does not get; the nav is somewhere you pass through, not somewhere you work.
 */
const SIDEBAR_DEFAULT_OPEN = false

function readSidebarOpen(fallback = SIDEBAR_DEFAULT_OPEN): boolean {
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

/**
 * Collapsed, a nav item is one 16px glyph on near-black and the default neutral
 * `bg-sidebar-accent` barely registers. Tinting the mark itself is what reads at rail
 * width — the shell owns its own emphasis; the shared primitive keeps its neutral default.
 */
const ACTIVE_ITEM =
  "data-active:bg-sky-500/12 data-active:text-sky-100 data-active:[&_svg]:text-sky-400"

const links = [
  { to: "/", label: "Projects", icon: FolderKanban },
  // `Boxes` for the shared pool, `Box` for a single project's agents below: collapsed,
  // the label is gone and one repeated glyph in the same column is unreadable.
  { to: "/agents", label: "Agents", icon: Boxes },
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
  // Start from the default on both sides of hydration, then restore a stored preference
  // that differs. Reading storage in the initializer looks tidier and does not work: this
  // route is server-rendered, the server has no `document` so it emits the default, and
  // React hydrates that markup without re-patching the attribute — the client's "correct"
  // first value is discarded silently. Changing it after mount is what actually commits.
  //
  // So one frame of correction is unavoidable for anyone whose preference differs from
  // the default. Making the rail the default is what shrinks that to the people who
  // deliberately opened the sidebar, instead of everyone who ever closed it.
  const [sidebarOpen, setSidebarOpen] = useState(SIDEBAR_DEFAULT_OPEN)

  useEffect(() => {
    const stored = readSidebarOpen()
    setSidebarOpen((current) => (current === stored ? current : stored))
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
                          className={ACTIVE_ITEM}
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
              // Collapsed, `SidebarGroupLabel` fades to nothing, and the two groups read
              // as one undifferentiated column of glyphs — with no way to tell an
              // instance-wide destination from a project-scoped one. The rule is the
              // label's stand-in, so it appears exactly when the label cannot.
              <SidebarGroup className="group-data-[collapsible=icon]:mt-1 group-data-[collapsible=icon]:border-sidebar-border/60 group-data-[collapsible=icon]:border-t group-data-[collapsible=icon]:pt-2">
                <SidebarGroupLabel className="truncate">
                  {projectName ?? "Project"}
                </SidebarGroupLabel>
                <SidebarGroupContent>
                  <SidebarMenu>
                    <SidebarMenuItem>
                      <SidebarMenuButton
                        tooltip="Overview"
                        className={ACTIVE_ITEM}
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
                        tooltip="Runs"
                        className={ACTIVE_ITEM}
                        isActive={pathname.startsWith(`/p/${projectId}/runs`)}
                        render={
                          <Link
                            to="/p/$projectId/runs"
                            params={{ projectId }}
                          />
                        }
                      >
                        <ListChecks />
                        <span>Runs</span>
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                    <SidebarMenuItem>
                      <SidebarMenuButton
                        tooltip="Project agents"
                        className={ACTIVE_ITEM}
                        isActive={pathname.startsWith(`/p/${projectId}/agents`)}
                        render={
                          <Link
                            to="/p/$projectId/agents"
                            params={{ projectId }}
                          />
                        }
                      >
                        <Box />
                        <span>Agents</span>
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                    <SidebarMenuItem>
                      <SidebarMenuButton
                        tooltip="Durable fibers"
                        className={ACTIVE_ITEM}
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
                    <SidebarMenuItem>
                      <SidebarMenuButton
                        tooltip="Project settings"
                        className={ACTIVE_ITEM}
                        isActive={pathname.startsWith(
                          `/p/${projectId}/settings`
                        )}
                        render={
                          <Link
                            to="/p/$projectId/settings"
                            params={{ projectId }}
                          />
                        }
                      >
                        <Sliders />
                        <span>Settings</span>
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
