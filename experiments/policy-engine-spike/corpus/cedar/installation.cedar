@id("inst.model.write") @scope("installation") @reason("write_project_or_scratch")
permit (principal is Asura::Job, action == Asura::Action::"write", resource is Asura::Path)
when { principal.profile == Asura::Profile::"model-1" &&
       (resource in principal.project.root || resource in principal.scratch) };

@id("inst.person.write") @scope("installation") @reason("person_write_roots")
permit (principal is Asura::Job, action == Asura::Action::"write", resource is Asura::Path)
when { principal.profile == Asura::Profile::"person-1" &&
       (resource in principal.project.root || resource in principal.scratch ||
        resource in principal.cache_roots) };

@id("inst.hard.implicit_exec") @scope("installation") @reason("implicit_exec_path")
forbid (principal is Asura::Job, action == Asura::Action::"write", resource is Asura::Path)
when { principal.origin == Asura::Origin::"model" &&
       resource.class == Asura::PathClass::"implicit_exec" };

@id("inst.hard.asura_state") @scope("installation") @reason("asura_state")
forbid (principal is Asura::Job, action == Asura::Action::"write", resource is Asura::Path)
when { resource.class == Asura::PathClass::"asura_state" && !(resource in principal.scratch) };

@id("inst.model.exec") @scope("installation") @reason("model_exec")
permit (principal is Asura::Job, action == Asura::Action::"exec", resource is Asura::Executable)
when { principal.profile == Asura::Profile::"model-1" };

@id("inst.model.exec_approval") @scope("installation") @reason("model_exec_approval_offered")
permit (principal is Asura::Job, action == Asura::Action::"exec_request_approval", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"model" };

@id("inst.hard.model_privilege") @scope("installation") @reason("privilege_escalation")
forbid (principal is Asura::Job, action in Asura::Action::"exec_any", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"model" && ["sudo", "su", "doas"].contains(resource.tool) };

@id("inst.hard.model_secret_reference") @scope("installation") @reason("model_secret_reference")
forbid (principal is Asura::Job, action in Asura::Action::"exec_any", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"model" && context.command.secret_reference };

@id("inst.model.env_clean") @scope("installation") @reason("model_env_not_clean")
forbid (principal is Asura::Job, action in Asura::Action::"exec_any", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"model" &&
       !(context.command.env_terminal.isEmpty() && context.command.env_declared.isEmpty()) };

@id("inst.model.shell_features") @scope("installation") @reason("shell_feature_needs_approval")
forbid (principal is Asura::Job, action == Asura::Action::"exec", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"model" &&
       (context.command.parse == Asura::Parse::"unparsed" ||
        context.command.shell_features.containsAny(["eval", "pipe_to_interpreter", "command_substitution_word"])) };

@id("inst.model.git_push") @scope("installation") @reason("git_push_needs_approval")
forbid (principal is Asura::Job, action == Asura::Action::"exec", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"model" && resource.tool == "git" &&
       context.command.subcommand == "push" };

@id("inst.model.git_force") @scope("installation") @reason("git_force_needs_approval")
forbid (principal is Asura::Job, action == Asura::Action::"exec", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"model" && resource.tool == "git" &&
       context.command.flags.containsAny(["--force", "-f", "--force-with-lease"]) };

@id("inst.model.publish") @scope("installation") @reason("publish_needs_approval")
forbid (principal is Asura::Job, action == Asura::Action::"exec", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"model" && ["cargo", "npm"].contains(resource.tool) &&
       context.command.subcommand like "publish*" };

@id("inst.person.exec") @scope("installation") @reason("person_exec")
permit (principal is Asura::Job, action == Asura::Action::"exec", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"person_typed" &&
       principal.profile == Asura::Profile::"person-1" };

@id("inst.person.exec_outside") @scope("installation") @reason("person_exec_outside")
permit (principal is Asura::Job, action == Asura::Action::"exec", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"person_typed" &&
       principal.profile == Asura::Profile::"person-outside-1" && context.outside_switch };

@id("inst.person.env_terminal") @scope("installation") @reason("env_not_allow_listed")
forbid (principal is Asura::Job, action in Asura::Action::"exec_any", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"person_typed" &&
       !["PATH", "LANG", "LC_ALL", "LC_COLLATE", "LC_CTYPE", "LC_MESSAGES", "LC_MONETARY",
         "LC_NUMERIC", "LC_TIME", "SSH_AUTH_SOCK", "TZ", "XDG_CONFIG_HOME", "XDG_CACHE_HOME",
         "XDG_DATA_HOME", "XDG_STATE_HOME", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN",
         "CARGO_TARGET_DIR", "DEVELOPER_DIR", "SDKROOT", "TOOLCHAINS", "HOMEBREW_PREFIX",
         "HOMEBREW_CELLAR", "HOMEBREW_REPOSITORY", "GOPATH", "GOROOT", "GOMODCACHE", "GOCACHE",
         "NVM_DIR", "VOLTA_HOME", "PNPM_HOME", "BUN_INSTALL", "PYENV_ROOT", "VIRTUAL_ENV",
         "CONDA_PREFIX", "JAVA_HOME", "RBENV_ROOT", "GEM_HOME", "GEM_PATH"]
         .containsAll(context.command.env_terminal) };

@id("inst.person.env_declared") @scope("installation") @reason("env_not_declared")
forbid (principal is Asura::Job, action in Asura::Action::"exec_any", resource is Asura::Executable)
when { principal.origin == Asura::Origin::"person_typed" &&
       !principal.project.declared_env.containsAll(context.command.env_declared) };

@id("inst.person.net") @scope("installation") @reason("person_network")
permit (principal is Asura::Job, action == Asura::Action::"connect", resource)
when { principal.origin == Asura::Origin::"person_typed" };

@id("inst.hard.model_unmediated_net") @scope("installation") @reason("model_network_unmediated")
forbid (principal is Asura::Job, action == Asura::Action::"connect", resource)
when { principal.origin == Asura::Origin::"model" && !context.via_proxy };

@id("inst.hard.model_local_address") @scope("installation") @reason("model_network_local_address")
forbid (principal is Asura::Job, action == Asura::Action::"connect", resource)
when { principal.origin == Asura::Origin::"model" &&
       (context.ip.isLoopback() || context.ip.isInRange(ip("169.254.0.0/16"))) };

@id("inst.person.secret") @scope("installation") @reason("person_secret_use")
permit (principal is Asura::Job, action == Asura::Action::"secret_use", resource is Asura::Secret)
when { principal.origin == Asura::Origin::"person_typed" && resource.project == principal.project };

@id("inst.hard.model_raw_secret") @scope("installation") @reason("model_raw_secret")
forbid (principal is Asura::Job, action == Asura::Action::"secret_use", resource is Asura::Secret)
when { principal.origin == Asura::Origin::"model" && context.delivery == Asura::Delivery::"raw" };

@id("inst.hard.secret_other_project") @scope("installation") @reason("secret_other_project")
forbid (principal is Asura::Job, action == Asura::Action::"secret_use", resource is Asura::Secret)
when { resource.project != principal.project };

@id("inst.egress.local") @scope("installation") @reason("egress_local")
permit (principal is Asura::Task, action == Asura::Action::"egress", resource is Asura::Destination)
when { !resource.remote };

@id("inst.hard.egress_tainted") @scope("installation") @reason("egress_tainted")
forbid (principal is Asura::Task, action == Asura::Action::"egress", resource is Asura::Destination)
when { context.tainted };
