//! One-shot preparation operations. Process lifetime belongs exclusively to the driver.
use crate::{
    HostToolchain, ToolchainLock,
    archive::{ArchiveSelection, MAX_COMPRESSED_BYTES, extract_archive},
    cache::*,
    parse_lock,
};
use std::os::unix::fs::MetadataExt;
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishIntent {
    candidate: [u64; 2],
    prior: Option<[u64; 2]>,
}
fn directory_identity(path: &Path) -> Result<[u64; 2]> {
    real_directory(path)?;
    let m = fs::symlink_metadata(path)?;
    Ok([m.dev(), m.ino()])
}

fn redirect_target(
    current: &str,
    locations: &[&str],
    count: usize,
    hosts: &[String],
) -> Result<(String, String)> {
    if count >= 5 || locations.len() != 1 {
        return Err(CacheError::InvalidInput);
    }
    let request = ureq::get(current);
    let parsed = request
        .request_url()
        .map_err(|_| CacheError::InvalidInput)?;
    let next = parsed
        .as_url()
        .join(locations[0])
        .map_err(|_| CacheError::InvalidInput)?;
    if next.scheme() != "https"
        || !next.username().is_empty()
        || next.password().is_some()
        || next.fragment().is_some()
        || next.port().is_some_and(|p| p != 443)
        || !next
            .host_str()
            .is_some_and(|h| hosts.iter().any(|v| v == h))
    {
        return Err(CacheError::InvalidInput);
    }
    Ok((
        next.to_string(),
        next.host_str().ok_or(CacheError::InvalidInput)?.into(),
    ))
}

fn response_policy(status: u16, encodings: &[&str]) -> Result<()> {
    if status != 200 || encodings.iter().any(|v| *v != "identity") {
        return Err(CacheError::InvalidInput);
    }
    Ok(())
}

fn save_archive(mut input: impl Read, part: &Path, target: &Path, expected: &str) -> Result<()> {
    write_new(part, b"", false)?;
    let result = (|| {
        let mut output = fs::OpenOptions::new().write(true).open(part)?;
        let mut total = 0usize;
        let mut buffer = [0u8; 65536];
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            total = total.checked_add(n).ok_or(CacheError::Limit)?;
            if total > MAX_COMPRESSED_BYTES {
                return Err(CacheError::Limit);
            }
            output.write_all(&buffer[..n])?;
        }
        output.sync_all()?;
        if digest(&read_file(part, MAX_COMPRESSED_BYTES)?) != expected {
            return Err(CacheError::DigestMismatch);
        }
        fs::rename(part, target)?;
        Ok(())
    })();
    if let Err(error) = result {
        if part.exists() && fs::remove_file(part).is_err() {
            return Err(CacheError::CleanupAfter(Box::new(error)));
        }
        return Err(error);
    }
    Ok(())
}

