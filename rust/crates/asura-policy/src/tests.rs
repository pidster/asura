//! PL-U1 to PL-U8 on the product schema and the built-in sets
//! (security-policy.md, PL1 validation map).
use super::*;
use crate::engine::slice_count;
use serde_json::json;

/// 2026-11-03T12:00:00Z.
const NOW: i64 = 1_793_707_200_000;
const JOB: &str = "00112233445566778899aabbccddeeff";
const HOME: &str = "/Users/person/.asura";
const ROOT: &str = "/Users/person/src/project";

fn builtin() -> Engine {
    Engine::build(&RevisionTexts::builtin()).expect("built-in sets load")
}
fn principal() -> JobPrincipal {
    model1_principal(JOB, "p0", ROOT, "c0:0", HOME)
}
fn intent() -> Intent {
    Intent {
        source: IntentSource::ModelTool,
        name: "exec_cmd".into(),
        digest: "ab".repeat(32),
    }
}
fn entry(principal: JobPrincipal, intent: Option<Intent>) -> Request {
    Request {
        principal,
        action: Action::ProcessExec,
        resource: Resource::Program(ProgramRef::unidentified(PathRef::ordinary("/bin/sh"))),
        context: Context::Exec {
            now_ms: NOW,
            request_digest: "00".repeat(32),
            env_terminal: Vec::new(),
            env_declared: Vec::new(),
            outside_switch: false,
            intent,
        },
    }
}
fn read(path: &str) -> Request {
    Request {
        principal: principal(),
        action: Action::FileRead,
        resource: Resource::Path(PathRef::ordinary(path)),
        context: Context::File {
            now_ms: NOW,
            intent: None,
        },
    }
}
fn reason(decision: &Decision) -> &str {
    decision.primary.as_ref().map_or("", |p| p.reason.as_str())
}
/// The built-in texts with `extra` appended to one scope.
fn with(scope: Scope, extra: &str) -> RevisionTexts {
    let mut texts = RevisionTexts::builtin();
    let text = texts
        .scopes
        .iter_mut()
        .find(|s| s.scope == scope)
        .expect("scope");
    text.text = format!("{}\n{extra}", text.text);
    texts
}
/// The built-in texts with the user scope replaced.
fn user(text: &str) -> RevisionTexts {
    let mut texts = RevisionTexts::builtin();
    texts.scopes[1].text = text.to_owned();
    texts
}

#[test]
fn builtin_allows_the_model_entry_and_engine_is_send() {
    fn send<T: Send>() {}
    send::<Engine>();
    let engine = builtin();
    assert_eq!(engine.scopes(), [Scope::Installation, Scope::User]);
    let decision = engine.decide(&entry(principal(), Some(intent())));
    assert!(decision.allow, "{decision:?}");
    assert_eq!(decision.scopes.len(), 2);
    assert!(decision.primary.is_none());
    assert_eq!(
        decision.scopes[0].rules,
        ["installation:model1.exec".to_owned()]
    );
}

// ----- PL-U1: W1 bounds, each limit and one more. -----

