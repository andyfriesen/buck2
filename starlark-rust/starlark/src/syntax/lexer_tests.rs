/*
 * Copyright 2018 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::fmt::Write;

use crate::assert;
use crate::slice_vec_ext::VecExt;
use crate::tests::golden_test_template::golden_test_template;

fn lexer_golden_test(name: &str, program: &str) {
    let program = program.trim();

    let mut out = String::new();

    writeln!(out, "Program:").unwrap();
    writeln!(out, "{}", program).unwrap();
    writeln!(out).unwrap();
    writeln!(out, "Tokens:").unwrap();

    let tokens =
        assert::lex_tokens(program).into_map(|(from, token, to)| (from, token.to_string(), to));
    let max_width = tokens
        .iter()
        .map(|(_, token, _)| token.len())
        .max()
        .unwrap_or(0);
    for (from, token, to) in &tokens {
        let source = program[*from..*to].replace('\n', "\\n");
        writeln!(out, "{token:<max_width$}  # {source}").unwrap();
    }

    golden_test_template(&format!("src/syntax/lexer_tests/{}.golden", name), &out);
}

#[test]
fn test_int_lit() {
    lexer_golden_test(
        "int_lit",
        r#"
0 123
0x7F 0x7d
0B1011 0b1010
0o755 0O753
"#,
    );
    // Starlark requires us to ban leading zeros (confusion with implicit octal)
    assert::parse_fail("x = !01!");
}

#[test]
fn test_indentation() {
    lexer_golden_test(
        "indentation",
        "
+
  -
      /
      *
  =
    %
      .
+=
",
    );
}

#[test]
fn test_symbols() {
    lexer_golden_test(
        "symbols",
        ", ; : += -= *= /= //= %= == != <= >= ** = < > - + * % / // . { } [ ] ( ) |\n\
        ,;:{}[]()|",
    );
}

#[test]
fn test_keywords() {
    lexer_golden_test(
        "keywords",
        "and else load break for not not  in continue if or def in pass elif return lambda",
    );
}

// Regression test for https://github.com/google/starlark-rust/issues/44.
#[test]
fn test_number_collated_with_keywords_or_identifier() {
    lexer_golden_test(
        "number_collated_with_keywords_or_identifier",
        "0in 1and 2else 3load 4break 5for 6not 7not  in 8continue 10identifier11",
    );
}

#[test]
fn test_reserved() {
    let reserved =
        "as import is class nonlocal del raise except try finally while from with global yield"
            .split_whitespace();
    for x in reserved {
        assert::parse_fail(&format!("!{}! = 1", x));
    }
}

#[test]
fn test_comment() {
    // Comment should be ignored
    lexer_golden_test(
        "comment",
        r#"
# first comment
  # second comment
a # third comment

# But it should not eat everything
[
# comment inside list
]
"#,
    );
}

#[test]
fn test_identifier() {
    lexer_golden_test("identifier", "a identifier CAPS _CAPS _0123");
}

#[test]
fn test_string_lit() {
    assert_eq!(
        assert::lex("'123' \"123\" '' \"\" '\\'' \"\\\"\" '\"' \"'\" '\\n' '\\w'"),
        "\"123\" \"123\" \"\" \"\" \"\'\" \"\\\"\" \"\\\"\" \"\'\" \"\\n\" \"\\\\w\" \n"
    );

    // unfinished string literal
    assert::parse_fail("!'!\n'");
    assert::parse_fail("!\"!\n\"");
    assert::parse_fail("this = a + test + !r\"!");
    assert::parse_fail("test + !\' of thing that!");
    assert::parse_fail("test + !\' of thing that!\n'");

    // Multiline string
    assert_eq!(
        assert::lex("'''''' '''\\n''' '''\n''' \"\"\"\"\"\" \"\"\"\\n\"\"\" \"\"\"\n\"\"\""),
        "\"\" \"\\n\" \"\\n\" \"\" \"\\n\" \"\\n\" \n"
    );
    // Raw string
    assert_eq!(
        assert::lex("r'' r\"\" r'\\'' r\"\\\"\" r'\"' r\"'\" r'\\n'"),
        "\"\" \"\" \"\'\" \"\\\"\" \"\\\"\" \"\'\" \"\\\\n\" \n"
    );
}

#[test]
fn test_string_escape() {
    lexer_golden_test(
        "string_escape",
        r#"
'\0\0\1n'
'\0\00\000\0000'
'\x000'
'\372x'
"#,
    );
    assert::parse_fail("test 'more !\\xT!Z");
    assert::parse_fail("test + 'more !\\UFFFFFFFF! overflows'");
    assert::parse_fail("test 'more !\\x0y!abc'");
    assert::parse_fail("test 'more !\\x0!");
}

#[test]
fn test_simple_example() {
    lexer_golden_test(
        "simple_example",
        "\"\"\"A docstring.\"\"\"

def _impl(ctx):
  # Print Hello, World!
  print('Hello, World!')
",
    );
}

#[test]
fn test_escape_newline() {
    lexer_golden_test(
        "escape_newline",
        r#"
a \
b
"#,
    );
}

#[test]
fn test_lexer_multiline_triple() {
    lexer_golden_test(
        "multiline_triple",
        r#"
cmd = """A \
    B \
    C \
    """"#,
    );
}

#[test]
fn test_span() {
    use crate::syntax::lexer::Token::*;
    let expected = vec![
        (0, Newline, 1),
        (1, Def, 4),
        (5, Identifier("test".to_owned()), 9),
        (9, OpeningRound, 10),
        (10, Identifier("a".to_owned()), 11),
        (11, ClosingRound, 12),
        (12, Colon, 13),
        (13, Newline, 14),
        (14, Indent, 16),
        (16, Identifier("fail".to_owned()), 20),
        (20, OpeningRound, 21),
        (21, Identifier("a".to_owned()), 22),
        (22, ClosingRound, 23),
        (23, Newline, 24),
        (24, Newline, 25),
        (25, Dedent, 25),
        (25, Identifier("test".to_owned()), 29),
        (29, OpeningRound, 30),
        (30, String("abc".to_owned()), 35),
        (35, ClosingRound, 36),
        (36, Newline, 37),
        (37, Newline, 37),
    ];

    let actual = assert::lex_tokens(
        r#"
def test(a):
  fail(a)

test("abc")
"#,
    );
    assert_eq!(expected, actual);
}

#[test]
fn test_lexer_final_comment() {
    lexer_golden_test(
        "final_comment",
        r#"
x
# test"#,
    );
}

#[test]
fn test_lexer_dedent() {
    lexer_golden_test(
        "dedent",
        r#"
def stuff():
  if 1:
    if 1:
      pass
  pass
"#,
    );
}

#[test]
fn test_lexer_operators() {
    lexer_golden_test(
        "operators",
        r#"
1+-2
1+------2
///==/+-
"#,
    );
    assert::eq("1+------2", "3");
}

#[test]
fn test_lexer_error_messages() {
    // What are the common errors people make.
    // Do they have good error messages and span locations.
    fn f(program: &str, msg: &str) {
        assert::parse_fail(program);
        assert::fail(&program.replace('!', ""), msg);
    }

    f("unknown !$!&%+ operator", "invalid input `$`");
    f("an !'incomplete string!\nends", "unfinished string literal");
    f(
        "an + 'invalid escape !\\x3 ! character'",
        "invalid string escape sequence `x3 `",
    );
    f(
        "leading_zero = !003! + 8",
        "integer cannot have leading 0, got `003`",
    );
    f("a + (test!]! + c", "unexpected symbol ']' here");
    f(
        "reserved_word = !raise! + 1",
        "cannot use reserved keyword `raise`",
    );
}

#[test]
fn test_float_lit() {
    lexer_golden_test(
        "float_lit",
        r#"
0.0 0. .0
1e10 1e+10 1e-10
1.1e10 1.1e+10 1.1e-10
0. .123 3.14 .2e3 1E+4
"#,
    );
}

#[test]
fn test_f_string() {
    lexer_golden_test(
        "f_string",
        r#"
f"basic1 {stuff1}"
f'basic2 {stuff2}'

# Raw f-string

fr'' fr"" fr'\'' fr"\"" fr'"' fr"'" fr'\n'
"#,
    );
}
