-- Migration: mechanic feature gates (all 12 mechanics + catch-all)
-- Assigns every mechanic_* feature and the 'all_mechanics' catch-all to the
-- pro and enterprise tiers (enabled=true). Free tier gets NO mechanic feature
-- rows, so public play endpoints return 402 (UpgradeRequired) for free-tier
-- accounts while pro/enterprise accounts pass the gate.

-- 1. Ensure the catch-all feature exists.
INSERT INTO public.features (id, key, label, category, description) VALUES
    (gen_random_uuid(), 'all_mechanics', 'All Mechanics', 'mechanic', 'Catch-all flag granting access to every mechanic type')
ON CONFLICT (key) DO NOTHING;

-- 2. Assign every mechanic_* feature + all_mechanics to pro & enterprise tiers.
DO $$
DECLARE
    feat_keys TEXT[] := ARRAY[
        'all_mechanics',
        'mechanic_score_reveal',
        'mechanic_spin_wheel',
        'mechanic_scratch_card',
        'mechanic_personality',
        'mechanic_calculator',
        'mechanic_mystery',
        'mechanic_countdown',
        'mechanic_poll',
        'mechanic_chat',
        'mechanic_leaderboard',
        'mechanic_raffle',
        'mechanic_long_form_qualifier',
        'mechanic_quiz',
        'mechanic_loyalty'
    ];
    fk text;
BEGIN
    FOREACH fk IN ARRAY feat_keys LOOP
        INSERT INTO public.tier_features (tier_id, feature_id, enabled)
        SELECT pt.id, f.id, true
        FROM public.plan_tiers pt
        JOIN public.features f ON f.key = fk
        WHERE pt.slug IN ('pro', 'enterprise')
        ON CONFLICT (tier_id, feature_id) DO NOTHING;
    END LOOP;
END $$;
