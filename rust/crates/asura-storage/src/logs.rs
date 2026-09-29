//! Diagnostic file destinations. Subscriber formatting belongs to the CLI.
use std::fs::File;
use std::path::Path;

/// Select the effective account's diagnostic directory, never an environment home.
pub fn open_account_log() -> asura_platform::Result<File> {
    let home = asura_platform::account_home_path()?;
    open_log(&home.join(".asura/logs"))
}

/// Open the diagnostic append sink selected by the caller's logging option.
/// Invoke during supervised startup, not from a client event loop.
pub fn open_log(directory: &Path) -> asura_platform::Result<File> {
    asura_platform::open_private_append(directory, "asura.log")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn diagnostic_adapter_selects_existing_filename_and_appends() {
        let root = Scratch(
            format!(
                "/private/tmp/asura-log-adapter-{:x}",
                u128::from_ne_bytes(asura_platform::random_id())
            )
            .into(),
        );
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root.0)
            .unwrap();
        open_log(&root.0).unwrap().write_all(b"first\n").unwrap();
        open_log(&root.0).unwrap().write_all(b"second\n").unwrap();
        let path = root.0.join("asura.log");
        assert_eq!(std::fs::read(&path).unwrap(), b"first\nsecond\n");
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
