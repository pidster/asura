// Asura default user scope (security-policy.md, PL1 implementation contract,
// built-in sets): the person's default model-1 regions.

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
