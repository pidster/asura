//! Generated specification policies of the proofs (security-policy.md,
//! PL3 admission and PL3 compile step). Every text is fixed or built from
//! the closed root and action lists; no candidate text enters them.

/// The person profiles of SP-AQ9.
pub(crate) const PERSON_PROFILES: [&str; 2] = ["person-1", "person-outside-1"];

/// SP-AQ9: every `network.connect` of a person job of `profile`.
pub(crate) fn person_network(profile: &str) -> String {
    format!(
        r#"permit (principal is Asura::Job, action == Asura::Action::"network.connect", resource) when {{ principal.origin == Asura::Origin::"person_typed" && principal.profile == Asura::Profile::"{profile}" }};"#
    )
}

/// The `model-1` template roots as Cedar expressions, in render order.
pub(crate) const ROOTS: [(&str, &str); 9] = [
    ("/bin", r#"Asura::Path::"/bin""#),
    ("/usr/bin", r#"Asura::Path::"/usr/bin""#),
    ("/sbin", r#"Asura::Path::"/sbin""#),
    ("/usr/sbin", r#"Asura::Path::"/usr/sbin""#),
    ("/usr/lib", r#"Asura::Path::"/usr/lib""#),
    ("/usr/share", r#"Asura::Path::"/usr/share""#),
    ("/System", r#"Asura::Path::"/System""#),
    ("project", "principal.project.root"),
    ("scratch", "principal.scratch"),
];

/// One compile check (a) pair: the region class, its action, the root.
#[derive(Clone, Debug)]
pub(crate) struct Region {
    pub name: String,
    pub action: &'static str,
    /// `S_rc`: every request of the class inside the root, minus the
    /// guardian's fixed deny rules.
    pub full: String,
    /// `R_rc`: the root itself.
    pub root: String,
}

/// A `model-1` region request as the compiler makes it: a model-origin
/// job of profile `model-1` (the entity loader never builds another
/// combination), with no intent and no risk.
const MODEL: &str = r#"principal.origin == Asura::Origin::"model" && principal.profile == Asura::Profile::"model-1" && !(context has intent) && !(context has risk)"#;

/// The 24 region pairs of compile check (a).
pub(crate) fn regions() -> Vec<Region> {
    let mut out = Vec::new();
    for (name, root) in ROOTS {
        out.push(Region {
            name: format!("read {name}"),
            action: "file.read",
            full: format!(
                r#"permit (principal is Asura::Job, action == Asura::Action::"file.read", resource is Asura::Path) when {{ {MODEL} && resource in {root} && !(resource.class == Asura::PathClass::"asura_state" && !(resource in principal.scratch)) }};"#
            ),
            root: format!(
                r#"permit (principal is Asura::Job, action == Asura::Action::"file.read", resource is Asura::Path) when {{ {MODEL} && resource == {root} }};"#
            ),
        });
        out.push(Region {
            name: format!("exec {name}"),
            action: "process.exec",
            full: format!(
                r#"permit (principal is Asura::Job, action == Asura::Action::"process.exec", resource is Asura::Program) when {{ {MODEL} && !resource.identity_known && resource.path in {root} && !(resource.path.class == Asura::PathClass::"asura_state" && !(resource.path in principal.scratch)) }};"#
            ),
            root: format!(
                r#"permit (principal is Asura::Job, action == Asura::Action::"process.exec", resource is Asura::Program) when {{ {MODEL} && !resource.identity_known && resource.path == {root} }};"#
            ),
        });
    }
    for (name, root) in &ROOTS[7..] {
        for action in ["file.write", "file.create", "file.delete"] {
            out.push(Region {
                name: format!("{} {name}", action.trim_start_matches("file.")),
                action,
                full: format!(
                    r#"permit (principal is Asura::Job, action == Asura::Action::"{action}", resource is Asura::Path) when {{ {MODEL} && resource in {root} && !(resource.class == Asura::PathClass::"asura_state" && !(resource in principal.scratch)) && resource.class != Asura::PathClass::"implicit_exec" }};"#
                ),
                root: format!(
                    r#"permit (principal is Asura::Job, action == Asura::Action::"{action}", resource is Asura::Path) when {{ {MODEL} && resource == {root} }};"#
                ),
            });
        }
    }
    out
}

const PERSON: &str = r#"(principal.profile == Asura::Profile::"person-1" || principal.profile == Asura::Profile::"person-outside-1")"#;

/// Compile check (b): the restriction forbid of one environment's action,
/// `None` when every request of the action is restricted, and the action
/// is not restricted at all when it is absent from the list.
pub(crate) fn restriction(action: &str) -> Option<Option<String>> {
    match action {
        "process.exec.ask" | "process.exec.approved" | "mach.lookup" | "context.egress" => {
            Some(None)
        }
        "file.read" | "file.write" | "file.create" | "file.delete" => Some(Some(format!(
            "forbid (principal is Asura::Job, action, resource) unless {{ {PERSON} }};"
        ))),
        "process.exec" => Some(Some(format!(
            "forbid (principal is Asura::Job, action, resource) unless {{ {PERSON} && !(context has intent) }};"
        ))),
        _ => None,
    }
}

/// The baseline permit of a workspace or repository set.
pub(crate) const BASELINE: &str = "permit (principal, action, resource);";

/// SP-AQ4 safety specifications S1 to S6, each "permit all, then forbid
/// the property": (id, action, forbid text). The same texts as the SV-P1
/// admission probe.
pub(crate) const SAFETY: [(&str, &str, &str); 6] = [
    (
        "S1",
        "file.write",
        r#"forbid (principal is Asura::Job, action in [Asura::Action::"file.write", Asura::Action::"file.create", Asura::Action::"file.delete"], resource is Asura::Path) when { principal.origin == Asura::Origin::"model" && resource.class == Asura::PathClass::"implicit_exec" };"#,
    ),
    (
        "S2",
        "file.write",
        r#"forbid (principal is Asura::Job, action in [Asura::Action::"file.write", Asura::Action::"file.create", Asura::Action::"file.delete"], resource is Asura::Path) when { resource.class == Asura::PathClass::"asura_state" && !(resource in principal.scratch) };"#,
    ),
    (
        "S3",
        "network.connect",
        r#"forbid (principal is Asura::Job, action == Asura::Action::"network.connect", resource) when { principal.origin == Asura::Origin::"model" && !context.via_proxy };"#,
    ),
    (
        "S4",
        "secret_use",
        r#"forbid (principal is Asura::Job, action == Asura::Action::"secret_use", resource) when { principal.origin == Asura::Origin::"model" };"#,
    ),
    (
        "S5",
        "context.egress",
        r#"forbid (principal is Asura::Task, action == Asura::Action::"context.egress", resource) when { context.tainted };"#,
    ),
    (
        "S6",
        "process.exec.approved",
        r#"forbid (principal, action == Asura::Action::"process.exec.approved", resource);"#,
    ),
];
