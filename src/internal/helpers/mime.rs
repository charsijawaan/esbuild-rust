// Port of upstream internal/helpers/mime.go.

/// Used instead of a platform MIME database to keep output deterministic.
#[must_use]
pub fn mime_type_by_extension(extension: &str) -> &'static str {
    builtin_type(extension).unwrap_or_else(|| {
        // All catalog keys are ASCII. In Go's Unicode 15 simple lowercase
        // table, these are the only non-ASCII runes that map to ASCII.
        // Keep other runes non-ASCII: full case folding or normalization can
        // incorrectly turn unknown extensions (such as ".cſſ") into keys.
        let lower: String = extension
            .chars()
            .map(|character| match character {
                '\u{0130}' => 'i',
                '\u{212a}' => 'k',
                _ => character.to_ascii_lowercase(),
            })
            .collect();
        builtin_type(&lower).unwrap_or("")
    })
}

/// Match esbuild's deterministic extension lookup and Go HTTP content sniffing.
#[must_use]
pub fn guess_mime_type(extension: &str, contents: &[u8]) -> String {
    let known = mime_type_by_extension(extension);
    let mime_type = if known.is_empty() {
        super::mime_sniff::detect_content_type(contents)
    } else {
        known
    };
    mime_type.replace("; ", ";")
}

fn builtin_type(extension: &str) -> Option<&'static str> {
    Some(match extension {
        // Text
        ".css" => "text/css; charset=utf-8",
        ".htm" | ".html" => "text/html; charset=utf-8",
        ".js" | ".mjs" => "text/javascript; charset=utf-8",
        ".json" => "application/json; charset=utf-8",
        ".markdown" | ".md" => "text/markdown; charset=utf-8",
        ".xhtml" => "application/xhtml+xml; charset=utf-8",
        ".xml" => "text/xml; charset=utf-8",

        // Images
        ".avif" => "image/avif",
        ".gif" => "image/gif",
        ".jpeg" | ".jpg" => "image/jpeg",
        ".png" => "image/png",
        ".svg" => "image/svg+xml",
        ".webp" => "image/webp",

        // Fonts
        ".eot" => "application/vnd.ms-fontobject",
        ".otf" => "font/otf",
        ".sfnt" => "font/sfnt",
        ".ttf" => "font/ttf",
        ".woff" => "font/woff",
        ".woff2" => "font/woff2",

        // Other
        ".pdf" => "application/pdf",
        ".wasm" => "application/wasm",
        ".webmanifest" => "application/manifest+json",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::mime_type_by_extension;

    #[test]
    fn uses_builtin_types_case_insensitively() {
        assert_eq!(
            mime_type_by_extension(".JS"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(mime_type_by_extension(".woff2"), "font/woff2");
        assert_eq!(mime_type_by_extension(".unknown"), "");
    }
}
