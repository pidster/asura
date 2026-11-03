//! Worker-level policy budgets (security-policy.md, performance budgets):
//! PL-B1 decision latency, PL-B4 startup, and the PL-B5 fuzz (PES-M4) and
//! determinism (PES-M5) parts, on a generated scale workload over the
//! effect schema: 204 static policies and 54 grants, 40 projects, 1,000
//! requests. Loops are bounded by counts, never by wall-clock deadlines.
//! Run in release: `cargo run --release -p asura-policy --bin policy-bench`.
use asura_policy::{
    Action, Context, Engine, Intent, IntentSource, JobPrincipal, Origin, PathClass, PathRef,
    ProfileKind, ProgramRef, Request, Resource, RevisionTexts, Scope, ScopeText, compile_model1,
    model1_principal,
};
use std::time::{Duration, Instant};

const PROJECTS: usize = 40;
const TOOLS: usize = 50;
const HOME: &str = "/Users/person";
const GRANTS: usize = 54;
const STATIC: usize = 204;
const REQUESTS: usize = 1000;
const PASSES: usize = 10;
const FUZZ: usize = 1_000_000;
/// 2026-11-03T12:00:00Z.
const NOW: i64 = 1_793_707_200_000;

/// A small deterministic generator (64-bit LCG).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The spike's scale corpus rewritten for the effect schema
/// (policy-engine-spike.md, scale corpus): 50 tool forbids, and for each of
/// 40 projects one egress permit (user), one vendor write forbid
/// (workspace) and one network forbid (repository); filler user forbids on
/// `secret_use` bring the static count to 204; 27 envelopes give 54 grant
/// permits (a write region with an expiry and a network region). Lower
/// scopes hold forbids only, plus their baseline permits.
fn workload() -> (RevisionTexts, usize, usize) {
    let mut texts = RevisionTexts::builtin();
    let builtin = count(&texts);
    let mut installation = String::new();
    let mut user = String::new();
    let mut workspace = String::new();
    let mut repository = String::new();
    let expiry = "2026-11-03T18:00:00Z";
    for t in 0..TOOLS {
        installation.push_str(&format!(
            "@id(\"tool.{t:02}\") @reason(\"tool_denied\")\nforbid (principal is Asura::Job, action == Asura::Action::\"process.exec\", resource is Asura::Program)\nwhen {{ principal.origin == Asura::Origin::\"model\" && resource.path == Asura::Path::\"/opt/tools/bin/tool-{t:02}\" }};\n"
        ));
    }
    for i in 0..PROJECTS {
        let root = format!("{HOME}/src/p{i:03}");
        user.push_str(&format!(
            "@id(\"egress.p{i:03}\") @reason(\"egress_grant\")\npermit (principal is Asura::Task, action == Asura::Action::\"context.egress\", resource == Asura::Destination::\"remote-a\")\nwhen {{ principal.project == Asura::Project::\"p{i:03}\" && [Asura::DataClass::\"prompt\", Asura::DataClass::\"history\"].contains(context.data_class) }};\n"
        ));
        workspace.push_str(&format!(
            "@id(\"vendor.p{i:03}\") @reason(\"workspace_write_limit\")\nforbid (principal is Asura::Job, action in [Asura::Action::\"file.write\", Asura::Action::\"file.create\", Asura::Action::\"file.delete\"], resource is Asura::Path)\nwhen {{ principal.origin == Asura::Origin::\"model\" && resource in Asura::Path::\"{root}/vendor\" }};\n"
        ));
        repository.push_str(&format!(
            "@id(\"net.p{i:03}\") @reason(\"repository_network_limit\")\nforbid (principal is Asura::Job, action == Asura::Action::\"network.connect\", resource)\nwhen {{ principal.origin == Asura::Origin::\"model\" && principal.project == Asura::Project::\"p{i:03}\" }}\nunless {{ resource in Asura::Domain::\"org{i:02}.example\" && context.port == 443 }};\n"
        ));
    }
    let filler = STATIC - builtin - TOOLS - 3 * PROJECTS;
    for f in 0..filler {
        user.push_str(&format!(
            "@id(\"secret.f{f:02}\") @reason(\"secret_owner\")\nforbid (principal is Asura::Job, action == Asura::Action::\"secret_use\", resource == Asura::Secret::\"secret-p{:03}-{}\")\nwhen {{ principal.project != Asura::Project::\"p{:03}\" }};\n",
            f % PROJECTS,
            f % 5,
            f % PROJECTS
        ));
    }
    for e in 0..GRANTS / 2 {
        let task = format!("task-p{e:03}-a");
        user.push_str(&format!(
            "@id(\"env.{task}.write\") @reason(\"envelope_write\")\npermit (principal is Asura::Job, action in [Asura::Action::\"file.write\", Asura::Action::\"file.create\", Asura::Action::\"file.delete\"], resource is Asura::Path)\nwhen {{ principal.task == Asura::Task::\"{task}\" && resource in Asura::Path::\"{HOME}/src/shared/p{e:03}-out\" && context.now < datetime(\"{expiry}\") }};\n@id(\"env.{task}.net\") @reason(\"envelope_network\")\npermit (principal is Asura::Job, action == Asura::Action::\"network.connect\", resource)\nwhen {{ principal.task == Asura::Task::\"{task}\" && resource in Asura::Domain::\"org{e:02}.example\" && context.port == 443 && context.now < datetime(\"{expiry}\") }};\n"
        ));
    }
    texts.scopes[0].text.push_str(&installation);
    texts.scopes[1].text.push_str(&user);
    texts.scopes.push(ScopeText {
        scope: Scope::Workspace,
        source: "bench",
        text: workspace,
    });
    texts.scopes.push(ScopeText {
        scope: Scope::Repository,
        source: "bench",
        text: repository,
    });
    let total = count(&texts);
    (texts, total - GRANTS, GRANTS)
}
fn count(texts: &RevisionTexts) -> usize {
    texts
        .scopes
        .iter()
        .map(|s| s.text.matches("@id(").count())
        .sum()
}

