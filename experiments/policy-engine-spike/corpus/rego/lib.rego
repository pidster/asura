package asura.lib

import rego.v1

job := data.entities.jobs[input.principal.id] if input.principal.type == "Job"

task := data.entities.tasks[input.principal.id] if input.principal.type == "Task"

executable := data.entities.executables[input.resource.id] if input.resource.type == "Executable"

exec_any := {"exec", "exec_request_approval", "exec_approved"}

# Reflexive and transitive containment over host-computed ancestors.
path_in(p, root) if p == root

path_in(p, root) if root in data.entities.paths[p].ancestors

domain_in(d, root) if d == root

domain_in(d, root) if root in data.entities.domains[d].ancestors

path_class(p) := data.entities.paths[p].class
