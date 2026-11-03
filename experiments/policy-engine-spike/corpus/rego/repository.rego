package asura.repository

import rego.v1

import data.asura.lib

deny contains {"id": "repo.studio.net", "reason": "repository_network_limit"} if {
	input.action == "connect"
	lib.job.origin == "model"
	lib.job.project == "studio"
	not studio_net_allowed
}

studio_net_allowed if {
	input.resource.type == "Domain"
	lib.domain_in(input.resource.id, "github.com")
	input.context.port == 443
}

studio_net_allowed if {
	net.cidr_contains("140.82.112.0/20", input.context.ip)
	input.context.port == 22
}
