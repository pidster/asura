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

// PL2 (security-policy.md, built-in set additions in PL2): person jobs.

@id("person.entry") @reason("person_entry")
permit (principal is Asura::Job, action == Asura::Action::"process.exec", resource is Asura::Program)
when {
  principal.origin == Asura::Origin::"person_typed" &&
  (principal.profile == Asura::Profile::"person-1" ||
   principal.profile == Asura::Profile::"person-outside-1") &&
  resource.path == Asura::Path::"/bin/sh"
};

@id("person.network") @reason("person_network")
permit (principal is Asura::Job, action == Asura::Action::"network.connect", resource)
when {
  principal.origin == Asura::Origin::"person_typed" &&
  (principal.profile == Asura::Profile::"person-1" ||
   principal.profile == Asura::Profile::"person-outside-1")
};

@id("person.secret") @reason("person_secret")
permit (principal is Asura::Job, action == Asura::Action::"secret_use", resource is Asura::Secret)
when {
  principal.origin == Asura::Origin::"person_typed" &&
  (principal.profile == Asura::Profile::"person-1" ||
   principal.profile == Asura::Profile::"person-outside-1") &&
  context.delivery == Asura::Delivery::"raw"
};

// PL2 hard forbids that state the person environment rules (PE-R1) and the
// outside switch; the environment builder and the guardian enforce them.

@id("person.env_terminal") @reason("env_not_allowed") @hard("true")
@explain("person commands receive only the listed terminal variables")
forbid (principal is Asura::Job, action in Asura::Action::"process.exec.any", resource is Asura::Program)
when {
  principal.origin == Asura::Origin::"person_typed" &&
  !([
     "LANG", "LC_ALL", "LC_COLLATE", "LC_CTYPE", "LC_MESSAGES",
     "LC_MONETARY", "LC_NUMERIC", "LC_TIME", "SSH_AUTH_SOCK", "TZ",
     "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "CARGO_HOME",
     "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "CARGO_TARGET_DIR", "DEVELOPER_DIR", "SDKROOT",
     "TOOLCHAINS", "HOMEBREW_PREFIX", "HOMEBREW_CELLAR", "HOMEBREW_REPOSITORY", "GOPATH",
     "GOROOT", "GOMODCACHE", "GOCACHE", "NVM_DIR", "VOLTA_HOME",
     "PNPM_HOME", "BUN_INSTALL", "PYENV_ROOT", "VIRTUAL_ENV", "CONDA_PREFIX",
     "JAVA_HOME", "RBENV_ROOT", "GEM_HOME", "GEM_PATH"
   ].containsAll(context.env_terminal))
};

@id("person.env_declared") @reason("env_not_allowed") @hard("true")
@explain("person commands receive only the project's declared variables")
forbid (principal is Asura::Job, action in Asura::Action::"process.exec.any", resource is Asura::Program)
when {
  principal.origin == Asura::Origin::"person_typed" &&
  !principal.project.declared_env.containsAll(context.env_declared)
};

@id("person.outside_switch") @reason("outside_sandbox_off") @hard("true")
@explain("commands outside the sandbox need the allowOutsideSandbox setting")
forbid (principal is Asura::Job, action in Asura::Action::"process.exec.any", resource is Asura::Program)
when {
  principal.profile == Asura::Profile::"person-outside-1" && !context.outside_switch
};
