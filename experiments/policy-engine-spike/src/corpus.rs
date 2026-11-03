//! Corpus loading and the deterministic scale generator. One Rust
//! description emits both the Cedar and the Rego text of every generated
//! policy, so the two encodings cannot drift.

use crate::model::{
    Action, Command, ConnectCtx, Context, DataClass, Delivery, EXPIRY_MS, EgressCtx, ExecCtx, Kind,
    NOW_MS, Request, Risk, SecretCtx, Uid, WriteCtx, rfc3339,
};
use crate::store::{JobE, Store};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

pub const HOME: &str = "/Users/person";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Installation,
    Classifier,
    User,
    Task,
    Workspace,
    Repository,
}

impl Scope {
    pub const ALL: [Scope; 6] = [
        Scope::Installation,
        Scope::Classifier,
        Scope::User,
        Scope::Task,
        Scope::Workspace,
        Scope::Repository,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scope::Installation => "installation",
            Scope::Classifier => "classifier",
            Scope::User => "user",
            Scope::Task => "task",
            Scope::Workspace => "workspace",
            Scope::Repository => "repository",
        }
    }

    /// Lower scopes are forbid-only under PES-A1.
    pub fn is_lower(self) -> bool {
        matches!(self, Scope::Workspace | Scope::Repository)
    }
}

/// One policy source file (or generated module) in one language.
#[derive(Clone, Debug)]
pub struct Source {
    pub scope: Scope,
    pub name: String,
    pub text: String,
}

pub fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

