-- 20261113 — api_keys lifecycle audit ledger: every credential event on record.
--
-- Mint / revoke / regenerate write one row here (action = 'created' | 'revoked' | 'regenerated')
-- INSIDE the same transaction as the api_keys change, so a credential change and its ledger row
-- commit or roll back together. NEVER a secret: key_prefix only — key_hash and key_sealed are
-- deliberately absent so a ledger dump hands over no credential material.
CREATE TABLE IF NOT EXISTS api_key_events (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    key_id        uuid,
    owner_id      uuid,
    action        text NOT NULL,
    key_prefix    text,
    actor_user_id uuid,
    metadata      jsonb DEFAULT '{}',
    created_at    timestamptz DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_api_key_events_owner
    ON api_key_events (owner_id, created_at DESC);
