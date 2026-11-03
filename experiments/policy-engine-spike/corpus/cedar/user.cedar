@id("user.egress.studio.anthropic") @scope("user") @reason("egress_grant")
permit (principal is Asura::Task, action == Asura::Action::"egress", resource == Asura::Destination::"anthropic")
when { principal.project == Asura::Project::"studio" &&
       [Asura::DataClass::"prompt", Asura::DataClass::"history", Asura::DataClass::"notice"].contains(context.data_class) };

@id("user.egress.observatory.anthropic") @scope("user") @reason("egress_grant")
permit (principal is Asura::Task, action == Asura::Action::"egress", resource == Asura::Destination::"anthropic")
when { principal.project == Asura::Project::"observatory" &&
       [Asura::DataClass::"prompt", Asura::DataClass::"history"].contains(context.data_class) };

@id("user.forbid.ssh_dir") @scope("user") @reason("user_write_limit")
forbid (principal is Asura::Job, action == Asura::Action::"write", resource is Asura::Path)
when { resource in Asura::Path::"/Users/person/.ssh" };
