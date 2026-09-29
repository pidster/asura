//! Consume an explicitly assembled local model identity; never build tools here.
use std::{env, fs, path::PathBuf};
fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../..");
    let identity = root.join(".build/model-tools/package-identity.txt");
    println!("cargo:rerun-if-changed={}", identity.display());
    let source = match fs::read_to_string(&identity) {
        Ok(text) => {
            let fields: Vec<_> = text.lines().collect();
            assert!(
                (3..=4).contains(&fields.len()),
                "model package identity must have three or four hashes"
            );
            let mut arrays = Vec::new();
            for field in fields {
                assert!(
                    field.len() == 64 && field.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid package identity hash"
                );
                let bytes: Vec<_> = (0..64)
                    .step_by(2)
                    .map(|n| u8::from_str_radix(&field[n..n + 2], 16).unwrap())
                    .collect();
                arrays.push(format!("{:?}", bytes));
            }
            format!(
                "const MODEL_PACKAGE: Option<model_owner::PackageIdentity> = Some(model_owner::PackageIdentity {{ build_id: {}, schema_digest: {}, helper_digest: {}, metallib_digest: {} }});",
                arrays[0],
                arrays[1],
                arrays[2],
                arrays
                    .get(3)
                    .map(|hash| format!("Some({hash})"))
                    .unwrap_or_else(|| "None".into())
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            "const MODEL_PACKAGE: Option<model_owner::PackageIdentity> = None;".into()
        }
        Err(error) => panic!("cannot read model package identity: {error}"),
    };
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("model-package.rs"),
        source,
    )
    .unwrap();
}