#[test]
fn pl_u1_string_limit() {
    let engine = builtin();
    let mut i = intent();
    i.name = "n".repeat(MAX_STRING_BYTES);
    assert!(engine.decide(&entry(principal(), Some(i.clone()))).allow);
    i.name.push('n');
    assert_eq!(
        reason(&engine.decide(&entry(principal(), Some(i)))),
        "request_too_large"
    );
}
#[test]
fn pl_u1_set_limit() {
    let engine = builtin();
    let mut request = entry(principal(), None);
    let set = |n: usize| (0..n).map(|i| format!("V{i}")).collect::<Vec<_>>();
    if let Context::Exec { env_terminal, .. } = &mut request.context {
        *env_terminal = set(MAX_SET_ELEMENTS);
    }
    assert!(engine.decide(&request).allow);
    if let Context::Exec { env_terminal, .. } = &mut request.context {
        *env_terminal = set(MAX_SET_ELEMENTS + 1);
    }
    assert_eq!(reason(&engine.decide(&request)), "request_too_large");
    let mut p = principal();
    p.cache_roots = (0..=MAX_SET_ELEMENTS)
        .map(|i| PathRef::ordinary(&format!("/c{i}")))
        .collect();
    assert_eq!(reason(&engine.decide(&entry(p, None))), "request_too_large");
}
#[test]
fn pl_u1_entity_limit() {
    let engine = builtin();
    let base = slice_count(&entry(principal(), None)).unwrap();
    // Each `/c<i>/d` cache root adds two path entities; `/z` adds one.
    let pairs = (MAX_ENTITIES - base) / 2;
    let mut p = principal();
    p.cache_roots = (0..pairs)
        .map(|i| PathRef::ordinary(&format!("/c{i}/d")))
        .collect();
    if (MAX_ENTITIES - base) % 2 == 1 {
        p.cache_roots.push(PathRef::ordinary("/z"));
    }
    let at = entry(p.clone(), None);
    assert_eq!(slice_count(&at).unwrap(), MAX_ENTITIES);
    assert_ne!(reason(&engine.decide(&at)), "request_too_large");
    p.cache_roots.push(PathRef::ordinary("/y"));
    let over = entry(p, None);
    assert_eq!(slice_count(&over), Err(ErrorClass::RequestTooLarge));
    assert_eq!(reason(&engine.decide(&over)), "request_too_large");
}
#[test]
fn pl_u1_ancestor_limit() {
    let engine = builtin();
    let deep = |n: usize| "/a".repeat(n);
    let at = engine.decide(&read(&deep(MAX_ANCESTORS)));
    assert_eq!(reason(&at), "no_permit");
    let over = engine.decide(&read(&deep(MAX_ANCESTORS + 1)));
    assert_eq!(reason(&over), "request_too_large");
}
#[test]
fn pl_u1_request_bytes_limit() {
    let engine = builtin();
    let set = |n: usize| (0..n).map(|i| format!("{i:0>200}")).collect::<Vec<_>>();
    let mut request = entry(principal(), None);
    if let Context::Exec { env_terminal, .. } = &mut request.context {
        *env_terminal = set(40);
    }
    assert!(engine.decide(&request).allow);
    if let Context::Exec {
        env_terminal,
        env_declared,
        ..
    } = &mut request.context
    {
        *env_terminal = set(MAX_SET_ELEMENTS);
        *env_declared = set(MAX_SET_ELEMENTS);
    }
    assert_eq!(reason(&engine.decide(&request)), "request_too_large");
}

// ----- PL-U2: W2 schema checks. -----

#[test]
fn pl_u2_schema_refusals() {
    let engine = builtin();
    let request = entry(principal(), None);
    let base = json!({
        "now": {"__extn": {"fn": "datetime", "arg": "2026-11-03T12:00:00Z"}},
        "request_digest": "x", "env_terminal": [], "env_declared": [], "outside_switch": false,
    });
    assert!(engine.decide_with_context(&request, base.clone()).allow);
    let mut unknown = base.clone();
    unknown["port"] = json!(22);
    assert_eq!(
        reason(&engine.decide_with_context(&request, unknown)),
        "request_invalid"
    );
    let mut wrong = base.clone();
    wrong["outside_switch"] = json!("false");
    assert_eq!(
        reason(&engine.decide_with_context(&request, wrong)),
        "request_invalid"
    );
    let mut entity = base;
    entity["intent"] = json!({"source": {"__entity": {"type": "Asura::Nope", "id": "x"}},
        "name": "n", "digest": "d"});
    assert_eq!(
        reason(&engine.decide_with_context(&request, entity)),
        "request_invalid"
    );
    // A file context on an exec action, and a path resource on it.
    let mut file_context = request.clone();
    file_context.context = Context::File {
        now_ms: NOW,
        intent: None,
    };
    assert_eq!(reason(&engine.decide(&file_context)), "request_invalid");
    let mut path_resource = request.clone();
    path_resource.resource = Resource::Path(PathRef::ordinary("/bin/sh"));
    assert_eq!(reason(&engine.decide(&path_resource)), "request_invalid");
    // Non-canonical paths and conflicting attributes.
    for bad in ["bin/sh", "/bin//sh", "/bin/./sh", "/bin/../sh", "/bin/", ""] {
        assert_eq!(
            reason(&engine.decide(&read(bad))),
            "request_invalid",
            "{bad}"
        );
    }
    let mut conflict = read(ROOT);
    conflict.resource = Resource::Path(PathRef::ordinary(ROOT));
    assert_eq!(reason(&engine.decide(&conflict)), "request_invalid");
    // Every request error is recorded in every scope with no rule.
    let d = engine.decide(&read("relative"));
    assert!(!d.allow && d.scopes.len() == 2);
    assert!(
        d.scopes
            .iter()
            .all(|s| s.error == Some(ErrorClass::RequestInvalid))
    );
    assert_eq!(d.primary.unwrap().scope, None);
}

