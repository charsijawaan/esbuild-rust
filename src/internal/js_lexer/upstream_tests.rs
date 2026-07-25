//! Behavioral translations of the pinned upstream `js_lexer_test.go`.
#![allow(clippy::needless_raw_string_hashes)]

use std::{collections::HashMap, panic::AssertUnwindSafe, sync::Arc};

use super::{Lexer, Token};
use crate::internal::{
    config::TsOptions,
    logger::{DeferLogKind, Log, Source},
};

fn source(text: &[u8]) -> Source {
    Source {
        contents: Arc::from(text),
        ..Source::default()
    }
}

fn lexer(text: &[u8]) -> (Lexer, Log) {
    let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
    (
        Lexer::new(log.clone(), source(text), TsOptions::default()),
        log,
    )
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn expect_string(input: &[u8], expected: &[u16]) {
    let (mut lexer, log) = lexer(input);
    assert_eq!(lexer.token, Token::StringLiteral, "{input:?}");
    assert_eq!(lexer.string_literal(), expected, "{input:?}");
    assert!(log.done().is_empty(), "{input:?}");
}

fn expect_error(input: &[u8], decode_string: bool) {
    let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut lexer = Lexer::new(log.clone(), source(input), TsOptions::default());
        if decode_string {
            let _ = lexer.string_literal();
        }
    }));
    assert!(result.is_err(), "expected lexer panic for {input:?}");
    assert!(!log.done().is_empty(), "expected diagnostic for {input:?}");
}

#[test]
fn upstream_comments_hashbang_and_identifiers() {
    for input in [b"/*".as_slice(), b"/*/"] {
        expect_error(input, false);
    }
    for input in [b"/**/".as_slice(), b"//"] {
        let (lexer, log) = lexer(input);
        assert_eq!(lexer.token, Token::EndOfFile);
        assert!(log.done().is_empty());
    }

    for (input, expected) in [
        (
            b"#!/usr/bin/env node".as_slice(),
            b"#!/usr/bin/env node".as_slice(),
        ),
        (b"#!/usr/bin/env node\n", b"#!/usr/bin/env node".as_slice()),
        (
            b"#!/usr/bin/env node\nlet x",
            b"#!/usr/bin/env node".as_slice(),
        ),
    ] {
        let (lexer, log) = lexer(input);
        assert_eq!(lexer.token, Token::Hashbang);
        assert_eq!(lexer.identifier.string, expected);
        assert!(log.done().is_empty());
    }
    expect_error(b" #!/usr/bin/env node", false);

    for (input, expected) in [
        (b"_".as_slice(), b"_".as_slice()),
        (b"$", b"$".as_slice()),
        (b"test", b"test".as_slice()),
        (br"t\u0065st", b"test".as_slice()),
        (br"t\u{65}st", b"test".as_slice()),
        ("a\u{200C}".as_bytes(), "a\u{200C}".as_bytes()),
        ("a\u{200D}b".as_bytes(), "a\u{200D}b".as_bytes()),
    ] {
        let (lexer, log) = lexer(input);
        assert_eq!(lexer.token, Token::Identifier, "{input:?}");
        assert_eq!(lexer.identifier.string, expected, "{input:?}");
        assert!(log.done().is_empty(), "{input:?}");
    }
    for input in [
        br"t\u.".as_slice(),
        br"t\u0.",
        br"t\u00.",
        br"t\u006.",
        br"t\u{.",
        br"t\u{0.",
    ] {
        expect_error(input, false);
    }
}

