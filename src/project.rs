use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{Metadata, MetadataCommand};
use std::fs;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct Project {
    pub root: Utf8PathBuf,
    pub manifest_path: Utf8PathBuf,
    pub rust_files: Vec<Utf8PathBuf>,
}

impl Project {
    pub fn load(manifest_path: Option<&Utf8Path>) -> Result<Self> {
        let mut command = MetadataCommand::new();
        command.no_deps();

        if let Some(manifest_path) = manifest_path {
            command.manifest_path(manifest_path);
        }

        let metadata = command
            .exec()
            .context("failed to load Cargo workspace metadata")?;
        Self::from_metadata(metadata)
    }

    fn from_metadata(metadata: Metadata) -> Result<Self> {
        let root = canonical_path(&metadata.workspace_root)?;
        let manifest_path = root.join("Cargo.toml");
        let mut rust_files = Vec::new();

        for package in metadata.workspace_packages() {
            let package_root = package
                .manifest_path
                .parent()
                .context("workspace package manifest has no parent directory")?;
            let package_root = canonical_path(package_root)?;

            collect_rust_files(&package_root, &mut rust_files)?;
        }

        rust_files.sort();
        rust_files.dedup();

        Ok(Self {
            root,
            manifest_path,
            rust_files,
        })
    }
}

fn canonical_path(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let canonical =
        fs::canonicalize(path).with_context(|| format!("failed to canonicalize {path}"))?;
    Utf8PathBuf::from_path_buf(canonical)
        .map_err(|path| anyhow::anyhow!("non-UTF-8 path: {}", path.display()))
}

fn collect_rust_files(root: &Utf8Path, files: &mut Vec<Utf8PathBuf>) -> Result<()> {
    for entry in WalkDir::new(root).into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        entry.depth() == 0
            || !matches!(
                name.as_ref(),
                "target" | ".git" | ".tmp" | ".claude" | ".agents" | ".codex"
            )
    }) {
        let entry = entry?;

        if !entry.file_type().is_file() {
            continue;
        }

        let path = Utf8PathBuf::from_path_buf(entry.path().to_path_buf())
            .map_err(|path| anyhow::anyhow!("non-UTF-8 path: {}", path.display()))?;

        if path.extension() == Some("rs") {
            files.push(path);
        }
    }

    Ok(())
}