// ----- PL-U3: W3 on the FE shapes. -----

#[test]
fn pl_u3_errors_deny_where_cedar_alone_allows() {
    let shapes = [
        (
            "fe1",
            "Asura::Project::\"absent\".root in Asura::Path::\"/\"",
        ),
        (
            "fe2",
            "(9223372036854775807 + (if context.outside_switch then 2 else 1)) > 0",
        ),
        ("fe3", "Asura::Path::\"/absent\".rel == \"x\""),
        (
            "fe4",
            "context.now.offset(duration(\"9223372036854775807ms\")) > context.now",
        ),
    ];
    for (id, condition) in shapes {
        let texts = with(
            Scope::User,
            &format!(
                "@id(\"{id}\") @reason(\"{id}\")\nforbid (principal is Asura::Job, action == Asura::Action::\"process.exec\", resource is Asura::Program)\nwhen {{ {condition} }};"
            ),
        );
        validate_text(Scope::User, &texts.scopes[1].text).expect("the shape validates");
        let engine = Engine::build(&texts).expect(id);
        let request = entry(principal(), None);
        assert!(
            engine.raw_allows(Scope::User, &request),
            "{id}: Cedar alone"
        );
        let d = engine.decide(&request);
        assert!(!d.allow, "{id}");
        let primary = d.primary.unwrap();
        assert_eq!(primary.scope, Some(Scope::User), "{id}");
        assert_eq!(primary.reason, "policy_error", "{id}");
        assert_eq!(primary.rule.as_deref(), Some(format!("user:{id}").as_str()));
        assert_eq!(d.scopes[1].error, Some(ErrorClass::PolicyError));
    }
}

// ----- PL-U4: strict validation, warnings and annotations refuse. -----

