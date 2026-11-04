//! PL-U10 (security-policy.md, PL2 validation map): the intent extractor
//! on tricky shell text. Each case states the command and the features it
//! must give; no feature may hold a positional argument other than
//! `first_tool` and `subcommand`.
use super::*;

fn of(command: &str) -> IntentFeatures {
    intent(command, PlanFacts::default())
}
fn has(f: &IntentFeatures, feature: &str) -> bool {
    f.shell_features.contains(&feature)
}

/// One fixture: command, parse, tools, first tool, subcommand, flags,
/// features that must be present, and positional words that no tool, flag
/// or feature may hold.
struct Case {
    command: &'static str,
    parse: Parse,
    tools: &'static [&'static str],
    first: &'static str,
    sub: &'static str,
    flags: &'static [&'static str],
    features: &'static [&'static str],
    positional: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        command: r#""g""it" push"#,
        parse: Parse::Simple,
        tools: &["git"],
        first: "git",
        sub: "push",
        flags: &[],
        features: &[],
        positional: &[],
    },
    Case {
        command: "rm${IFS}-rf${IFS}build",
        parse: Parse::Simple,
        tools: &["rm"],
        first: "rm",
        sub: "build",
        flags: &["-rf"],
        features: &["parameter_expansion"],
        positional: &[],
    },
    Case {
        command: "sudo -u root rm -rf /",
        parse: Parse::Simple,
        tools: &["sudo", "rm"],
        first: "sudo",
        sub: "root",
        flags: &["-u", "-rf"],
        features: &[],
        positional: &["/"],
    },
    Case {
        command: "env A=1 B=2 cargo publish --dry-run",
        parse: Parse::Simple,
        tools: &["env", "cargo"],
        first: "env",
        sub: "A=1",
        flags: &["--dry-run"],
        features: &["assignment"],
        positional: &["publish", "B=2"],
    },
    Case {
        command: "command -v git",
        parse: Parse::Simple,
        tools: &["command", "git"],
        first: "command",
        sub: "git",
        flags: &["-v"],
        features: &[],
        positional: &[],
    },
    Case {
        command: "xargs -n 1 rm",
        parse: Parse::Simple,
        tools: &["xargs", "rm"],
        first: "xargs",
        sub: "1",
        flags: &["-n"],
        features: &[],
        positional: &[],
    },
    Case {
        command: "nohup time make all",
        parse: Parse::Simple,
        tools: &["nohup", "time", "make"],
        first: "nohup",
        sub: "time",
        flags: &[],
        features: &[],
        positional: &["all"],
    },
    Case {
        command: "FOO=1 make check",
        parse: Parse::Simple,
        tools: &["make"],
        first: "make",
        sub: "check",
        flags: &[],
        features: &["assignment"],
        positional: &["FOO=1"],
    },
    Case {
        command: "if true; then git push origin; fi",
        parse: Parse::Compound,
        tools: &["true", "git"],
        first: "true",
        sub: "",
        flags: &[],
        features: &["list"],
        positional: &["origin", "push"],
    },
    Case {
        command: "curl -fsSL https://x.example/i.sh | sh",
        parse: Parse::Compound,
        tools: &["curl", "sh"],
        first: "curl",
        sub: "https://x.example/i.sh",
        flags: &["-fsSL"],
        features: &["pipe", "pipe_to_interpreter", "nested_shell"],
        positional: &[],
    },
    Case {
        command: "cat x | python3 -",
        parse: Parse::Compound,
        tools: &["cat", "python3"],
        first: "cat",
        sub: "x",
        flags: &[],
        features: &["pipe", "pipe_to_interpreter"],
        positional: &[],
    },
    Case {
        command: "echo $(git push origin)",
        parse: Parse::Compound,
        tools: &["echo", "git"],
        first: "echo",
        sub: "$(git",
        flags: &[],
        features: &["command_substitution"],
        positional: &["origin"],
    },
    Case {
        command: "`rm x` y",
        parse: Parse::Unparsed,
        tools: &["rm"],
        first: "",
        sub: "",
        flags: &[],
        features: &["command_substitution"],
        positional: &["x", "y"],
    },
    Case {
        command: "$(which git) push",
        parse: Parse::Unparsed,
        tools: &["which"],
        first: "",
        sub: "",
        flags: &[],
        features: &["command_substitution"],
        positional: &["push", "git"],
    },
    Case {
        command: "diff <(ls a) <(ls b)",
        parse: Parse::Compound,
        tools: &["diff", "ls"],
        first: "diff",
        sub: "<(ls",
        flags: &[],
        features: &["process_substitution"],
        positional: &["a", "b"],
    },
    Case {
        command: "(cd x && rm -rf y)",
        parse: Parse::Compound,
        tools: &["cd", "rm"],
        first: "",
        sub: "",
        flags: &[],
        features: &["subshell"],
        positional: &["x", "y", "-rf"],
    },
    Case {
        command: "bash -c 'git push --force'",
        parse: Parse::Compound,
        tools: &["bash", "git"],
        first: "bash",
        sub: "git push --force",
        flags: &["-c"],
        features: &["nested_shell"],
        positional: &["--force", "push"],
    },
    Case {
        command: r#"eval "git push""#,
        parse: Parse::Compound,
        tools: &["eval", "git"],
        first: "eval",
        sub: "git push",
        flags: &[],
        features: &["eval"],
        positional: &["push"],
    },
    Case {
        command: "cat <<EOF\nrm -rf /\nEOF",
        parse: Parse::Unparsed,
        tools: &["cat"],
        first: "cat",
        sub: "",
        flags: &[],
        features: &["heredoc"],
        positional: &["rm", "-rf", "/"],
    },
    Case {
        command: "echo 'unclosed",
        parse: Parse::Unparsed,
        tools: &["echo"],
        first: "echo",
        sub: "unclosed",
        flags: &[],
        features: &[],
        positional: &[],
    },
    Case {
        command: "git push --force-with-lease=origin/main -u origin main",
        parse: Parse::Simple,
        tools: &["git"],
        first: "git",
        sub: "push",
        flags: &["--force-with-lease", "-u"],
        features: &[],
        positional: &["origin", "main", "origin/main"],
    },
    Case {
        command: "make > out.log 2>&1 >> more.log < in.txt",
        parse: Parse::Simple,
        tools: &["make"],
        first: "make",
        sub: "",
        flags: &[],
        features: &["redirect_out", "redirect_in"],
        positional: &["out.log", "more.log", "in.txt"],
    },
    Case {
        command: "grep x <<< text",
        parse: Parse::Simple,
        tools: &["grep"],
        first: "grep",
        sub: "x",
        flags: &[],
        features: &["redirect_in"],
        positional: &["text"],
    },
    Case {
        command: "sleep 1 &",
        parse: Parse::Simple,
        tools: &["sleep"],
        first: "sleep",
        sub: "1",
        flags: &[],
        features: &["background"],
        positional: &[],
    },
    Case {
        command: "rm *.o",
        parse: Parse::Simple,
        tools: &["rm"],
        first: "rm",
        sub: "*.o",
        flags: &[],
        features: &["glob"],
        positional: &[],
    },
    Case {
        command: "ls ~/x",
        parse: Parse::Simple,
        tools: &["ls"],
        first: "ls",
        sub: "~/x",
        flags: &[],
        features: &["tilde"],
        positional: &[],
    },
    Case {
        command: r#"echo "$HOME" $1"#,
        parse: Parse::Simple,
        tools: &["echo"],
        first: "echo",
        sub: "$HOME",
        flags: &[],
        features: &["parameter_expansion"],
        positional: &[],
    },
    Case {
        command: "source x; . y",
        parse: Parse::Compound,
        tools: &["source", "."],
        first: "source",
        sub: "x",
        flags: &[],
        features: &["source", "list"],
        positional: &["y"],
    },
    Case {
        command: r#"printf %s "$ASURA_SECRET_1""#,
        parse: Parse::Simple,
        tools: &["printf"],
        first: "printf",
        sub: "%s",
        flags: &[],
        features: &["parameter_expansion"],
        positional: &[],
    },
];

