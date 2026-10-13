//! Tiny colour helpers: normalise to an `rgba()` CSS string.
//!
//! A faithful port of `colors.py`. Components are kept as `i64` (not clamped
//! to 0-255) so an out-of-range `rgb(...)` string round-trips exactly as the
//! Python did.

/// Return `(r, g, b)` from `#rgb`, `#rrggbb`, or `rgb(...)`/`rgba(...)`.
fn parse(color: &str) -> (i64, i64, i64) {
    let color = color.trim();
    if color.is_empty() {
        return (0, 0, 0);
    }

    if let Some(hex) = color.strip_prefix('#') {
        // expand shorthand `#rgb` -> `#rrggbb`
        let expanded;
        let hex: &str = if hex.len() == 3 {
            expanded = hex.chars().flat_map(|c| [c, c]).collect::<String>();
            &expanded
        } else {
            hex
        };
        if hex.len() >= 6 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                i64::from_str_radix(&hex[0..2], 16),
                i64::from_str_radix(&hex[2..4], 16),
                i64::from_str_radix(&hex[4..6], 16),
            ) {
                return (r, g, b);
            }
        }
        return (0, 0, 0);
    }

    if color.starts_with("rgb(") || color.starts_with("rgba(") {
        if let (Some(lp), Some(rp)) = (color.find('('), color.rfind(')')) {
            let nums: Vec<&str> = color[lp + 1..rp].split(',').collect();
            if nums.len() >= 3 {
                // Python does int(float(x)) — parse as float, truncate toward 0.
                let comp = |s: &str| s.trim().parse::<f64>().ok().map(|v| v as i64);
                if let (Some(r), Some(g), Some(b)) =
                    (comp(nums[0]), comp(nums[1]), comp(nums[2]))
                {
                    return (r, g, b);
                }
            }
        }
        return (0, 0, 0);
    }

    (0, 0, 0)
}

/// Format an alpha value like Python's `("%.3f" % a).rstrip("0").rstrip(".")`.
fn fmt_alpha(a: f64) -> String {
    let s = format!("{:.3}", a);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// `color` normalised to `rgba(r,g,b,a)`.
pub fn rgba(color: &str, alpha: f64) -> String {
    let (r, g, b) = parse(color);
    format!("rgba({},{},{},{})", r, g, b, fmt_alpha(alpha))
}

/// `color` pulled `t` of the way toward mid grey (0 = unchanged, 1 = grey).
///
/// For chrome that should read as secondary beside the text it sits next to.
/// Derived from that text colour rather than being a grey of its own, so it
/// inherits whatever contrast against the background the user picked the text
/// for: a fixed light grey is invisible on a pale panel, and a fixed dark one
/// disappears on a dark panel — this stays visible on either while never
/// competing with the text.
pub fn muted(color: &str, t: f64) -> String {
    let (r, g, b) = parse(color);
    let t = t.clamp(0.0, 1.0);
    let mix = |v: i64| (v as f64 + (128.0 - v as f64) * t).round() as i64;
    format!("rgb({},{},{})", mix(r), mix(g), mix(b))
}

/// A readable text colour (near-black or near-white) for this background.
///
/// The threshold is deliberately high. At the original 0.6 it fell right in
/// the middle of ordinary mid-tone accents — `#62a0ea` (0.59) got light text
/// while the barely-different `#7aa2f7` (0.63) got dark, so two hedges that
/// look the same colour were captioned in opposite ones. Only genuinely pale
/// accents now take dark text.
pub fn contrast_text(color: &str) -> String {
    let (r, g, b) = parse(color);
    let lum = (0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64) / 255.0;
    if lum > 0.75 {
        "#12141c".to_string()
    } else {
        "#f4f5f9".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_forms() {
        assert_eq!(rgba("#7aa2f7", 1.0), "rgba(122,162,247,1)");
        assert_eq!(rgba("#fff", 0.5), "rgba(255,255,255,0.5)");
        assert_eq!(rgba("#12141c", 0.55), "rgba(18,20,28,0.55)");
    }

    #[test]
    fn alpha_formatting() {
        assert_eq!(rgba("#000", 0.0), "rgba(0,0,0,0)");
        assert_eq!(rgba("#000", 1.0), "rgba(0,0,0,1)");
        assert_eq!(rgba("#000", 0.16), "rgba(0,0,0,0.16)");
    }

    #[test]
    fn rgb_string_and_garbage() {
        assert_eq!(rgba("rgb(10, 20, 30)", 1.0), "rgba(10,20,30,1)");
        assert_eq!(rgba("nope", 1.0), "rgba(0,0,0,1)");
    }

    /// The point of `muted` is that it moves *toward* grey from either end, so
    /// chrome derived from dark text stays dark (visible on the pale panel that
    /// text was chosen for) and chrome derived from light text stays light.
    #[test]
    fn muting_never_crosses_to_the_far_side_of_grey() {
        assert_eq!(muted("#0d0d0f", 0.5), "rgb(71,71,72)"); // dark text -> dark grey
        assert_eq!(muted("#ffffff", 0.5), "rgb(192,192,192)"); // light -> light grey
        assert_eq!(muted("#0d0d0f", 0.0), "rgb(13,13,15)"); // 0 leaves it alone
        assert_eq!(muted("#0d0d0f", 1.0), "rgb(128,128,128)"); // 1 is grey itself
        // out-of-range factors are clamped, not wrapped
        assert_eq!(muted("#ffffff", 5.0), "rgb(128,128,128)");
        assert_eq!(muted("#ffffff", -1.0), "rgb(255,255,255)");
    }

    #[test]
    fn contrast() {
        assert_eq!(contrast_text("#ffffff"), "#12141c");
        assert_eq!(contrast_text("#12141c"), "#f4f5f9");
        // pale accents still get dark text
        assert_eq!(contrast_text("#ffe66d"), "#12141c");
    }

    #[test]
    fn similar_accents_agree() {
        // these two blues differ by a hair and must not be captioned
        // in opposite colours
        assert_eq!(contrast_text("#62a0ea"), contrast_text("#7aa2f7"));
        assert_eq!(contrast_text("#7aa2f7"), "#f4f5f9");
    }
}