#[test]
fn pl_u4_seeded_defects_are_refused() {
    let f = |action: &str, resource: &str, when: &str| {
        format!(
            "@id(\"defect\") @reason(\"defect\")\nforbid (principal is Asura::Job, action == Asura::Action::\"{action}\", resource{resource})\nwhen {{ {when} }};"
        )
    };
    let defects = [
        ("SD1 attribute typo", f("file.write", " is Asura::Path", "resource.clas == Asura::PathClass::\"implicit_exec\"")),
        ("SD2 enumeration literal typo", f("file.write", " is Asura::Path", "resource.class == Asura::PathClass::\"implicit-exec\"")),
        ("SD3 entity type typo", "@id(\"defect\") @reason(\"defect\")\nforbid (principal is Asura::Jobb, action == Asura::Action::\"file.write\", resource);".to_owned()),
        ("SD4 action typo", "@id(\"defect\") @reason(\"defect\")\nforbid (principal, action == Asura::Action::\"file.wrtie\", resource);".to_owned()),
        ("SD5 number and string", f("network.connect", "", "context.port == \"443\"")),
        ("SD6 context of another action", f("file.write", " is Asura::Path", "context.port == 22")),
        ("SD7 impossible comparison", f("file.write", " is Asura::Path", "resource == Asura::Domain::\"github.com\"")),
        ("SD8 misspelt extension method", f("network.connect", "", "context.ip.isInRnage(ip(\"10.0.0.0/8\"))")),
        ("SD9 set element type", f("process.exec", " is Asura::Program", "context.env_terminal.contains(1)")),
        ("SD10 entity data path typo", f("process.exec", " is Asura::Program", "principal.projetc.root == principal.scratch")),
        ("SD11 Boolean and string", f("network.connect", "", "context.via_proxy == \"true\"")),
        ("SD12 undefined method", f("process.exec", " is Asura::Program", "resource.path.rel.lowercase() == \"git\"")),
    ];
    for (name, text) in &defects {
        assert!(validate_text(Scope::User, text).is_err(), "{name}");
        assert!(Engine::build(&with(Scope::User, text)).is_err(), "{name}");
    }
    assert_eq!(defects.len(), 12);
    // Annotation and shape refusals.
    let ok = "@id(\"x\") @reason(\"r\")\nforbid (principal, action, resource);";
    assert!(validate_text(Scope::User, ok).is_ok());
    for bad in [
        "forbid (principal, action, resource);",
        "@id(\"X\") @reason(\"r\")\nforbid (principal, action, resource);",
        "@id(\"x\")\nforbid (principal, action, resource);",
        "@id(\"x\") @reason(\"R-1\")\nforbid (principal, action, resource);",
        "@id(\"x\") @reason(\"r\") @hard(\"true\")\nforbid (principal, action, resource);",
        "@id(\"x\") @reason(\"r\") @grant(\"g\")\nforbid (principal, action, resource);",
        "@id(\"x\") @reason(\"r\") @color(\"red\")\nforbid (principal, action, resource);",
        "@id(\"x\") @reason(\"r\")\nforbid (principal, action, resource);\n@id(\"x\") @reason(\"r\")\nforbid (principal, action, resource);",
        "@id(\"x\") @reason(\"r\")\npermit (principal == ?principal, action, resource);",
    ] {
        assert!(validate_text(Scope::User, bad).is_err(), "{bad}");
    }
    let long = format!(
        "@id(\"x\") @reason(\"r\") @explain(\"{}\")\nforbid (principal, action, resource);",
        "e".repeat(201)
    );
    assert!(validate_text(Scope::User, &long).is_err());
    assert!(validate_text(Scope::User, &" ".repeat(MAX_SCOPE_TEXT_BYTES + 1)).is_err());
    // @hard belongs to the installation scope only.
    let hard = "@id(\"x\") @reason(\"r\") @hard(\"true\")\nforbid (principal, action, resource);";
    assert!(validate_text(Scope::Installation, hard).is_ok());
    // A lower scope holds forbids only; its baseline permit is added.
    let mut texts = RevisionTexts::builtin();
    texts.scopes.push(ScopeText {
        scope: Scope::Workspace,
        source: "test",
        text: "@id(\"p\") @reason(\"r\")\npermit (principal, action, resource);".into(),
    });
    assert!(Engine::build(&texts).is_err());
    texts.scopes[2].text = ok.into();
    let engine = Engine::build(&texts).unwrap();
    assert_eq!(
        engine.policy_ids(Scope::Workspace),
        ["workspace:baseline".to_owned(), "workspace:x".to_owned()]
    );
}

// ----- PL-U5: pattern A. -----

#[test]
fn pl_u5_scope_combinations() {
    let forbid = |id: &str| {
        format!(
            "@id(\"{id}\") @reason(\"{id}_reason\")\nforbid (principal is Asura::Job, action == Asura::Action::\"process.exec\", resource);"
        )
    };
    let request = entry(principal(), None);
    // Both allow.
    assert!(builtin().decide(&request).allow);
    // Only the user scope denies.
    let d = Engine::build(&with(Scope::User, &forbid("u")))
        .unwrap()
        .decide(&request);
    assert!(!d.allow && d.scopes[0].allow && !d.scopes[1].allow);
    assert_eq!(d.primary.as_ref().unwrap().scope, Some(Scope::User));
    assert_eq!(reason(&d), "u_reason");
    // Both deny: the installation scope is primary; both are recorded.
    let mut texts = with(Scope::User, &forbid("u"));
    texts.scopes[0].text = format!("{}\n{}", texts.scopes[0].text, forbid("i"));
    let d = Engine::build(&texts).unwrap().decide(&request);
    assert_eq!(d.primary.as_ref().unwrap().scope, Some(Scope::Installation));
    assert_eq!(reason(&d), "i_reason");
    assert_eq!(d.scopes[1].rules, ["user:u".to_owned()]);
    // A user scope with no permit: no_permit in the user scope.
    let d = Engine::build(&user("")).unwrap().decide(&request);
    assert_eq!(d.primary.as_ref().unwrap().scope, Some(Scope::User));
    assert_eq!(reason(&d), "no_permit");
    // Without a user text only the installation scope is evaluated, and
    // absent lower scopes never appear.
    let mut only = RevisionTexts::builtin();
    only.scopes.truncate(1);
    let d = Engine::build(&only).unwrap().decide(&request);
    assert!(d.allow);
    assert_eq!(d.scopes.len(), 1);
    // The first reason of a scope is ordered by rule ID.
    let d = Engine::build(&with(
        Scope::User,
        &format!("{}\n{}", forbid("b"), forbid("a")),
    ))
    .unwrap()
    .decide(&request);
    assert_eq!(
        d.scopes[1].rules,
        ["user:a".to_owned(), "user:b".to_owned()]
    );
    assert_eq!(reason(&d), "a_reason");
}

