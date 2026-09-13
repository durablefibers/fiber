import { createFileRoute, Outlet } from "@tanstack/react-router"

export const Route = createFileRoute("/p/$projectId")({
  component: ProjectLayout,
})

function ProjectLayout() {
  return <Outlet />
}
