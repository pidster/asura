// Asura installation scope, policy set asura-policy/1 (security-policy.md,
// PL1 implementation contract, built-in sets). Changes only with a release.

// Ceiling permits of the model-1 template (today's profile) and the runtime set.

@id("model1.read") @reason("model1_region")
permit (principal is Asura::Job, action == Asura::Action::"file.read", resource is Asura::Path)
when {
  principal.profile == Asura::Profile::"model-1" &&
  (resource in Asura::Path::"/bin" || resource in Asura::Path::"/usr/bin" ||
   resource in Asura::Path::"/sbin" || resource in Asura::Path::"/usr/sbin" ||
   resource in Asura::Path::"/usr/lib" || resource in Asura::Path::"/usr/share" ||
   resource in Asura::Path::"/System" ||
   resource in principal.project.root || resource in principal.scratch ||
   resource == Asura::Path::"/" || resource == Asura::Path::"/private/var/select/sh" ||
   resource == Asura::Path::"/dev/null" || resource == Asura::Path::"/dev/random" ||
   resource == Asura::Path::"/dev/urandom")
};

@id("model1.write") @reason("model1_region")
permit (principal is Asura::Job,
        action in [Asura::Action::"file.write", Asura::Action::"file.create", Asura::Action::"file.delete"],
        resource is Asura::Path)
when {
  principal.profile == Asura::Profile::"model-1" &&
  (resource in principal.project.root || resource in principal.scratch ||
   resource == Asura::Path::"/dev/null")
};

@id("model1.exec") @reason("model1_region")
permit (principal is Asura::Job, action == Asura::Action::"process.exec", resource is Asura::Program)
when {
  principal.profile == Asura::Profile::"model-1" &&
  (resource.path in Asura::Path::"/bin" || resource.path in Asura::Path::"/usr/bin" ||
   resource.path in Asura::Path::"/sbin" || resource.path in Asura::Path::"/usr/sbin" ||
   resource.path in Asura::Path::"/usr/lib" || resource.path in Asura::Path::"/usr/share" ||
   resource.path in Asura::Path::"/System" ||
   resource.path in principal.project.root || resource.path in principal.scratch)
};

// Hard forbids: no approval, envelope or lower scope lifts them.

@id("s1.implicit_exec") @reason("implicit_exec_write") @hard("true")
@explain("model commands may not change Git hooks or other files that run code implicitly")
forbid (principal is Asura::Job,
        action in [Asura::Action::"file.write", Asura::Action::"file.create", Asura::Action::"file.delete"],
        resource is Asura::Path)
when {
  principal.origin == Asura::Origin::"model" &&
  resource.class == Asura::PathClass::"implicit_exec"
};

@id("s2.asura_state_write") @reason("asura_state") @hard("true")
@explain("jobs may not change Asura's own files")
forbid (principal is Asura::Job,
        action in [Asura::Action::"file.write", Asura::Action::"file.create", Asura::Action::"file.delete"],
        resource is Asura::Path)
when {
  resource.class == Asura::PathClass::"asura_state" && !(resource in principal.scratch)
};

@id("s2.asura_state_read") @reason("asura_state") @hard("true")
@explain("model commands may not read Asura's own files")
forbid (principal is Asura::Job, action == Asura::Action::"file.read", resource is Asura::Path)
when {
  principal.origin == Asura::Origin::"model" &&
  resource.class == Asura::PathClass::"asura_state" && !(resource in principal.scratch)
};

@id("s2.asura_state_exec") @reason("asura_state") @hard("true")
@explain("model commands may not run Asura's own files")
forbid (principal is Asura::Job, action in Asura::Action::"process.exec.any", resource is Asura::Program)
when {
  principal.origin == Asura::Origin::"model" &&
  resource.path.class == Asura::PathClass::"asura_state" && !(resource.path in principal.scratch)
};

@id("s3.model_network") @reason("model_network") @hard("true")
@explain("model commands have no network access except through Asura")
forbid (principal is Asura::Job, action == Asura::Action::"network.connect", resource)
when { principal.origin == Asura::Origin::"model" && !context.via_proxy };

@id("s4.model_secret") @reason("model_secret") @hard("true")
@explain("model commands never receive a named secret")
forbid (principal is Asura::Job, action == Asura::Action::"secret_use", resource)
when { principal.origin == Asura::Origin::"model" };
