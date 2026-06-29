-- Insert surface engagement features
INSERT INTO public.features (id, key, label, category, description) VALUES
    (gen_random_uuid(), 'surface_custom_domains', 'Custom Domains', 'surface', 'Host campaigns on custom domains'),
    (gen_random_uuid(), 'surface_tablet_mode', 'Tablet Mode', 'surface', 'Full-screen tablet-optimized engagement'),
    (gen_random_uuid(), 'surface_widget_embed', 'Widget Embed', 'surface', 'Floating widget for embedding on any website'),
    (gen_random_uuid(), 'surface_full_page', 'Full Page Experience', 'surface', 'Branded full-page gamified campaign landing page'),
    (gen_random_uuid(), 'surface_white_label', 'White Label', 'surface', 'Remove all IncentiveSwift branding from surfaces')
ON CONFLICT (key) DO NOTHING;
