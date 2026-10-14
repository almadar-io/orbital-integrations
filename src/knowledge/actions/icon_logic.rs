//! The icon decisions that need no I/O (`iconLogic.ts`).

use regex::Regex;

use crate::uricomponent::encode_uri_component;

/// Perceptual luminance (0-255) at or above which an icon is light and needs a dark tile.
pub const LIGHT_THRESHOLD: f64 = 170.0;

const DEVICON_SLUG_OVERRIDES: [(&str, &str); 12] = [
    ("c++", "cpp"),
    ("cpp", "cpp"),
    ("c#", "csharp"),
    ("csharp", "csharp"),
    ("nodejs", "nodejs"),
    ("node", "nodejs"),
    ("dotnet", "dotnet"),
    (".net", "dotnet"),
    ("golang", "go"),
    ("typescript", "typescript"),
    ("javascript", "javascript"),
    ("objective-c", "objectivec"),
];

/// Lossless Firestore document id for a label (case-insensitive).
pub fn icon_cache_key(label: &str) -> String {
    format!("i:{}", encode_uri_component(&label.trim().to_lowercase()))
}

pub fn devicon_slug(label: &str) -> String {
    let norm = label.trim().to_lowercase();
    if let Some((_, slug)) = DEVICON_SLUG_OVERRIDES.iter().find(|(k, _)| *k == norm) {
        return (*slug).to_string();
    }
    norm.chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

pub fn hex_luminance(hex: &str) -> f64 {
    let mut h: String = hex.replace('#', "");
    if h.len() == 3 {
        h = h.chars().flat_map(|c| [c, c]).collect();
    }
    if h.len() < 6 {
        return 0.0;
    }
    let channel = |from: usize| f64::from(u8::from_str_radix(&h[from..from + 2], 16).unwrap_or(0));
    0.299 * channel(0) + 0.587 * channel(2) + 0.114 * channel(4)
}

/// `themed` uses currentColor; `dark` is predominantly dark (or unfilled); `light` is predominantly light.
pub fn classify_tone(body: &str) -> &'static str {
    if body.to_lowercase().contains("currentcolor") {
        return "themed";
    }
    let hexes: Vec<&str> = match Regex::new(r"#[0-9a-fA-F]{6}\b") {
        Ok(re) => re.find_iter(body).map(|m| m.as_str()).collect(),
        Err(_) => Vec::new(),
    };
    if hexes.is_empty() {
        return "dark";
    }
    let avg = hexes.iter().map(|h| hex_luminance(h)).sum::<f64>() / hexes.len() as f64;
    if avg >= LIGHT_THRESHOLD {
        "light"
    } else {
        "dark"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_are_case_insensitive_and_lossless() {
        assert_eq!(icon_cache_key(" Light "), "i:light");
        assert_eq!(icon_cache_key("C++"), "i:c%2B%2B");
        assert_ne!(icon_cache_key("a b"), icon_cache_key("a-b"));
    }

    #[test]
    fn slugs_follow_the_overrides_then_strip_to_alphanumerics() {
        assert_eq!(devicon_slug("C#"), "csharp");
        assert_eq!(devicon_slug("Node.js Framework"), "nodejsframework");
    }

    #[test]
    fn tone_is_themed_dark_or_light() {
        assert_eq!(classify_tone("<svg fill=\"currentColor\"/>"), "themed");
        assert_eq!(classify_tone("<svg fill=\"#ffffff\"/>"), "light");
        assert_eq!(classify_tone("<svg fill=\"#102030\"/>"), "dark");
        assert_eq!(classify_tone("<svg/>"), "dark");
    }
}
