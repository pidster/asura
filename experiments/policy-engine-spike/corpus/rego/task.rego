package asura.task

import rego.v1

import data.asura.lib

permit contains {"id": "env.t-studio-1.write", "reason": "envelope_write"} if {
	input.action == "write"
	lib.job.task == "t-studio-1"
	lib.path_in(input.resource.id, "/Users/person/src/shared/out")
	input.context.now_ms < 1793728800000
}

permit contains {"id": "env.t-studio-1.net", "reason": "envelope_network"} if {
	input.action == "connect"
	lib.job.task == "t-studio-1"
	t_studio_1_endpoint
	input.context.now_ms < 1793728800000
}

t_studio_1_endpoint if {
	input.resource.type == "Domain"
	lib.domain_in(input.resource.id, "github.com")
	input.context.port == 443
}

t_studio_1_endpoint if {
	net.cidr_contains("140.82.112.0/20", input.context.ip)
	input.context.port == 22
}

permit contains {"id": "env.t-studio-1.secret", "reason": "envelope_secret"} if {
	input.action == "secret_use"
	lib.job.task == "t-studio-1"
	input.resource.id == "gh_token"
	input.context.delivery == "brokered"
	input.context.now_ms < 1793728800000
}

permit contains {"id": "grant.a1", "reason": "approved_by_person"} if {
	input.action == "exec_approved"
	input.principal.id == "j-studio-model"
	input.resource.id == "/usr/bin/git"
	input.context.request_digest == "sha256:4f1c0d2a"
	input.context.now_ms < 1793728800000
}
