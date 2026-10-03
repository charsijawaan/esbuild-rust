//! JSX option diagnostics through the native public APIs, without a service.

use esbuild_rs::api::{
    self, BuildOptions, BuildStdin, Engine, EngineName, Loader, Message, MessageKind,
    TransformOptions,
};
use std::collections::HashMap;

fn errors_for_apis(
    factory: &str,
    fragment: &str,
    loader: Loader,
    define: HashMap<String, String>,
    mangle_props: &str,
) -> [Vec<Message>; 3] {
    let options = BuildOptions {
        jsx_factory: factory.into(),
        jsx_fragment: fragment.into(),
        define: define.clone(),
        mangle_props: mangle_props.into(),
        stdin: Some(BuildStdin {
            contents: "{}".into(),
            loader,
            ..BuildStdin::default()
        }),
        ..BuildOptions::default()
    };
    let built = api::build(options.clone());
    let context_errors = match api::context(options) {
        Ok(context) => {
            context.dispose();
            Vec::new()
        }
        Err(error) => error.errors,
    };
    let transformed = api::transform(
        "{}",
        TransformOptions {
            jsx_factory: factory.into(),
            jsx_fragment: fragment.into(),
            define,
            mangle_props: mangle_props.into(),
            loader,
            ..TransformOptions::default()
        },
    );
    [built.errors, context_errors, transformed.errors]
}

fn assert_option_error(message: &Message, expected: &str) {
    assert_eq!(message.text, expected);
    assert_eq!(message.kind, MessageKind::Error);
    assert!(message.id.is_empty());
    assert!(message.plugin_name.is_empty());
    assert!(message.location.is_none());
    assert!(message.notes.is_empty());
    assert!(message.detail.is_none());
}

#[test]
fn invalid_jsx_identifier_chains_use_go_text_quoting_and_public_fields() {
    // Expected quotes are literals from pinned Go, including NUL's \x00.
    for (value, quoted) in [
        ("React..factory", "\"React..factory\""),
        (".factory", "\".factory\""),
        ("React.", "\"React.\""),
        ("React[\"factory\"]", "\"React[\\\"factory\\\"]\""),
        (
            "React\\u002ecreateElement",
            "\"React\\\\u002ecreateElement\"",
        ),
        ("for.factory", "\"for.factory\""),
        ("class", "\"class\""),
        ("a + b", "\"a + b\""),
        ("foo\nbar", "\"foo\\nbar\""),
        ("a\0b", "\"a\\x00b\""),
        ("😀.create", "\"😀.create\""),
        ("{}", "\"{}\""),
    ] {
        for (factory, fragment, name) in [(value, "", "factory"), ("", value, "fragment")] {
            for errors in errors_for_apis(factory, fragment, Loader::Js, HashMap::new(), "") {
                assert_eq!(errors.len(), 1, "{name}: {value:?}: {errors:?}");
                assert_option_error(&errors[0], &format!("Invalid JSX {name}: {quoted}"));
            }
        }
    }
}

#[test]
fn jsx_paths_preserve_unicode_and_keyword_acceptance() {
    for value in [
        "",
        "React.createElement",
        "α.β",
        "e\u{301}.create",
        "React.default",
        "this.factory",
        "null.factory",
        "import.meta.factory",
        "await.factory",
    ] {
        for errors in errors_for_apis(value, value, Loader::Js, HashMap::new(), "") {
            assert!(errors.is_empty(), "{value:?}: {errors:?}");
        }
    }
}

#[test]
fn jsx_fragment_accepts_primitive_constants_but_factory_rejects_them() {
    for (value, quoted) in [
        ("\"fragment\"", "\"\\\"fragment\\\"\""),
        ("true", "\"true\""),
        ("123", "\"123\""),
    ] {
        for errors in errors_for_apis("", value, Loader::Js, HashMap::new(), "") {
            assert!(errors.is_empty(), "fragment {value}: {errors:?}");
        }
        for errors in errors_for_apis(value, "", Loader::Js, HashMap::new(), "") {
            assert_eq!(errors.len(), 1);
            assert_option_error(&errors[0], &format!("Invalid JSX factory: {quoted}"));
        }
    }
}

#[test]
fn jsx_validation_precedes_parsing_for_js_css_text_and_json() {
    for loader in [Loader::Js, Loader::Css, Loader::Text, Loader::Json] {
        for errors in errors_for_apis("bad..factory", "bad..fragment", loader, HashMap::new(), "") {
            assert_eq!(errors.len(), 2, "{loader:?}: {errors:?}");
            assert_option_error(&errors[0], "Invalid JSX factory: \"bad..factory\"");
            assert_option_error(&errors[1], "Invalid JSX fragment: \"bad..fragment\"");
        }
        // A parser error must not replace invalid JSX options in a fast path.
        let result = api::transform(
            "@",
            TransformOptions {
                loader,
                jsx_fragment: "bad..fragment".into(),
                ..TransformOptions::default()
            },
        );
        assert_eq!(result.errors.len(), 1);
        assert_option_error(&result.errors[0], "Invalid JSX fragment: \"bad..fragment\"");
    }
}

#[test]
fn jsx_errors_follow_defines_and_precede_property_regex_errors() {
    for loader in [Loader::Js, Loader::Css, Loader::Text, Loader::Json] {
        let define = HashMap::from([("BAD".into(), "bad + value".into())]);
        for errors in errors_for_apis("bad..factory", "bad..fragment", loader, define, "") {
            assert_eq!(errors.len(), 3);
            assert_option_error(
                &errors[0],
                "Invalid define value (must be an entity name or JS literal): bad + value",
            );
            assert_option_error(&errors[1], "Invalid JSX factory: \"bad..factory\"");
            assert_option_error(&errors[2], "Invalid JSX fragment: \"bad..fragment\"");
        }
        for errors in errors_for_apis("bad..factory", "bad..fragment", loader, HashMap::new(), "[")
        {
            assert_eq!(errors.len(), 3);
            assert_option_error(&errors[0], "Invalid JSX factory: \"bad..factory\"");
            assert_option_error(&errors[1], "Invalid JSX fragment: \"bad..fragment\"");
            // Regex text parity is a separate inherited defect. This assertion
            // checks that a later regex error cannot mask either JSX error.
            assert!(errors[2].text.starts_with("The \"mangle props\" setting"));
        }
    }
}

#[test]
fn target_errors_precede_factory_and_fragment_errors() {
    let engines = vec![Engine {
        name: EngineName::Chrome,
        version: "invalid".into(),
    }];
    let options = BuildOptions {
        engines: engines.clone(),
        jsx_factory: "bad..factory".into(),
        jsx_fragment: "bad..fragment".into(),
        ..BuildOptions::default()
    };
    let built = api::build(options.clone());
    let context_errors = match api::context(options) {
        Err(error) => error.errors,
        Ok(context) => {
            context.dispose();
            panic!("invalid options unexpectedly accepted");
        }
    };
    let transformed = api::transform(
        "",
        TransformOptions {
            engines,
            jsx_factory: "bad..factory".into(),
            jsx_fragment: "bad..fragment".into(),
            ..TransformOptions::default()
        },
    );
    for errors in [built.errors, context_errors, transformed.errors] {
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0].text, "Invalid version: \"invalid\"");
        assert_option_error(&errors[1], "Invalid JSX factory: \"bad..factory\"");
        assert_option_error(&errors[2], "Invalid JSX fragment: \"bad..fragment\"");
    }
}
