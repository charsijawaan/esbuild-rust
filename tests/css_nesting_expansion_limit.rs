use esbuild_rs::api::{self, Engine, EngineName, Loader, TransformOptions, TransformResult};

const ERROR: &str = "CSS nesting is causing too much expansion";

fn transform(css: &str, safari: &str) -> TransformResult {
    api::transform(
        css,
        TransformOptions {
            loader: Loader::Css,
            sourcefile: "limit.css".into(),
            engines: vec![Engine {
                name: EngineName::Safari,
                version: safari.into(),
            }],
            ..TransformOptions::default()
        },
    )
}

fn names(prefix: &str, count: usize, separator: &str) -> String {
    (0..count)
        .map(|index| format!("{prefix}{index}"))
        .collect::<Vec<_>>()
        .join(separator)
}

fn nested(depth: usize) -> String {
    format!("{}color:red{}", "a,b{".repeat(depth), "}".repeat(depth))
}

// Counts and complete diagnostic fields were independently captured from Go
// 6ff1d8b0d8c134e867a397eef39702a223ebef9e using the original JS wrapper.
fn assert_expansion_error(result: &TransformResult, css: &str, column: usize, count: usize) {
    assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert!(result.code.is_empty());
    let error = &result.errors[0];
    assert_eq!(error.text, ERROR);
    assert!(error.id.is_empty());
    assert!(error.plugin_name.is_empty());
    assert!(error.detail.is_none());
    let location = error.location.as_ref().unwrap();
    assert_eq!(location.file, "limit.css");
    assert!(location.namespace.is_empty());
    assert_eq!(location.line, 1);
    assert_eq!(location.column, column);
    assert_eq!(location.length, 0);
    assert_eq!(location.line_text, css);
    assert!(location.suggestion.is_empty());
    assert_eq!(error.notes.len(), 1);
    assert!(error.notes[0].location.is_none());
    assert_eq!(
        error.notes[0].text,
        format!(
            "CSS nesting expansion was terminated because a rule was generated with {count} selectors. \
             This limit exists to prevent esbuild from using too much time and/or memory. \
             Please change your CSS to use fewer levels of nesting."
        )
    );
}

fn code(result: TransformResult) -> String {
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    String::from_utf8(result.code).unwrap()
}

#[test]
fn original_twenty_level_nesting_reports_both_go_guard_counts() {
    // Original cssNestingExpansionLimitWithoutIs / WithIs input and engines.
    let css = nested(20);
    assert_expansion_error(&transform(&css, "1"), &css, 60, 65_536);
    // Go calls this recursive complexity value "selectors" in the note too.
    assert_expansion_error(&transform(&css, "14"), &css, 60, 65_534);
}

#[test]
fn selector_count_limit_allows_65280_and_rejects_growth_above_it() {
    let parents = names("p", 256, ",");
    let css = format!("{parents}{{{}{{color:red}}}}", names("b", 255, ","));
    let expected = (0..256)
        .flat_map(|parent| (0..255).map(move |child| format!("p{parent} b{child}")))
        .collect::<Vec<_>>()
        .join(",\n")
        + " {\n  color: red;\n}\n";
    assert_eq!(code(transform(&css, "1")), expected);

    let css = format!("{parents}{{{}{{color:red}}}}", names("b", 256, ","));
    assert_expansion_error(&transform(&css, "1"), &css, parents.len() + 1, 65_536);
}

#[test]
fn recursive_term_limit_allows_65280_and_rejects_growth_above_it() {
    let parents = names(".p", 255, ",");
    let css = format!("{parents}{{{}{{color:red}}}}", names("b", 255, ","));
    let formatted_parents = names(".p", 255, ", ");
    let expected = (0..255)
        .map(|child| format!(":is({formatted_parents}) b{child}"))
        .collect::<Vec<_>>()
        .join(",\n")
        + " {\n  color: red;\n}\n";
    assert_eq!(code(transform(&css, "14")), expected);

    let parents = names(".p", 256, ",");
    let css = format!("{parents}{{{}{{color:red}}}}", names("b", 255, ","));
    assert_expansion_error(&transform(&css, "14"), &css, parents.len() + 1, 65_535);
}

#[test]
fn selector_count_diagnostic_precedes_recursive_term_diagnostic() {
    // Both count (65536) and complexity (131072) exceed the limit.
    let parents = names(".p", 256, ",");
    let css = format!("{parents}{{{}{{color:red}}}}", names(".b", 256, ","));
    assert_expansion_error(&transform(&css, "1"), &css, parents.len() + 1, 65_536);
}

#[test]
fn preexisting_large_selector_list_is_allowed_when_it_does_not_grow() {
    let css = format!("p{{{}{{color:red}}}}", names("b", 65_536, ","));
    let expected = (0..65_536)
        .map(|child| format!("p b{child}"))
        .collect::<Vec<_>>()
        .join(",\n")
        + " {\n  color: red;\n}\n";
    for safari in ["1", "14"] {
        assert_eq!(code(transform(&css, safari)), expected);
    }
}

#[test]
fn preexisting_large_recursive_terms_are_allowed_when_they_do_not_grow() {
    let css = format!("p{{&:is({}){{color:red}}}}", names(".x", 65_536, ","));
    let expected = format!(
        "p:is({}) {{\n  color: red;\n}}\n",
        names(".x", 65_536, ", ")
    );
    for safari in ["1", "14"] {
        assert_eq!(code(transform(&css, safari)), expected);
    }
}

#[test]
fn preexisting_large_recursive_terms_are_rejected_when_they_grow() {
    let css = format!(".p{{&:is({}){{color:red}}}}", names(".x", 65_280, ","));
    assert_expansion_error(&transform(&css, "14"), &css, 3, 65_282);
}

#[test]
fn depth_fifteen_succeeds_and_depth_sixteen_stops_before_deeper_recursion() {
    // Type selectors themselves are not included in Go's term count. These
    // output byte lengths are independent Go results for the allowed depth.
    for (safari, length, count) in [("1", 1_015_825, 65_536), ("14", 327_683, 65_534)] {
        let output = code(transform(&nested(15), safari));
        assert_eq!(output.len(), length);
        assert!(output.ends_with(" {\n  color: red;\n}\n"));
        let css = nested(16);
        assert_expansion_error(&transform(&css, safari), &css, 60, count);
    }
}
