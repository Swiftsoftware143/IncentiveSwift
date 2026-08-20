-- Register surface feature keys (simple keys for code check)
INSERT INTO public.features (id, key, label, category, description) VALUES
    (gen_random_uuid(), 'custom_domains', 'Custom Domains (check)', 'surface', 'Custom domain feature gate'),
    (gen_random_uuid(), 'tablet_mode', 'Tablet Mode (check)', 'surface', 'Tablet mode feature gate'),
    (gen_random_uuid(), 'widget_embed', 'Widget Embed (check)', 'surface', 'Widget embed feature gate'),
    (gen_random_uuid(), 'full_page', 'Full Page (check)', 'surface', 'Full page experience feature gate'),
    (gen_random_uuid(), 'white_label', 'White Label (check)', 'surface', 'Remove branding feature gate')
ON CONFLICT (key) DO NOTHING;

-- Assign surface features to enterprise plan
DO $$
DECLARE
    enterprise_tier_id uuid;
    feature_id uuid;
    plan_tiers RECORD;
    feat_keys TEXT[] := ARRAY['custom_domains', 'tablet_mode', 'widget_embed', 'full_page', 'white_label',
                              'surface_custom_domains', 'surface_tablet_mode', 'surface_widget_embed',
                              'surface_full_page', 'surface_white_label'];
BEGIN
    FOR plan_tiers IN SELECT id, slug FROM plan_tiers WHERE slug IN ('enterprise') LOOP
        FOREACH feature_id IN ARRAY feat_keys::text[] LOOP
            BEGIN
                INSERT INTO public.tier_features (tier_id, feature_id, enabled)
                SELECT plan_tiers.id, f.id, true
                FROM public.features f
                WHERE f.key = feature_id
                ON CONFLICT (tier_id, feature_id) DO NOTHING;
            END;
        END LOOP;
    END LOOP;

    -- Assign tablet_mode and widget_embed to pro plan
    FOR plan_tiers IN SELECT id, slug FROM plan_tiers WHERE slug IN ('pro') LOOP
        FOREACH feature_id IN ARRAY ARRAY['tablet_mode', 'widget_embed', 'surface_tablet_mode', 'surface_widget_embed']::text[] LOOP
            BEGIN
                INSERT INTO public.tier_features (tier_id, feature_id, enabled)
                SELECT plan_tiers.id, f.id, true
                FROM public.features f
                WHERE f.key = feature_id
                ON CONFLICT (tier_id, feature_id) DO NOTHING;
            END;
        END LOOP;
    END LOOP;

    -- Assign nothing to free — surface features require upgrade
END $$;
