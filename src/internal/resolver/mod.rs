//! Port of upstream `internal/resolver`.

use std::any::Any;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::internal::{
    config::{TsAlwaysStrict, TsConfig, TsConfigJsx},
    fs::DifferentCase,
    js_ast::ModuleTypeData,
    logger::{LineColumnTracker, Loc, Log, Msg, MsgData, MsgKind, Path, Range, Source},
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PathPair {
    pub primary: Path,
    pub secondary: Path,
    pub is_external: bool,
}

impl PathPair {
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Path> {
        let has_secondary = self.has_secondary();
        [&mut self.primary, &mut self.secondary]
            .into_iter()
            .take(if has_secondary { 2 } else { 1 })
    }

    #[must_use]
    pub fn has_secondary(&self) -> bool {
        !self.secondary.text.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct SideEffectsData {
    pub source: Option<Source>,
    pub plugin_name: String,
    pub range: Range,
    pub is_side_effects_array_in_json: bool,
}

#[derive(Clone, Default)]
pub struct ResolveResult {
    pub path_pair: PathPair,
    pub plugin_data: Option<Arc<dyn Any + Send + Sync>>,
    pub different_case: Option<DifferentCase>,
    pub primary_side_effects_data: Option<SideEffectsData>,
    pub ts_config_jsx: TsConfigJsx,
    pub ts_config: Option<TsConfig>,
    pub ts_always_strict: Option<TsAlwaysStrict>,
    pub module_type_data: ModuleTypeData,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SuggestionRange {
    #[default]
    Full,
    End,
}

#[derive(Clone, Default)]
pub struct DebugMeta {
    notes: Vec<MsgData>,
    suggestion_text: String,
    suggestion_message: String,
    suggestion_range: SuggestionRange,
    pub modified_import_path: String,
}

impl DebugMeta {
    pub fn log_error_msg(
        mut self,
        log: &Log,
        source: Option<&Source>,
        range: Range,
        text: impl Into<String>,
        suggestion: &str,
        notes: &[MsgData],
    ) {
        let mut tracker = LineColumnTracker::new(source);
        if source.is_some() && !self.suggestion_message.is_empty() {
            let suggestion_range = if self.suggestion_range == SuggestionRange::End {
                Range {
                    loc: Loc {
                        start: range.end() - 1,
                    },
                    ..Range::default()
                }
            } else {
                range
            };
            let mut data = tracker.msg_data(suggestion_range, self.suggestion_message);
            if let Some(location) = &mut data.location {
                location.suggestion = self.suggestion_text;
            }
            self.notes.push(data);
        }

        let mut data = tracker.msg_data(range, text);
        if !suggestion.is_empty()
            && let Some(location) = &mut data.location
        {
            location.suggestion = suggestion.to_string();
        }
        self.notes.extend_from_slice(notes);
        log.add_msg(Msg {
            notes: self.notes,
            data,
            kind: MsgKind::Error,
            ..Msg::new(MsgKind::Error, "")
        });
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataUrl {
    mime_type: String,
    data: String,
    is_base64: bool,
}

impl DataUrl {
    #[must_use]
    pub fn parse(url: &str) -> Option<Self> {
        let contents = url.strip_prefix("data:")?;
        let comma = contents.find(',')?;
        let (mut mime_type, data_with_comma) = contents.split_at(comma);
        let mut is_base64 = false;
        if let Some(without_base64) = mime_type.strip_suffix(";base64") {
            mime_type = without_base64;
            is_base64 = true;
        }
        Some(Self {
            mime_type: mime_type.to_string(),
            data: data_with_comma[1..].to_string(),
            is_base64,
        })
    }

    #[must_use]
    pub fn decode_mime_type(&self) -> MimeType {
        match self
            .mime_type
            .split_once(';')
            .map_or(self.mime_type.as_str(), |(mime_type, _)| mime_type)
        {
            "text/css" => MimeType::TextCss,
            "text/javascript" => MimeType::TextJavaScript,
            "application/json" => MimeType::ApplicationJson,
            _ => MimeType::Unsupported,
        }
    }

    /// # Errors
    ///
    /// Returns an error when base64 or percent-escaped data is malformed.
    pub fn decode_data(&self) -> Result<Vec<u8>, String> {
        if self.is_base64 {
            return STANDARD
                .decode(self.data.as_bytes())
                .map_err(|error| format!("could not decode base64 data: {error}"));
        }
        decode_percent_escaped(self.data.as_bytes())
            .map_err(|error| format!("could not decode percent-escaped data: {error}"))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MimeType {
    #[default]
    Unsupported,
    TextCss,
    TextJavaScript,
    ApplicationJson,
}

fn decode_percent_escaped(data: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut decoded = Vec::with_capacity(data.len());
    let mut index = 0;
    while index < data.len() {
        if data[index] != b'%' {
            decoded.push(data[index]);
            index += 1;
            continue;
        }
        if index + 2 >= data.len() {
            return Err("invalid URL escape");
        }
        let Some(high) = hex_value(data[index + 1]) else {
            return Err("invalid URL escape");
        };
        let Some(low) = hex_value(data[index + 2]) else {
            return Err("invalid URL escape");
        };
        decoded.push((high << 4) | low);
        index += 3;
    }
    Ok(decoded)
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::{DataUrl, DebugMeta, MimeType, PathPair};
    use crate::internal::logger::{DeferLogKind, Loc, Log, Path, PrettyPaths, Range, Source};

    #[test]
    fn path_pair_iterates_primary_and_optional_secondary() {
        let mut pair = PathPair {
            primary: Path {
                text: "module.js".into(),
                ..Path::default()
            },
            ..PathPair::default()
        };
        assert_eq!(pair.iter_mut().count(), 1);
        pair.secondary.text = "main.js".into();
        assert_eq!(
            pair.iter_mut()
                .map(|path| path.text.clone())
                .collect::<Vec<_>>(),
            vec!["module.js", "main.js"]
        );
    }

    #[test]
    fn parses_and_decodes_data_urls_like_upstream() {
        let css = DataUrl::parse("data:text/css;charset=utf-8,a+b%20c").expect("data URL");
        assert_eq!(css.decode_mime_type(), MimeType::TextCss);
        assert_eq!(css.decode_data().expect("percent data"), b"a+b c");

        let json =
            DataUrl::parse("data:application/json;base64,eyJhIjoxfQ==").expect("base64 data URL");
        assert_eq!(json.decode_mime_type(), MimeType::ApplicationJson);
        assert_eq!(json.decode_data().expect("base64 data"), br#"{"a":1}"#);

        assert!(DataUrl::parse("text/css,body{}").is_none());
        assert!(DataUrl::parse("data:text/css").is_none());
    }

    #[test]
    fn data_url_decoding_preserves_arbitrary_bytes_and_rejects_bad_escapes() {
        let binary = DataUrl::parse("data:text/javascript,%FF").expect("data URL");
        assert_eq!(binary.decode_data().expect("arbitrary bytes"), vec![0xff]);
        assert!(
            DataUrl::parse("data:text/css,%xy")
                .expect("data URL")
                .decode_data()
                .expect_err("invalid escape")
                .starts_with("could not decode percent-escaped data:")
        );
        assert!(
            DataUrl::parse("data:text/css;base64,?")
                .expect("data URL")
                .decode_data()
                .expect_err("invalid base64")
                .starts_with("could not decode base64 data:")
        );
    }

    #[test]
    fn debug_meta_attaches_source_suggestions_and_notes() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            pretty_paths: PrettyPaths {
                abs: "/entry.js".into(),
                rel: "entry.js".into(),
            },
            contents: Arc::from(&b"import 'bad'"[..]),
            ..Source::default()
        };
        DebugMeta::default().log_error_msg(
            &log,
            Some(&source),
            Range {
                loc: Loc { start: 8 },
                len: 5,
            },
            "Could not resolve",
            "'good'",
            &[],
        );
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data.text, "Could not resolve");
        assert_eq!(
            messages[0]
                .data
                .location
                .as_ref()
                .expect("location")
                .suggestion,
            "'good'"
        );
    }
}
