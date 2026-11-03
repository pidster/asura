package asura.user

import rego.v1

import data.asura.lib

permit contains {"id": "user.egress.studio.anthropic", "reason": "egress_grant"} if {
	input.action == "egress"
	input.resource.id == "anthropic"
	lib.task.project == "studio"
	input.context.data_class in {"prompt", "history", "notice"}
}

permit contains {"id": "user.egress.observatory.anthropic", "reason": "egress_grant"} if {
	input.action == "egress"
	input.resource.id == "anthropic"
	lib.task.project == "observatory"
	input.context.data_class in {"prompt", "history"}
}

deny contains {"id": "user.forbid.ssh_dir", "reason": "user_write_limit"} if {
	input.action == "write"
	lib.path_in(input.resource.id, "/Users/person/.ssh")
}
