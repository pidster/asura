//! The `model-1` region compiler and the job decision (security-policy.md,
//! effect envelope of a job; PL1 implementation contract, constraint set).
use crate::engine::Engine;
use crate::request::{
    Action, Context, Decision, ErrorClass, Intent, JobPrincipal, Origin, PathClass, PathRef,
    ProfileKind, ProgramRef, Request, Resource,
};
use crate::{hex, sha256};

/// The nine `model-1` template roots, in render order. The discriminant is
/// the bit position in a [`RootSet`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Root {
    Bin = 0,
    UsrBin,
    Sbin,
    UsrSbin,
    UsrLib,
    UsrShare,
    System,
    Project,
    Scratch,
}
pub const ROOTS: [Root; 9] = [
    Root::Bin,
    Root::UsrBin,
    Root::Sbin,
    Root::UsrSbin,
    Root::UsrLib,
    Root::UsrShare,
    Root::System,
    Root::Project,
    Root::Scratch,
];
impl Root {
    /// The fixed path of a system root; `None` for the project and scratch.
    pub fn system_path(self) -> Option<&'static str> {
        Some(match self {
            Root::Bin => "/bin",
            Root::UsrBin => "/usr/bin",
            Root::Sbin => "/sbin",
            Root::UsrSbin => "/usr/sbin",
            Root::UsrLib => "/usr/lib",
            Root::UsrShare => "/usr/share",
            Root::System => "/System",
            Root::Project | Root::Scratch => return None,
        })
    }
    pub fn bit(self) -> u16 {
        1 << (self as u16)
    }
}
/// A set of template roots, one bit per root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct RootSet(pub u16);
impl RootSet {
    pub fn contains(self, root: Root) -> bool {
        self.0 & root.bit() != 0
    }
    pub fn insert(&mut self, root: Root) {
        self.0 |= root.bit();
    }
    pub fn remove(&mut self, root: Root) {
        self.0 &= !root.bit();
    }
    pub fn of(roots: &[Root]) -> Self {
        let mut set = Self::default();
        for root in roots {
            set.insert(*root);
        }
        set
    }
}
/// The allowed regions of one `model-1` job shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Regions {
    pub read: RootSet,
    pub exec: RootSet,
    pub write: RootSet,
}
impl Regions {
    /// Today's `model-1` profile: every root readable and executable, the
    /// project root and scratch writable.
    pub fn template() -> Self {
        Self {
            read: RootSet::of(&ROOTS),
            exec: RootSet::of(&ROOTS),
            write: RootSet::of(&[Root::Project, Root::Scratch]),
        }
    }
}

/// The fixed runtime set literals that every `model-1` profile allows
/// (read), with `/dev/null` also written.
const RUNTIME_READ: [&str; 5] = [
    "/",
    "/private/var/select/sh",
    "/dev/null",
    "/dev/random",
    "/dev/urandom",
];
const RUNTIME_WRITE: [&str; 1] = ["/dev/null"];
/// The entry program of every job today.
const ENTRY: &str = "/bin/sh";

fn path_of(principal: &JobPrincipal, root: Root) -> PathRef {
    match root {
        Root::Project => principal.project_root.clone(),
        Root::Scratch => principal.scratch.clone(),
        system => PathRef::ordinary(system.system_path().expect("system root")),
    }
}
fn file(principal: &JobPrincipal, action: Action, path: PathRef, now_ms: i64) -> Request {
    Request {
        principal: principal.clone(),
        action,
        resource: Resource::Path(path),
        context: Context::File {
            now_ms,
            intent: None,
        },
    }
}
fn exec(
    principal: &JobPrincipal,
    program: ProgramRef,
    now_ms: i64,
    request_digest: String,
    intent: Option<Intent>,
) -> Request {
    Request {
        principal: principal.clone(),
        action: Action::ProcessExec,
        resource: Resource::Program(program),
        context: Context::Exec {
            now_ms,
            request_digest,
            env_terminal: Vec::new(),
            env_declared: Vec::new(),
            outside_switch: false,
            intent,
        },
    }
}
/// A failed region request: a request error stays itself; anything else
/// is `cannot_enforce`.
fn unenforceable(decision: Decision) -> Decision {
    let request_error = decision.primary.as_ref().is_some_and(|p| {
        p.scope.is_none()
            && (p.reason == ErrorClass::RequestInvalid.reason()
                || p.reason == ErrorClass::RequestTooLarge.reason())
    });
    if request_error {
        decision
    } else {
        Decision::cannot_enforce(decision.scopes)
    }
}