// ----- PL-U6: reasons, rule IDs and explanations. -----

#[test]
fn pl_u6_reason_mapping() {
    let request = entry(principal(), None);
    let explained = "@id(\"e\") @reason(\"explained\") @explain(\"a person-facing text\")\nforbid (principal is Asura::Job, action == Asura::Action::\"process.exec\", resource);";
    let d = Engine::build(&with(Scope::User, explained))
        .unwrap()
        .decide(&request);
    let p = d.primary.unwrap();
    assert_eq!(
        (
            p.scope,
            p.reason.as_str(),
            p.rule.as_deref(),
            p.explain.as_deref()
        ),
        (
            Some(Scope::User),
            "explained",
            Some("user:e"),
            Some("a person-facing text")
        )
    );
    // A workspace explanation is never forwarded.
    let mut texts = RevisionTexts::builtin();
    texts.scopes.push(ScopeText {
        scope: Scope::Workspace,
        source: "test",
        text: explained.into(),
    });
    let p = Engine::build(&texts)
        .unwrap()
        .decide(&request)
        .primary
        .unwrap();
    assert_eq!(p.scope, Some(Scope::Workspace));
    assert_eq!(p.rule.as_deref(), Some("workspace:e"));
    assert_eq!(p.explain, None);
    // no_permit has no rule; no reason holds a path or command text.
    let engine = builtin();
    let p = engine.decide(&read("/etc/passwd")).primary.unwrap();
    assert_eq!((p.reason.as_str(), p.rule.as_deref()), ("no_permit", None));
    let mut state = read(&format!("{HOME}/journal"));
    state.resource = Resource::Path(PathRef {
        path: format!("{HOME}/journal"),
        class: PathClass::AsuraState,
        in_project: false,
        rel: String::new(),
    });
    let d = engine.decide(&state);
    let p = d.primary.unwrap();
    assert_eq!(p.reason, "asura_state");
    assert_eq!(p.rule.as_deref(), Some("installation:s2.asura_state_read"));
    for text in [Some(p.reason), p.rule, p.explain].into_iter().flatten() {
        assert!(!text.contains('/'), "{text}");
    }
}

// ----- PL-U7: the model-1 region compiler. -----

