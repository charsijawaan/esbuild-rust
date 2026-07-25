// Port of upstream internal/helpers/typos.go.

use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub struct TypoDetector {
    one_char_typos: HashMap<String, String>,
}

impl TypoDetector {
    #[must_use]
    pub fn new<S: AsRef<str>>(valid: &[S]) -> Self {
        let mut detector = Self::default();

        // Add all combinations of each valid word with one character missing.
        for correct in valid {
            let correct = correct.as_ref();
            if correct.len() > 3 {
                for (index, character) in correct.char_indices() {
                    let after = index + character.len_utf8();
                    detector.one_char_typos.insert(
                        format!("{}{}", &correct[..index], &correct[after..]),
                        correct.to_string(),
                    );
                }
            }
        }

        detector
    }

    #[must_use]
    pub fn maybe_correct_typo(&self, typo: &str) -> Option<&str> {
        // Check for a single deleted character.
        if let Some(corrected) = self.one_char_typos.get(typo) {
            return Some(corrected);
        }

        // Check for a single misplaced character.
        for (index, character) in typo.char_indices() {
            let after = index + character.len_utf8();
            let candidate = format!("{}{}", &typo[..index], &typo[after..]);
            if let Some(corrected) = self.one_char_typos.get(&candidate) {
                return Some(corrected);
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::TypoDetector;

    #[test]
    fn detects_deleted_and_misplaced_characters() {
        let detector = TypoDetector::new(&["external", "sourcemap", "λvalue"]);
        assert_eq!(detector.maybe_correct_typo("externl"), Some("external"));
        assert_eq!(detector.maybe_correct_typo("externax"), Some("external"));
        assert_eq!(detector.maybe_correct_typo("λvalu"), Some("λvalue"));
        assert_eq!(detector.maybe_correct_typo("other"), None);
    }
}
