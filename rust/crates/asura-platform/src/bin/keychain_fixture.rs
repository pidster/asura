//! Keychain fixture binary (`docs/designs/secret-store.md`, fixture binary;
//! test-support only). The body is the shared
//! [`asura_platform::keychain::scratch::fixture_main`].
fn main() -> std::process::ExitCode {
    asura_platform::keychain::scratch::fixture_main()
}
