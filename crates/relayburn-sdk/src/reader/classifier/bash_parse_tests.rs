use super::*;

fn toks(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

#[test]
fn shell_command_arg_returns_token_after_c_flag() {
    assert_eq!(
        shell_command_arg(&toks(&["bash", "-c", "ls"]), 1),
        Some("ls".to_string())
    );
    assert_eq!(
        shell_command_arg(&toks(&["sh", "-lc", "make test"]), 1),
        Some("make test".to_string())
    );
    assert_eq!(
        shell_command_arg(&toks(&["bash", "-x", "-c", "ls"]), 1),
        Some("ls".to_string())
    );
}

#[test]
fn shell_command_arg_ignores_long_options_and_tokens_before_start() {
    assert_eq!(
        shell_command_arg(&toks(&["bash", "--rcfile", "rc", "s.sh"]), 1),
        None
    );
    assert_eq!(shell_command_arg(&toks(&["bash", "-l", "s.sh"]), 1), None);
    assert_eq!(
        shell_command_arg(&toks(&["-c", "skipped", "bash", "s.sh"]), 2),
        None
    );
    assert_eq!(shell_command_arg(&toks(&["bash", "-c"]), 1), None);
    assert_eq!(shell_command_arg(&toks(&["bash"]), 1), None);
}

#[test]
fn env_command_args_skips_assignments_and_options() {
    assert_eq!(
        env_command_args(&toks(&["env", "FOO=1", "-i", "ls", "-la"]), 1),
        toks(&["ls", "-la"])
    );
    assert_eq!(
        env_command_args(&toks(&["FOO=1", "env", "BAR=2", "cmd"]), 2),
        toks(&["cmd"])
    );
    assert_eq!(
        env_command_args(&toks(&["env", "1A=2", "cmd"]), 1),
        toks(&["1A=2", "cmd"])
    );
}

#[test]
fn env_command_args_stops_after_double_dash() {
    assert_eq!(
        env_command_args(&toks(&["env", "A=1", "--", "B=2", "-x"]), 1),
        toks(&["B=2", "-x"])
    );
    assert_eq!(env_command_args(&toks(&["env", "--"]), 1), toks(&[]));
}

#[test]
fn env_command_args_returns_empty_when_only_prefix_tokens() {
    assert_eq!(env_command_args(&toks(&["env"]), 1), toks(&[]));
    assert_eq!(env_command_args(&toks(&["env", "A=1", "-u"]), 1), toks(&[]));
}

#[test]
fn unwrap_subshell_requires_wrapping_parens() {
    assert_eq!(unwrap_subshell("ls"), None);
    assert_eq!(unwrap_subshell("(ls"), None);
    assert_eq!(unwrap_subshell("ls)"), None);
    assert_eq!(unwrap_subshell("(ls -la)"), Some("ls -la".to_string()));
    assert_eq!(unwrap_subshell("(  ls  )"), Some("ls".to_string()));
    assert_eq!(
        unwrap_subshell("(cd x && (ls))"),
        Some("cd x && (ls)".to_string())
    );
}

#[test]
fn unwrap_subshell_rejects_sibling_groups_and_unbalanced_input() {
    assert_eq!(unwrap_subshell("(a) && (b)"), None);
    assert_eq!(unwrap_subshell("((ls)"), None);
    assert_eq!(unwrap_subshell(r#"(echo ")"#), None);
}

#[test]
fn unwrap_subshell_ignores_quoted_and_escaped_parens() {
    assert_eq!(
        unwrap_subshell("(echo ')' x)"),
        Some("echo ')' x".to_string())
    );
    assert_eq!(
        unwrap_subshell(r#"(echo ")" x)"#),
        Some(r#"echo ")" x"#.to_string())
    );
    assert_eq!(
        unwrap_subshell(r#"(echo "a\"b")"#),
        Some(r#"echo "a\"b""#.to_string())
    );
    assert_eq!(
        unwrap_subshell(r"(echo \) x)"),
        Some(r"echo \) x".to_string())
    );
    assert_eq!(
        unwrap_subshell(r"(echo 'a\')"),
        Some(r"echo 'a\'".to_string())
    );
}