fn read(rel: &str) -> String {
    let path = corpus_dir().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

pub fn schema_text() -> String {
    read("asura.cedarschema")
}

pub fn core_cedar() -> Vec<Source> {
    Scope::ALL
        .iter()
        .map(|s| Source {
            scope: *s,
            name: format!("cedar/{}.cedar", s.name()),
            text: read(&format!("cedar/{}.cedar", s.name())),
        })
        .collect()
}

/// Rego scope modules. The helper and the combiner are returned separately.
pub fn core_rego() -> Vec<Source> {
    Scope::ALL
        .iter()
        .map(|s| Source {
            scope: *s,
            name: format!("rego/{}.rego", s.name()),
            text: read(&format!("rego/{}.rego", s.name())),
        })
        .collect()
}

pub fn rego_lib() -> String {
    read("rego/lib.rego")
}

pub fn rego_decision() -> String {
    read("rego/decision.rego")
}

#[derive(Clone, Debug, Deserialize)]
pub struct Case {
    pub id: String,
    pub request: Value,
    pub allow: bool,
    pub rules: Vec<String>,
    #[serde(default)]
    pub cedar_rules: Option<Vec<String>>,
    #[serde(default)]
    pub cedar_error: Option<String>,
}

pub fn conformance() -> Vec<Case> {
    serde_json::from_str(&read("conformance.json")).expect("conformance.json")
}

fn job(
    origin: &'static str,
    profile: &'static str,
    project: &str,
    task: &str,
    id: &str,
    cache_roots: Vec<String>,
) -> JobE {
    JobE {
        origin,
        profile,
        project: project.into(),
        task: task.into(),
        scratch: format!("{HOME}/.asura/scratch/{id}"),
        cache_roots,
    }
}

fn add_job(s: &mut Store, id: &str, j: JobE) {
    s.add_path(&j.scratch, "asura_state");
    s.add_path(&format!("{}/out.txt", j.scratch), "asura_state");
    s.jobs.insert(id.into(), j);
}

/// The core entities named by the conformance cases.
pub fn core_store() -> Store {
    let mut s = Store::default();
    let studio = format!("{HOME}/src/studio");
    let obs = format!("{HOME}/src/observatory");
    for (p, c) in [
        (format!("{studio}/src/main.rs"), "ordinary"),
        (format!("{studio}/vendor/lib.rs"), "ordinary"),
        (format!("{studio}/.git/hooks"), "implicit_exec"),
        (format!("{studio}/.git/hooks/pre-commit"), "implicit_exec"),
        (format!("{studio}/.git/config"), "implicit_exec"),
        (format!("{studio}/.envrc"), "implicit_exec"),
        (format!("{studio}/build.rs"), "build_test"),
        (format!("{studio}/Makefile"), "build_test"),
        (format!("{obs}/src/lib.rs"), "ordinary"),
        (format!("{HOME}/src/shared/out/report.md"), "ordinary"),
        (format!("{HOME}/.asura"), "asura_state"),
        (format!("{HOME}/.asura/config.yaml"), "asura_state"),
        (format!("{HOME}/.asura/scratch"), "asura_state"),
        (format!("{HOME}/.ssh/config"), "ordinary"),
        (format!("{HOME}/.cargo/registry/index"), "ordinary"),
        (format!("{HOME}/.cargo/bin/cargo"), "ordinary"),
        (format!("{HOME}/Library/Caches"), "ordinary"),
        ("/etc/hosts".to_string(), "ordinary"),
    ] {
        s.add_path(&p, c);
    }
    s.projects
        .insert("studio".into(), (studio.clone(), vec!["RUST_LOG".into()]));
    s.projects.insert("observatory".into(), (obs, vec![]));
    for (t, p) in [
        ("t-studio-1", "studio"),
        ("t-studio-2", "studio"),
        ("t-obs-1", "observatory"),
    ] {
        s.tasks.insert(t.into(), p.into());
    }
    let caches = vec![format!("{HOME}/.cargo"), format!("{HOME}/Library/Caches")];
    for (id, j) in [
        (
            "j-studio-model",
            job(
                "model",
                "model-1",
                "studio",
                "t-studio-1",
                "j-studio-model",
                vec![],
            ),
        ),
        (
            "j-studio-model-2",
            job(
                "model",
                "model-1",
                "studio",
                "t-studio-2",
                "j-studio-model-2",
                vec![],
            ),
        ),
        (
            "j-studio-person",
            job(
                "person_typed",
                "person-1",
                "studio",
                "t-studio-1",
                "j-studio-person",
                caches,
            ),
        ),
        (
            "j-studio-outside",
            job(
                "person_typed",
                "person-outside-1",
                "studio",
                "t-studio-1",
                "j-studio-outside",
                vec![],
            ),
        ),
        (
            "j-obs-model",
            job(
                "model",
                "model-1",
                "observatory",
                "t-obs-1",
                "j-obs-model",
                vec![],
            ),
        ),
    ] {
        add_job(&mut s, id, j);
    }
    for (e, t) in [
        ("/usr/bin/git", "git"),
        ("/usr/bin/sudo", "sudo"),
        ("/bin/sh", "sh"),
        ("/usr/bin/make", "make"),
        ("/usr/local/bin/npm", "npm"),
    ] {
        s.executables.insert(e.into(), t.into());
    }
    s.executables
        .insert(format!("{HOME}/.cargo/bin/cargo"), "cargo".into());
    for d in ["api.github.com", "github.com", "evil.com"] {
        s.add_domain(d);
    }
    s.secrets.insert("gh_token".into(), "studio".into());
    s.secrets.insert("obs_key".into(), "observatory".into());
    s.destinations.insert("local-mlx".into(), false);
    s.destinations.insert("anthropic".into(), true);
    s.destinations.insert("remote-a".into(), true);
    s
}

/// Small deterministic generator (xorshift64*); no external randomness.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

pub const PROJECTS: usize = 40;
pub const TOOLS: usize = 50;
pub const ENVELOPES: usize = 25;
pub const FILES_PER_PROJECT: usize = 220;
pub const SCALE_REQUESTS: usize = 1000;
pub const SEED: u64 = 0x5EED_A50A_0001;

/// The scale corpus: generated policies in both languages, the full entity
/// store (core plus generated) and the generated requests.
pub struct Scale {
    pub cedar: Vec<Source>,
    pub rego: Vec<Source>,
    pub store: Store,
    pub requests: Vec<Request>,
}

fn pname(i: usize) -> String {
    format!("p{i:03}")
}

fn org(i: usize) -> String {
    format!("org{i:02}.example")
}

pub fn scale() -> Scale {
    let mut store = core_store();
    let mut cedar: Vec<(Scope, String)> = Vec::new();
    let mut rego: Vec<(Scope, String)> = Vec::new();
    let expiry = rfc3339(EXPIRY_MS);

    for t in 0..TOOLS {
        let tool = format!("tool-{t:02}");
        store
            .executables
            .insert(format!("/opt/tools/bin/{tool}"), tool.clone());
        cedar.push((Scope::Installation, format!(
            "@id(\"inst.tool.{t:02}\") @scope(\"installation\") @reason(\"tool_needs_approval\")\n\
             forbid (principal is Asura::Job, action == Asura::Action::\"exec\", resource is Asura::Executable)\n\
             when {{ principal.origin == Asura::Origin::\"model\" && resource.tool == \"{tool}\" }};\n")));
        rego.push((Scope::Installation, format!(
            "deny contains {{\"id\": \"inst.tool.{t:02}\", \"reason\": \"tool_needs_approval\"}} if {{\n\
             \tinput.action == \"exec\"\n\tlib.job.origin == \"model\"\n\tlib.executable.tool == \"{tool}\"\n}}\n")));
    }
    store.add_path("/opt/tools/bin", "ordinary");

    let classes = [
        &["prompt"][..],
        &["prompt", "history"][..],
        &["prompt", "history", "notice"][..],
    ];
    for i in 0..PROJECTS {
        let p = pname(i);
        let root = format!("{HOME}/src/{p}");
        for f in 0..FILES_PER_PROJECT {
            store.add_path(&format!("{root}/src/f{f:03}.rs"), "ordinary");
        }
        store.add_path(&format!("{root}/vendor/v.rs"), "ordinary");
        store.add_path(&format!("{root}/docs/readme.md"), "ordinary");
        store.add_path(&format!("{root}/.git/hooks"), "implicit_exec");
        store.add_path(&format!("{root}/.git/hooks/pre-push"), "implicit_exec");
        store.add_path(&format!("{HOME}/src/shared/{p}-out/r.md"), "ordinary");
        store.projects.insert(p.clone(), (root.clone(), vec![]));
        let o = org(i);
        for sub in ["api", "www", "git"] {
            store.add_domain(&format!("{sub}.{o}"));
        }
        for k in 0..5 {
            store.secrets.insert(format!("secret-{p}-{k}"), p.clone());
        }
        for half in ["a", "b"] {
            let task = format!("task-{p}-{half}");
            store.tasks.insert(task.clone(), p.clone());
            add_job(
                &mut store,
                &format!("job-{p}-{half}-model"),
                job(
                    "model",
                    "model-1",
                    &p,
                    &task,
                    &format!("job-{p}-{half}-model"),
                    vec![],
                ),
            );
            add_job(
                &mut store,
                &format!("job-{p}-{half}-person"),
                job(
                    "person_typed",
                    "person-1",
                    &p,
                    &task,
                    &format!("job-{p}-{half}-person"),
                    vec![],
                ),
            );
        }

        let cls = classes[i % 3];
        let cedar_cls: Vec<String> = cls
            .iter()
            .map(|c| format!("Asura::DataClass::\"{c}\""))
            .collect();
        let rego_cls: Vec<String> = cls.iter().map(|c| format!("\"{c}\"")).collect();
        cedar.push((Scope::User, format!(
            "@id(\"user.egress.{p}\") @scope(\"user\") @reason(\"egress_grant\")\n\
             permit (principal is Asura::Task, action == Asura::Action::\"egress\", resource == Asura::Destination::\"remote-a\")\n\
             when {{ principal.project == Asura::Project::\"{p}\" && [{}].contains(context.data_class) }};\n",
            cedar_cls.join(", "))));
        rego.push((Scope::User, format!(
            "permit contains {{\"id\": \"user.egress.{p}\", \"reason\": \"egress_grant\"}} if {{\n\
             \tinput.action == \"egress\"\n\tinput.resource.id == \"remote-a\"\n\tlib.task.project == \"{p}\"\n\
             \tinput.context.data_class in {{{}}}\n}}\n",
            rego_cls.join(", "))));

        cedar.push((Scope::Workspace, format!(
            "@id(\"ws.{p}.vendor\") @scope(\"workspace\") @reason(\"workspace_write_limit\")\n\
             forbid (principal is Asura::Job, action == Asura::Action::\"write\", resource is Asura::Path)\n\
             when {{ principal.origin == Asura::Origin::\"model\" && resource in Asura::Path::\"{root}/vendor\" }};\n")));
        rego.push((Scope::Workspace, format!(
            "deny contains {{\"id\": \"ws.{p}.vendor\", \"reason\": \"workspace_write_limit\"}} if {{\n\
             \tinput.action == \"write\"\n\tlib.job.origin == \"model\"\n\tlib.path_in(input.resource.id, \"{root}/vendor\")\n}}\n")));

        cedar.push((Scope::Repository, format!(
            "@id(\"repo.{p}.net\") @scope(\"repository\") @reason(\"repository_network_limit\")\n\
             forbid (principal is Asura::Job, action == Asura::Action::\"connect\", resource)\n\
             when {{ principal.origin == Asura::Origin::\"model\" && principal.project == Asura::Project::\"{p}\" }}\n\
             unless {{ resource in Asura::Domain::\"{o}\" && context.port == 443 }};\n")));
        rego.push((Scope::Repository, format!(
            "deny contains {{\"id\": \"repo.{p}.net\", \"reason\": \"repository_network_limit\"}} if {{\n\
             \tinput.action == \"connect\"\n\tlib.job.origin == \"model\"\n\tlib.job.project == \"{p}\"\n\
             \tnot repo_{i:03}_allowed\n}}\n\n\
             repo_{i:03}_allowed if {{\n\tinput.resource.type == \"Domain\"\n\tlib.domain_in(input.resource.id, \"{o}\")\n\
             \tinput.context.port == 443\n}}\n")));

        if i < ENVELOPES {
            let task = format!("task-{p}-a");
            cedar.push((Scope::Task, format!(
                "@id(\"env.{task}.write\") @scope(\"task\") @envelope(\"{task}\") @reason(\"envelope_write\")\n\
                 permit (principal is Asura::Job, action == Asura::Action::\"write\", resource is Asura::Path)\n\
                 when {{ principal.task == Asura::Task::\"{task}\" && resource in Asura::Path::\"{HOME}/src/shared/{p}-out\" &&\n\
                 \x20      context.now < datetime(\"{expiry}\") }};\n\n\
                 @id(\"env.{task}.net\") @scope(\"task\") @envelope(\"{task}\") @reason(\"envelope_network\")\n\
                 permit (principal is Asura::Job, action == Asura::Action::\"connect\", resource)\n\
                 when {{ principal.task == Asura::Task::\"{task}\" && resource in Asura::Domain::\"{o}\" && context.port == 443 &&\n\
                 \x20      context.now < datetime(\"{expiry}\") }};\n")));
            rego.push((Scope::Task, format!(
                "permit contains {{\"id\": \"env.{task}.write\", \"reason\": \"envelope_write\"}} if {{\n\
                 \tinput.action == \"write\"\n\tlib.job.task == \"{task}\"\n\
                 \tlib.path_in(input.resource.id, \"{HOME}/src/shared/{p}-out\")\n\tinput.context.now_ms < {EXPIRY_MS}\n}}\n\n\
                 permit contains {{\"id\": \"env.{task}.net\", \"reason\": \"envelope_network\"}} if {{\n\
                 \tinput.action == \"connect\"\n\tlib.job.task == \"{task}\"\n\tinput.resource.type == \"Domain\"\n\
                 \tlib.domain_in(input.resource.id, \"{o}\")\n\tinput.context.port == 443\n\tinput.context.now_ms < {EXPIRY_MS}\n}}\n")));
        }
    }

    let group = |v: Vec<(Scope, String)>, ext: &str| -> Vec<Source> {
        Scope::ALL
            .iter()
            .filter_map(|s| {
                let text: Vec<&str> = v
                    .iter()
                    .filter(|(sc, _)| sc == s)
                    .map(|(_, t)| t.as_str())
                    .collect();
                if text.is_empty() {
                    return None;
                }
                let body = text.join("\n");
                let text = if ext == "rego" {
                    format!(
                        "package asura.{}\n\nimport rego.v1\n\nimport data.asura.lib\n\n{body}",
                        s.name()
                    )
                } else {
                    body
                };
                Some(Source {
                    scope: *s,
                    name: format!("generated/{}.{ext}", s.name()),
                    text,
                })
            })
            .collect()
    };

    let requests = scale_requests();
    Scale {
        cedar: group(cedar, "cedar"),
        rego: group(rego, "rego"),
        store,
        requests,
    }
}

fn scale_requests() -> Vec<Request> {
    let mut rng = Rng::new(SEED);
    let mut out = Vec::with_capacity(SCALE_REQUESTS);
    let risks = [
        Some(Risk::Safe),
        Some(Risk::Dangerous),
        Some(Risk::Unknown),
        None,
    ];
    let classes = [
        DataClass::Prompt,
        DataClass::History,
        DataClass::Notice,
        DataClass::FilePage,
        DataClass::ToolResult,
    ];
    for _ in 0..SCALE_REQUESTS {
        let i = rng.below(PROJECTS as u64) as usize;
        let p = pname(i);
        let half = if rng.below(2) == 0 { "a" } else { "b" };
        let model = format!("job-{p}-{half}-model");
        let person = format!("job-{p}-{half}-person");
        let root = format!("{HOME}/src/{p}");
        let w = |who: &str, path: String| Request {
            principal: Uid::new(Kind::Job, who),
            action: Action::Write,
            resource: Uid::new(Kind::Path, path),
            context: Context::Write(WriteCtx { now_ms: NOW_MS }),
        };
        let req = match rng.below(10) {
            0 => w(
                &model,
                format!("{root}/src/f{:03}.rs", rng.below(FILES_PER_PROJECT as u64)),
            ),
            1 => w(&model, format!("{root}/vendor/v.rs")),
            2 => w(&model, format!("{root}/.git/hooks/pre-push")),
            3 => w(&model, format!("{HOME}/src/shared/{p}-out/r.md")),
            4 => w(
                &person,
                format!("{HOME}/src/{}/src/f000.rs", pname((i + 1) % PROJECTS)),
            ),
            5 | 6 => {
                let (who, exe, sub) = if rng.below(3) == 0 {
                    (person.clone(), "/usr/bin/git".to_string(), "push")
                } else if rng.below(2) == 0 {
                    (
                        model.clone(),
                        format!("/opt/tools/bin/tool-{:02}", rng.below(TOOLS as u64)),
                        "",
                    )
                } else {
                    (model.clone(), "/usr/bin/git".to_string(), "status")
                };
                Request {
                    principal: Uid::new(Kind::Job, who),
                    action: if rng.below(4) == 0 {
                        Action::ExecRequestApproval
                    } else {
                        Action::Exec
                    },
                    resource: Uid::new(Kind::Executable, exe),
                    context: Context::Exec(ExecCtx {
                        now_ms: NOW_MS,
                        request_digest: "sha256:0".into(),
                        command: Command::simple(sub),
                        outside_switch: false,
                        risk: risks[rng.below(4) as usize],
                    }),
                }
            }
            7 => {
                let target = if rng.below(4) == 0 {
                    (i + 7) % PROJECTS
                } else {
                    i
                };
                let host = format!(
                    "{}.{}",
                    ["api", "www", "git"][rng.below(3) as usize],
                    org(target)
                );
                Request {
                    principal: Uid::new(
                        Kind::Job,
                        if rng.below(4) == 0 { &person } else { &model },
                    ),
                    action: Action::Connect,
                    resource: Uid::new(Kind::Domain, host),
                    context: Context::Connect(ConnectCtx {
                        now_ms: NOW_MS,
                        ip: format!("198.51.100.{}", rng.below(250) + 1),
                        port: if rng.below(4) == 0 { 80 } else { 443 },
                        via_proxy: rng.below(5) != 0,
                    }),
                }
            }
            8 => {
                let owner = if rng.below(5) == 0 {
                    pname((i + 3) % PROJECTS)
                } else {
                    p.clone()
                };
                Request {
                    principal: Uid::new(
                        Kind::Job,
                        if rng.below(2) == 0 { &person } else { &model },
                    ),
                    action: Action::SecretUse,
                    resource: Uid::new(Kind::Secret, format!("secret-{owner}-{}", rng.below(5))),
                    context: Context::SecretUse(SecretCtx {
                        now_ms: NOW_MS,
                        delivery: if rng.below(2) == 0 {
                            Delivery::Raw
                        } else {
                            Delivery::Brokered
                        },
                    }),
                }
            }
            _ => Request {
                principal: Uid::new(Kind::Task, format!("task-{p}-{half}")),
                action: Action::Egress,
                resource: Uid::new(
                    Kind::Destination,
                    if rng.below(3) == 0 {
                        "local-mlx"
                    } else {
                        "remote-a"
                    },
                ),
                context: Context::Egress(EgressCtx {
                    now_ms: NOW_MS,
                    data_class: classes[rng.below(5) as usize],
                    tainted: rng.below(8) == 0,
                }),
            },
        };
        out.push(req);
    }
    out
}

/// The full Cedar source list (core plus generated).
pub fn all_cedar(scale: &Scale) -> Vec<Source> {
    let mut v = core_cedar();
    v.extend(scale.cedar.iter().cloned());
    v
}

/// The full Rego source list (core plus generated).
pub fn all_rego(scale: &Scale) -> Vec<Source> {
    let mut v = core_rego();
    v.extend(scale.rego.iter().cloned());
    v
}

/// Conformance requests parsed into the typed model.
pub fn conformance_requests() -> Vec<(Case, Request)> {
    conformance()
        .into_iter()
        .map(|c| {
            let r = Request::from_json(&c.request)
                .unwrap_or_else(|e| panic!("case {} does not parse: {e}", c.id));
            (c, r)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_corpus_meets_the_designed_counts() {
        let s = scale();
        assert!(
            s.store.entity_count() >= 10_000,
            "{}",
            s.store.entity_count()
        );
        assert_eq!(s.requests.len(), SCALE_REQUESTS);
        let cedar_policies: usize = s.cedar.iter().map(|x| x.text.matches("@id(").count()).sum();
        assert_eq!(cedar_policies, TOOLS + 3 * PROJECTS + 2 * ENVELOPES);
    }

    #[test]
    fn generator_is_deterministic() {
        let a = scale();
        let b = scale();
        assert_eq!(a.requests, b.requests);
        assert_eq!(
            a.cedar.iter().map(|s| &s.text).collect::<Vec<_>>(),
            b.cedar.iter().map(|s| &s.text).collect::<Vec<_>>()
        );
        assert_eq!(a.store.to_rego_data(), b.store.to_rego_data());
    }
}
