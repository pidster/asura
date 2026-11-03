package asura.classifier

import rego.v1

import data.asura.lib

deny contains {"id": "clf.dangerous", "reason": "classifier_dangerous"} if {
	input.action == "exec"
	lib.job.origin == "model"
	input.context.risk == "dangerous"
}

deny contains {"id": "clf.unknown", "reason": "classifier_unknown"} if {
	input.action == "exec"
	lib.job.origin == "model"
	input.context.risk == "unknown"
}

deny contains {"id": "clf.absent", "reason": "classifier_unknown"} if {
	input.action == "exec"
	lib.job.origin == "model"
	not input.context.risk
}
