@id("repo.studio.net") @scope("repository") @reason("repository_network_limit")
forbid (principal is Asura::Job, action == Asura::Action::"connect", resource)
when { principal.origin == Asura::Origin::"model" &&
       principal.project == Asura::Project::"studio" }
unless { (resource in Asura::Domain::"github.com" && context.port == 443) ||
         (context.ip.isInRange(ip("140.82.112.0/20")) && context.port == 22) };
