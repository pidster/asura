package asura.decision

import rego.v1

denies contains d if some d in data.asura.installation.deny
denies contains d if some d in data.asura.classifier.deny
denies contains d if some d in data.asura.user.deny
denies contains d if some d in data.asura.workspace.deny
denies contains d if some d in data.asura.repository.deny

permits contains p if some p in data.asura.installation.permit
permits contains p if some p in data.asura.user.permit
permits contains p if some p in data.asura.task.permit

result := {"allow": false, "rules": denies} if count(denies) > 0

result := {"allow": true, "rules": permits} if {
	count(denies) == 0
	count(permits) > 0
}

result := {"allow": false, "rules": set()} if {
	count(denies) == 0
	count(permits) == 0
}
