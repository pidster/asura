use prost::Message;
use prost_types::{
    DescriptorProto, FileDescriptorSet,
    field_descriptor_proto::{Label, Type},
};
use std::os::unix::fs::PermissionsExt;
use std::{collections::BTreeMap, env, fs, path::PathBuf, process::Command};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../..");
    let schema = root.join("contracts/control/v1/control.proto");
    let model = root.join("contracts/model/v1/model.proto");
    println!("cargo:rerun-if-changed={}", model.display());
    let lock = root.join("tools/protobuf/lock.json");
    println!("cargo:rerun-if-changed={}", schema.display());
    println!("cargo:rerun-if-changed={}", lock.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ASURA_PROTOC");
    let executable = PathBuf::from(
        env::var_os("ASURA_PROTOC").ok_or("set ASURA_PROTOC to verified pinned protoc 36.2")?,
    );
    if !executable.is_absolute() {
        return Err("ASURA_PROTOC must be absolute".into());
    }
    let metadata = fs::metadata(&executable)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err("ASURA_PROTOC must be a regular executable".into());
    }
    println!("cargo:rerun-if-changed={}", executable.display());
    let lock: serde_json::Value = serde_json::from_slice(&fs::read(lock)?)?;
    let version = lock["protoc"]["version"]
        .as_str()
        .ok_or("missing locked protoc version")?;
    if version != "36.2" {
        return Err("unsupported protoc lock revision".into());
    }
    let output = Command::new(&executable).arg("--version").output()?;
    if !output.status.success()
        || output.stdout != format!("libprotoc {version}\n").as_bytes()
        || !output.stderr.is_empty()
    {
        return Err("ASURA_PROTOC version differs from tool lock".into());
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let descriptor = out.join("control-descriptor.bin");
    prost_build::Config::new()
        .protoc_executable(&executable)
        .file_descriptor_set_path(&descriptor)
        .compile_protos(&[schema], &[root.join("contracts/control/v1")])?;
    let set = FileDescriptorSet::decode(fs::read(descriptor)?.as_slice())?;
    fs::write(
        out.join("validation.rs"),
        tables(&set, ".asura.control.v1.Envelope", None)?,
    )?;
    let descriptor = out.join("model-descriptor.bin");
    prost_build::Config::new()
        .protoc_executable(&executable)
        .file_descriptor_set_path(&descriptor)
        .compile_protos(&[model], &[root.join("contracts/model/v1")])?;
    let set = FileDescriptorSet::decode(fs::read(descriptor)?.as_slice())?;
    fs::write(
        out.join("model-validation.rs"),
        tables(
            &set,
            ".asura.model.v1.Envelope",
            Some(".asura.model.v1.ModelInput"),
        )?,
    )?;
    Ok(())
}

fn tables(
    set: &FileDescriptorSet,
    root: &str,
    input: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut messages = BTreeMap::<String, &DescriptorProto>::new();
    let mut enums = BTreeMap::<String, Vec<i32>>::new();
    for file in &set.file {
        if !file.extension.is_empty() {
            return Err("extensions unsupported".into());
        }
        let package = file.package.as_deref().ok_or("missing package")?;
        for message in &file.message_type {
            if !message.nested_type.is_empty()
                || !message.extension.is_empty()
                || !message.extension_range.is_empty()
                || message.options.as_ref().is_some_and(|v| v.map_entry())
            {
                return Err("nested/map/extension schema unsupported".into());
            }
            messages.insert(format!(".{package}.{}", message.name()), message);
        }
        for en in &file.enum_type {
            enums.insert(
                format!(".{package}.{}", en.name()),
                en.value.iter().map(|v| v.number()).collect(),
            );
        }
    }
    let names: Vec<_> = messages.keys().cloned().collect();
    let index = |name: &str| {
        names
            .iter()
            .position(|n| n == name)
            .ok_or("unresolved message")
    };
    let mut edges = vec![Vec::new(); names.len()];
    let mut result = format!(
        "const ROOT: usize = {};\nconst TABLES: &[&[Field]] = &[\n",
        index(root)?
    );
    for (i, name) in names.iter().enumerate() {
        result.push_str("&[\n");
        for field in &messages[name].field {
            let kind = match field.r#type() {
                Type::Bool => "Kind::Bool".to_string(),
                Type::Uint64 => "Kind::U64".to_string(),
                Type::Uint32 => "Kind::U32".to_string(),
                Type::String => "Kind::String".to_string(),
                Type::Bytes => "Kind::Bytes".to_string(),
                Type::Enum => format!(
                    "Kind::Enum(&{:?})",
                    enums.get(field.type_name()).ok_or("unresolved enum")?
                ),
                Type::Message => {
                    let target = index(field.type_name())?;
                    edges[i].push(target);
                    format!("Kind::Message({target})")
                }
                _ => return Err("unsupported descriptor field type".into()),
            };
            let repeated = field.label() == Label::Repeated;
            if repeated && !matches!(field.r#type(), Type::Enum | Type::Message) {
                return Err("unsupported repeated field".into());
            }
            let repeated_limit = match (name.as_str(), field.name()) {
                (".asura.control.v1.ModelsReply", "models") => 65,
                (".asura.model.v1.Hello", "models") => 64,
                (".asura.control.v1.SensorsReply", "observations") => 16,
                (".asura.control.v1.AuditReply", "entries") => 16,
                (".asura.control.v1.SensorsReply", "proposals") => 8,
                _ => 32,
            };
            result.push_str(&format!(
                "Field {{ repeated_limit: {repeated_limit}, tag: {}, kind: {kind}, repeated: {repeated}, oneof: {:?} }},\n",
                field.number(),
                field.oneof_index.map(|v| v as usize)
            ));
        }
        result.push_str("],\n");
    }
    fn visit(
        node: usize,
        edges: &[Vec<usize>],
        stack: &mut Vec<usize>,
    ) -> Result<(), &'static str> {
        if stack.contains(&node) {
            return Err("recursive schema unsupported");
        }
        stack.push(node);
        for &child in &edges[node] {
            visit(child, edges, stack)?;
        }
        stack.pop();
        Ok(())
    }
    for i in 0..names.len() {
        visit(i, &edges, &mut Vec::new())?;
    }
    result.push_str("];\n");
    if let Some(input) = input {
        result.push_str(&format!("const INPUT: usize = {};\n", index(input)?));
    }
    Ok(result)
}
