//! Campaign theme contract, resolution, and CSS-variable rendering.
//!
//! Standard `theme` object lives at `surface_config.theme` (falls back to
//! `surface_config` itself when no `theme` key is present) with optional keys:
//!
//! ```json
//! {
//!   "primary_color": "#2563eb",
//!   "accent_color": "#7c3aed",
//!   "background_color": "#ffffff",
//!   "text_color": "#1e293b",
//!   "button_text_color": "#ffffff",
//!   "font_family": "Inter, system-ui, sans-serif",
//!   "border_radius": "12",
//!   "dark_mode": false
//! }
//! ```
//!
//! Missing keys fall back to the INCENTIVE defaults (matching the current
//! hardcoded admin/player colors) so existing surfaces never break visually.
//! Hex colors are validated and normalized; invalid values fall back to the
//! default for that key.

use serde_json::{json, Value};

/// Keys that may appear in a theme object.
const THEME_KEYS: &[&str] = &[
    "primary_color",
    "accent_color",
    "background_color",
    "text_color",
    "button_text_color",
    "font_family",
    "border_radius",
    "dark_mode",
];

/// Default INCENTIVE theme values.
pub fn default_theme() -> Value {
    json!({
        "primary_color": "#2563eb",
        "accent_color": "#7c3aed",
        "background_color": "#ffffff",
        "text_color": "#1e293b",
        "button_text_color": "#ffffff",
        "font_family": "Inter, system-ui, sans-serif",
        "border_radius": "12",
        "dark_mode": false,
    })
}

/// Normalize and validate a hex color. Accepts `#rgb` (expanded to `#rrggbb`)
/// and `#rrggbb`. Returns the normalized lowercase value, or `None` if invalid.
fn normalize_hex(input: &str) -> Option<String> {
    let s = input.trim();
    let hex = s.strip_prefix('#')?;
    if hex.len() == 3 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut expanded = String::with_capacity(7);
        expanded.push('#');
        for c in hex.chars() {
            expanded.push(c);
            expanded.push(c);
        }
        return Some(expanded);
    }
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(format!("#{}", hex.to_ascii_lowercase()));
    }
    None
}

/// Extract the `theme` object from `surface_config` (falling back to the
/// surface_config object itself if no `theme` key is present), then merge
/// missing keys with defaults and validate/normalize hex colors. Always
/// returns a complete, ready-to-use theme object.
pub fn resolve_theme(surface_config: &Value) -> Value {
    let src = surface_config
        .get("theme")
        .filter(|v| v.is_object())
        .unwrap_or(surface_config);

    let mut out = default_theme();

    if let Some(obj) = src.as_object() {
        for key in THEME_KEYS {
            let Some(v) = obj.get(*key) else { continue };
            match *key {
                "dark_mode" => {
                    if let Some(b) = v.as_bool() {
                        out[key] = json!(b);
                    }
                }
                "border_radius" => {
                    if let Some(s) = v.as_str() {
                        if !s.is_empty() {
                            out[key] = json!(s);
                        }
                    } else if v.is_number() {
                        out[key] = json!(v.to_string());
                    }
                }
                "font_family" => {
                    if let Some(s) = v.as_str() {
                        if !s.is_empty() {
                            out[key] = json!(s);
                        }
                    }
                }
                // hex colors
                _ => {
                    if let Some(s) = v.as_str() {
                        if let Some(hex) = normalize_hex(s) {
                            out[key] = json!(hex);
                        }
                    }
                }
            }
        }
    }

    out
}

