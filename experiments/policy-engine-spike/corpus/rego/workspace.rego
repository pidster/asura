package asura.workspace

import rego.v1

import data.asura.lib

deny contains {"id": "ws.studio.vendor", "reason": "workspace_write_limit"} if {
	input.action == "write"
	lib.job.origin == "model"
	lib.path_in(input.resource.id, "/Users/person/src/studio/vendor")
}

deny contains {"id": "ws.observatory.no_remote_history", "reason": "workspace_egress_limit"} if {
	input.action == "egress"
	lib.task.project == "observatory"
	data.entities.destinations[input.resource.id].remote == true
	input.context.data_class == "history"
}
