// Port of upstream internal/helpers/mime.go.

/// Used instead of a platform MIME database to keep output deterministic.
#[must_use]
pub fn mime_type_by_extension(extension: &str) -> &'static str {
    builtin_type(extension).unwrap_or_else(|| {
        let lower = extension.to_ascii_lowercase();
        builtin_type(&lower).unwrap_or("")
    })
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
