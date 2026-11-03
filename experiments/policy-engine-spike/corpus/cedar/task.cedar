@id("env.t-studio-1.write") @scope("task") @envelope("t-studio-1") @reason("envelope_write")
permit (principal is Asura::Job, action == Asura::Action::"write", resource is Asura::Path)
when { principal.task == Asura::Task::"t-studio-1" &&
       resource in Asura::Path::"/Users/person/src/shared/out" &&
       context.now < datetime("2026-11-03T18:00:00Z") };

@id("env.t-studio-1.net") @scope("task") @envelope("t-studio-1") @reason("envelope_network")
permit (principal is Asura::Job, action == Asura::Action::"connect", resource)
when { principal.task == Asura::Task::"t-studio-1" &&
       ((resource in Asura::Domain::"github.com" && context.port == 443) ||
        (context.ip.isInRange(ip("140.82.112.0/20")) && context.port == 22)) &&
       context.now < datetime("2026-11-03T18:00:00Z") };

@id("env.t-studio-1.secret") @scope("task") @envelope("t-studio-1") @reason("envelope_secret")
permit (principal is Asura::Job, action == Asura::Action::"secret_use", resource is Asura::Secret)
when { principal.task == Asura::Task::"t-studio-1" &&
       resource == Asura::Secret::"gh_token" &&
       context.delivery == Asura::Delivery::"brokered" &&
       context.now < datetime("2026-11-03T18:00:00Z") };

@id("grant.a1") @scope("task") @reason("approved_by_person")
permit (principal == Asura::Job::"j-studio-model", action == Asura::Action::"exec_approved",
        resource == Asura::Executable::"/usr/bin/git")
when { context.request_digest == "sha256:4f1c0d2a" &&
       context.now < datetime("2026-11-03T18:00:00Z") };
