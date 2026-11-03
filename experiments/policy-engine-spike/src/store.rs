//! Engine-neutral entity store with encoders for Cedar entity JSON and the
//! Rego data document. Paths and domains carry their immediate parent; the
//! Rego encoder computes the transitive ancestor list that Cedar computes itself.

use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct JobE {
    pub origin: &'static str,
    pub profile: &'static str,
    pub project: String,
    pub task: String,
    pub scratch: String,
    pub cache_roots: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Store {
    /// path -> (class, parent)
    pub paths: BTreeMap<String, (&'static str, Option<String>)>,
    /// project -> (root, declared_env)
    pub projects: BTreeMap<String, (String, Vec<String>)>,
    /// task -> project
    pub tasks: BTreeMap<String, String>,
    pub jobs: BTreeMap<String, JobE>,
    /// executable -> tool
    pub executables: BTreeMap<String, String>,
    /// domain -> parent
    pub domains: BTreeMap<String, Option<String>>,
    /// secret -> project
    pub secrets: BTreeMap<String, String>,
    /// destination -> remote
    pub destinations: BTreeMap<String, bool>,
}

fn parent_of(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(i) => Some(path[..i].to_string()),
        None => None,
    }
}

impl Store {
    /// Add a path and any missing ancestors. Ancestors inherit `ordinary`
    /// unless they already exist; `class_for` lets a caller classify them.
    pub fn add_path(&mut self, path: &str, class: &'static str) {
        let mut cur = Some(path.to_string());
        let mut first = true;
        while let Some(p) = cur {
            let parent = parent_of(&p);
            if first {
                self.paths.insert(p.clone(), (class, parent.clone()));
                first = false;
            } else if self.paths.contains_key(&p) {
                break;
            } else {
                self.paths.insert(p.clone(), ("ordinary", parent.clone()));
            }
            cur = parent;
        }
    }

    pub fn add_domain(&mut self, domain: &str) {
        let mut cur = Some(domain.to_string());
        while let Some(d) = cur {
            if self.domains.contains_key(&d) {
                break;
            }
            let parent = d.split_once('.').map(|(_, rest)| rest.to_string());
            self.domains.insert(d, parent.clone());
            cur = parent;
        }
    }

    pub fn entity_count(&self) -> usize {
        self.paths.len()
            + self.projects.len()
            + self.tasks.len()
            + self.jobs.len()
            + self.executables.len()
            + self.domains.len()
            + self.secrets.len()
            + self.destinations.len()
    }

    pub fn to_cedar_json(&self) -> Value {
        fn uid(t: &str, id: &str) -> Value {
            json!({"__entity": {"type": format!("Asura::{t}"), "id": id}})
        }
        fn ent(t: &str, id: &str, attrs: Value, parents: Vec<Value>) -> Value {
            json!({"uid": {"type": format!("Asura::{t}"), "id": id}, "attrs": attrs, "parents": parents})
        }
        let pref = |t: &str, id: &str| json!({"type": format!("Asura::{t}"), "id": id});
        let mut out = Vec::with_capacity(self.entity_count());
        for (p, (class, parent)) in &self.paths {
            let parents = parent.iter().map(|q| pref("Path", q)).collect();
            out.push(ent(
                "Path",
                p,
                json!({"class": uid("PathClass", class)}),
                parents,
            ));
        }
        for (id, (root, env)) in &self.projects {
            out.push(ent(
                "Project",
                id,
                json!({"root": uid("Path", root), "declared_env": env}),
                vec![],
            ));
        }
        for (id, project) in &self.tasks {
            out.push(ent(
                "Task",
                id,
                json!({"project": uid("Project", project)}),
                vec![],
            ));
        }
        for (id, j) in &self.jobs {
            let roots: Vec<Value> = j.cache_roots.iter().map(|r| uid("Path", r)).collect();
            out.push(ent(
                "Job",
                id,
                json!({
                    "origin": uid("Origin", j.origin),
                    "profile": uid("Profile", j.profile),
                    "project": uid("Project", &j.project),
                    "task": uid("Task", &j.task),
                    "scratch": uid("Path", &j.scratch),
                    "cache_roots": roots,
                }),
                vec![],
            ));
        }
        for (id, tool) in &self.executables {
            out.push(ent("Executable", id, json!({"tool": tool}), vec![]));
        }
        for (d, parent) in &self.domains {
            let parents = parent.iter().map(|q| pref("Domain", q)).collect();
            out.push(ent("Domain", d, json!({}), parents));
        }
        for (id, project) in &self.secrets {
            out.push(ent(
                "Secret",
                id,
                json!({"project": uid("Project", project)}),
                vec![],
            ));
        }
        for (id, remote) in &self.destinations {
            out.push(ent("Destination", id, json!({"remote": remote}), vec![]));
        }
        Value::Array(out)
    }

    fn ancestors<F: Fn(&str) -> Option<String>>(start: &str, parent: F) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = parent(start);
        while let Some(p) = cur {
            cur = parent(&p);
            out.push(p);
        }
        out
    }

    /// The Rego data document: `{"entities": {...}}`.
    pub fn to_rego_data(&self) -> Value {
        let mut paths = Map::new();
        for (p, (class, _)) in &self.paths {
            let anc = Self::ancestors(p, |q| self.paths.get(q).and_then(|(_, par)| par.clone()));
            paths.insert(p.clone(), json!({"class": class, "ancestors": anc}));
        }
        let mut domains = Map::new();
        for d in self.domains.keys() {
            let anc = Self::ancestors(d, |q| self.domains.get(q).cloned().flatten());
            domains.insert(d.clone(), json!({"ancestors": anc}));
        }
        let projects: Map<String, Value> = self
            .projects
            .iter()
            .map(|(id, (root, env))| (id.clone(), json!({"root": root, "declared_env": env})))
            .collect();
        let tasks: Map<String, Value> = self
            .tasks
            .iter()
            .map(|(id, p)| (id.clone(), json!({"project": p})))
            .collect();
        let jobs: Map<String, Value> = self
            .jobs
            .iter()
            .map(|(id, j)| {
                (
                    id.clone(),
                    json!({"origin": j.origin, "profile": j.profile, "project": j.project,
                        "task": j.task, "scratch": j.scratch, "cache_roots": j.cache_roots}),
                )
            })
            .collect();
        let executables: Map<String, Value> = self
            .executables
            .iter()
            .map(|(id, t)| (id.clone(), json!({"tool": t})))
            .collect();
        let secrets: Map<String, Value> = self
            .secrets
            .iter()
            .map(|(id, p)| (id.clone(), json!({"project": p})))
            .collect();
        let destinations: Map<String, Value> = self
            .destinations
            .iter()
            .map(|(id, r)| (id.clone(), json!({"remote": r})))
            .collect();
        json!({"entities": {
            "paths": paths, "domains": domains, "projects": projects, "tasks": tasks,
            "jobs": jobs, "executables": executables, "secrets": secrets,
            "destinations": destinations,
        }})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_path_creates_ancestors_and_rego_lists_them() {
        let mut s = Store::default();
        s.add_path("/a/b/c", "implicit_exec");
        assert_eq!(s.paths.len(), 4);
        assert_eq!(s.paths["/a/b/c"].0, "implicit_exec");
        assert_eq!(s.paths["/a"].0, "ordinary");
        let data = s.to_rego_data();
        assert_eq!(
            data["entities"]["paths"]["/a/b/c"]["ancestors"],
            json!(["/a/b", "/a", "/"])
        );
    }

    #[test]
    fn add_domain_creates_parent_domains() {
        let mut s = Store::default();
        s.add_domain("api.github.com");
        assert_eq!(s.domains["api.github.com"], Some("github.com".into()));
        assert_eq!(s.domains["com"], None);
    }
}
