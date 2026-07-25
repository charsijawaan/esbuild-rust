// Port of upstream internal/helpers/glob.go.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GlobWildcard {
    #[default]
    None,
    AllExceptSlash,
    AllIncludingSlash,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GlobPart {
    pub prefix: String,
    pub wildcard: GlobWildcard,
}

/// The returned array is always non-empty. It has exactly one element when
/// there are no wildcards and more than one element when wildcards are present.
#[must_use]
pub fn parse_glob_pattern(mut text: &str) -> Vec<GlobPart> {
    let mut pattern = Vec::new();
    loop {
        let Some(star) = text.find('*') else {
            pattern.push(GlobPart {
                prefix: text.to_string(),
                wildcard: GlobWildcard::None,
            });
            break;
        };

        let count = text.as_bytes()[star..]
            .iter()
            .take_while(|byte| **byte == b'*')
            .count();
        let mut wildcard = GlobWildcard::AllExceptSlash;

        // Allow both "/" and "\" as slashes.
        let is_segment_start = star == 0 || matches!(text.as_bytes()[star - 1], b'/' | b'\\');
        let after_stars = star + count;
        let is_segment_end =
            after_stars == text.len() || matches!(text.as_bytes()[after_stars], b'/' | b'\\');
        if count > 1 && is_segment_start && is_segment_end {
            wildcard = GlobWildcard::AllIncludingSlash;
        }

        pattern.push(GlobPart {
            prefix: text[..star].to_string(),
            wildcard,
        });
        text = &text[after_stars..];
    }
    pattern
}

#[must_use]
pub fn glob_pattern_to_string(pattern: &[GlobPart]) -> String {
    let mut result = String::new();
    for part in pattern {
        result.push_str(&part.prefix);
        match part.wildcard {
            GlobWildcard::None => {}
            GlobWildcard::AllExceptSlash => result.push('*'),
            GlobWildcard::AllIncludingSlash => result.push_str("**"),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{GlobPart, GlobWildcard, glob_pattern_to_string, parse_glob_pattern};

    #[test]
    fn distinguishes_star_from_globstar_segments() {
        assert_eq!(
            parse_glob_pattern("a/**/b*.js"),
            vec![
                GlobPart {
                    prefix: "a/".to_string(),
                    wildcard: GlobWildcard::AllIncludingSlash,
                },
                GlobPart {
                    prefix: "/b".to_string(),
                    wildcard: GlobWildcard::AllExceptSlash,
                },
                GlobPart {
                    prefix: ".js".to_string(),
                    wildcard: GlobWildcard::None,
                },
            ]
        );
        assert_eq!(
            glob_pattern_to_string(&parse_glob_pattern(r"a\**\b")),
            r"a\**\b"
        );
        assert_eq!(parse_glob_pattern("plain").len(), 1);
    }
}
