package asura.installation

import rego.v1

import data.asura.lib

permit contains {"id": "inst.model.write", "reason": "write_project_or_scratch"} if {
	input.action == "write"
	input.resource.type == "Path"
	lib.job.profile == "model-1"
	some root in [data.entities.projects[lib.job.project].root, lib.job.scratch]
	lib.path_in(input.resource.id, root)
}

permit contains {"id": "inst.person.write", "reason": "person_write_roots"} if {
	input.action == "write"
	input.resource.type == "Path"
	lib.job.profile == "person-1"
	roots := {data.entities.projects[lib.job.project].root, lib.job.scratch} | {r | some r in lib.job.cache_roots}
	some root in roots
	lib.path_in(input.resource.id, root)
}

deny contains {"id": "inst.hard.implicit_exec", "reason": "implicit_exec_path"} if {
	input.action == "write"
	lib.job.origin == "model"
	lib.path_class(input.resource.id) == "implicit_exec"
}

deny contains {"id": "inst.hard.asura_state", "reason": "asura_state"} if {
	input.action == "write"
	input.principal.type == "Job"
	lib.path_class(input.resource.id) == "asura_state"
	not lib.path_in(input.resource.id, lib.job.scratch)
}

permit contains {"id": "inst.model.exec", "reason": "model_exec"} if {
	input.action == "exec"
	input.resource.type == "Executable"
	lib.job.profile == "model-1"
}

permit contains {"id": "inst.model.exec_approval", "reason": "model_exec_approval_offered"} if {
	input.action == "exec_request_approval"
	input.resource.type == "Executable"
	lib.job.origin == "model"
}

deny contains {"id": "inst.hard.model_privilege", "reason": "privilege_escalation"} if {
	input.action in lib.exec_any
	lib.job.origin == "model"
	lib.executable.tool in {"sudo", "su", "doas"}
}

deny contains {"id": "inst.hard.model_secret_reference", "reason": "model_secret_reference"} if {
	input.action in lib.exec_any
	lib.job.origin == "model"
	input.context.command.secret_reference == true
}

deny contains {"id": "inst.model.env_clean", "reason": "model_env_not_clean"} if {
	input.action in lib.exec_any
	lib.job.origin == "model"
	count(input.context.command.env_terminal) + count(input.context.command.env_declared) > 0
}

deny contains {"id": "inst.model.shell_features", "reason": "shell_feature_needs_approval"} if {
	input.action == "exec"
	lib.job.origin == "model"
	input.context.command.parse == "unparsed"
}

deny contains {"id": "inst.model.shell_features", "reason": "shell_feature_needs_approval"} if {
	input.action == "exec"
	lib.job.origin == "model"
	some f in input.context.command.shell_features
	f in {"eval", "pipe_to_interpreter", "command_substitution_word"}
}

deny contains {"id": "inst.model.git_push", "reason": "git_push_needs_approval"} if {
	input.action == "exec"
	lib.job.origin == "model"
	lib.executable.tool == "git"
	input.context.command.subcommand == "push"
}

deny contains {"id": "inst.model.git_force", "reason": "git_force_needs_approval"} if {
	input.action == "exec"
	lib.job.origin == "model"
	lib.executable.tool == "git"
	some f in input.context.command.flags
	f in {"--force", "-f", "--force-with-lease"}
}

deny contains {"id": "inst.model.publish", "reason": "publish_needs_approval"} if {
	input.action == "exec"
	lib.job.origin == "model"
	lib.executable.tool in {"cargo", "npm"}
	startswith(input.context.command.subcommand, "publish")
}

terminal_allow := {
	"PATH", "LANG", "LC_ALL", "LC_COLLATE", "LC_CTYPE", "LC_MESSAGES", "LC_MONETARY",
	"LC_NUMERIC", "LC_TIME", "SSH_AUTH_SOCK", "TZ", "XDG_CONFIG_HOME", "XDG_CACHE_HOME",
	"XDG_DATA_HOME", "XDG_STATE_HOME", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN",
	"CARGO_TARGET_DIR", "DEVELOPER_DIR", "SDKROOT", "TOOLCHAINS", "HOMEBREW_PREFIX",
	"HOMEBREW_CELLAR", "HOMEBREW_REPOSITORY", "GOPATH", "GOROOT", "GOMODCACHE", "GOCACHE",
	"NVM_DIR", "VOLTA_HOME", "PNPM_HOME", "BUN_INSTALL", "PYENV_ROOT", "VIRTUAL_ENV",
	"CONDA_PREFIX", "JAVA_HOME", "RBENV_ROOT", "GEM_HOME", "GEM_PATH",
}

permit contains {"id": "inst.person.exec", "reason": "person_exec"} if {
	input.action == "exec"
	input.resource.type == "Executable"
	lib.job.origin == "person_typed"
	lib.job.profile == "person-1"
}

permit contains {"id": "inst.person.exec_outside", "reason": "person_exec_outside"} if {
	input.action == "exec"
	input.resource.type == "Executable"
	lib.job.origin == "person_typed"
	lib.job.profile == "person-outside-1"
	input.context.outside_switch == true
}

deny contains {"id": "inst.person.env_terminal", "reason": "env_not_allow_listed"} if {
	input.action in lib.exec_any
	lib.job.origin == "person_typed"
	some n in input.context.command.env_terminal
	not n in terminal_allow
}

deny contains {"id": "inst.person.env_declared", "reason": "env_not_declared"} if {
	input.action in lib.exec_any
	lib.job.origin == "person_typed"
	declared := {x | some x in data.entities.projects[lib.job.project].declared_env}
	some n in input.context.command.env_declared
	not n in declared
}

permit contains {"id": "inst.person.net", "reason": "person_network"} if {
	input.action == "connect"
	lib.job.origin == "person_typed"
}

deny contains {"id": "inst.hard.model_unmediated_net", "reason": "model_network_unmediated"} if {
	input.action == "connect"
	lib.job.origin == "model"
	input.context.via_proxy == false
}

deny contains {"id": "inst.hard.model_local_address", "reason": "model_network_local_address"} if {
	input.action == "connect"
	lib.job.origin == "model"
	some cidr in {"127.0.0.0/8", "::1/128", "169.254.0.0/16"}
	net.cidr_contains(cidr, input.context.ip)
}

permit contains {"id": "inst.person.secret", "reason": "person_secret_use"} if {
	input.action == "secret_use"
	lib.job.origin == "person_typed"
	data.entities.secrets[input.resource.id].project == lib.job.project
}

deny contains {"id": "inst.hard.model_raw_secret", "reason": "model_raw_secret"} if {
	input.action == "secret_use"
	lib.job.origin == "model"
	input.context.delivery == "raw"
}

deny contains {"id": "inst.hard.secret_other_project", "reason": "secret_other_project"} if {
	input.action == "secret_use"
	data.entities.secrets[input.resource.id].project != lib.job.project
}

permit contains {"id": "inst.egress.local", "reason": "egress_local"} if {
	input.action == "egress"
	data.entities.destinations[input.resource.id].remote == false
}

deny contains {"id": "inst.hard.egress_tainted", "reason": "egress_tainted"} if {
	input.action == "egress"
	input.context.tainted == true
}
