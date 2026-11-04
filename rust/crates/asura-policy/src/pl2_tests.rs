//! PL2 cases on the product schema and the built-in sets
//! (security-policy.md, PL2 validation map): person decisions (PL-I15,
//! engine part), command intent features (PL-U10, policy part), named
//! secrets, the AD-7 load check, domains and the new request bounds.
use super::*;

/// 2026-11-04T12:00:00Z.
const NOW: i64 = 1_793_793_600_000;
const JOB: &str = "00112233445566778899aabbccddeeff";
const HOME: &str = "/Users/person/.asura";
const ROOT: &str = "/Users/person/src/project";

fn builtin() -> Engine {
    Engine::build(&RevisionTexts::builtin()).expect("built-in sets load")
}
fn person(outside: bool) -> JobPrincipal {
    person_principal(
        JOB,
        "p0",
        ROOT,
        "c0:0",
        HOME,
        outside,
        vec!["RUST_LOG".into()],
    )
}
fn sh() -> ProgramRef {
    ProgramRef {
        path: PathRef::ordinary("/bin/sh"),
        identity_known: true,
        platform: true,
        team_id: String::new(),
        signing_id: "com.apple.sh".into(),
        cdhash: "43".repeat(20),
    }
}
fn command(first: &str, sub: &str, flags: &[&str]) -> Command {
    Command {
        parse: Parse::Simple,
        tools: vec![first.into()],
        first_tool: first.into(),
        subcommand: sub.into(),
        flags: flags.iter().map(|f| (*f).to_owned()).collect(),
        shell_features: Vec::new(),
        bytes: 20,
        secret_vars: 0,
        secret_reference: false,
        whole_command: false,
    }
}
fn intent(source: IntentSource, command: Option<Command>) -> Intent {
    Intent {
        source,
        name: "exec_cmd".into(),
        digest: "ab".repeat(32),
        command,
    }
}
fn facts(terminal: &[&str], declared: &[&str], switch: bool, secrets: &[&str]) -> PersonEntry {
    PersonEntry {
        env_terminal: terminal.iter().map(|s| (*s).to_owned()).collect(),
        env_declared: declared.iter().map(|s| (*s).to_owned()).collect(),
        outside_switch: switch,
        secrets: secrets.iter().map(|s| (*s).to_owned()).collect(),
    }
}
fn with(scope: Scope, extra: &str) -> RevisionTexts {
    let mut texts = RevisionTexts::builtin();
    let text = texts
        .scopes
        .iter_mut()
        .find(|s| s.scope == scope)
        .expect("scope");
    text.text.push_str(extra);
    texts
}
fn reason(decision: &Decision) -> (&str, Option<&str>) {
    decision
        .primary
        .as_ref()
        .map_or(("", None), |p| (p.reason.as_str(), p.rule.as_deref()))
}
fn person_intent() -> Intent {
    Intent {
        name: "!".into(),
        ..intent(
            IntentSource::PersonCommand,
            Some(command("cargo", "build", &[])),
        )
    }
}

/// PL-I15 (engine): a `!` command with listed terminal names and the
/// project's declared names runs in both scopes; an unlisted terminal name
/// and an undeclared name are each denied `env_not_allowed` by their hard
/// forbid, with the installation `@explain` text.
#[test]
fn pl_i15_person_entry_and_the_environment_forbids() {
    let engine = builtin();
    let ok = decide_person_job(
        &engine,
        &person(false),
        &sh(),
        &facts(
            &["LANG", "SSH_AUTH_SOCK", "GEM_PATH"],
            &["RUST_LOG"],
            false,
            &[],
        ),
        Some(&person_intent()),
        NOW,
    );
    assert!(ok.decision.allow, "{:?}", ok.decision);
    assert_eq!(ok.action, Action::ProcessExec);
    assert_eq!(ok.decision.scopes.len(), 2);
    for (terminal, declared, rule) in [
        (
            &["LANG", "PATH"][..],
            &[][..],
            "installation:person.env_terminal",
        ),
        (&["HOME"][..], &[][..], "installation:person.env_terminal"),
        (
            &[][..],
            &["UNDECLARED"][..],
            "installation:person.env_declared",
        ),
    ] {
        let denied = decide_person_job(
            &engine,
            &person(false),
            &sh(),
            &facts(terminal, declared, true, &[]),
            Some(&person_intent()),
            NOW,
        );
        assert!(!denied.decision.allow);
        assert_eq!(reason(&denied.decision), ("env_not_allowed", Some(rule)));
        let primary = denied.decision.primary.unwrap();
        assert_eq!(primary.scope, Some(Scope::Installation));
        assert!(primary.explain.is_some());
    }
}