#[test]
fn pl_u7_builtin_regions_are_the_template() {
    let engine = builtin();
    let regions = compile_model1(&engine, &principal(), NOW).unwrap();
    assert_eq!(regions, Regions::template());
    assert_eq!(regions.read.0, 0x1ff);
    assert_eq!(regions.exec.0, 0x1ff);
    assert_eq!(regions.write, RootSet::of(&[Root::Project, Root::Scratch]));
}
#[test]
fn pl_u7_denied_roots_drop_out_and_base_needs_refuse() {
    let deny = |action: &str, resource: &str| {
        format!(
            "@id(\"d\") @reason(\"narrow\")\nforbid (principal is Asura::Job, action == Asura::Action::\"{action}\", resource{resource});"
        )
    };
    // /usr/share read denied: it leaves the read set only.
    let engine = Engine::build(&with(
        Scope::User,
        &deny("file.read", " in Asura::Path::\"/usr/share\""),
    ))
    .unwrap();
    let regions = compile_model1(&engine, &principal(), NOW).unwrap();
    assert!(!regions.read.contains(Root::UsrShare));
    assert!(regions.exec.contains(Root::UsrShare));
    // Project writes denied: the job still compiles without them.
    let engine = Engine::build(&with(
        Scope::User,
        "@id(\"d\") @reason(\"narrow\")\nforbid (principal is Asura::Job, action == Asura::Action::\"file.delete\", resource) when { resource in principal.project.root };",
    ))
    .unwrap();
    let regions = compile_model1(&engine, &principal(), NOW).unwrap();
    assert_eq!(regions.write, RootSet::of(&[Root::Scratch]));
    // A denied runtime literal, and each base need, give cannot_enforce.
    for text in [
        deny("file.read", " == Asura::Path::\"/dev/random\""),
        deny("file.write", " == Asura::Path::\"/dev/null\""),
        deny("file.read", " in Asura::Path::\"/bin\""),
        "@id(\"d\") @reason(\"narrow\")\nforbid (principal is Asura::Job, action == Asura::Action::\"file.write\", resource) when { resource in principal.scratch };".to_owned(),
        "@id(\"d\") @reason(\"narrow\")\nforbid (principal is Asura::Job, action == Asura::Action::\"process.exec\", resource is Asura::Program) when { resource.path in principal.scratch };".to_owned(),
    ] {
        let engine = Engine::build(&with(Scope::User, &text)).unwrap();
        let d = compile_model1(&engine, &principal(), NOW).unwrap_err();
        assert!(!d.allow);
        let p = d.primary.unwrap();
        assert_eq!((p.scope, p.reason.as_str()), (None, "cannot_enforce"), "{text}");
        let outcome = decide_model1_job(&engine, &principal(), None, NOW, None);
        assert_eq!(reason(&outcome.decision), "cannot_enforce");
        assert!(outcome.regions.is_none());
    }
}
#[test]
fn job_decision_digests_and_cache() {
    let engine = builtin();
    let a = decide_model1_job(&engine, &principal(), Some(&intent()), NOW, None);
    assert!(a.decision.allow);
    assert_eq!(a.regions, Some(Regions::template()));
    let cached = decide_model1_job(&engine, &principal(), Some(&intent()), NOW + 5, a.regions);
    assert_eq!(cached.request_digest, a.request_digest);
    assert_eq!(cached.resource_digest, a.resource_digest);
    let mut other = intent();
    other.digest = "cd".repeat(32);
    let b = decide_model1_job(&engine, &principal(), Some(&other), NOW, a.regions);
    assert_ne!(b.request_digest, a.request_digest);
    assert_ne!(b.intent_digest, a.intent_digest);
    assert!(
        decide_model1_job(&engine, &principal(), None, NOW, None)
            .intent_digest
            .is_none()
    );
    // A cached set skips the compiler, never the entry decision.
    let denying = Engine::build(&with(
        Scope::User,
        "@id(\"d\") @reason(\"narrow\")\nforbid (principal is Asura::Job, action == Asura::Action::\"process.exec\", resource) when { resource.path == Asura::Path::\"/bin/sh\" };",
    ))
    .unwrap();
    let d = decide_model1_job(&denying, &principal(), None, NOW, Some(Regions::template()));
    assert!(!d.decision.allow && d.regions.is_none());
    assert_eq!(reason(&d.decision), "narrow");
}

// ----- PL-U8: determinism. -----

