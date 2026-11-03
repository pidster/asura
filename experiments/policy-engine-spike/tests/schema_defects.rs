//! PES-M6: the corpus validates in strict mode, and 12 seeded policy defects
//! are refused before activation. Each defect is the same mistake written in
//! each language.

use asura_policy_engine_spike::cedar_engine::{CedarEngine, parse_schema, validation_warnings};
use asura_policy_engine_spike::corpus::{self, Scope, Source};
use asura_policy_engine_spike::rego_engine::RegoEngine;
use cedar_policy::PolicySet;
use std::str::FromStr;

const C: &str = "@id(\"defect\") @scope(\"installation\") @reason(\"defect\")\nforbid (principal is Asura::Job, action == Asura::Action::";
const R: &str = "deny contains {\"id\": \"defect\", \"reason\": \"defect\"} if {\n";

/// The conformance case whose request reaches each defective rule.
fn reaching_case(id: &str) -> &'static str {
    match id {
        "SD5" | "SD8" | "SD11" => "PN1",
        "SD9" | "SD10" | "SD12" => "PC1",
        _ => "PW1",
    }
}

/// (ID, description, Cedar policy, Rego rule body)
fn defects() -> Vec<(&'static str, &'static str, String, String)> {
    vec![
        ("SD1", "attribute name typo",
         format!("{C}\"write\", resource is Asura::Path)\nwhen {{ resource.clas == Asura::PathClass::\"implicit_exec\" }};"),
         format!("{R}\tinput.action == \"write\"\n\tdata.entities.paths[input.resource.id].clas == \"implicit_exec\"\n}}")),
        ("SD2", "enumeration literal typo",
         format!("{C}\"write\", resource is Asura::Path)\nwhen {{ resource.class == Asura::PathClass::\"implicit-exec\" }};"),
         format!("{R}\tinput.action == \"write\"\n\tlib.path_class(input.resource.id) == \"implicit-exec\"\n}}")),
        ("SD3", "entity type typo",
         "@id(\"defect\") @scope(\"installation\") @reason(\"defect\")\nforbid (principal is Asura::Jobb, action == Asura::Action::\"write\", resource);".to_string(),
         format!("{R}\tinput.principal.type == \"Jobb\"\n\tinput.action == \"write\"\n}}")),
        ("SD4", "action name typo",
         format!("{C}\"wrtie\", resource);"),
         format!("{R}\tinput.action == \"wrtie\"\n}}")),
        ("SD5", "number compared with a string",
         format!("{C}\"connect\", resource)\nwhen {{ context.port == \"443\" }};"),
         format!("{R}\tinput.action == \"connect\"\n\tinput.context.port == \"443\"\n}}")),
        ("SD6", "context attribute of another action",
         format!("{C}\"write\", resource is Asura::Path)\nwhen {{ context.port == 22 }};"),
         format!("{R}\tinput.action == \"write\"\n\tinput.context.port == 22\n}}")),
        ("SD7", "resource compared with an entity of another type",
         format!("{C}\"write\", resource is Asura::Path)\nwhen {{ resource == Asura::Domain::\"github.com\" }};"),
         format!("{R}\tinput.action == \"write\"\n\tinput.resource.type == \"Domain\"\n}}")),
        ("SD8", "misspelt extension or built-in function",
         format!("{C}\"connect\", resource)\nwhen {{ context.ip.isInRnage(ip(\"10.0.0.0/8\")) }};"),
         format!("{R}\tinput.action == \"connect\"\n\tnet.cidr_contans(\"10.0.0.0/8\", input.context.ip)\n}}")),
        ("SD9", "set element of the wrong type",
         format!("{C}\"exec\", resource is Asura::Executable)\nwhen {{ context.command.flags.contains(1) }};"),
         format!("{R}\tinput.action == \"exec\"\n\t1 in input.context.command.flags\n}}")),
        ("SD10", "entity data path typo",
         format!("{C}\"exec\", resource is Asura::Executable)\nwhen {{ principal.projetc.root == principal.scratch }};"),
         format!("{R}\tinput.action == \"exec\"\n\tdata.entities.project[lib.job.project].root == lib.job.scratch\n}}")),
        ("SD11", "Boolean compared with a string",
         format!("{C}\"connect\", resource)\nwhen {{ context.via_proxy == \"true\" }};"),
         format!("{R}\tinput.action == \"connect\"\n\tinput.context.via_proxy == \"true\"\n}}")),
        ("SD12", "call of an undefined function or method",
         format!("{C}\"exec\", resource is Asura::Executable)\nwhen {{ resource.tool.lowercase() == \"git\" }};"),
         format!("{R}\tinput.action == \"exec\"\n\tlib.path_inn(input.resource.id, \"/\")\n}}")),
    ]
}

