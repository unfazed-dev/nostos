-- Example schema + publication for local-first demo.
-- Mirrors the canonical "tasks" workload used in the benchmark.
CREATE TABLE IF NOT EXISTS tasks (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      UUID NOT NULL,
    assignee_id UUID,
    title       TEXT NOT NULL,
    completed   BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_tasks_org_assignee
    ON tasks (org_id, assignee_id);

-- The publication is what the replicator subscribes to via logical replication.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = 'cairn_pub') THEN
        CREATE PUBLICATION cairn_pub FOR TABLE tasks;
    END IF;
END $$;
