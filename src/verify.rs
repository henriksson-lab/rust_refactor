use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use std::{collections::BTreeSet, fs, process::Command};

#[derive(Debug, Clone, Copy)]
pub struct CargoVerification<'a> {
    pub manifest_path: Option<&'a Utf8Path>,
    pub all_features: bool,
    pub target: Option<&'a str>,
}

pub fn run_cargo_fmt(manifest_path: Option<&Utf8Path>) -> Result<()> {
    let mut command = Command::new("cargo");
    command.arg("fmt");
    if let Some(manifest_path) = manifest_path {
        command.arg("--manifest-path").arg(manifest_path);
    }

    let status = command.status().context("failed to start cargo fmt")?;

    if !status.success() {
        bail!("cargo fmt failed with status {status}");
    }

    Ok(())
}

pub fn run_rustfmt_files(manifest_path: &Utf8Path, files: &BTreeSet<Utf8PathBuf>) -> Result<()> {
    let metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(manifest_path)
        .no_deps()
        .exec()
        .context("failed to read package editions for rustfmt")?;
    for file in files {
        let edition = metadata
            .workspace_packages()
            .into_iter()
            .filter_map(|package| {
                let root = package.manifest_path.parent()?;
                let canonical = fs::canonicalize(root).ok()?;
                file.as_std_path()
                    .starts_with(&canonical)
                    .then_some((canonical.as_os_str().len(), package.edition.to_string()))
            })
            .max_by_key(|(length, _)| *length)
            .map(|(_, edition)| edition)
            .unwrap_or_else(|| "2021".to_owned());
        let status = Command::new("rustfmt")
            .arg("--edition")
            .arg(edition)
            .arg("--config")
            .arg("skip_children=true")
            .arg(file)
            .status()
            .with_context(|| format!("failed to format {file}"))?;
        if !status.success() {
            bail!("rustfmt failed for {file} with status {status}");
        }
    }
    Ok(())
}

pub fn run_cargo_check(options: CargoVerification<'_>) -> Result<()> {
    let mut command = Command::new("cargo");
    command.arg("check");
    add_verification_args(&mut command, options);

    let status = command.status().context("failed to start cargo check")?;

    if !status.success() {
        bail!("cargo check failed with status {status}");
    }

    Ok(())
}

pub fn run_cargo_test(options: CargoVerification<'_>) -> Result<()> {
    let mut command = Command::new("cargo");
    command.arg("test");
    add_verification_args(&mut command, options);

    let status = command.status().context("failed to start cargo test")?;

    if !status.success() {
        bail!("cargo test failed with status {status}");
    }

    Ok(())
}

fn add_verification_args(command: &mut Command, options: CargoVerification<'_>) {
    if let Some(manifest_path) = options.manifest_path {
        command.arg("--manifest-path").arg(manifest_path);
    }

    if options.all_features {
        command.arg("--all-features");
    }

    if let Some(target) = options.target {
        command.arg("--target").arg(target);
    }
}
