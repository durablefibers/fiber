-- Project-scoped agent pools. NULL project_id = global (any project).
ALTER TABLE agents
    ADD COLUMN IF NOT EXISTS project_id UUID REFERENCES projects(id) ON DELETE CASCADE;

CREATE INDEX IF NOT EXISTS idx_agents_project
    ON agents(project_id)
    WHERE project_id IS NOT NULL;