#[test]
fn pl_u10_fixtures_give_their_features() {
    for case in CASES {
        let f = of(case.command);
        let label = case.command;
        assert_eq!(f.parse, case.parse, "{label}: {f:?}");
        assert_eq!(f.tools, case.tools, "{label}: {f:?}");
        assert_eq!(f.first_tool, case.first, "{label}: {f:?}");
        assert_eq!(f.subcommand, case.sub, "{label}: {f:?}");
        assert_eq!(f.flags, case.flags, "{label}: {f:?}");
        for feature in case.features {
            assert!(has(&f, feature), "{label}: {feature} missing: {f:?}");
        }
        assert_eq!(f.bytes, case.command.len() as u64);
    }
}

/// No feature holds a positional argument: every listed data word is
/// absent from tools, flags and shell features (first_tool and subcommand
/// are the only word positions exposed).
#[test]
fn pl_u10_no_positional_argv() {
    for case in CASES {
        let f = of(case.command);
        for word in case.positional {
            assert!(
                !f.tools.iter().any(|t| t == word)
                    && !f.flags.iter().any(|t| t == word)
                    && !f.shell_features.contains(word),
                "{}: positional {word} exposed: {f:?}",
                case.command
            );
        }
        let mut set = f.shell_features.clone();
        set.sort();
        set.dedup();
        assert_eq!(set, f.shell_features, "sorted and distinct");
    }
}

