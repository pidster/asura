@id("ws.studio.vendor") @scope("workspace") @reason("workspace_write_limit")
forbid (principal is Asura::Job, action == Asura::Action::"write", resource is Asura::Path)
when { principal.origin == Asura::Origin::"model" &&
       resource in Asura::Path::"/Users/person/src/studio/vendor" };

@id("ws.observatory.no_remote_history") @scope("workspace") @reason("workspace_egress_limit")
forbid (principal is Asura::Task, action == Asura::Action::"egress", resource is Asura::Destination)
when { principal.project == Asura::Project::"observatory" && resource.remote &&
       context.data_class == Asura::DataClass::"history" };
