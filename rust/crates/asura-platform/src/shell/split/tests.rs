//! Splitter port tests: Wisp's `words(of:)` expectations, segments,
//! substitutions, inner scripts, heredocs and per-byte contexts.
use super::*;

fn words(s: &str) -> Vec<String> {
    tokens(s)
        .into_iter()
        .map(|w| w.value)
        .filter(|v| !v.is_empty())
        .collect()
}
fn parts(s: &str) -> Vec<&str> {
    segments(s).into_iter().map(|r| s[r].trim()).collect()
}

#[test]
fn words_honour_quotes_escapes_and_ifs() {
    // Wisp CommandSplitterTests, ShellSplittingTests and ShellLoopTests.
    assert_eq!(
        words(r#"git commit -m "a message" --no-verify"#),
        ["git", "commit", "-m", "a message", "--no-verify"]
    );
    assert_eq!(words(r"echo $'a\'b' c"), ["echo", "a'b", "c"]);
    assert_eq!(words("git push \\\n --force"), ["git", "push", "--force"]);
    assert_eq!(words(r"a 'b\' c"), ["a", r"b\", "c"]);
    assert_eq!(words("rm${IFS}-rf${IFS}build"), ["rm", "-rf", "build"]);
    assert_eq!(words("rm$IFS-rf x"), ["rm", "-rf", "x"]);
    assert_eq!(
        words(r#"echo "a${IFS}b" $IFSX"#),
        ["echo", "a${IFS}b", "$IFSX"]
    );
    assert_eq!(words(r#"'a b'\''c'"#), ["a b'c"]);
    // An empty quoted word is kept; a lone continuation is not.
    let t = tokens("a '' \\\n b");
    assert_eq!(
        t.iter().map(|w| w.value.as_str()).collect::<Vec<_>>(),
        ["a", "", "b"]
    );
    assert_eq!(t[1].raw, 2..4);
    // UTF-8 survives escapes and quotes.
    assert_eq!(words("echo \\é \"ü x\""), ["echo", "é", "ü x"]);
}

#[test]
fn segments_split_on_top_level_operators() {
    assert_eq!(
        parts("a; b && c || d | e & f\ng"),
        ["a", "b", "c", "d", "e", "f", "g"]
    );
    assert_eq!(
        parts(r"echo $'a\'b'; rm -rf ~"),
        [r"echo $'a\'b'", "rm -rf ~"]
    );
    assert_eq!(
        parts("echo 'a;b' \"c|d\" `e;f`"),
        ["echo 'a;b' \"c|d\" `e;f`"]
    );
    assert_eq!(
        parts("(true; rm x) && $(a; b)"),
        ["(true; rm x)", "$(a; b)"]
    );
    assert_eq!(parts("make 2>&1 &> log &"), ["make 2>&1 &> log", ""]);
    assert_eq!(parts("(unbalanced; rm x"), ["(unbalanced; rm x"]);
    assert_eq!(parts("a\\;b"), ["a\\;b"]);
}

#[test]
fn substitutions_are_outermost_and_respect_quotes() {
    let s = "echo $(a $(b)) `c` (d) '$(e)' \"$(f) (g)\" $'$(h)'";
    let bodies: Vec<_> = substitutions(s).into_iter().map(|r| &s[r]).collect();
    assert_eq!(bodies, ["a $(b)", "c", "d", "f"]);
    assert!(!groups(b"echo $(a").1);
    assert!(!groups(b"echo `a").1);
    assert!(groups(b"f() { x; }").1);
}

#[test]
fn inner_scripts_follow_wisp() {
    assert_eq!(inner_scripts("sh -c 'rm -rf ~/x'"), ["rm -rf ~/x"]);
    assert_eq!(
        inner_scripts(r#"bash -c "git push --force""#),
        ["git push --force"]
    );
    assert_eq!(inner_scripts("bash -lc 'ls; rm y'"), ["ls; rm y"]);
    assert_eq!(inner_scripts("zsh -o pipefail -c 'make'"), ["make"]);
    assert_eq!(
        inner_scripts(r#"eval "git reset --hard" x"#),
        ["git reset --hard x"]
    );
    assert_eq!(inner_scripts("xargs -I{} sh -c 'rm {}'"), ["rm {}"]);
    assert_eq!(inner_scripts("sudo -u root env A=1 bash -c 'id'"), ["id"]);
    assert_eq!(inner_scripts("timeout 5 sh -c 'id'"), ["id"]);
    assert!(inner_scripts("sh script.sh").is_empty());
    assert!(inner_scripts("python3 -c 'print(1)'").is_empty());
    assert!(inner_scripts("sh -- -c x").is_empty());
    assert_eq!(
        inner_scripts(r"find . -name x -exec sh -c 'rm $1' sh {} \;"),
        ["sh -c 'rm $1' sh {}"]
    );
}

#[test]
fn heredoc_bodies_run_to_their_delimiter() {
    let s = "cat <<EOF > out\nline 'one\nEOF\necho after";
    let (bodies, parsed) = heredocs(s.as_bytes());
    assert!(parsed);
    assert_eq!(
        bodies.iter().map(|r| &s[r.clone()]).collect::<Vec<_>>(),
        ["line 'one\nEOF"]
    );
    let s = "cat <<-\"END\" <<X\n\tbody\n\tEND\nsecond\nX\n";
    let (bodies, _) = heredocs(s.as_bytes());
    assert_eq!(
        bodies.iter().map(|r| &s[r.clone()]).collect::<Vec<_>>(),
        ["\tbody\n\tEND", "second\nX"]
    );
    assert_eq!(heredocs(b"cat <<< word").0.len(), 0);
    assert!(!heredocs(b"cat <<").1);
    assert!(!heredocs(b"cat <<'EOF").1);
    assert!(!heredocs(b"echo 'open").1);
    // Unterminated body runs to the end.
    let (bodies, parsed) = heredocs(b"cat <<EOF\nno end");
    assert!(parsed);
    assert_eq!((bodies.len(), bodies[0].clone()), (1, 10..16));
}

#[test]
fn analysis_records_quoting_words_and_nesting() {
    let s = r#"a "b c" 'd' $'e' f\g $(h) <<X"#;
    let a = analyze(&format!("{s}\nbody\nX\nsh -c 'i' && eval j"));
    assert!(a.parsed);
    let at = |needle: &str| a.bytes[s.find(needle).unwrap()];
    assert_eq!(
        (at("b").quoting, at("b").role, at("b").word),
        (Quoting::Double, Role::Text, Some(1))
    );
    assert_eq!(at("\"b").role, Role::QuoteMark);
    assert_eq!(at(" c").role, Role::Text);
    assert_eq!((at("d").quoting, at("d").word), (Quoting::Single, Some(2)));
    assert_eq!(
        (at("$'e").role, at("e'").quoting),
        (Role::QuoteMark, Quoting::Ansi)
    );
    assert_eq!(
        (at("\\g").role, at("g").role),
        (Role::Escape, Role::Escaped)
    );
    assert!(at("h").substitution && at("$(h").substitution);
    let full = format!("{s}\nbody\nX\nsh -c 'i' && eval j");
    let at = |needle: &str| a.bytes[full.rfind(needle).unwrap()];
    assert!(at("body").heredoc && at("body").word.is_none());
    assert!(at("i'").inner_script && at("j").inner_script);
    assert!(!at("sh").inner_script && !at("eval").inner_script);
    assert_eq!(a.words.len(), 12);
    assert!(!analyze("echo 'x").parsed);
    assert!(!analyze("echo $(x").parsed);
    assert!(!analyze("echo (x").parsed);
    assert!(analyze("f() { x; }").parsed);
}