/// PL-I15 (engine): the outside switch denies only `!!`.
#[test]
fn pl_i15_outside_switch() {
    let engine = builtin();
    let decide = |outside, switch| {
        decide_person_job(
            &engine,
            &person(outside),
            &sh(),
            &facts(&[], &[], switch, &[]),
            Some(&person_intent()),
            NOW,
        )
        .decision
    };
    assert!(decide(true, true).allow);
    assert!(decide(false, false).allow);
    let off = decide(true, false);
    assert_eq!(
        reason(&off),
        (
            "outside_sandbox_off",
            Some("installation:person.outside_switch")
        )
    );
}

/// PL-I15 (engine): named secrets give `secret_use` with delivery `raw`:
/// allowed for a person in both scopes; a model principal is denied by
/// the installation hard forbid S4; brokered delivery has no permit; a
/// user forbid of one name makes that `secret_use` the deciding request.
#[test]
fn pl_i15_named_secret_decisions() {
    let engine = builtin();
    let allowed = decide_person_job(
        &engine,
        &person(false),
        &sh(),
        &facts(&[], &[], false, &["GITHUB_TOKEN", "NPM_TOKEN"]),
        Some(&person_intent()),
        NOW,
    );
    assert!(allowed.decision.allow && allowed.action == Action::ProcessExec);
    let secret = |principal: JobPrincipal, delivery| Request {
        principal,
        action: Action::SecretUse,
        resource: Resource::Secret("GITHUB_TOKEN".into()),
        context: Context::Secret {
            now_ms: NOW,
            delivery,
            intent: None,
        },
    };
    let model = model1_principal(JOB, "p0", ROOT, "c0:0", HOME);
    let d = engine.decide(&secret(model, Delivery::Raw));
    assert_eq!(
        reason(&d),
        ("model_secret", Some("installation:s4.model_secret"))
    );
    let d = engine.decide(&secret(person(false), Delivery::Brokered));
    assert_eq!(reason(&d), ("no_permit", None));
    let d = engine.decide(&secret(person(false), Delivery::Raw));
    assert!(d.allow);
    // A user forbid of one name: the secret decides and names its rule.
    let engine = Engine::build(&with(
        Scope::User,
        "\n@id(\"no-npm\") @reason(\"secret_forbidden\")\nforbid (principal, action == Asura::Action::\"secret_use\", resource == Asura::Secret::\"NPM_TOKEN\");\n",
    ))
    .unwrap();
    let denied = decide_person_job(
        &engine,
        &person(false),
        &sh(),
        &facts(&[], &[], false, &["GITHUB_TOKEN", "NPM_TOKEN"]),
        Some(&person_intent()),
        NOW,
    );
    assert!(!denied.decision.allow);
    assert_eq!(denied.action, Action::SecretUse);
    assert_ne!(denied.resource_digest, allowed.resource_digest);
    assert_eq!(
        reason(&denied.decision),
        ("secret_forbidden", Some("user:no-npm"))
    );
    assert_eq!(denied.decision.primary.unwrap().scope, Some(Scope::User));
    // An invalid secret name is a request error, never an allow.
    let mut bad = secret(person(false), Delivery::Raw);
    bad.resource = Resource::Secret("1BAD-NAME".into());
    assert_eq!(reason(&builtin().decide(&bad)).0, "request_invalid");
}

