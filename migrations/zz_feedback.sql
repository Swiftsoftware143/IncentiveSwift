-- zz_feedback — feedback from any signed-in user (kanban B209, t_2b4e36f9).
--
-- A "Submit Feedback" button in the console sends an idea / bug / complaint that is NEVER
-- anonymous or untraceable: the row records WHO (user_id), WHICH ACCOUNT (tenant_id), WHICH APP
-- (app) and WHICH SCREEN (screen) the message was sent from. The team reads it in the admin
-- console (platform operator only) — it is a real, readable queue, not a write-and-drop.
CREATE TABLE IF NOT EXISTS feedback (
    id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    tenant_id uuid NOT NULL,
    user_id uuid NOT NULL,
    app text NOT NULL,
    screen text NOT NULL,
    kind text NOT NULL,
    message text NOT NULL,
    status text NOT NULL DEFAULT 'open',
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_feedback_created_at ON feedback (created_at DESC);