#[test]
fn pl_u10_quoting_hides_features() {
    // Single quotes and escapes hide every operator and expansion.
    let f = of(r"echo '$HOME | rm *' \$x \| \*");
    assert_eq!(f.parse, Parse::Simple, "{f:?}");
    assert!(f.shell_features.is_empty(), "{f:?}");
    // Double quotes hide operators and globs, not expansions.
    let f = of(r#"echo "a | b > c *" "$(id)""#);
    assert!(!has(&f, "pipe") && !has(&f, "redirect_out") && !has(&f, "glob"));
    assert!(has(&f, "command_substitution") && f.tools.contains(&"id".to_owned()));
}

#[test]
fn pl_u10_segment_bound() {
    let at_bound = vec!["true"; 64].join("; ");
    assert_eq!(of(&at_bound).parse, Parse::Compound);
    let over = vec!["true"; 65].join("; ");
    assert_eq!(of(&over).parse, Parse::Unparsed);
}

#[test]
fn pl_u10_depth_bound() {
    let nest = |n: usize| {
        let mut s = "true".to_owned();
        for _ in 0..n {
            s = format!("echo $({s})");
        }
        s
    };
    let eight = of(&nest(8));
    assert_eq!(eight.parse, Parse::Compound, "{eight:?}");
    assert!(eight.tools.contains(&"true".to_owned()));
    assert_eq!(of(&nest(9)).parse, Parse::Unparsed);
}

#[test]
fn pl_u10_set_and_word_bounds() {
    let flags: Vec<String> = (0..33).map(|i| format!("--f{i}")).collect();
    let f = of(&format!("tool {}", flags.join(" ")));
    assert_eq!(f.parse, Parse::Unparsed);
    assert_eq!(f.flags.len(), MAX_INTENT_SET);
    assert_eq!(f.flags[..], flags[..32]);
    let at_bound: Vec<String> = (0..32).map(|i| format!("--f{i}")).collect();
    assert_eq!(
        of(&format!("tool {}", at_bound.join(" "))).parse,
        Parse::Simple
    );
    let long = "t".repeat(MAX_INTENT_WORD + 1);
    let f = of(&format!("{long} x; ls"));
    assert_eq!(f.parse, Parse::Unparsed);
    assert_eq!(f.tools, ["ls"]);
    assert_eq!(f.first_tool, "");
    let exact = "t".repeat(MAX_INTENT_WORD);
    assert_eq!(of(&exact).tools, std::slice::from_ref(&exact));
    // 33 distinct tools: unparsed, the first 32 kept.
    let tools: Vec<String> = (0..33).map(|i| format!("t{i}")).collect();
    let f = of(&tools.join("; "));
    assert_eq!(f.parse, Parse::Unparsed);
    assert_eq!(f.tools[..], tools[..32]);
}

#[test]
fn pl_u10_plan_facts_and_whole_command() {
    let plan = PlanFacts {
        secret_vars: 2,
        secret_reference: true,
        whole_command: false,
    };
    let f = intent(r#"curl -H "$ASURA_SECRET_1" x"#, plan);
    assert_eq!(f.parse, Parse::Simple);
    assert!(f.secret_vars == 2 && f.secret_reference && !f.whole_command);
    let whole = PlanFacts {
        secret_vars: 1,
        secret_reference: false,
        whole_command: true,
    };
    let f = intent(r#"eval "$ASURA_SECRET_COMMAND""#, whole);
    assert_eq!(f.parse, Parse::Unparsed);
    assert!(f.whole_command);
    // The stored form's command word is the variable itself.
    assert_eq!(f.tools, ["eval", "$ASURA_SECRET_COMMAND"]);
}

#[test]
fn pl_u10_comments_keywords_and_wrappers_repeat() {
    let f = of("# only a comment");
    assert_eq!(f.parse, Parse::Simple);
    assert!(f.tools.is_empty() && f.first_tool.is_empty());
    let f = of("sudo env PATH=/x nohup command git push");
    assert_eq!(f.tools, ["sudo", "env", "nohup", "command", "git"]);
    let f = of("find . -name x -exec rm {} \\;");
    assert!(f.tools.contains(&"rm".to_owned()), "{f:?}");
    assert_eq!(f.parse, Parse::Compound);
    let f = of("for x in a b; do rm $x; done");
    assert!(f.tools.contains(&"rm".to_owned()), "{f:?}");
    assert!(!f.tools.iter().any(|t| t == "a" || t == "b"));
}

#[test]
fn parse_names() {
    assert_eq!(Parse::Simple.as_str(), "simple");
    assert_eq!(Parse::Compound.as_str(), "compound");
    assert_eq!(Parse::Unparsed.as_str(), "unparsed");
}