/// Render a resolved theme as CSS custom-property declarations, e.g.
/// `--is-primary:#ff0000;--is-accent:#7c3aed;--is-bg:#ffffff;...`.
pub fn theme_to_css_vars(theme: &Value) -> String {
    let get = |key: &str, default: &str| -> String {
        theme
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| default.to_string())
    };

    let primary = get("primary_color", "#2563eb");
    let accent = get("accent_color", "#7c3aed");
    let bg = get("background_color", "#ffffff");
    let text = get("text_color", "#1e293b");
    let btn = get("button_text_color", "#ffffff");
    let font = get("font_family", "Inter, system-ui, sans-serif");
    let radius = get("border_radius", "12");
    let radius = if radius.ends_with("px") {
        radius
    } else {
        format!("{}px", radius)
    };

    format!(
        "--is-primary:{primary};--is-accent:{accent};--is-bg:{bg};--is-text:{text};--is-btn-text:{btn};--is-font:{font};--is-radius:{radius};"
    )
}

/// Build a full CSS rule block from a resolved theme: declares the custom
/// properties on `:root` and `[data-is-theme]`, then applies them to common
/// widget/player elements (buttons, inputs, links, highlights).
pub fn theme_css(theme: &Value) -> String {
    let vars = theme_to_css_vars(theme);
    format!(
        ":root, [data-is-theme] {{ {vars} }}\n\
         [data-is-theme] {{ background: var(--is-bg); color: var(--is-text); font-family: var(--is-font); }}\n\
         [data-is-theme] button, [data-is-theme] .is-btn, [data-is-theme] .btn-primary, [data-is-theme] .is-result-btn {{ background: var(--is-primary); color: var(--is-btn-text); border-radius: var(--is-radius); }}\n\
         [data-is-theme] a, [data-is-theme] .is-highlight, [data-is-theme] .is-result, [data-is-theme] .is-accent {{ color: var(--is-accent); }}\n\
         [data-is-theme] input, [data-is-theme] textarea, [data-is-theme] select {{ border-radius: var(--is-radius); }}\n"
    )
}

/// Escape a string for embedding inside a single-quoted JavaScript string
/// literal (used to inject the theme CSS into the runtime widget snippet).
pub fn js_string_literal(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace("</", "<\\/")
}

/// Deep-merge `patch` into `base`, mutating `base`. Objects merge recursively;
/// everything else (arrays, strings, bools, numbers, null) is replaced.
pub fn deep_merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults_when_empty() {
        let t = resolve_theme(&json!({}));
        assert_eq!(t["primary_color"], "#2563eb");
        assert_eq!(t["border_radius"], "12");
        assert_eq!(t["dark_mode"], false);
    }

    #[test]
    fn resolve_overrides_and_fills_gaps() {
        let sc = json!({ "theme": { "primary_color": "#FF0000" } });
        let t = resolve_theme(&sc);
        assert_eq!(t["primary_color"], "#ff0000");
        assert_eq!(t["accent_color"], "#7c3aed");
    }

    #[test]
    fn resolve_invalid_hex_falls_back() {
        let sc = json!({ "theme": { "primary_color": "not-a-color" } });
        let t = resolve_theme(&sc);
        assert_eq!(t["primary_color"], "#2563eb");
    }

    #[test]
    fn resolve_expands_short_hex() {
        let sc = json!({ "theme": { "primary_color": "#f00" } });
        let t = resolve_theme(&sc);
        assert_eq!(t["primary_color"], "#ff0000");
    }

    #[test]
    fn css_vars_include_all_properties() {
        let t = resolve_theme(&json!({}));
        let vars = theme_to_css_vars(&t);
        assert!(vars.contains("--is-primary:#2563eb"));
        assert!(vars.contains("--is-radius:12px"));
        assert!(vars.contains("--is-font:Inter, system-ui, sans-serif"));
    }

    #[test]
    fn deep_merge_merges_theme_subkeys() {
        let mut base = json!({ "theme": { "primary_color": "#111111" }, "tablet": { "a": 1 } });
        let patch = json!({ "theme": { "accent_color": "#222222" } });
        deep_merge(&mut base, &patch);
        assert_eq!(base["theme"]["primary_color"], "#111111");
        assert_eq!(base["theme"]["accent_color"], "#222222");
        assert_eq!(base["tablet"]["a"], 1);
    }
}
