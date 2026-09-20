use anyhow::Result;
use camino::Utf8PathBuf;
use ra_ap_syntax::ast::{self, AstNode, HasName};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    analysis,
    cli::{OutputFormat, ToOopStatsCommand},
    project::Project,
};

#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub struct_name: String,
    pub function_name: String,
    pub file: Utf8PathBuf,
    pub line: usize,
    pub column: usize,
    pub receiver: String,
    pub local_struct: bool,
}

impl Candidate {
    pub(crate) fn selectable(&self) -> bool {
        self.local_struct && self.receiver != "optional"
    }
}

pub(crate) fn collect(files: &[analysis::ParsedFile]) -> Vec<Candidate> {
    let known_structs: BTreeSet<String> = files
        .iter()
        .flat_map(|file| {
            file.tree
                .syntax()
                .descendants()
                .filter_map(ast::Struct::cast)
                .filter_map(|item| item.name().map(|name| name.text().to_string()))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut result = Vec::new();
    for file in files {
        for function in file.tree.syntax().descendants().filter_map(ast::Fn::cast) {
            if function.syntax().ancestors().skip(1).any(|node| {
                ast::Impl::cast(node.clone()).is_some()
                    || ast::Trait::cast(node.clone()).is_some()
                    || ast::ExternBlock::cast(node.clone()).is_some()
                    || ast::Fn::cast(node).is_some()
            }) {
                continue;
            }
            let Some(name) = function.name() else {
                continue;
            };
            let Some(first) = function
                .param_list()
                .and_then(|params| params.params().next())
            else {
                continue;
            };
            let Some(ast::Pat::IdentPat(binding)) = first.pat() else {
                continue;
            };
            if !binding.is_simple_ident() {
                continue;
            }
            let Some(ty) = first.ty() else {
                continue;
            };
            let Some((struct_name, receiver)) = receiver_type(&ty.syntax().text().to_string())
            else {
                continue;
            };
            if !known_structs.contains(&struct_name) {
                continue;
            }
            let parent = function.syntax().parent();
            let local_struct = file
                .tree
                .syntax()
                .descendants()
                .filter_map(ast::Struct::cast)
                .any(|item| {
                    item.name()
                        .is_some_and(|name| name.text().to_string() == struct_name)
                        && item.syntax().parent() == parent
                });
            let offset = u32::from(name.syntax().text_range().start()) as usize;
            let before = &file.source[..offset];
            let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
            let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
            result.push(Candidate {
                struct_name,
                function_name: name.text().to_string(),
                file: file.path.clone(),
                line,
                column,
                receiver: receiver.to_owned(),
                local_struct,
            });
        }
    }
    result
        .sort_by(|a, b| (&a.struct_name, &a.file, a.line).cmp(&(&b.struct_name, &b.file, b.line)));
    result
}

fn receiver_type(raw: &str) -> Option<(String, &'static str)> {
    let text = raw.trim();
    let (name, receiver) = if let Some(inner) = text
        .strip_prefix("Option<&mut ")
        .and_then(|s| s.strip_suffix('>'))
    {
        (inner, "optional")
    } else if let Some(inner) = text
        .strip_prefix("Option<&")
        .and_then(|s| s.strip_suffix('>'))
    {
        (inner, "optional")
    } else if let Some(inner) = text.strip_prefix("&mut ") {
        (inner, "&mut")
    } else if let Some(inner) = text.strip_prefix('&') {
        (inner.trim(), "&")
    } else {
        (text, "value")
    };
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || name.chars().next()?.is_ascii_digit()
    {
        return None;
    }
    Some((name.to_owned(), receiver))
}

pub fn run(command: ToOopStatsCommand) -> Result<()> {
    let project = Project::load(command.manifest_path.as_deref())?;
    let files = analysis::parse_project_files(&project)?;
    let candidates = collect(&files);
    let mut groups: BTreeMap<String, Vec<&Candidate>> = BTreeMap::new();
    for candidate in &candidates {
        if command
            .struct_name
            .as_ref()
            .is_some_and(|name| name != &candidate.struct_name)
        {
            continue;
        }
        groups
            .entry(candidate.struct_name.clone())
            .or_default()
            .push(candidate);
    }
    let mut sorted: Vec<_> = groups.into_iter().collect();
    sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    match command.format {
        OutputFormat::Json => {
            let output: Vec<_> = sorted.iter().map(|(name, functions)| json!({
                "struct": name,
                "count": functions.len(),
                "selectable_count": functions.iter().filter(|f| f.selectable()).count(),
                "functions": functions.iter().map(|f| {
                    let relative = f.file.strip_prefix(&project.root).unwrap_or(&f.file);
                    json!({"name":f.function_name,"file":relative,"line":f.line,"column":f.column,
                        "receiver":f.receiver,"local_struct":f.local_struct,"selectable":f.selectable(),
                        "selection":format!("{}:{}:{}",relative,f.line,f.column)})
                }).collect::<Vec<_>>()
            })).collect();
            println!("{}", json!({"groups":output}));
        }
        OutputFormat::Text => {
            println!("COUNT  READY  STRUCT  FUNCTION  RECEIVER  SELECTION");
            for (name, functions) in sorted {
                let count = functions.len();
                let ready = functions.iter().filter(|f| f.selectable()).count();
                for function in functions {
                    let relative = function
                        .file
                        .strip_prefix(&project.root)
                        .unwrap_or(&function.file);
                    println!(
                        "{:<6} {:<6} {:<7} {:<30} {:<9} {}:{}:{}",
                        count,
                        ready,
                        name,
                        function.function_name,
                        function.receiver,
                        relative,
                        function.line,
                        function.column
                    );
                }
            }
        }
    }
    Ok(())
}