fn job(i: usize, half: &str, model: bool) -> JobPrincipal {
    let mut p = model1_principal(
        &format!(
            "job-p{i:03}-{half}-{}",
            if model { "model" } else { "person" }
        ),
        &format!("p{i:03}"),
        &format!("{HOME}/src/p{i:03}"),
        &format!("task-p{i:03}-{half}"),
        &format!("{HOME}/.asura"),
    );
    if !model {
        p.origin = Origin::PersonTyped;
        p.profile = ProfileKind::Person1;
    }
    p
}
fn in_project(root: &str, path: String, class: PathClass) -> PathRef {
    PathRef {
        rel: path
            .strip_prefix(&format!("{root}/"))
            .unwrap_or_default()
            .to_owned(),
        in_project: path.starts_with(&format!("{root}/")),
        path,
        class,
    }
}

/// 1,000 requests with the spike's mix; its network, secret and egress
/// requests (cases 7 to 9) become file reads, which PL1's typed requests
/// cover.
fn requests() -> Vec<Request> {
    let mut rng = Lcg(0x5EED_A50A_0001);
    (0..REQUESTS)
        .map(|_| {
            let i = rng.below(PROJECTS);
            let half = if rng.below(2) == 0 { "a" } else { "b" };
            let root = format!("{HOME}/src/p{i:03}");
            let file = |p: JobPrincipal, action, path: PathRef| Request {
                principal: p,
                action,
                resource: Resource::Path(path),
                context: Context::File {
                    now_ms: NOW,
                    intent: None,
                },
            };
            let ordinary = PathClass::Ordinary;
            match rng.below(10) {
                0 => file(
                    job(i, half, true),
                    Action::FileWrite,
                    in_project(
                        &root,
                        format!("{root}/src/f{:03}.rs", rng.below(220)),
                        ordinary,
                    ),
                ),
                1 => file(
                    job(i, half, true),
                    Action::FileWrite,
                    in_project(&root, format!("{root}/vendor/v.rs"), ordinary),
                ),
                2 => file(
                    job(i, half, true),
                    Action::FileWrite,
                    in_project(
                        &root,
                        format!("{root}/.git/hooks/pre-push"),
                        PathClass::ImplicitExec,
                    ),
                ),
                3 => file(
                    job(i, half, true),
                    Action::FileWrite,
                    PathRef::ordinary(&format!("{HOME}/src/shared/p{i:03}-out/r.md")),
                ),
                4 => file(
                    job(i, half, false),
                    Action::FileWrite,
                    PathRef::ordinary(&format!(
                        "{HOME}/src/p{:03}/src/f000.rs",
                        (i + 1) % PROJECTS
                    )),
                ),
                5 | 6 => {
                    let (model, program) = if rng.below(3) == 0 {
                        (false, "/usr/bin/git".to_owned())
                    } else if rng.below(2) == 0 {
                        (true, format!("/opt/tools/bin/tool-{:02}", rng.below(TOOLS)))
                    } else {
                        (true, "/usr/bin/git".to_owned())
                    };
                    Request {
                        principal: job(i, half, model),
                        action: Action::ProcessExec,
                        resource: Resource::Program(ProgramRef::unidentified(PathRef::ordinary(
                            &program,
                        ))),
                        context: Context::Exec {
                            now_ms: NOW,
                            request_digest: format!("{:064x}", rng.next()),
                            env_terminal: Vec::new(),
                            env_declared: Vec::new(),
                            outside_switch: false,
                            intent: Some(Intent {
                                source: IntentSource::ModelTool,
                                name: "exec_cmd".into(),
                                digest: format!("{:064x}", rng.next()),
                            }),
                        },
                    }
                }
                _ => file(
                    job(i, half, rng.below(4) != 0),
                    Action::FileRead,
                    in_project(&root, format!("{root}/docs/readme.md"), ordinary),
                ),
            }
        })
        .collect()
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}
fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// One malformed variant of `base` (PES-M4 classes).
fn malformed(base: &Request, class: usize, rng: &mut Lcg) -> Request {
    let mut r = base.clone();
    let bad_path = |rng: &mut Lcg| -> String {
        match rng.below(6) {
            0 => "relative/path".into(),
            1 => "/w//x".into(),
            2 => "/w/./x".into(),
            3 => "/w/../x".into(),
            4 => "/w/x/".into(),
            _ => "/w/\u{7}x".into(),
        }
    };
    match class {
        0 => {
            let (Context::Exec { intent, .. } | Context::File { intent, .. }) = &mut r.context;
            {
                *intent = Some(Intent {
                    source: IntentSource::ModelTool,
                    name: "x".repeat(1025 + rng.below(64)),
                    digest: String::new(),
                });
            }
        }
        1 => {
            r.principal.cache_roots = (0..65 + rng.below(8))
                .map(|i| PathRef::ordinary(&format!("/c{i}")))
                .collect()
        }
        2 => r.resource = Resource::Path(PathRef::ordinary(&bad_path(rng))),
        3 => r.principal.project_root.path = bad_path(rng),
        4 => r.resource = Resource::Path(PathRef::ordinary(&"/d".repeat(65 + rng.below(8)))),
        5 => {
            r.context = match r.context {
                Context::Exec { now_ms, intent, .. } => Context::File { now_ms, intent },
                Context::File { now_ms, intent } => Context::Exec {
                    now_ms,
                    request_digest: String::new(),
                    env_terminal: Vec::new(),
                    env_declared: Vec::new(),
                    outside_switch: false,
                    intent,
                },
            }
        }
        6 => {
            r.resource = match r.resource {
                Resource::Path(p) => Resource::Program(ProgramRef::unidentified(p)),
                Resource::Program(p) => Resource::Path(p.path),
            }
        }
        7 => {
            // The resource is the project root with other attributes.
            r.resource = Resource::Path(PathRef {
                path: r.principal.project_root.path.clone(),
                class: PathClass::AsuraState,
                in_project: false,
                rel: "x".into(),
            })
        }
        8 => r.principal.job = String::new(),
        _ => {
            if let Context::Exec { env_declared, .. } = &mut r.context {
                *env_declared = (0..65).map(|i| format!("V{i}")).collect();
            } else {
                r.principal.scratch.path = bad_path(rng);
            }
        }
    }
    r
}

