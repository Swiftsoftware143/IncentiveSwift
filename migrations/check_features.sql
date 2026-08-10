SELECT key, label, category FROM features WHERE category = 'surface';
SELECT key FROM features WHERE key LIKE 'surface_%' OR key IN ('custom_domains','tablet_mode','widget_embed','full_page','white_label');
