// Port of upstream internal/helpers/stack.go.

use std::backtrace::Backtrace;

#[must_use]
pub fn pretty_printed_stack() -> String {
    pretty_print_stack(&Backtrace::force_capture().to_string())
}

fn pretty_print_stack(raw: &str) -> String {
    let mut lines = raw.trim().lines();
    let first = lines.next();
    let mut selected: Vec<&str> = Vec::new();
    if let Some(first) = first
        && !(first.starts_with("goroutine ") && first.ends_with(':'))
    {
        selected.push(first);
    }
    selected.extend(lines);

    let mut result = String::new();
    for mut line in selected {
        if let Some(location) = line.strip_prefix('\t') {
            line = location
                .strip_prefix("github.com/evanw/esbuild/")
                .unwrap_or(location);
            if let Some(offset) = line.rfind(" +0x") {
                line = &line[..offset];
            }
            result.push_str(" (");
            result.push_str(line);
            result.push(')');
            continue;
        }

        if !result.is_empty() {
            result.push('\n');
        }
        if line.ends_with(')')
            && let Some(parenthesis) = line.rfind('(')
        {
            line = &line[..parenthesis];
        }
        if let Some(slash) = line.rfind('/') {
            line = &line[slash + 1..];
        }
        result.push_str(line);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{pretty_print_stack, pretty_printed_stack};

    #[test]
    fn formats_go_style_stack_frames() {
        let raw = "goroutine 1 [running]:\n\
github.com/evanw/esbuild/internal/foo.Call(0x1)\n\
\tgithub.com/evanw/esbuild/internal/foo/foo.go:10 +0x42\n\
main.main()\n\
\tgithub.com/evanw/esbuild/cmd/esbuild/main.go:20 +0x10";
        assert_eq!(
            pretty_print_stack(raw),
            "foo.Call (internal/foo/foo.go:10)\nmain.main (cmd/esbuild/main.go:20)"
        );
    }

    #[test]
    fn captured_stack_is_nonempty() {
        assert!(!pretty_printed_stack().is_empty());
    }
}
