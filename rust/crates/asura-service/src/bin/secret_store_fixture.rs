//! Secret store fixture binary (`docs/designs/secret-store.md`, fixture
//! binary; test-support only). A different executable from the test that
//! runs it, so its items are denied to the test binary (SS-W3, SS-W9). The
//! body is the shared [`asura_platform::keychain::scratch::fixture_main`].
fn main() -> std::process::ExitCode {
    asura_platform::keychain::scratch::fixture_main()
}
