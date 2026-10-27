//! The signed qualification run of the team Keychain store
//! (`docs/designs/release-signing.md#signed-qualification-run`; SGQ6).
//! Test-support only. It runs only from the qualification bundle that
//! `tools/release/secret-store-qualification` signs with the test App ID;
//! anywhere else it refuses with no Keychain call.
fn main() -> std::process::ExitCode {
    asura_service::secret_store::qualification_main()
}