/// AD-7 load check: a revision that forbids person network fails to load;
/// a model network forbid loads.
#[test]
fn ad7_load_check_refuses_a_person_network_forbid() {
    for (scope, text) in [
        (
            Scope::User,
            "forbid (principal, action == Asura::Action::\"network.connect\", resource) when { principal.origin == Asura::Origin::\"person_typed\" };",
        ),
        (
            Scope::Installation,
            "forbid (principal is Asura::Job, action, resource) when { principal.profile == Asura::Profile::\"person-outside-1\" };",
        ),
    ] {
        let extra = format!("\n@id(\"no-net\") @reason(\"no_net\")\n{text}\n");
        let error = match Engine::build(&with(scope, &extra)) {
            Ok(_) => panic!("loaded a person network forbid"),
            Err(error) => error,
        };
        assert_eq!(error.scope, Some(scope));
        assert!(error.messages[0].contains("AD-7"), "{error:?}");
    }
    let model = "\n@id(\"no-net\") @reason(\"no_net\")\nforbid (principal, action == Asura::Action::\"network.connect\", resource) when { principal.origin == Asura::Origin::\"model\" };\n";
    assert!(Engine::build(&with(Scope::User, model)).is_ok());
}

/// PL-U10 (policy part): a tighten-only forbid reading the command
/// features denies the matching entry only; it changes no region; the
/// intent digest covers the features.
#[test]
fn pl_u10_intent_features_tighten_the_entry_only() {
    let forbid = "\n@id(\"no-force-push\") @reason(\"force_push\") @intent(\"true\")\nforbid (principal, action == Asura::Action::\"process.exec\", resource)\nwhen { context has intent && context.intent has command && context.intent.command.tools.contains(\"git\") && context.intent.command.subcommand == \"push\" && context.intent.command.flags.contains(\"--force\") };\n";
    let engine = Engine::build(&with(Scope::User, forbid)).unwrap();
    let principal = model1_principal(JOB, "p0", ROOT, "c0:0", HOME);
    let push = intent(
        IntentSource::ModelTool,
        Some(command("git", "push", &["--force", "-u"])),
    );
    let plain = intent(
        IntentSource::ModelTool,
        Some(command("git", "push", &["-u"])),
    );
    let denied = decide_model1_job(&engine, &principal, &sh(), Some(&push), NOW, None);
    assert!(!denied.decision.allow && denied.regions.is_none());
    assert_eq!(
        reason(&denied.decision),
        ("force_push", Some("user:no-force-push"))
    );
    let ok = decide_model1_job(&engine, &principal, &sh(), Some(&plain), NOW, None);
    assert!(ok.decision.allow);
    assert_eq!(ok.regions, Some(Regions::template()));
    assert_eq!(
        compile_model1(&engine, &principal, NOW).unwrap(),
        compile_model1(&builtin(), &principal, NOW).unwrap()
    );
    assert_ne!(denied.intent_digest, ok.intent_digest);
    // The same features give the same digest.
    let again = decide_model1_job(&engine, &principal, &sh(), Some(&plain), NOW, ok.regions);
    assert_eq!(again.intent_digest, ok.intent_digest);
    // The built-in sets ignore the features: both run.
    let builtin = builtin();
    assert!(
        decide_model1_job(&builtin, &principal, &sh(), Some(&push), NOW, None)
            .decision
            .allow
    );
}

