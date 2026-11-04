// Test-only addition to the installation scope (test-support feature only;
// security-policy.md, test-only sets). Appended to installation.cedar.

// A tighten-only forbid: it reads the intent, so it denies the entry
// request of a model job and never a region request.
@id("test_deny_model_exec") @reason("test_denied") @intent("true")
@explain("the test policy denies model commands")
forbid (principal is Asura::Job, action == Asura::Action::"process.exec", resource)
when { principal.origin == Asura::Origin::"model" && context has intent };

// PL2: a tighten-only forbid of the person command whose first tool is the
// sentinel name; every other person command is unaffected.
@id("test_deny_person_exec") @reason("test_denied") @intent("true")
@explain("the test policy denies this person command")
forbid (principal is Asura::Job, action == Asura::Action::"process.exec", resource)
when {
  principal.origin == Asura::Origin::"person_typed" && context has intent &&
  context.intent has command &&
  context.intent.command.first_tool == "asura-policy-test-deny"
};
