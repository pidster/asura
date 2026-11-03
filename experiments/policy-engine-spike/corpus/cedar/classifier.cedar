@id("clf.dangerous") @scope("installation") @classifier("risk") @reason("classifier_dangerous")
forbid (principal is Asura::Job, action == Asura::Action::"exec", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"model" && context has risk &&
       context.risk == Asura::Risk::"dangerous" };

@id("clf.unknown") @scope("installation") @classifier("risk") @reason("classifier_unknown")
forbid (principal is Asura::Job, action == Asura::Action::"exec", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"model" && context has risk &&
       context.risk == Asura::Risk::"unknown" };

@id("clf.absent") @scope("installation") @classifier("risk") @reason("classifier_unknown")
forbid (principal is Asura::Job, action == Asura::Action::"exec", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"model" && !(context has risk) };
