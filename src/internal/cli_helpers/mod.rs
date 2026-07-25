//! Port of upstream `internal/cli_helpers`.

use crate::api::Loader;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErrorWithNote {
    pub text: String,
    pub note: String,
}

#[must_use]
pub fn make_error_with_note(text: impl Into<String>, note: impl Into<String>) -> ErrorWithNote {
    ErrorWithNote {
        text: text.into(),
        note: note.into(),
    }
}

/// # Errors
///
/// Returns a diagnostic with the valid loader values when `text` is unknown.
pub fn parse_loader(text: &str) -> Result<Loader, ErrorWithNote> {
    Ok(match text {
        "base64" => Loader::Base64,
        "binary" => Loader::Binary,
        "copy" => Loader::Copy,
        "css" => Loader::Css,
        "dataurl" => Loader::DataUrl,
        "default" => Loader::Default,
        "empty" => Loader::Empty,
        "file" => Loader::File,
        "global-css" => Loader::GlobalCss,
        "js" => Loader::Js,
        "json" => Loader::Json,
        "jsx" => Loader::Jsx,
        "local-css" => Loader::LocalCss,
        "text" => Loader::Text,
        "ts" => Loader::Ts,
        "tsx" => Loader::Tsx,
        _ => {
            return Err(make_error_with_note(
                format!("Invalid loader value: {text:?}"),
                "Valid values are \"base64\", \"binary\", \"copy\", \"css\", \"dataurl\", \
                 \"empty\", \"file\", \"global-css\", \"js\", \"json\", \"jsx\", \
                 \"local-css\", \"text\", \"ts\", or \"tsx\".",
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::parse_loader;
    use crate::api::Loader;

    #[test]
    fn parses_all_upstream_loader_names() {
        let cases = [
            ("base64", Loader::Base64),
            ("binary", Loader::Binary),
            ("copy", Loader::Copy),
            ("css", Loader::Css),
            ("dataurl", Loader::DataUrl),
            ("default", Loader::Default),
            ("empty", Loader::Empty),
            ("file", Loader::File),
            ("global-css", Loader::GlobalCss),
            ("js", Loader::Js),
            ("json", Loader::Json),
            ("jsx", Loader::Jsx),
            ("local-css", Loader::LocalCss),
            ("text", Loader::Text),
            ("ts", Loader::Ts),
            ("tsx", Loader::Tsx),
        ];
        for (text, loader) in cases {
            assert_eq!(parse_loader(text), Ok(loader));
        }
    }

    #[test]
    fn invalid_loader_includes_upstream_note() {
        let error = parse_loader("wat").expect_err("invalid loader");
        assert_eq!(error.text, "Invalid loader value: \"wat\"");
        assert!(error.note.starts_with("Valid values are \"base64\""));
        assert!(error.note.ends_with("or \"tsx\"."));
    }
}