fn main() {
    let (texts, statics, grants) = workload();
    let requests = requests();
    let workers = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .div_euclid(2)
        .clamp(2, 8);
    println!(
        "workload: {statics} static policies, {grants} grants, {PROJECTS} projects, {} requests; schema {}; Cedar SDK {} language {}",
        requests.len(),
        asura_policy::SCHEMA_ID,
        asura_policy::CEDAR_SDK,
        asura_policy::cedar_lang()
    );
    let engine = Engine::build(&texts).expect("workload loads");
    let mut allowed = 0;
    for r in &requests {
        allowed += usize::from(engine.decide(r).allow);
    }
    println!(
        "decisions: {allowed} allow, {} deny",
        requests.len() - allowed
    );

    // PL-B1: one engine, every applicable scope, per request.
    let mut times = Vec::with_capacity(REQUESTS * PASSES);
    for _ in 0..PASSES {
        for r in &requests {
            let start = Instant::now();
            let d = engine.decide(r);
            times.push(start.elapsed());
            std::hint::black_box(d);
        }
    }
    times.sort();
    let p50 = percentile(&times, 0.5);
    println!(
        "PL-B1 decision: p50 {:.1} µs, p90 {:.1} µs, p99 {:.1} µs, max {:.1} µs over {} decisions; budget p50 <= 100 µs: {}",
        micros(p50),
        micros(percentile(&times, 0.9)),
        micros(percentile(&times, 0.99)),
        micros(*times.last().unwrap()),
        times.len(),
        if p50 <= Duration::from_micros(100) {
            "PASS"
        } else {
            "FAIL"
        }
    );
    // Breakdown: W1 and W2 (entity slice, context, request) alone.
    let mut build = Vec::with_capacity(REQUESTS);
    for r in &requests {
        let start = Instant::now();
        std::hint::black_box(engine.build_only(r));
        build.push(start.elapsed());
    }
    build.sort();
    println!(
        "breakdown: request and entity construction with schema checks p50 {:.1} µs; evaluation is the rest",
        micros(percentile(&build, 0.5))
    );
    // The shipped PL1 sets: the model exec_cmd entry decision alone.
    let shipped = Engine::build(&RevisionTexts::builtin()).expect("built-in sets load");
    let entries: Vec<&Request> = requests
        .iter()
        .filter(|r| r.action == Action::ProcessExec)
        .collect();
    let mut entry = Vec::new();
    for _ in 0..PASSES {
        for r in &entries {
            let start = Instant::now();
            std::hint::black_box(shipped.decide(r));
            entry.push(start.elapsed());
        }
    }
    entry.sort();
    println!(
        "built-in sets (PL1 production): exec entry decision p50 {:.1} µs, p99 {:.1} µs over {}",
        micros(percentile(&entry, 0.5)),
        micros(percentile(&entry, 0.99)),
        entry.len()
    );
    // The model-1 region compile (cached per revision and project).
    let mut compile = Vec::new();
    for n in 0..100 {
        let p = job(n % PROJECTS, if n % 2 == 0 { "a" } else { "b" }, true);
        let start = Instant::now();
        let regions = compile_model1(&engine, &p, NOW);
        compile.push(start.elapsed());
        assert!(regions.is_ok());
    }
    compile.sort();
    println!(
        "model-1 region compile (cache miss): p50 {:.1} µs, p99 {:.1} µs over {}",
        micros(percentile(&compile, 0.5)),
        micros(percentile(&compile, 0.99)),
        compile.len()
    );

    // PL-B4: every worker builds its own engine in parallel, then the first decision.
    let mut startups = Vec::new();
    for _ in 0..10 {
        let start = Instant::now();
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let texts = texts.clone();
                std::thread::spawn(move || Engine::build(&texts).expect("loads"))
            })
            .collect();
        let engines: Vec<Engine> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        std::hint::black_box(engines[0].decide(&requests[0]));
        startups.push(start.elapsed());
    }
    startups.sort();
    let median = startups[startups.len() / 2];
    println!(
        "PL-B4 startup ({workers} workers, schema and every scope per worker, first decision): median {:.1} ms (min {:.1}, max {:.1}) over {}; budget <= 200 ms: {}",
        median.as_secs_f64() * 1e3,
        startups[0].as_secs_f64() * 1e3,
        startups.last().unwrap().as_secs_f64() * 1e3,
        startups.len(),
        if median <= Duration::from_millis(200) {
            "PASS"
        } else {
            "FAIL"
        }
    );

    // PL-B5, PES-M5: two fresh engines decide identically.
    let other = Engine::build(&texts).expect("loads");
    let same = requests.iter().all(|r| engine.decide(r) == other.decide(r));
    println!(
        "PL-B5 determinism (PES-M5): {} over {} requests on two fresh engines",
        if same { "PASS, identical" } else { "FAIL" },
        requests.len()
    );

    // PL-B5, PES-M4: malformed requests never allow.
    let mut rng = Lcg(0xf022);
    let mut fuzz_allows = 0;
    let start = Instant::now();
    for i in 0..FUZZ {
        let base = &requests[i % requests.len()];
        let r = malformed(base, rng.below(10), &mut rng);
        fuzz_allows += usize::from(engine.decide(&r).allow);
    }
    println!(
        "PL-B5 fuzz (PES-M4): {fuzz_allows} allows in {FUZZ} malformed requests ({:.1} s); budget 0: {}",
        start.elapsed().as_secs_f64(),
        if fuzz_allows == 0 { "PASS" } else { "FAIL" }
    );
}