/// Compile the `model-1` regions of one principal shape: each template
/// root enters a set only when every applicable scope allows every action
/// of that set on it, with no intent and no risk. A denied runtime literal
/// or a missing base need (`/bin` and scratch readable and executable,
/// scratch writable) denies the job with `cannot_enforce`.
pub fn compile_model1(
    engine: &Engine,
    principal: &JobPrincipal,
    now_ms: i64,
) -> Result<Regions, Decision> {
    let mut regions = Regions {
        read: RootSet::default(),
        exec: RootSet::default(),
        write: RootSet::default(),
    };
    let mut first_failure: Option<(Root, Decision)> = None;
    let mut note = |root: Root, decision: Decision| {
        if first_failure.is_none() {
            first_failure = Some((root, decision));
        }
    };
    for root in ROOTS {
        let path = path_of(principal, root);
        let read = engine.decide(&file(principal, Action::FileRead, path.clone(), now_ms));
        if read.allow {
            regions.read.insert(root);
        } else {
            note(root, read);
        }
        let run = engine.decide(&exec(
            principal,
            ProgramRef::unidentified(path.clone()),
            now_ms,
            String::new(),
            None,
        ));
        if run.allow {
            regions.exec.insert(root);
        } else {
            note(root, run);
        }
        if matches!(root, Root::Project | Root::Scratch) {
            let mut all = true;
            for action in [Action::FileWrite, Action::FileCreate, Action::FileDelete] {
                let write = engine.decide(&file(principal, action, path.clone(), now_ms));
                if !write.allow {
                    all = false;
                    note(root, write);
                    break;
                }
            }
            if all {
                regions.write.insert(root);
            }
        }
    }
    for literal in RUNTIME_READ {
        let read = engine.decide(&file(
            principal,
            Action::FileRead,
            PathRef::ordinary(literal),
            now_ms,
        ));
        if !read.allow {
            return Err(unenforceable(read));
        }
    }
    for literal in RUNTIME_WRITE {
        let write = engine.decide(&file(
            principal,
            Action::FileWrite,
            PathRef::ordinary(literal),
            now_ms,
        ));
        if !write.allow {
            return Err(unenforceable(write));
        }
    }
    let base = regions.read.contains(Root::Bin)
        && regions.read.contains(Root::Scratch)
        && regions.exec.contains(Root::Bin)
        && regions.exec.contains(Root::Scratch)
        && regions.write.contains(Root::Scratch);
    if !base {
        let decision = first_failure
            .filter(|(root, _)| matches!(root, Root::Bin | Root::Scratch))
            .map(|(_, d)| d)
            .unwrap_or_else(|| Decision::cannot_enforce(Vec::new()));
        return Err(unenforceable(decision));
    }
    Ok(regions)
}

/// The decision of one model `exec_cmd` job: its regions (cached or
/// compiled) and its entry `process.exec` of `/bin/sh` with the intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobOutcome {
    pub decision: Decision,
    /// Present only when the job may run.
    pub regions: Option<Regions>,
    pub resource_digest: [u8; 32],
    pub request_digest: [u8; 32],
    pub intent_digest: Option<[u8; 32]>,
}

fn entry_program() -> ProgramRef {
    ProgramRef::unidentified(PathRef::ordinary(ENTRY))
}
fn program_digest(program: &ProgramRef) -> [u8; 32] {
    let p = &program.path;
    sha256(
        format!(
            "asura-policy-resource/1\0program\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            p.path,
            p.class.as_str(),
            u8::from(p.in_project),
            p.rel,
            u8::from(program.identity_known),
            u8::from(program.platform),
            program.team_id,
            program.signing_id,
            program.cdhash
        )
        .as_bytes(),
    )
}
fn intent_digest(intent: &Intent) -> [u8; 32] {
    sha256(
        format!(
            "asura-policy-intent/1\0{}\0{}\0{}",
            intent.source.as_str(),
            intent.name,
            intent.digest
        )
        .as_bytes(),
    )
}

/// Decide one `model-1` job. `cached` is the principal shape's regions
/// under the same revision, when the owner has them.
pub fn decide_model1_job(
    engine: &Engine,
    principal: &JobPrincipal,
    intent: Option<&Intent>,
    now_ms: i64,
    cached: Option<Regions>,
) -> JobOutcome {
    let program = entry_program();
    let resource_digest = program_digest(&program);
    let intent_digest = intent.map(intent_digest);
    let request_digest = sha256(
        format!(
            "asura-policy-request/1\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            principal.job,
            principal.origin.as_str(),
            principal.profile.as_str(),
            principal.project,
            principal.project_root.path,
            principal.task,
            principal.scratch.path,
            Action::ProcessExec.as_str(),
            hex(&resource_digest),
            intent_digest.map(|d| hex(&d)).unwrap_or_default()
        )
        .as_bytes(),
    );
    let regions = match cached {
        Some(regions) => Ok(regions),
        None => compile_model1(engine, principal, now_ms),
    };
    let (decision, regions) = match regions {
        Err(decision) => (decision, None),
        Ok(regions) => {
            let decision = engine.decide(&exec(
                principal,
                program,
                now_ms,
                hex(&request_digest),
                intent.cloned(),
            ));
            let regions = decision.allow.then_some(regions);
            (decision, regions)
        }
    };
    JobOutcome {
        decision,
        regions,
        resource_digest,
        request_digest,
        intent_digest,
    }
}

/// The principal of a model `exec_cmd` job: scratch
/// `<asura_home>/tmp/exec-cmd-<job_hex>` (`asura_state`), the project root
/// (`ordinary`, in the project, `rel` empty) and no developer directory.
pub fn model1_principal(
    job_hex: &str,
    project_hex: &str,
    project_root: &str,
    task: &str,
    asura_home: &str,
) -> JobPrincipal {
    JobPrincipal {
        job: job_hex.to_owned(),
        origin: Origin::Model,
        profile: ProfileKind::Model1,
        project: project_hex.to_owned(),
        project_root: PathRef {
            path: project_root.to_owned(),
            class: PathClass::Ordinary,
            in_project: true,
            rel: String::new(),
        },
        task: task.to_owned(),
        scratch: PathRef {
            path: format!(
                "{}/tmp/exec-cmd-{job_hex}",
                asura_home.trim_end_matches('/')
            ),
            class: PathClass::AsuraState,
            in_project: false,
            rel: String::new(),
        },
        cache_roots: Vec::new(),
        developer_dir: None,
    }
}