#[test]
fn upstream_string_literal_matrix() {
    expect_string(b"''", &[]);
    expect_string(b"'123'", &utf16("123"));
    expect_string(br#"'"'"#, &utf16("\""));
    expect_string(br#"'\''"#, &utf16("'"));
    expect_string(br#"'\"'"#, &utf16("\""));
    expect_string(br#"'\\'"#, &utf16("\\"));
    expect_string(br#"'\a'"#, &utf16("a"));
    expect_string(br#"'\b'"#, &[8]);
    expect_string(br#"'\f'"#, &[12]);
    expect_string(br#"'\n'"#, &[10]);
    expect_string(br#"'\r'"#, &[13]);
    expect_string(br#"'\t'"#, &[9]);
    expect_string(br#"'\v'"#, &[11]);

    for digit in 0_u8..=7 {
        let input = [b'\'', b'\\', b'0' + digit, b'\''];
        expect_string(&input, &[u16::from(digit)]);
        let input = [b'\'', b'\\', b'0', b'0', b'0' + digit, b'\''];
        expect_string(&input, &[u16::from(digit)]);
    }
    for (input, expected) in [
        (br"'\100'".as_slice(), vec![0o100]),
        (br"'\200'", vec![0x80]),
        (br"'\300'", vec![0xC0]),
        (br"'\377'", vec![0xFF]),
        (br"'\378'", vec![0o37, u16::from(b'8')]),
        (br"'\400'", vec![0o40, u16::from(b'0')]),
        (br"'\500'", vec![0o50, u16::from(b'0')]),
        (br"'\600'", vec![0o60, u16::from(b'0')]),
        (br"'\700'", vec![0o70, u16::from(b'0')]),
    ] {
        expect_string(input, &expected);
    }

    expect_string(br"'\x00'", &[0]);
    expect_string(br"'\X11'", &utf16("X11"));
    expect_string(br"'\x71'", &utf16("q"));
    expect_string(br"'\x7f'", &[0x7F]);
    expect_string(br"'\x7F'", &[0x7F]);
    expect_string(br"'\u0000'", &[0]);
    expect_string(br"'\ucafe\uCAFE\u7FFF'", &[0xCAFE, 0xCAFE, 0x7FFF]);
    expect_string(br"'\uD800'", &[0xD800]);
    expect_string(br"'\uDC00'", &[0xDC00]);
    expect_string(br"'\U0000'", &utf16("U0000"));
    expect_string(br"'\u{100000}'", &[0xDBC0, 0xDC00]);
    expect_string(br"'\u{10FFFF}'", &[0xDBFF, 0xDFFF]);

    expect_string("'\u{2028}'".as_bytes(), &utf16("\u{2028}"));
    expect_string("'\u{2029}'".as_bytes(), &utf16("\u{2029}"));
    expect_string("\"\\\u{2028}x\"".as_bytes(), &utf16("x"));
    expect_string(b"'1\\\r2'", &utf16("12"));
    expect_string(b"'1\\\n2'", &utf16("12"));
    expect_string(b"'1\\\r\n2'", &utf16("12"));
    expect_string("'1\\\u{2028}2'".as_bytes(), &utf16("12"));
    expect_string("'1\\\u{2029}2'".as_bytes(), &utf16("12"));
}

#[test]
fn upstream_invalid_string_literal_matrix() {
    for input in [
        br"'\u{110000}'".as_slice(),
        br"'\u{FFFFFFFF}'",
        b"'\n'",
        b"'\r'",
        b"\"\n\"",
        b"\"\r\"",
        b"'1\\\n\r2'",
        b"\"'",
        b"'\"",
        b"'\\",
        b"'\\'",
        br"'\x",
        br"'\x'",
        br"'\xG'",
        br"'\xF'",
        br"'\xFG'",
        br"'\u",
        br"'\u'",
        br"'\u0'",
        br"'\u00'",
        br"'\u000'",
    ] {
        expect_error(input, true);
    }
}

#[test]
#[allow(clippy::float_cmp)]
fn upstream_numeric_boundaries_and_separators() {
    let cases = [
        ("9999999999", 9_999_999_999.0),
        ("123456789123456789", 123_456_789_123_456_780.0),
        ("2.2250738585072014e-308", 2.225_073_858_507_201_4e-308),
        ("5e-324", 5e-324),
        ("1e-325", 0.0),
        ("1.797693134862315808e+308", f64::INFINITY),
        ("1e+309", f64::INFINITY),
        ("0x7fff_ffff", 2_147_483_647.0),
        ("0x1_0000_0000", 4_294_967_296.0),
        ("1_2_3", 123.0),
        (".1_2", 0.12),
        ("1_2.3_4", 12.34),
        ("1e2_3", 1e23),
        ("1_2e3_4", 12e34),
        ("08.0_1", 8.01),
        ("09.0_1", 9.01),
    ];
    for (input, expected) in cases {
        let (lexer, log) = lexer(input.as_bytes());
        assert_eq!(lexer.token, Token::NumericLiteral, "{input}");
        assert_eq!(lexer.number, expected, "{input}");
        assert!(log.done().is_empty(), "{input}");
    }
}

#[test]
fn upstream_invalid_numeric_separator_matrix() {
    for input in [
        "0b",
        "0B",
        "0b012",
        "0b018",
        "0o",
        "0o018",
        "0x",
        "0xGFEDCBA",
        "1e",
        ".1e",
        "1.e",
        "1.1e",
        "1e+",
        "1e-",
        "1e+-1",
        "1e-+1",
        "1z",
        "1.z",
        "1.0f",
        "0b1z",
        "0o1z",
        "0x1z",
        "1e1z",
        "0_0",
        "0_8",
        "00_0",
        "08_0",
        "1__2",
        ".1__2",
        "1e2__3",
        "0b1__0",
        "0o1__2",
        "0x1__2",
        "1_",
        "1._",
        "1_.",
        ".1_",
        "1e_",
        "1e1_",
        "1_e1",
        ".1_e1",
        "1._2",
        "1_.2",
        "0b_1",
        "0o_1",
        "0x_1",
        "0b1_",
        "0o1_",
        "0x1_",
        "1e2n",
        "1.0n",
        ".1n",
        "000n",
        "0123n",
        "089n",
    ] {
        expect_error(input.as_bytes(), false);
    }
}

#[test]
fn upstream_keyword_token_matrix() {
    let cases = [
        ("", Token::EndOfFile),
        ("\0", Token::SyntaxError),
        ("#!", Token::Hashbang),
        ("(", Token::OpenParen),
        (")", Token::CloseParen),
        ("[", Token::OpenBracket),
        ("]", Token::CloseBracket),
        ("{", Token::OpenBrace),
        ("}", Token::CloseBrace),
        ("break", Token::Break),
        ("case", Token::Case),
        ("catch", Token::Catch),
        ("class", Token::Class),
        ("const", Token::Const),
        ("continue", Token::Continue),
        ("debugger", Token::Debugger),
        ("default", Token::Default),
        ("delete", Token::Delete),
        ("do", Token::Do),
        ("else", Token::Else),
        ("enum", Token::Enum),
        ("export", Token::Export),
        ("extends", Token::Extends),
        ("false", Token::False),
        ("finally", Token::Finally),
        ("for", Token::For),
        ("function", Token::Function),
        ("if", Token::If),
        ("import", Token::Import),
        ("in", Token::In),
        ("instanceof", Token::Instanceof),
        ("new", Token::New),
        ("null", Token::Null),
        ("return", Token::Return),
        ("super", Token::Super),
        ("switch", Token::Switch),
        ("this", Token::This),
        ("throw", Token::Throw),
        ("true", Token::True),
        ("try", Token::Try),
        ("typeof", Token::Typeof),
        ("var", Token::Var),
        ("void", Token::Void),
        ("while", Token::While),
        ("with", Token::With),
    ];
    for (input, expected) in cases {
        let (lexer, _) = lexer(input.as_bytes());
        assert_eq!(lexer.token, expected, "{input:?}");
    }
}

#[test]
fn raw_wtf8_surrogates_are_not_treated_as_end_of_file() {
    let surrogate = [0xED, 0xA0, 0x80];

    let mut string = vec![b'\''];
    string.extend(surrogate);
    string.push(b'\'');
    expect_string(&string, &[0xD800]);

    let mut regexp = vec![b'/'];
    regexp.extend(surrogate);
    regexp.extend(b"/g");
    let (mut regexp_lexer, log) = lexer(&regexp);
    assert_eq!(regexp_lexer.token, Token::Slash);
    regexp_lexer.scan_reg_exp();
    assert_eq!(regexp_lexer.raw(), regexp);
    assert!(log.done().is_empty());

    let mut comment = b"// before ".to_vec();
    comment.extend(surrogate);
    comment.extend(b"\nafter");
    let (lexer, log) = lexer(&comment);
    assert_eq!(lexer.token, Token::Identifier);
    assert_eq!(lexer.raw(), b"after");
    assert!(log.done().is_empty());
}