#[test]
fn pl_u8_determinism_across_fresh_engines_and_threads() {
    let requests: Vec<Request> = vec![
        entry(principal(), Some(intent())),
        read("/usr/lib/libz.dylib"),
        read("/etc/passwd"),
        read(&format!("{ROOT}/src/main.rs")),
        read("relative"),
    ];
    let run = |requests: &[Request]| -> Vec<Decision> {
        let engine = builtin();
        requests.iter().map(|r| engine.decide(r)).collect()
    };
    let expected = run(&requests);
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let requests = requests.clone();
            let expected = expected.clone();
            std::thread::spawn(move || {
                for _ in 0..13 {
                    assert_eq!(run(&requests), expected);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

// ----- Texts and the test-only set. -----

#[test]
fn revision_digest_names_every_text() {
    let a = RevisionTexts::builtin();
    assert_eq!(a.digest(), RevisionTexts::builtin().digest());
    let b = with(Scope::User, "");
    assert_ne!(a.digest(), b.digest());
    assert_eq!(
        a.text_digest(Scope::Installation),
        b.text_digest(Scope::Installation)
    );
    assert_eq!(a.text_digest(Scope::Workspace), None);
    assert_eq!(schema_digest(), crate::sha256(schema_text().as_bytes()));
    assert!(!cedar_lang().is_empty());
    let engine = builtin();
    assert!(
        engine
            .policy_ids(Scope::Installation)
            .contains(&"installation:s4.model_secret".to_owned())
    );
    assert_eq!(
        engine
            .rule_reason("installation:s1.implicit_exec")
            .map(|r| r.0),
        Some("implicit_exec_write")
    );
}
#[cfg(feature = "test-support")]
#[test]
fn test_deny_set_refuses_the_model_entry_in_the_installation_scope() {
    let texts = RevisionTexts::builtin_test_deny();
    assert_ne!(texts.digest(), RevisionTexts::builtin().digest());
    let engine = Engine::build(&texts).unwrap();
    let outcome = decide_model1_job(&engine, &principal(), Some(&intent()), NOW, None);
    assert!(!outcome.decision.allow && outcome.regions.is_none());
    let p = outcome.decision.primary.unwrap();
    assert_eq!(p.scope, Some(Scope::Installation));
    assert_eq!(p.reason, "test_denied");
    assert_eq!(p.rule.as_deref(), Some("installation:test_deny_model_exec"));
    assert_eq!(
        p.explain.as_deref(),
        Some("the test policy denies model commands")
    );
    // The regions themselves still compile.
    assert_eq!(
        compile_model1(&engine, &principal(), NOW),
        Ok(Regions::template())
    );
}

// ----- Principal entity cache (security-policy.md, principal entity cache) -----

fn principal_n(n: usize) -> JobPrincipal {
    model1_principal(
        &format!("{n:032x}"),
        &format!("p{n}"),
        &format!("/Users/person/src/project-{n}"),
        &format!("c{n}:0"),
        HOME,
    )
}
/// A mixed request set over several principals: entry, region reads,
/// writes and paths that collide with principal paths and ancestors.
fn mixed() -> Vec<Request> {
    let mut out = Vec::new();
    for n in 0..6 {
        let p = principal_n(n);
        out.push(entry(p.clone(), Some(intent())));
        out.push(entry(p.clone(), None));
        for path in [
            "/bin",
            "/usr/share",
            "/",
            "/dev/null",
            "/private/etc/hosts",
            "/Users/person",
            "/Users/person/.asura",
        ] {
            let mut r = read(path);
            r.principal = p.clone();
            out.push(r);
        }
        // The project root and scratch themselves, with their own
        // attributes, and with conflicting ones (uncached path).
        for resource in [p.project_root.clone(), p.scratch.clone()] {
            let mut r = read("/");
            r.principal = p.clone();
            r.resource = Resource::Path(resource.clone());
            out.push(r.clone());
            let mut conflicting = resource;
            conflicting.in_project = !conflicting.in_project;
            r.resource = Resource::Path(conflicting);
            out.push(r);
        }
        let mut write = read(&format!("/Users/person/src/project-{n}/src/main.rs"));
        write.principal = p.clone();
        write.action = Action::FileWrite;
        if let Resource::Path(path) = &mut write.resource {
            path.in_project = true;
            path.rel = "src/main.rs".into();
        }
        out.push(write);
    }
    out
}

#[test]
fn principal_cache_hit_miss_and_invalidation() {
    let engine = builtin();
    let request = entry(principal(), Some(intent()));
    let first = engine.decide(&request);
    assert_eq!(
        engine.principal_cache_stats(),
        PrincipalCacheStats {
            entries: 1,
            bytes: engine.principal_cache_stats().bytes,
            hits: 0,
            misses: 1
        }
    );
    assert!(engine.principal_cache_stats().bytes > 0);
    // A second request of the same principal hits.
    assert_eq!(engine.decide(&request), first);
    let mut other = read("/bin");
    other.principal = principal();
    engine.decide(&other);
    let stats = engine.principal_cache_stats();
    assert_eq!((stats.entries, stats.hits, stats.misses), (1, 2, 1));
    // Another principal misses.
    engine.decide(&entry(principal_n(9), Some(intent())));
    assert_eq!(engine.principal_cache_stats().entries, 2);
    assert_eq!(engine.principal_cache_stats().misses, 2);
    // Invalidation (activation, revocation, schema change) empties it.
    engine.invalidate_principal_cache();
    assert_eq!(
        engine.principal_cache_stats(),
        PrincipalCacheStats::default()
    );
    assert_eq!(engine.decide(&request), first);
    assert_eq!(engine.principal_cache_stats().misses, 1);
}

/// An activation installs a new engine for the new revision: its cache
/// starts empty and its decisions follow the new texts, never an entry
/// built under the old revision.
#[test]
fn principal_cache_does_not_survive_activation() {
    let old = builtin();
    let request = entry(principal(), Some(intent()));
    assert!(old.decide(&request).allow);
    assert_eq!(old.principal_cache_stats().entries, 1);
    let texts = with(
        Scope::User,
        "@id(\"no_model_exec\") @reason(\"no_exec\") forbid (principal is Asura::Job, action == Asura::Action::\"process.exec\", resource) when { principal.origin == Asura::Origin::\"model\" };",
    );
    assert_ne!(texts.digest(), RevisionTexts::builtin().digest());
    let new = Engine::build(&texts).expect("loads");
    assert_eq!(new.principal_cache_stats(), PrincipalCacheStats::default());
    let decision = new.decide(&request);
    assert!(!decision.allow);
    assert_eq!(reason(&decision), "no_exec");
    assert_eq!(new.principal_cache_stats().misses, 1);
}

/// A decision with the cache equals the decision without it, over mixed
/// requests, repeated (hits), and over malformed variants (the bench
/// repeats this over the PL-B5 corpus and 1,000,000 malformed requests).
#[test]
fn principal_cache_changes_no_decision() {
    let engine = builtin();
    let requests = mixed();
    for _ in 0..3 {
        for r in &requests {
            assert_eq!(
                engine.decide(r),
                engine.decide_uncached(r),
                "{:?}",
                r.resource
            );
        }
    }
    assert!(engine.principal_cache_stats().hits > 0);
    // Malformed variants: a bad principal path, an over-long string, a
    // non-canonical resource.
    for r in &requests {
        let mut bad = r.clone();
        bad.principal.scratch.path = "relative/scratch".into();
        assert_eq!(engine.decide(&bad), engine.decide_uncached(&bad));
        let mut long = r.clone();
        long.principal.task = "t".repeat(MAX_STRING_BYTES + 1);
        assert_eq!(engine.decide(&long), engine.decide_uncached(&long));
        let mut dotted = r.clone();
        dotted.resource = Resource::Path(PathRef::ordinary("/bin/../etc"));
        assert_eq!(engine.decide(&dotted), engine.decide_uncached(&dotted));
        assert!(!engine.decide(&dotted).allow);
    }
}

/// The memory bound: at most 64 entries and 64 × 16 KiB of charged bytes;
/// the oldest entry goes first.
#[test]
fn principal_cache_is_bounded() {
    let engine = builtin();
    for n in 0..(PRINCIPAL_CACHE_ENTRIES + 36) {
        engine.decide(&entry(principal_n(n), Some(intent())));
        let stats = engine.principal_cache_stats();
        assert!(stats.entries <= PRINCIPAL_CACHE_ENTRIES);
        assert!(stats.bytes <= PRINCIPAL_CACHE_BYTES);
    }
    let stats = engine.principal_cache_stats();
    assert_eq!(stats.entries, PRINCIPAL_CACHE_ENTRIES);
    assert_eq!(PRINCIPAL_CACHE_BYTES, 64 * MAX_REQUEST_BYTES);
    // The newest principal is cached; the oldest was evicted.
    engine.decide(&entry(
        principal_n(PRINCIPAL_CACHE_ENTRIES + 35),
        Some(intent()),
    ));
    assert_eq!(engine.principal_cache_stats().hits, stats.hits + 1);
    engine.decide(&entry(principal_n(0), Some(intent())));
    assert_eq!(engine.principal_cache_stats().misses, stats.misses + 1);
    // Large principals (many cache roots near the string limit) are
    // charged their size, and the byte bound still holds.
    let engine = builtin();
    for n in 0..80 {
        let mut p = principal_n(n);
        p.cache_roots = (0..MAX_SET_ELEMENTS)
            .map(|i| PathRef::ordinary(&format!("/Users/person/c/{n}/{i}/{}", "x".repeat(100))))
            .collect();
        engine.decide(&entry(p, Some(intent())));
        assert!(engine.principal_cache_stats().bytes <= PRINCIPAL_CACHE_BYTES);
    }
}
