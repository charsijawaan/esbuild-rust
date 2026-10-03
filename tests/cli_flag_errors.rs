use std::process::Command;

fn check(arguments: &[&str], level: &str, expected: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
        .args(arguments)
        .arg(format!("--log-level={level}"))
        .arg("--color=false")
        .output()
        .expect("run Rust CLI");
    assert_eq!(output.status.code(), Some(1), "{arguments:?}");
    assert!(output.stdout.is_empty());
    let expected = match level {
        "silent" => String::new(),
        "info" => format!("{expected}1 error\n"),
        _ => expected.to_string(),
    };
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        expected,
        "{arguments:?}, {level}"
    );
}

#[test]
fn invalid_flags_use_build_or_transform_diagnostics_independent_of_order() {
    for level in ["info", "warning", "silent"] {
        for arguments in [
            vec!["--invalid-flag", "in.js", "--analyze"],
            vec!["in.js", "--invalid-flag", "--analyze"],
            vec!["--invalid-flag", "--bundle"],
            vec!["--bundle", "--invalid-flag"],
        ] {
            check(
                &arguments,
                level,
                "✘ [ERROR] Invalid build flag: \"--invalid-flag\"\n\n",
            );
        }
        for arguments in [
            vec!["--invalid-flag", "--minify"],
            vec!["--minify", "--invalid-flag"],
        ] {
            check(
                &arguments,
                level,
                "✘ [ERROR] Invalid transform flag: \"--invalid-flag\"\n\n",
            );
        }
        check(
            &["--analyze"],
            level,
            "✘ [ERROR] Invalid transform flag: \"--analyze\"\n\n",
        );
    }
}

#[test]
fn literal_single_quotes_on_flags_report_the_shell_quoting_note() {
    let flag = "'--define:process.env.NODE_ENV=\"production\"'";
    let expected = format!(
        "✘ [ERROR] Unexpected single quote character before flag: {flag}\n\n  This typically happens when attempting to use single quotes to quote arguments with a shell that doesn't recognize single quotes. Try using double quote characters to quote arguments instead.\n\n"
    );
    for level in ["info", "warning", "silent"] {
        for arguments in [vec!["in.js", flag], vec![flag, "in.js"], vec![flag]] {
            check(&arguments, level, &expected);
        }
    }
}