struct Context {
    run: PathBuf,
    id: String,
    lock: ToolchainLock,
    cargo: Vec<u8>,
    packages: Vec<LockedPackage>,
    tool_root: PathBuf,
    tools: PathBuf,
    tool_stage: PathBuf,
    tool_backup: PathBuf,
    cargo_root: PathBuf,
    cargo_stage: PathBuf,
    cargo_backup: PathBuf,
}
impl Context {
    fn load(root: &Path, id: &str) -> Result<Self> {
        real_directory(root)?;
        if root.canonicalize()? != root
            || id.is_empty()
            || id.len() > 64
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(CacheError::InvalidInput);
        }
        let tool_root = root.join(".build/asura-protobuf");
        let cargo_root = root.join(".build/asura-deps/cargo");
        real_directory(&tool_root)?;
        real_directory(&cargo_root)?;
        let run = tool_root.join(".runs").join(id);
        real_directory(&run)?;
        let lock = parse_lock(&read_file(
            &root.join("tools/protobuf/lock.json"),
            crate::MAX_LOCK_BYTES,
        )?)
        .map_err(|_| CacheError::InvalidInput)?;
        let host: HostToolchain = serde_json::from_slice(&read_file(&run.join("host.json"), 4096)?)
            .map_err(|_| CacheError::InvalidInput)?;
        lock.validate_host(&host)
            .map_err(|_| CacheError::InvalidInput)?;
        let cargo = read_file(&root.join("Cargo.lock"), 1024 * 1024)?;
        let metadata = read_file(&run.join("cargo-metadata.json"), MAX_MANIFEST)?;
        let observed: serde_json::Value =
            serde_json::from_slice(&metadata).map_err(|_| CacheError::InvalidInput)?;
        if observed["workspace_root"].as_str() != root.to_str() {
            return Err(CacheError::InvalidInput);
        }
        let packages = packages(&cargo, &metadata)?;
        let key: String = lock.digest().iter().map(|v| format!("{v:02x}")).collect();
        Ok(Self {
            run,
            id: id.into(),
            lock,
            cargo,
            packages,
            tools: tool_root.join(key),
            tool_stage: tool_root.join(format!(".stage-{id}")),
            tool_backup: tool_root.join(format!(".backup-{id}")),
            cargo_stage: cargo_root.join(format!(".stage-{id}")),
            cargo_backup: cargo_root.join(format!(".backup-{id}")),
            tool_root,
            cargo_root,
        })
    }
    fn receipt(&self, op: &str, qualification: &str) -> Result<()> {
        let key: String = self
            .lock
            .digest()
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect();
        write_new(
            &self.run.join(format!("{op}.receipt")),
            format!(
                "ASURA-PB03 1 {op} {} {key} {} {qualification}\n",
                self.id,
                digest(&self.cargo)
            )
            .as_bytes(),
            false,
        )
    }
    fn archive(&self, swift: bool) -> Result<Vec<u8>> {
        let name = if swift { "swift.tar.gz" } else { "protoc.zip" };
        let expected = if swift {
            self.lock.swift_protobuf().sha256()
        } else {
            self.lock.protoc().sha256()
        };
        for parent in [self.run.join("archives"), self.tools.join("archives")] {
            let path = parent.join(name);
            match read_file(&path, MAX_COMPRESSED_BYTES) {
                Ok(bytes) if digest(&bytes) == expected => return Ok(bytes),
                Ok(_) => return Err(CacheError::DigestMismatch),
                Err(CacheError::MissingInput) => (),
                Err(CacheError::Io) if !parent.exists() => (),
                Err(e) => return Err(e),
            }
        }
        Err(CacheError::MissingInput)
    }
    fn fetch(&self, swift: bool) -> Result<()> {
        let folder = self.run.join("archives");
        ensure_directory(&folder)?;
        let name = if swift { "swift.tar.gz" } else { "protoc.zip" };
        let target = folder.join(name);
        match self.archive(swift) {
            Ok(bytes) => {
                if !target.exists() {
                    write_new(&target, &bytes, false)?;
                }
                return Ok(());
            }
            Err(CacheError::MissingInput) => (),
            Err(e) => return Err(e),
        }
        let (url, expected, hosts) = if swift {
            (
                self.lock.swift_protobuf().url(),
                self.lock.swift_protobuf().sha256(),
                self.lock.swift_protobuf().redirect_hosts(),
            )
        } else {
            (
                self.lock.protoc().url(),
                self.lock.protoc().sha256(),
                self.lock.protoc().redirect_hosts(),
            )
        };
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .https_only(true)
            .try_proxy_from_env(false)
            .timeout(Duration::from_secs(120))
            .build();
        let mut current = url.to_owned();
        let mut chain = vec!["github.com".to_owned()];
        for redirect in 0..=5 {
            let request = agent.get(&current).set("Accept-Encoding", "identity");
            let response = request.call().map_err(|_| CacheError::Io)?;
            if matches!(response.status(), 301 | 302 | 303 | 307 | 308) {
                let (next, host) =
                    redirect_target(&current, &response.all("Location"), redirect, hosts)?;
                chain.push(host);
                current = next;
                continue;
            }
            response_policy(response.status(), &response.all("Content-Encoding"))?;
            let part = folder.join(format!("{name}.part"));
            save_archive(response.into_reader(), &part, &target, expected)?;
            return write_new(
                &self.run.join(if swift {
                    "fetch-swift.redirects.json"
                } else {
                    "fetch-protoc.redirects.json"
                }),
                &serde_json::to_vec(&chain).map_err(|_| CacheError::InvalidInput)?,
                false,
            );
        }
        Err(CacheError::InvalidInput)
    }
    fn stage_tools(&self) -> Result<()> {
        let protoc = self.archive(false)?;
        let swift = self.archive(true)?;
        new_directory(&self.tool_stage)?;
        let result = (|| {
            new_directory(&self.tool_stage.join("archives"))?;
            write_new(&self.tool_stage.join("archives/protoc.zip"), &protoc, false)?;
            write_new(
                &self.tool_stage.join("archives/swift.tar.gz"),
                &swift,
                false,
            )?;
            extract_archive(
                &protoc,
                ArchiveSelection::Protoc(self.lock.protoc()),
                &self.tool_stage.join("protoc"),
            )
            .map_err(archive_error)?;
            new_directory(&self.tool_stage.join("swift"))?;
            extract_archive(
                &swift,
                ArchiveSelection::SwiftProtobuf(self.lock.swift_protobuf()),
                &self.tool_stage.join("swift/source"),
            )
            .map_err(archive_error)?;
            new_directory(&self.tool_stage.join("bin"))?;
            copy_tree(
                &self
                    .tool_stage
                    .join("swift/source")
                    .join(self.lock.swift_protobuf().top_level()),
                &self.run.join("swift-build"),
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            if fs::remove_dir_all(&self.tool_stage).is_err() {
                return Err(CacheError::CleanupAfter(Box::new(error)));
            }
            return Err(error);
        }
        Ok(())
    }
    fn versions(&self, published: bool) -> Result<()> {
        for (file, prefix, version) in [
            (
                if published {
                    "published-protoc.version"
                } else {
                    "protoc.version"
                },
                "libprotoc",
                self.lock.protoc().expected_tool_version(),
            ),
            (
                if published {
                    "published-generator.version"
                } else {
                    "generator.version"
                },
                "protoc-gen-swift",
                self.lock.swift_protobuf().expected_tool_version(),
            ),
        ] {
            if read_file(&self.run.join(file), 4096)? != format!("{prefix} {version}\n").as_bytes()
            {
                return Err(CacheError::InvalidInput);
            }
        }
        Ok(())
    }
    fn verify_tools(&self) -> Result<()> {
        self.versions(false)?;
        let scratch = self.run.join("swift-scratch");
        real_directory(&scratch)?;
        let binary = scratch.join("release/protoc-gen-swift").canonicalize()?;
        if !binary.starts_with(&scratch)
            || !fs::symlink_metadata(&binary)?.is_file()
            || fs::metadata(&binary)?.permissions().mode() & 0o111 == 0
        {
            return Err(CacheError::UnsafePath);
        }
        let bytes = read_file(&binary, 256 * 1024 * 1024)?;
        write_new(&self.tool_stage.join("bin/protoc-gen-swift"), &bytes, true)?;
        let mut manifest = Manifest::new("tools", &self.lock, &self.cargo, &self.tool_stage)?;
        manifest.archives = Some(vec![
            ArchiveRecord {
                id: "protoc".into(),
                version: self.lock.protoc().version().into(),
                sha256: self.lock.protoc().sha256().into(),
                path: "archives/protoc.zip".into(),
                source_commit: None,
            },
            ArchiveRecord {
                id: "swift".into(),
                version: self.lock.swift_protobuf().version().into(),
                sha256: self.lock.swift_protobuf().sha256().into(),
                path: "archives/swift.tar.gz".into(),
                source_commit: Some(self.lock.swift_protobuf().source_commit().into()),
            },
        ]);
        manifest.executables = Some(vec![
            ExecutableRecord {
                id: "protoc".into(),
                path: "protoc/bin/protoc".into(),
                sha256: digest(&read_file(
                    &self.tool_stage.join("protoc/bin/protoc"),
                    256 * 1024 * 1024,
                )?),
                expected_version: self.lock.protoc().expected_tool_version().into(),
            },
            ExecutableRecord {
                id: "generator".into(),
                path: "bin/protoc-gen-swift".into(),
                sha256: digest(&bytes),
                expected_version: self.lock.swift_protobuf().expected_tool_version().into(),
            },
        ]);
        manifest.generator_build_path =
            Some(self.tools.to_str().ok_or(CacheError::UnsafePath)?.into());
        manifest.write(&self.tool_stage)?;
        self.validate_tools(&self.tool_stage)
    }
    fn validate_tools(&self, path: &Path) -> Result<()> {
        self.validate_tool_inputs(path, false)
    }
    fn validate_tool_inputs(&self, path: &Path, rebuild: bool) -> Result<()> {
        let m = if rebuild {
            Manifest::read_for_generator_rebuild(path, &self.lock, &self.cargo)?
        } else {
            Manifest::read(path, "tools", &self.lock, &self.cargo)?
        };
        let archives = m.archives.ok_or(CacheError::InvalidInput)?;
        for (a, id, version, sha, member, commit) in [
            (
                &archives[0],
                "protoc",
                self.lock.protoc().version(),
                self.lock.protoc().sha256(),
                "archives/protoc.zip",
                None,
            ),
            (
                &archives[1],
                "swift",
                self.lock.swift_protobuf().version(),
                self.lock.swift_protobuf().sha256(),
                "archives/swift.tar.gz",
                Some(self.lock.swift_protobuf().source_commit()),
            ),
        ] {
            if a.id != id
                || a.version != version
                || a.sha256 != sha
                || a.path != member
                || a.source_commit.as_deref() != commit
                || digest(&read_file(&path.join(member), MAX_COMPRESSED_BYTES)?) != sha
            {
                return Err(CacheError::DigestMismatch);
            }
        }
        let executables = m.executables.ok_or(CacheError::InvalidInput)?;
        for (e, id, member, version) in [
            (
                &executables[0],
                "protoc",
                "protoc/bin/protoc",
                self.lock.protoc().expected_tool_version(),
            ),
            (
                &executables[1],
                "generator",
                "bin/protoc-gen-swift",
                self.lock.swift_protobuf().expected_tool_version(),
            ),
        ] {
            if e.id != id
                || e.path != member
                || e.expected_version != version
                || !valid_digest(&e.sha256)
            {
                return Err(CacheError::DigestMismatch);
            }
            if !(rebuild && id == "generator")
                && (digest(&read_file(&path.join(member), 256 * 1024 * 1024)?) != e.sha256
                    || fs::metadata(path.join(member))?.permissions().mode() & 0o111 == 0)
            {
                return Err(CacheError::DigestMismatch);
            }
        }
        if !rebuild && m.generator_build_path.as_deref() != self.tools.to_str() {
            return Err(CacheError::Incomplete);
        }
        Ok(())
    }
    fn stage_cargo(&self) -> Result<()> {
        let work = self.cargo_root.join("work/registry");
        real_directory(&work)?;
        let mut registries = Vec::new();
        for entry in fs::read_dir(work.join("index"))? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|s| s.starts_with("index.crates.io-"))
            {
                real_directory(&entry.path())?;
                registries.push(entry.file_name());
            }
        }
        if registries.len() != 1 {
            return Err(CacheError::InvalidInput);
        }
        let registry = registries[0].to_str().ok_or(CacheError::UnsafePath)?;
        new_directory(&self.cargo_stage)?;
        new_directory(&self.cargo_stage.join("registry"))?;
        new_directory(&self.cargo_stage.join("registry/cache"))?;
        new_directory(&self.cargo_stage.join("registry/index"))?;
        new_directory(&self.cargo_stage.join("registry/cache").join(registry))?;
        new_directory(&self.cargo_stage.join("registry/index").join(registry))?;
        new_directory(
            &self
                .cargo_stage
                .join("registry/index")
                .join(registry)
                .join(".cache"),
        )?;
        let config = read_file(&work.join("index").join(registry).join("config.json"), 4096)?;
        let config_json: serde_json::Value =
            serde_json::from_slice(&config).map_err(|_| CacheError::InvalidInput)?;
        if config_json
            != serde_json::json!({"dl":"https://static.crates.io/crates","api":"https://crates.io"})
        {
            return Err(CacheError::InvalidInput);
        }
        write_new(
            &self
                .cargo_stage
                .join("registry/index")
                .join(registry)
                .join("config.json"),
            &config,
            false,
        )?;
        for p in self.packages.iter().filter(|p| p.source.is_some()) {
            let archive = PathBuf::from("registry/cache")
                .join(registry)
                .join(format!("{}-{}.crate", p.name, p.version));
            let bytes = read_file(
                &self.cargo_root.join("work").join(&archive),
                MAX_COMPRESSED_BYTES,
            )?;
            if Some(digest(&bytes)).as_ref() != p.checksum.as_ref() {
                return Err(CacheError::DigestMismatch);
            }
            write_new(&self.cargo_stage.join(&archive), &bytes, false)?;
            let index = PathBuf::from("registry/index")
                .join(registry)
                .join(".cache")
                .join(index_path(&p.name));
            let bytes = read_file(&self.cargo_root.join("work").join(&index), MAX_MANIFEST)?;
            check_index(&bytes, p)?;
            let target = self.cargo_stage.join(&index);
            create_parents(
                &self.cargo_stage,
                target.parent().ok_or(CacheError::UnsafePath)?,
            )?;
            if !target.exists() {
                write_new(&target, &bytes, false)?;
            }
        }
        Ok(())
    }
    fn package_records(&self, root: &Path) -> Result<Vec<PackageRecord>> {
        let (_, files) = inventory(root, "cargo")?;
        let mut records = Vec::new();
        for p in self.packages.iter().filter(|p| p.source.is_some()) {
            let suffix = format!("/{}-{}.crate", p.name, p.version);
            let matches: Vec<_> = files
                .iter()
                .filter(|f| f.role == "archive" && f.path.ends_with(&suffix))
                .collect();
            if matches.len() != 1 || Some(&matches[0].sha256) != p.checksum.as_ref() {
                return Err(CacheError::DigestMismatch);
            }
            records.push(PackageRecord {
                name: p.name.clone(),
                version: p.version.clone(),
                source: p.source.clone().ok_or(CacheError::InvalidInput)?,
                checksum: p.checksum.clone().ok_or(CacheError::InvalidInput)?,
                archive_path: matches[0].path.clone(),
            });
        }
        if files.iter().filter(|f| f.role == "archive").count() != records.len() {
            return Err(CacheError::InvalidInput);
        }
        records.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
        Ok(records)
    }
    fn verify_cargo(&self) -> Result<()> {
        if read_file(&self.run.join("offline-build.receipt"), 64)? != b"bootstrap-only\n" {
            return Err(CacheError::Incomplete);
        }
        let mut m = Manifest::new("cargo", &self.lock, &self.cargo, &self.cargo_stage)?;
        m.packages = Some(self.package_records(&self.cargo_stage)?);
        m.checked_targets = Some(vec!["asura-toolchain-bootstrap".into()]);
        m.write(&self.cargo_stage)?;
        self.validate_cargo(&self.cargo_stage)
    }
    fn validate_cargo(&self, path: &Path) -> Result<()> {
        let m = Manifest::read(path, "cargo", &self.lock, &self.cargo)?;
        if m.packages.as_ref() != Some(&self.package_records(path)?) {
            return Err(CacheError::DigestMismatch);
        }
        let mut expected = std::collections::BTreeSet::new();
        for package in m.packages.as_ref().ok_or(CacheError::InvalidInput)? {
            let components: Vec<_> = package.archive_path.split('/').collect();
            if components.len() != 4 || components[0] != "registry" || components[1] != "cache" {
                return Err(CacheError::InvalidInput);
            }
            let registry = components[2];
            let config = format!("registry/index/{registry}/config.json");
            let value: serde_json::Value =
                serde_json::from_slice(&read_file(&path.join(&config), 4096)?)
                    .map_err(|_| CacheError::InvalidInput)?;
            if value
                != serde_json::json!({"dl":"https://static.crates.io/crates","api":"https://crates.io"})
            {
                return Err(CacheError::InvalidInput);
            }
            let index = format!(
                "registry/index/{registry}/.cache/{}",
                index_path(&package.name)
            );
            let locked = self
                .packages
                .iter()
                .find(|p| p.name == package.name && p.version == package.version)
                .ok_or(CacheError::InvalidInput)?;
            check_index(&read_file(&path.join(&index), MAX_MANIFEST)?, locked)?;
            expected.insert(config);
            expected.insert(index);
            expected.insert(package.archive_path.clone());
        }
        if m.files
            .iter()
            .any(|f| !expected.contains(&f.path) || f.executable)
        {
            return Err(CacheError::InvalidInput);
        }
        Ok(())
    }
    fn paths(&self, tools: bool) -> (PathBuf, PathBuf, PathBuf) {
        if tools {
            (
                self.tool_stage.clone(),
                self.tools.clone(),
                self.tool_backup.clone(),
            )
        } else {
            (
                self.cargo_stage.clone(),
                self.cargo_root.join("snapshot"),
                self.cargo_backup.clone(),
            )
        }
    }
    fn validate(&self, path: &Path, tools: bool) -> Result<()> {
        if tools {
            self.validate_tools(path)
        } else {
            self.validate_cargo(path)
        }
    }
    fn begin(&self, tools: bool) -> Result<()> {
        self.begin_with(tools, |source, target| fs::rename(source, target))
    }
    fn begin_with(
        &self,
        tools: bool,
        mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<()> {
        let (stage, published, backup) = self.paths(tools);
        self.validate(&stage, tools)?;
        if backup.exists() {
            return Err(CacheError::CleanupRequired);
        }
        let intent = PublishIntent {
            candidate: directory_identity(&stage)?,
            prior: if published.exists() {
                Some(directory_identity(&published)?)
            } else {
                None
            },
        };
        write_new(
            &self.run.join(if tools {
                "publish-tools.intent.json"
            } else {
                "publish-cargo.intent.json"
            }),
            &serde_json::to_vec(&intent).map_err(|_| CacheError::InvalidInput)?,
            false,
        )?;
        if published.exists() {
            real_directory(&published)?;
            rename(&published, &backup)?;
        }
        if rename(&stage, &published).is_err() {
            if backup.exists() && rename(&backup, &published).is_err() {
                return Err(CacheError::CleanupAfter(Box::new(CacheError::Io)));
            }
            return Err(CacheError::Io);
        }
        Ok(())
    }
    fn finish(&self, tools: bool) -> Result<()> {
        self.finish_with(tools, |path| fs::remove_dir_all(path))
    }
    fn finish_with(
        &self,
        tools: bool,
        mut remove: impl FnMut(&Path) -> std::io::Result<()>,
    ) -> Result<()> {
        let (_, published, backup) = self.paths(tools);
        self.validate(&published, tools)?;
        if tools {
            self.versions(true)?;
        }
        if backup.exists() {
            real_directory(&backup)?;
            remove(&backup).map_err(|_| CacheError::CleanupRequired)?;
        }
        Ok(())
    }
    fn rollback(&self, tools: bool) -> Result<()> {
        let (_, published, backup) = self.paths(tools);
        let bytes = match read_file(
            &self.run.join(if tools {
                "publish-tools.intent.json"
            } else {
                "publish-cargo.intent.json"
            }),
            4096,
        ) {
            Ok(bytes) => bytes,
            Err(CacheError::MissingInput) => return Ok(()),
            Err(e) => return Err(e),
        };
        let intent: PublishIntent =
            serde_json::from_slice(&bytes).map_err(|_| CacheError::CleanupRequired)?;
        if backup.exists() {
            if Some(directory_identity(&backup)?) != intent.prior {
                return Err(CacheError::CleanupRequired);
            }
            self.validate(&backup, tools)
                .map_err(|_| CacheError::CleanupRequired)?;
            if published.exists() {
                self.quarantine(&published)?;
            }
            fs::rename(&backup, &published).map_err(|_| CacheError::CleanupRequired)?;
            self.validate(&published, tools)
                .map_err(|_| CacheError::CleanupRequired)
        } else if published.exists() {
            let current = directory_identity(&published)?;
            if Some(current) == intent.prior {
                Ok(())
            } else if current == intent.candidate {
                self.quarantine(&published)
            } else {
                Err(CacheError::CleanupRequired)
            }
        } else {
            if intent.prior.is_none() {
                Ok(())
            } else {
                Err(CacheError::CleanupRequired)
            }
        }
    }
    fn quarantine(&self, path: &Path) -> Result<()> {
        real_directory(path)?;
        let name = path
            .file_name()
            .and_then(|p| p.to_str())
            .ok_or(CacheError::UnsafePath)?;
        let target = path.with_file_name(format!(".quarantine-{}-{name}", self.id));
        if target.exists() {
            return Err(CacheError::CleanupRequired);
        }
        fs::rename(path, target).map_err(|_| CacheError::CleanupRequired)
    }
    fn recover(&self) -> Result<()> {
        for tools in [true, false] {
            let parent = if tools {
                &self.tool_root
            } else {
                &self.cargo_root
            };
            let (_, published, _) = self.paths(tools);
            let mut backups = Vec::new();
            let mut stages = Vec::new();
            for entry in fs::read_dir(parent)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_str().ok_or(CacheError::UnsafePath)?;
                if name.starts_with(".backup-") {
                    backups.push(entry.path());
                }
                if name.starts_with(".stage-") {
                    stages.push(entry.path());
                }
            }
            if backups.len() > 1 {
                return Err(CacheError::CleanupRequired);
            }
            if let Some(backup) = backups.first() {
                self.validate_recovery_input(backup, tools)
                    .map_err(|_| CacheError::CleanupRequired)?;
                if published.exists() {
                    self.quarantine(&published)?;
                }
                fs::rename(backup, &published).map_err(|_| CacheError::CleanupRequired)?;
            } else if published.exists() && self.validate_recovery_input(&published, tools).is_err()
            {
                // Preserve unresolved evidence and fail again on retry until repaired.
                return Err(CacheError::CleanupRequired);
            }
            for stage in stages {
                self.quarantine(&stage)?;
            }
        }
        Ok(())
    }
    fn validate_recovery_input(&self, path: &Path, tools: bool) -> Result<()> {
        if tools {
            self.validate_tool_inputs(path, true)
        } else {
            self.validate_cargo(path)
        }
    }
}
fn archive_error(error: crate::archive::ArchiveError) -> CacheError {
    CacheError::Archive {
        kind: error.kind,
        cleanup_failed: error.cleanup_failed,
    }
}
fn create_parents(root: &Path, parent: &Path) -> Result<()> {
    let mut current = root.to_owned();
    for part in parent
        .strip_prefix(root)
        .map_err(|_| CacheError::UnsafePath)?
        .components()
    {
        if let std::path::Component::Normal(name) = part {
            current.push(name);
            ensure_directory(&current)?;
        } else {
            return Err(CacheError::UnsafePath);
        }
    }
    Ok(())
}
fn index_path(name: &str) -> String {
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}
fn check_index(bytes: &[u8], p: &LockedPackage) -> Result<()> {
    let mut found = 0;
    for item in bytes.split(|b| *b == 0) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(item)
            && v["vers"].as_str() == Some(&p.version)
        {
            if v["name"].as_str() != Some(&p.name) || v["cksum"].as_str() != p.checksum.as_deref() {
                return Err(CacheError::DigestMismatch);
            }
            found += 1;
        }
    }
    if found == 1 {
        Ok(())
    } else {
        Err(CacheError::InvalidInput)
    }
}
pub fn run_operation(root: &Path, id: &str, operation: &str) -> Result<()> {
    let context = Context::load(root, id)?;
    let qualification = match operation {
        "inspect" => {
            if context.cargo_root.join("snapshot").exists() {
                context.validate_cargo(&context.cargo_root.join("snapshot"))?;
            }
            if context.tools.exists() {
                context.validate_tool_inputs(&context.tools, true)?;
            }
            "unqualified"
        }
        "fetch-protoc" => {
            context.fetch(false)?;
            "unqualified"
        }
        "fetch-swift" => {
            context.fetch(true)?;
            "unqualified"
        }
        "stage-tools" => {
            context.stage_tools()?;
            "unqualified"
        }
        "verify-tools" => {
            context.verify_tools()?;
            "tools-verified"
        }
        "stage-cargo" => {
            context.stage_cargo()?;
            "unqualified"
        }
        "verify-cargo" => {
            context.verify_cargo()?;
            "bootstrap-only"
        }
        "begin-publish-tools" => {
            context.begin(true)?;
            "tools-verified"
        }
        "begin-publish-cargo" => {
            context.begin(false)?;
            "bootstrap-only"
        }
        "finish-publish-tools" => {
            context.finish(true)?;
            "tools-verified"
        }
        "finish-publish-cargo" => {
            context.finish(false)?;
            "bootstrap-only"
        }
        "rollback-tools" => {
            context.rollback(true)?;
            "unqualified"
        }
        "rollback-cargo" => {
            context.rollback(false)?;
            "unqualified"
        }
        "recover" | "recover-start" => {
            context.recover()?;
            "unqualified"
        }
        _ => return Err(CacheError::InvalidInput),
    };
    context.receipt(operation, qualification)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};
    #[test]
    fn redirect_and_response_policy_rejects_unapproved_inputs() {
        let hosts = vec!["github.com".into(), "codeload.github.com".into()];
        let current = "https://github.com/source";
        assert!(
            redirect_target(current, &["https://codeload.github.com/archive"], 4, &hosts).is_ok()
        );
        assert!(redirect_target(current, &["/next"], 5, &hosts).is_err());
        assert!(redirect_target(current, &[], 0, &hosts).is_err());
        assert!(redirect_target(current, &["/a", "/b"], 0, &hosts).is_err());
        for location in [
            "http://github.com/a",
            "https://user@github.com/a",
            "https://u:p@github.com/a",
            "https://other.invalid/a",
            "https://github.com:444/a",
            "https://github.com/a#fragment",
        ] {
            assert!(
                redirect_target(current, &[location], 0, &hosts).is_err(),
                "{location}"
            );
        }
        assert!(response_policy(200, &[]).is_ok());
        assert!(response_policy(200, &["identity"]).is_ok());
        assert!(response_policy(206, &[]).is_err());
        assert!(response_policy(200, &["identity", "gzip"]).is_err());
    }
    #[test]
    fn bounded_archive_stream_cleans_up_failed_downloads() {
        let root = std::env::temp_dir().join(format!("asura-fetch-body-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let part = root.join("archive.part");
        let target = root.join("archive");
        let bytes = vec![7u8; MAX_COMPRESSED_BYTES];
        save_archive(Cursor::new(&bytes), &part, &target, &digest(&bytes)).unwrap();
        assert_eq!(
            fs::metadata(&target).unwrap().len(),
            MAX_COMPRESSED_BYTES as u64
        );
        assert!(!part.exists());
        fs::remove_file(&target).unwrap();
        assert!(matches!(
            save_archive(
                io::repeat(7).take(MAX_COMPRESSED_BYTES as u64 + 1),
                &part,
                &target,
                &digest(&bytes)
            ),
            Err(CacheError::Limit)
        ));
        assert!(!part.exists() && !target.exists());
        assert!(matches!(
            save_archive(Cursor::new(b"bad"), &part, &target, &digest(b"expected")),
            Err(CacheError::DigestMismatch)
        ));
        assert!(!part.exists() && !target.exists());
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("injected stream failure"))
            }
        }
        assert!(save_archive(Broken, &part, &target, &digest(b"")).is_err());
        assert!(!part.exists() && !target.exists());
        fs::remove_dir(&root).unwrap();
    }

    fn publication_fixture(name: &str) -> (PathBuf, Context) {
        let root =
            std::env::temp_dir().join(format!("asura-publication-{}-{name}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let tool_root = root.join("tools");
        let cargo_root = root.join("cargo");
        let run = root.join("run");
        for path in [&tool_root, &cargo_root, &run] {
            fs::create_dir(path).unwrap();
        }
        let ctx = Context {
            run,
            id: "fault".into(),
            lock: parse_lock(include_bytes!("../tests/fixtures/lock.json")).unwrap(),
            cargo: b"version=4\n".to_vec(),
            packages: vec![],
            tools: tool_root.join("published"),
            tool_stage: tool_root.join(".stage-fault"),
            tool_backup: tool_root.join(".backup-fault"),
            tool_root,
            cargo_stage: cargo_root.join(".stage-fault"),
            cargo_backup: cargo_root.join(".backup-fault"),
            cargo_root,
        };
        for path in [ctx.cargo_root.join("snapshot"), ctx.cargo_stage.clone()] {
            fs::create_dir(&path).unwrap();
            let mut manifest = Manifest::new("cargo", &ctx.lock, &ctx.cargo, &path).unwrap();
            manifest.packages = Some(vec![]);
            manifest.checked_targets = Some(vec!["asura-toolchain-bootstrap".into()]);
            manifest.write(&path).unwrap();
        }
        (root, ctx)
    }

    #[test]
    fn publication_rename_failures_preserve_prior_state_or_recovery_evidence() {
        for failed_calls in [&[1usize][..], &[2][..], &[2, 3][..]] {
            let (root, ctx) = publication_fixture(&format!(
                "rename-{}",
                failed_calls.len() * 10 + failed_calls[0]
            ));
            let published = ctx.cargo_root.join("snapshot");
            let prior = directory_identity(&published).unwrap();
            let mut calls = 0;
            let result = ctx.begin_with(false, |source, target| {
                calls += 1;
                if failed_calls.contains(&calls) {
                    Err(io::Error::other("injected rename failure"))
                } else {
                    fs::rename(source, target)
                }
            });
            assert!(result.is_err());
            assert!(ctx.run.join("publish-cargo.intent.json").is_file());
            if failed_calls.len() == 2 {
                assert!(matches!(result, Err(CacheError::CleanupAfter(_))));
                assert!(!published.exists());
                assert_eq!(directory_identity(&ctx.cargo_backup).unwrap(), prior);
            } else {
                assert_eq!(directory_identity(&published).unwrap(), prior);
            }
            ctx.rollback(false).unwrap();
            assert_eq!(directory_identity(&published).unwrap(), prior);
            assert!(!ctx.run.join("begin-publish-cargo.receipt").exists());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn publication_final_check_and_backup_removal_failures_can_roll_back() {
        for corrupt in [false, true] {
            let (root, ctx) = publication_fixture(if corrupt {
                "final-check"
            } else {
                "backup-removal"
            });
            let published = ctx.cargo_root.join("snapshot");
            let prior = directory_identity(&published).unwrap();
            ctx.begin(false).unwrap();
            if corrupt {
                fs::write(published.join("manifest.json"), b"corrupt").unwrap();
                assert!(ctx.finish(false).is_err());
            } else {
                assert_eq!(
                    ctx.finish_with(false, |_| Err(io::Error::other("injected removal failure")))
                        .unwrap_err(),
                    CacheError::CleanupRequired
                );
            }
            assert_eq!(directory_identity(&ctx.cargo_backup).unwrap(), prior);
            assert!(!ctx.run.join("finish-publish-cargo.receipt").exists());
            ctx.rollback(false).unwrap();
            assert_eq!(directory_identity(&published).unwrap(), prior);
            ctx.validate_cargo(&published).unwrap();
            fs::remove_dir_all(root).unwrap();
        }
    }
}
