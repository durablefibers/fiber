import { createFileRoute } from "@tanstack/react-router"
import { AppShell } from "@/components/app-shell"
import { AgentsContent } from "@/routes/agents"

export const Route = createFileRoute("/p/$projectId/agents")({
  component: ProjectAgentsPage,
})

function ProjectAgentsPage() {
  const { projectId } = Route.useParams()
  return (
    <AppShell projectId={projectId}>
      <AgentsContent projectId={projectId} />
    </AppShell>
  )
}
