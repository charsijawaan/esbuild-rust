//! Port of Go's `isUnsupportedRegularExpression` and `ERegExp` visitor fallback.

use crate::internal::{
    compat::JsFeature,
    config::pretty_print_target_environment,
    helpers::string_to_utf16,
    js_ast::{Expr, ExprData, IdentifierExpr, NewExpr, StringExpr},
    logger::{Loc, MsgData, MsgId, MsgKind, Range},
};

use super::parser_core::ParserCore;

fn range(loc: Loc, offset: usize, len: usize) -> Range {
    Range {
        loc: Loc {
            start: loc.start + i32::try_from(offset).expect("source offset fits in i32"),
        },
        len: i32::try_from(len).expect("source range fits in i32"),
    }
}

// Like Go, this is a feature scan, not a complete RegExp grammar validator.
fn scan_pattern(
    loc: Loc,
    pattern: &str,
    flags: &str,
    unsupported: JsFeature,
) -> Result<Option<(&'static str, Range)>, Range> {
    let bytes = pattern.as_bytes();
    let is_unicode = flags.contains('u');
    let mut paren_depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        match byte {
            b'[' => {
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    match byte {
                        b']' => break,
                        b'\\' => index += 1,
                        _ => {}
                    }
                }
            }
            b'(' => {
                let tail = &pattern[index..];
                if tail.starts_with("?<=") || tail.starts_with("?<!") {
                    if unsupported.contains(JsFeature::REGEXP_LOOKBEHIND_ASSERTIONS) {
                        return Ok(Some((
                            "Lookbehind assertions in regular expressions are not available",
                            range(loc, index + 1, 3),
                        )));
                    }
                } else if tail.starts_with("?<")
                    && unsupported.contains(JsFeature::REGEXP_NAMED_CAPTURE_GROUPS)
                    && let Some(end) = tail.find('>')
                {
                    return Ok(Some((
                        "Named capture groups in regular expressions are not available",
                        range(loc, index + 1, end + 1),
                    )));
                }
                paren_depth += 1;
            }
            b')' => {
                if paren_depth == 0 {
                    return Err(range(loc, index, 1));
                }
                paren_depth -= 1;
            }
            b'\\' => {
                let tail = &pattern[index..];
                if is_unicode
                    && (tail.starts_with("p{") || tail.starts_with("P{"))
                    && unsupported.contains(JsFeature::REGEXP_UNICODE_PROPERTY_ESCAPES)
                    && let Some(end) = tail.find('}')
                {
                    return Ok(Some((
                        "Unicode property escapes in regular expressions are not available",
                        range(loc, index, end + 2),
                    )));
                }
                index += 1;
            }
            _ => {}
        }
    }
    Ok(None)
}

pub(crate) fn lower_regexp(core: &mut ParserCore, loc: Loc, value: &str) -> Option<ExprData> {
    let end = value.rfind('/')?;
    let pattern = value.get(1..end)?;
    let flags = &value[end + 1..];
    let unsupported = core.options.unsupported_js_features;
    let issue = match scan_pattern(loc, pattern, flags, unsupported) {
        Ok(issue) => issue.map(|(text, range)| (text.to_string(), range)),
        Err(range) => {
            core.add_error_range(range, "Unexpected \")\" in regular expression");
            return None;
        }
    };
    let issue = issue.or_else(|| {
        flags.char_indices().find_map(|(index, flag)| {
            let feature = match flag {
                'g' | 'i' | 'm' => return None,
                's' => Some(JsFeature::REGEXP_DOT_ALL_FLAG),
                'y' | 'u' => Some(JsFeature::REGEXP_STICKY_AND_UNICODE_FLAGS),
                'd' => Some(JsFeature::REGEXP_MATCH_INDICES),
                'v' => Some(JsFeature::REGEXP_SET_NOTATION),
                _ => None,
            };
            if feature.is_some_and(|feature| !unsupported.contains(feature)) {
                return None;
            }
            Some((
                format!("The regular expression flag \"{flag}\" is not available"),
                range(loc, end + 1 + index, 1),
            ))
        })
    })?;

    if let Some(log) = core.log.clone() {
        let environment = pretty_print_target_environment(
            &core.options.original_target_env,
            core.options.unsupported_js_feature_overrides_mask,
        );
        log.add_id_with_notes(
            MsgId::JsUnsupportedRegExp,
            MsgKind::Debug,
            Some(&mut core.tracker),
            issue.1,
            format!("{} in {environment}", issue.0),
            vec![MsgData {
                text: "This regular expression literal has been converted to a \"new RegExp()\" constructor \
                       to avoid generating code with a syntax error. However, you will need to include a \
                       polyfill for \"RegExp\" for your code to have the correct behavior at run-time."
                    .into(),
                ..MsgData::default()
            }],
        );
    }

    let mut args = vec![Expr::new(
        range(loc, 1, 0).loc,
        ExprData::String(StringExpr {
            value: string_to_utf16(pattern.as_bytes()),
            ..StringExpr::default()
        }),
    )];
    if !flags.is_empty() {
        args.push(Expr::new(
            range(loc, pattern.len() + 2, 0).loc,
            ExprData::String(StringExpr {
                value: string_to_utf16(flags.as_bytes()),
                ..StringExpr::default()
            }),
        ));
    }
    let reference = core.make_reg_exp_ref();
    core.record_usage(reference);
    Some(ExprData::New(NewExpr {
        target: Expr::new(
            loc,
            ExprData::Identifier(IdentifierExpr {
                reference,
                ..IdentifierExpr::default()
            }),
        ),
        args,
        close_paren_loc: range(loc, value.len(), 0).loc,
        // Constructor calls may have side effects even if their result is unused.
        ..NewExpr::default()
    }))
}
