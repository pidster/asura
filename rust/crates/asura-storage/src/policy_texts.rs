//! The policy text store (security-policy.md, PL3 policy text store):
//! admitted policy texts named by their SHA-256, kept for every revision
//! (SP-R13). The admission worker is the only caller; every call blocks on
//! file I/O and never runs on the reactor.
use crate::authority::conversation::Hash;
use asura_platform::RuntimeDirectory;
use asura_platform::policy_text_io::{PolicyTextDirectory, PolicyTextError};
use sha2::{Digest, Sha256};

/// Texts the store holds at most (about 1 GiB at the text bound).
pub const MAX_POLICY_TEXTS: usize = 4096;
/// One text's bound: the scope text bound of admission step 1.
pub const MAX_POLICY_TEXT_BYTES: usize = 256 * 1024;

/// A text store failure.
#[derive(Debug)]
pub enum TextError {
    /// No text has this digest.
    Missing,
    /// The stored bytes have another digest or are not UTF-8.
    Mismatch,
    /// The store holds [`MAX_POLICY_TEXTS`] texts.
    Full,
    /// The text is over [`MAX_POLICY_TEXT_BYTES`].
    TooLarge,
    /// A symbolic link, another owner or a wrong mode or name.
    Unsafe,
    /// The volume is full.
    NoSpace,
    Io(std::io::Error),
}
impl From<PolicyTextError> for TextError {
    fn from(error: PolicyTextError) -> Self {
        match error {
            PolicyTextError::Missing => Self::Missing,
            PolicyTextError::Unsafe => Self::Unsafe,
            PolicyTextError::NoSpace => Self::NoSpace,
            PolicyTextError::TooLarge => Self::TooLarge,
            PolicyTextError::Io(error) => Self::Io(error),
        }
    }
}

fn hex(digest: &Hash) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The held text store.
pub struct PolicyTexts(PolicyTextDirectory);
impl PolicyTexts {
    /// Holds `~/.asura/policy/admitted`, creating it when `create` is set.
    pub fn open(runtime: &RuntimeDirectory, create: bool) -> Result<Self, TextError> {
        Ok(Self(PolicyTextDirectory::open(runtime, create)?))
    }
    /// Stores `text` and returns its digest. A stored text with the same
    /// digest is reused after its bytes are checked; a new one at the
    /// bound is refused `Full`.
    pub fn put(&self, text: &str) -> Result<Hash, TextError> {
        if text.len() > MAX_POLICY_TEXT_BYTES {
            return Err(TextError::TooLarge);
        }
        let digest: Hash = Sha256::digest(text.as_bytes()).into();
        match self.get(&digest) {
            Ok(_) => return Ok(digest),
            Err(TextError::Missing) => {}
            Err(error) => return Err(error),
        }
        if self.0.count()? >= MAX_POLICY_TEXTS {
            return Err(TextError::Full);
        }
        self.0.write(&hex(&digest), text.as_bytes())?;
        // The installed file must hold exactly these bytes.
        self.get(&digest)?;
        Ok(digest)
    }
    /// The text with `digest`, checked against it.
    pub fn get(&self, digest: &Hash) -> Result<String, TextError> {
        let bytes = self.0.read(&hex(digest), MAX_POLICY_TEXT_BYTES)?;
        if <Hash>::from(Sha256::digest(&bytes)) != *digest {
            return Err(TextError::Mismatch);
        }
        String::from_utf8(bytes).map_err(|_| TextError::Mismatch)
    }
    /// The number of stored texts.
    pub fn count(&self) -> Result<usize, TextError> {
        Ok(self.0.count()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct Home(std::path::PathBuf);
    impl Home {
        fn new() -> Self {
            let name: String = asura_platform::random_id()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let path = std::path::PathBuf::from("/private/tmp").join(format!("as-ptx-{name}"));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
        fn file(&self, digest: &Hash) -> std::path::PathBuf {
            self.0.join(".asura/policy/admitted").join(hex(digest))
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn store(home: &Home) -> PolicyTexts {
        let runtime = RuntimeDirectory::scratch(&home.0, true).unwrap();
        PolicyTexts::open(&runtime, true).unwrap()
    }

    #[test]
    fn put_get_reuse_missing_and_mismatch() {
        let home = Home::new();
        let texts = store(&home);
        let digest = texts.put("forbid (principal, action, resource);").unwrap();
        assert_eq!(
            texts.get(&digest).unwrap(),
            "forbid (principal, action, resource);"
        );
        assert_eq!(
            texts.put("forbid (principal, action, resource);").unwrap(),
            digest
        );
        assert_eq!(texts.count().unwrap(), 1);
        assert!(matches!(texts.get(&[9; 32]), Err(TextError::Missing)));
        // Damage: the stored bytes no longer match their name.
        fs::write(home.file(&digest), b"permit (principal, action, resource);").unwrap();
        assert!(matches!(texts.get(&digest), Err(TextError::Mismatch)));
        // A reuse never trusts a damaged file.
        assert!(matches!(
            texts.put("forbid (principal, action, resource);"),
            Err(TextError::Mismatch)
        ));
        assert!(matches!(
            texts.put(&"x".repeat(MAX_POLICY_TEXT_BYTES + 1)),
            Err(TextError::TooLarge)
        ));
        let largest = "y".repeat(MAX_POLICY_TEXT_BYTES);
        let d = texts.put(&largest).unwrap();
        assert_eq!(texts.get(&d).unwrap().len(), MAX_POLICY_TEXT_BYTES);
    }

    #[test]
    fn the_store_refuses_a_new_text_at_its_bound() {
        let home = Home::new();
        let texts = store(&home);
        let directory = home.0.join(".asura/policy/admitted");
        for n in 0..MAX_POLICY_TEXTS {
            let digest: Hash = Sha256::digest(format!("t{n}").as_bytes()).into();
            fs::write(directory.join(hex(&digest)), format!("t{n}")).unwrap();
            fs::set_permissions(
                directory.join(hex(&digest)),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        assert_eq!(texts.count().unwrap(), MAX_POLICY_TEXTS);
        assert!(matches!(texts.put("new"), Err(TextError::Full)));
        // An existing text is still reused at the bound.
        assert!(texts.put("t7").is_ok());
    }

    #[test]
    fn a_symlinked_text_is_refused() {
        let home = Home::new();
        let texts = store(&home);
        let digest: Hash = Sha256::digest(b"x").into();
        std::os::unix::fs::symlink("/etc/hosts", home.file(&digest)).unwrap();
        assert!(matches!(texts.get(&digest), Err(TextError::Unsafe)));
        assert!(matches!(texts.put("x"), Err(TextError::Unsafe)));
    }
}
