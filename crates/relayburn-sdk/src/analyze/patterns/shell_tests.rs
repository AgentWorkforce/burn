use super::*;

fn toks(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

#[test]
fn is_pure_redirect_accepts_only_digits_then_gt_run() {
    for token in [">", ">>", "2>", "2>>", "10>"] {
        assert!(is_pure_redirect(token), "{token}");
    }
    for token in ["", "2", "12", "2>&1", "&>", ">file", "2>x", "a"] {
        assert!(!is_pure_redirect(token), "{token}");
    }
}

#[test]
fn shell_words_splits_on_whitespace_and_keeps_quoted_runs() {
    assert_eq!(shell_words(""), Vec::<String>::new());
    assert_eq!(shell_words("  \t "), Vec::<String>::new());
    assert_eq!(shell_words("cat  a.txt\tb"), toks(&["cat", "a.txt", "b"]));
    assert_eq!(
        shell_words(r#"cat "my file.txt" 'x y'"#),
        toks(&["cat", "\"my file.txt\"", "'x y'"])
    );
    assert_eq!(shell_words(r#""a"b"#), toks(&["\"a\"", "b"]));
}

#[test]
fn shell_words_falls_back_to_non_space_run_on_unterminated_quote() {
    assert_eq!(
        shell_words(r#"cat "abc def"#),
        toks(&["cat", "\"abc", "def"])
    );
    assert_eq!(shell_words("'tail"), toks(&["'tail"]));
}

#[test]
fn file_operand_detects_plain_and_quoted_paths() {
    assert!(has_shell_file_operand("cat", &toks(&["a.txt"])));
    assert!(has_shell_file_operand("cat", &toks(&["\"my file.txt\""])));
    assert!(has_shell_file_operand("cat", &toks(&["-v", "a.txt"])));
    assert!(!has_shell_file_operand("cat", &toks(&[])));
    assert!(!has_shell_file_operand("cat", &toks(&["-v", "-"])));
    assert!(!has_shell_file_operand("cat", &toks(&["<input.txt"])));
}

#[test]
fn file_operand_stops_at_control_operators() {
    for op in ["|", "&&", "||", ";"] {
        assert!(
            !has_shell_file_operand("cat", &toks(&[op, "a.txt"])),
            "{op}"
        );
    }
}

#[test]
fn file_operand_skips_redirect_targets() {
    assert!(!has_shell_file_operand("cat", &toks(&[">", "out.txt"])));
    assert!(!has_shell_file_operand("cat", &toks(&["2>>", "err.log"])));
    assert!(has_shell_file_operand(
        "cat",
        &toks(&["2>", "err.log", "a.txt"])
    ));
    assert!(!has_shell_file_operand("cat", &toks(&["2>/dev/null"])));
    assert!(has_shell_file_operand(
        "cat",
        &toks(&["2>/dev/null", "a.txt"])
    ));
}

#[test]
fn file_operand_skips_head_tail_count_values() {
    for cmd in ["head", "tail"] {
        for flag in ["-n", "-c", "--lines", "--bytes"] {
            assert!(
                !has_shell_file_operand(cmd, &toks(&[flag, "5"])),
                "{cmd} {flag}"
            );
            assert!(has_shell_file_operand(cmd, &toks(&[flag, "5", "f"])));
        }
        assert!(!has_shell_file_operand(cmd, &toks(&["+5"])));
        assert!(!has_shell_file_operand(cmd, &toks(&["10"])));
    }
    assert!(has_shell_file_operand("cat", &toks(&["-n", "5"])));
    assert!(has_shell_file_operand("cat", &toks(&["+5"])));
}
