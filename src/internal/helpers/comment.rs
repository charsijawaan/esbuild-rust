// Port of upstream internal/helpers/comment.go.

#[must_use]
pub fn escape_closing_tag(text: &str, slash_tag: &str) -> String {
    if slash_tag.is_empty() {
        return text.to_string();
    }

    let Some(mut index) = text.find("</") else {
        return text.to_string();
    };
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    loop {
        result.push_str(&remaining[..=index]);
        remaining = &remaining[index + 1..];
        if remaining.len() >= slash_tag.len()
            && remaining.as_bytes()[..slash_tag.len()].eq_ignore_ascii_case(slash_tag.as_bytes())
        {
            result.push('\\');
        }
        let Some(next) = remaining.find("</") else {
            break;
        };
        index = next;
    }
    result.push_str(remaining);
    result
}

#[cfg(test)]
mod tests {
    use super::escape_closing_tag;

    #[test]
    fn escapes_matching_closing_tags_case_insensitively() {
        assert_eq!(
            escape_closing_tag("x</script>y</SCRIPT>z</style>", "/script"),
            "x<\\/script>y<\\/SCRIPT>z</style>"
        );
        assert_eq!(escape_closing_tag("unchanged", "/script"), "unchanged");
        assert_eq!(escape_closing_tag("</script>", ""), "</script>");
    }
}
