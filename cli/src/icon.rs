//! A project's own logo, found in its folder the way Sidra finds tab icons
//! (sidra-full/Sidra/Herdr.swift HerdrLogo): design icon, iOS AppIcon, touch
//! icon, Tauri icon, Flutter/Expo/Godot icons, then favicons. Shrunk to 128 px
//! PNG with sips (or rsvg-convert / Quick Look for SVG), under 512 KB, and the
//! Rails starter icon (a tiny red-circle icon.svg beside it) skipped.

use std::process::Command;

pub const CANDIDATES: &[&str] = &[
    "design/icon/icon-1024.png", "design/icon/icon-512.png", "design/icon/icon.png",
    "*/Assets.xcassets/AppIcon.appiconset/*1024*.png", "*/*/Assets.xcassets/AppIcon.appiconset/*1024*.png", "*/*/*/Assets.xcassets/AppIcon.appiconset/*1024*.png",
    "apple-touch-icon.png", "public/apple-touch-icon.png", "*/public/apple-touch-icon.png", "*/*/public/apple-touch-icon.png",
    "public/icon-512.png", "*/public/icon-512.png",
    "src-tauri/icons/icon.png", "*/src-tauri/icons/icon.png", "*/*/src-tauri/icons/icon.png",
    "assets/brand/app_icon.png", "*/*/assets/brand/app_icon.png", "assets/icon/app_icon.png", "*/*/assets/icon/app_icon.png",
    "assets/icon.png", "*/assets/icon.png", "*/*/assets/icon.png",
    "public/favicon.png", "*/public/favicon.png", "public/icon.png", "*/public/icon.png",
    "public/favicon.ico", "*/public/favicon.ico",
    "public/favicon.svg", "*/public/favicon.svg", "*/*/public/favicon.svg",
];

/// The shell script, run under sh: zsh aborts on a glob that matches nothing.
fn script() -> String {
    format!(
        r#"cd "$1" 2>/dev/null || exit 1; cd "$(git rev-parse --show-toplevel 2>/dev/null || pwd)"; t=$(mktemp -d); trap 'rm -rf "$t"' EXIT; for f in {}; do [ -s "$f" ] || continue; v="${{f%.png}}.svg"; [ -f "$v" ] && [ "$(wc -c < "$v")" -lt 200 ] && grep -q 'fill="red"' "$v" && continue; rm -f "$t"/*; case "$f" in *.svg) if command -v rsvg-convert >/dev/null; then rsvg-convert -w 128 -h 128 "$f" > "$t/l.png" 2>/dev/null; elif command -v qlmanage >/dev/null; then qlmanage -t -s 128 -o "$t" "$f" >/dev/null 2>&1 && mv "$t"/*.png "$t/l.png" 2>/dev/null; fi;; *) if command -v sips >/dev/null; then sips -Z 128 -s format png "$f" --out "$t/l.png" >/dev/null 2>&1; fi; [ -s "$t/l.png" ] || cp "$f" "$t/l.png";; esac; [ -s "$t/l.png" ] && [ "$(wc -c < "$t/l.png")" -lt 524288 ] || continue; cat "$t/l.png"; exit 0; done; exit 1"#,
        CANDIDATES.join(" ")
    )
}

/// The logo for a project folder as PNG bytes (a JPEG passes through when
/// there is no sips), or None.
pub fn detect(folder: &str) -> Option<Vec<u8>> {
    if folder.is_empty() {
        return None;
    }
    let out = Command::new("/bin/sh").arg("-c").arg(script()).arg("sh").arg(folder).output().ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let b = out.stdout;
    let png = b.starts_with(b"\x89PNG\r\n\x1a\n");
    let jpeg = b.starts_with(&[0xFF, 0xD8, 0xFF]);
    (png || jpeg).then_some(b)
}

/// The README's first paragraph of prose, as plain text: what testers read about
/// the project. Headings, badges, images, HTML and code are skipped.
pub fn readme_summary(folder: &str) -> Option<String> {
    let root = std::path::Path::new(folder);
    let text = ["README.md", "readme.md", "Readme.md", "README"].iter().find_map(|f| std::fs::read_to_string(root.join(f)).ok())?;
    let mut para = Vec::new();
    let (mut fence, mut html) = (false, false);
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("```") { fence = !fence; continue; }
        if fence { continue; }
        // An HTML block (a centred logo, badges) runs to the next blank line.
        if l.starts_with('<') { html = true; }
        if html { if l.is_empty() { html = false; } continue; }
        let skip = l.starts_with('#') || l.starts_with('<') || l.starts_with("![") || l.starts_with("[![") || l.starts_with('|') || l.starts_with("---") || l.starts_with('>');
        if l.is_empty() || skip {
            if !para.is_empty() { break; }
            continue;
        }
        para.push(l.to_string());
    }
    let mut s = para.join(" ");
    // [text](url) -> text; drop emphasis and inline code marks.
    while let (Some(a), Some(m)) = (s.find('['), s.find("](")) {
        let Some(e) = s[m..].find(')').map(|e| m + e) else { break };
        if a > m { break; }
        s = format!("{}{}{}", &s[..a], &s[a + 1..m], &s[e + 1..]);
    }
    let s: String = s.replace("**", "").replace('`', "").replace("__", "");
    let s = s.trim().to_string();
    if s.len() < 12 { return None; }
    Some(if s.chars().count() > 480 { format!("{}…", s.chars().take(480).collect::<String>().trim_end()) } else { s })
}

#[cfg(test)]
mod tests {
    #[test]
    fn readme_first_paragraph() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "<p align=\"center\">\n  <img\n    src=\"logo.png\">\n</p>\n\n# Messy\n\n[![ci](x)](y)\n\nMessy sends **email**, SMS and [WhatsApp](https://x) from one API.\nIt runs on Rails.\n\n## Setup\n").unwrap();
        assert_eq!(super::readme_summary(dir.path().to_str().unwrap()).unwrap(), "Messy sends email, SMS and WhatsApp from one API. It runs on Rails.");
    }

    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\rIDATx\x9cc\xf8\x0f\x00\x00\x01\x01\x00\x05\x18\xd8N\x00\x00\x00\x00IEND\xaeB`\x82";

    #[test]
    fn finds_the_first_candidate_and_skips_the_rails_starter() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        // Rails starter: public/icon.png with a tiny red-circle icon.svg beside it.
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(root.join("public/icon.png"), PNG).unwrap();
        std::fs::write(root.join("public/icon.svg"), r#"<svg><circle fill="red"/></svg>"#).unwrap();
        assert!(detect(root.to_str().unwrap()).is_none(), "the Rails starter icon is not a logo");
        // A real touch icon wins.
        std::fs::write(root.join("public/apple-touch-icon.png"), PNG).unwrap();
        let got = detect(root.to_str().unwrap()).expect("found the touch icon");
        assert!(got.starts_with(b"\x89PNG"));
        assert!(detect(root.join("nope").to_str().unwrap()).is_none());
    }
}
