use std::{cell::RefCell, collections::BTreeMap, time::Instant};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use ra_ap_ide::{
    Analysis, AnalysisHost, FileId, FilePosition, FindAllRefsConfig, GotoDefinitionConfig,
    RaFixtureConfig,
};
use ra_ap_load_cargo::{LoadCargoConfig, ProcMacroServerChoice};
use ra_ap_paths::AbsPathBuf;
use ra_ap_project_model::{CargoConfig, CargoFeatures, ProjectManifest, ProjectWorkspace};
use ra_ap_vfs::{FileExcluded, Vfs, VfsPath};

use crate::project::Project;
use text_size::TextRange;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticReference {
    pub file: Utf8PathBuf,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDefinition {
    pub file: Utf8PathBuf,
    pub name_range: TextRange,
}

pub struct SemanticProject {
    _host: AnalysisHost,
    vfs: Vfs,
    reference_cache: RefCell<BTreeMap<(Utf8PathBuf, u32, u32), Vec<SemanticReference>>>,
    definition_cache: RefCell<BTreeMap<(Utf8PathBuf, u32, u32), Vec<SemanticDefinition>>>,
}

impl SemanticProject {
    pub fn load(project: &Project) -> Result<Self> {
        Self::load_with(project, false, None)
    }

    pub fn load_with(project: &Project, all_features: bool, target: Option<&str>) -> Result<Self> {
        let started = Instant::now();
        let manifest = AbsPathBuf::assert_utf8(project.manifest_path.clone().into_std_path_buf());
        let manifest = ProjectManifest::from_manifest_file(manifest)
            .context("failed to create rust-analyzer project manifest")?;
        let cargo_config = CargoConfig {
            all_targets: false,
            set_test: false,
            features: if all_features {
                CargoFeatures::All
            } else {
                CargoFeatures::default()
            },
            target: target.map(str::to_owned),
            ..CargoConfig::default()
        };
        let load_config = LoadCargoConfig {
            load_out_dirs_from_check: false,
            with_proc_macro_server: ProcMacroServerChoice::None,
            prefill_caches: false,
            num_worker_threads: 1,
            proc_macro_processes: 1,
        };
        let workspace = ProjectWorkspace::load(manifest, &cargo_config, &|_| {})
            .context("failed to load rust-analyzer project workspace")?;
        report_timing("semantic workspace metadata", started);
        let started = Instant::now();
        let (db, vfs, _proc_macro) =
            ra_ap_load_cargo::load_workspace(workspace, &cargo_config.extra_env, &load_config)
                .context("failed to load workspace into rust-analyzer database")?;
        report_timing("semantic workspace load", started);

        Ok(Self {
            _host: AnalysisHost::with_database(db),
            vfs,
            reference_cache: RefCell::new(BTreeMap::new()),
            definition_cache: RefCell::new(BTreeMap::new()),
        })
    }

    pub fn analysis(&self) -> Analysis {
        self._host.analysis()
    }

    pub fn file_id(&self, path: &Utf8Path) -> Option<FileId> {
        let path = VfsPath::new_real_path(path.to_string());
        self.vfs
            .file_id(&path)
            .and_then(|(file_id, excluded)| (excluded == FileExcluded::No).then_some(file_id))
    }

    pub fn references_to(
        &self,
        file: &Utf8Path,
        name_range: TextRange,
    ) -> Result<Vec<SemanticReference>> {
        let cache_key = (
            file.to_owned(),
            name_range.start().into(),
            name_range.end().into(),
        );
        if let Some(references) = self.reference_cache.borrow().get(&cache_key) {
            return Ok(references.clone());
        }
        let file_id = self
            .file_id(file)
            .with_context(|| format!("rust-analyzer VFS does not contain {file}"))?;
        let config = FindAllRefsConfig {
            search_scope: None,
            ra_fixture: RaFixtureConfig::default(),
            exclude_imports: false,
            exclude_tests: false,
        };
        let results = self
            .analysis()
            .find_all_refs(
                FilePosition {
                    file_id,
                    offset: name_range.start(),
                },
                &config,
            )
            .context("rust-analyzer reference search was cancelled")?
            .unwrap_or_default();
        let mut references = Vec::new();

        for result in results {
            for (file_id, ranges) in result.references {
                let path = self.file_path(file_id).with_context(|| {
                    format!("reference search found an unmapped file: {file_id:?}")
                })?;

                references.extend(
                    ranges
                        .into_iter()
                        .map(|(range, _category)| SemanticReference {
                            file: path.clone(),
                            range,
                        }),
                );
            }
        }

        self.reference_cache
            .borrow_mut()
            .insert(cache_key, references.clone());
        Ok(references)
    }

    pub fn definition_at(
        &self,
        file: &Utf8Path,
        range: TextRange,
    ) -> Result<Option<SemanticDefinition>> {
        let mut definitions = self.definitions_at(file, range)?;
        Ok((definitions.len() == 1).then(|| definitions.remove(0)))
    }

    pub fn definitions_at(
        &self,
        file: &Utf8Path,
        range: TextRange,
    ) -> Result<Vec<SemanticDefinition>> {
        let cache_key = (file.to_owned(), range.start().into(), range.end().into());
        if let Some(definitions) = self.definition_cache.borrow().get(&cache_key) {
            return Ok(definitions.clone());
        }
        let file_id = self
            .file_id(file)
            .with_context(|| format!("rust-analyzer VFS does not contain {file}"))?;
        let position = FilePosition {
            file_id,
            offset: range.start(),
        };
        let config = GotoDefinitionConfig {
            ra_fixture: RaFixtureConfig::default(),
        };
        let targets = self
            .analysis()
            .goto_definition(position, &config)
            .context("rust-analyzer definition lookup was cancelled")?;
        let definitions = targets
            .into_iter()
            .flat_map(|targets| targets.info)
            .filter_map(|target| {
                Some(SemanticDefinition {
                    file: self.file_path(target.file_id)?,
                    name_range: target.focus_range?,
                })
            })
            .collect::<Vec<_>>();
        self.definition_cache
            .borrow_mut()
            .insert(cache_key, definitions.clone());
        Ok(definitions)
    }

    fn file_path(&self, file_id: FileId) -> Option<Utf8PathBuf> {
        self.vfs
            .file_path(file_id)
            .as_path()
            .map(|path| Utf8PathBuf::from(<_ as AsRef<Utf8Path>>::as_ref(path)))
    }
}

fn report_timing(label: &str, started: Instant) {
    if std::env::var_os("RUST_REFACTOR_TIMING").is_some() {
        eprintln!(
            "rust-refactor timing: {label}: {:.3}s",
            started.elapsed().as_secs_f64()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn loads_workspace_and_maps_real_file_path() {
        let project_dir = TempDir::new().unwrap();
        let src = project_dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(
            project_dir.path().join("Cargo.toml"),
            r#"
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        let lib_rs = src.join("lib.rs");
        fs::write(&lib_rs, "pub fn helper() {}\n").unwrap();

        let project = Project::load(Some(
            Utf8Path::from_path(project_dir.path().join("Cargo.toml").as_path()).unwrap(),
        ))
        .unwrap();
        let semantic = SemanticProject::load(&project).unwrap();
        let lib_rs = camino::Utf8PathBuf::from_path_buf(lib_rs).unwrap();

        assert!(semantic.file_id(&lib_rs).is_some());
    }
}