/// The test-only person forbid denies the sentinel first tool only.
#[cfg(feature = "test-support")]
#[test]
fn test_deny_person_sentinel() {
    let engine = Engine::build(&RevisionTexts::builtin_test_deny()).unwrap();
    let decide = |first: &str| {
        let intent = Intent {
            name: "!".into(),
            ..intent(IntentSource::PersonCommand, Some(command(first, "", &[])))
        };
        decide_person_job(
            &engine,
            &person(false),
            &sh(),
            &facts(&[], &[], false, &[]),
            Some(&intent),
            NOW,
        )
        .decision
    };
    assert!(decide("printf").allow);
    assert_eq!(
        reason(&decide("asura-policy-test-deny")),
        ("test_denied", Some("installation:test_deny_person_exec"))
    );
}

/// W1 for the PL2 request parts: command sets, declared names, a domain.
#[test]
fn pl2_request_bounds() {
    let engine = builtin();
    let principal = model1_principal(JOB, "p0", ROOT, "c0:0", HOME);
    let mut big = command("git", "", &[]);
    big.tools = (0..65).map(|i| format!("t{i}")).collect();
    let d = decide_model1_job(
        &engine,
        &principal,
        &sh(),
        Some(&intent(IntentSource::ModelTool, Some(big))),
        NOW,
        Some(Regions::template()),
    );
    assert_eq!(reason(&d.decision).0, "request_too_large");
    let mut p = person(false);
    p.declared_env = (0..65).map(|i| format!("V{i}")).collect();
    let d = decide_person_job(&engine, &p, &sh(), &facts(&[], &[], false, &[]), None, NOW);
    assert_eq!(reason(&d.decision).0, "request_too_large");
    let connect = |name: &str| Request {
        principal: person(false),
        action: Action::NetworkConnect,
        resource: Resource::Domain(name.into()),
        context: Context::Connect {
            now_ms: NOW,
            ip: "192.0.2.7".into(),
            port: 443,
            via_proxy: false,
            intent: None,
        },
    };
    assert!(engine.decide(&connect("api.github.com")).allow);
    for bad in ["API.github.com", "-a.com", "a..com", ""] {
        assert_eq!(reason(&engine.decide(&connect(bad))).0, "request_invalid");
    }
}

/// Domain parents are label suffixes: a forbid on `github.com` covers
/// `api.github.com` and not `github.community`.
#[test]
fn domain_hierarchy() {
    let engine = Engine::build(&with(
        Scope::User,
        "\n@id(\"no-gh\") @reason(\"host_forbidden\")\nforbid (principal, action == Asura::Action::\"network.connect\", resource) when { principal.origin == Asura::Origin::\"model\" && resource in Asura::Domain::\"github.com\" };\n",
    ))
    .unwrap();
    let connect = |name: &str| Request {
        principal: model1_principal(JOB, "p0", ROOT, "c0:0", HOME),
        action: Action::NetworkConnect,
        resource: Resource::Domain(name.into()),
        context: Context::Connect {
            now_ms: NOW,
            ip: "192.0.2.7".into(),
            port: 443,
            via_proxy: true,
            intent: None,
        },
    };
    let d = engine.decide(&connect("api.github.com"));
    assert!(d.scopes.iter().any(|s| s.rules == ["user:no-gh"]), "{d:?}");
    let d = engine.decide(&connect("github.community"));
    assert!(
        d.scopes
            .iter()
            .all(|s| !s.rules.contains(&"user:no-gh".to_owned()))
    );
}

/// An identified entry program decides like the PL1 unidentified one
/// under the built-in sets, and its identity is in the resource digest.
#[test]
fn identified_entry_runs() {
    let engine = builtin();
    let principal = model1_principal(JOB, "p0", ROOT, "c0:0", HOME);
    let known = decide_model1_job(&engine, &principal, &sh(), None, NOW, None);
    let unknown = decide_model1_job(
        &engine,
        &principal,
        &ProgramRef::unidentified(PathRef::ordinary("/bin/sh")),
        None,
        NOW,
        None,
    );
    assert!(known.decision.allow && unknown.decision.allow);
    assert_ne!(known.resource_digest, unknown.resource_digest);
}