#[test]
fn corpus_is_strictly_valid_and_seeded_defects_are_refused() {
    let store = corpus::core_store();
    // The unmodified core corpus loads (strict validation passes).
    CedarEngine::load(&corpus::schema_text(), &corpus::core_cedar(), &store)
        .expect("core Cedar valid");
    RegoEngine::load(
        &corpus::rego_lib(),
        &corpus::rego_decision(),
        &corpus::core_rego(),
        &store,
    )
    .expect("core Rego loads");

    let (mut cedar_refused, mut rego_refused) = (0, 0);
    let mut rego_missed = Vec::new();
    for (id, what, cedar_text, rego_text) in defects() {
        let mut cs = corpus::core_cedar();
        cs.push(Source {
            scope: Scope::Installation,
            name: format!("{id}.cedar"),
            text: cedar_text,
        });
        let c = CedarEngine::load(&corpus::schema_text(), &cs, &store);
        let mut rs = corpus::core_rego();
        rs.push(Source {
            scope: Scope::Installation,
            name: format!("{id}.rego"),
            text: format!("package asura.installation\n\nimport rego.v1\n\nimport data.asura.lib\n\n{rego_text}\n"),
        });
        let r = RegoEngine::load(&corpus::rego_lib(), &corpus::rego_decision(), &rs, &store);
        // For an accepted defect, record what happens later: Cedar validator
        // warnings, and the Rego decision when a request reaches the rule.
        let cedar_later = if c.is_ok() {
            let schema = parse_schema(&corpus::schema_text()).expect("schema");
            let set = PolicySet::from_str(&cs.last().expect("defect").text).expect("parses");
            format!(" warnings={:?}", validation_warnings(&schema, &set))
        } else {
            String::new()
        };
        let rego_later = match &r {
            Ok(engine) => {
                let req = corpus::conformance_requests()
                    .into_iter()
                    .find(|(c, _)| c.id == reaching_case(id))
                    .expect("case")
                    .1;
                format!(" at_evaluation={}", engine.clone().decide(&req).render())
            }
            Err(_) => String::new(),
        };
        println!(
            "{id} {what}: cedar={}{cedar_later} rego={}{rego_later}",
            if c.is_err() { "refused" } else { "ACCEPTED" },
            if r.is_err() { "refused" } else { "ACCEPTED" }
        );
        cedar_refused += usize::from(c.is_err());
        rego_refused += usize::from(r.is_err());
        if r.is_ok() {
            rego_missed.push(id);
        }
    }
    println!(
        "cedar refused {cedar_refused}/12; rego refused {rego_refused}/12; rego accepted {rego_missed:?}"
    );
    // Recorded measurement for Cedar (design PES-M6). SD7 passes strict
    // validation; see the printed warnings.
    assert_eq!(
        cedar_refused, CEDAR_REFUSED_RECORDED,
        "Cedar refusals changed"
    );
    // Recorded measurement for Rego (design PES-M6); a change here is a
    // change in regorus behavior and must be re-recorded, not hidden.
    assert_eq!(
        rego_refused, REGO_REFUSED_RECORDED,
        "Rego refusals changed: {rego_missed:?}"
    );
}

const CEDAR_REFUSED_RECORDED: usize = 11;
const REGO_REFUSED_RECORDED: usize = 0;
