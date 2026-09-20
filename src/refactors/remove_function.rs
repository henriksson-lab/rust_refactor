//! Remove a free function and standalone calls to it. Unknown references abort the whole edit.
use anyhow::{anyhow, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use ra_ap_syntax::{
    ast::{self, AstNode, HasName},
    SyntaxKind,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs};
use text_size::{TextRange, TextSize};

use crate::{
    analysis,
    cli::{OutputFormat, RemoveFunctionCommand},
    edits::{apply_plan, RefactorPlan, TextEdit},
    project::Project,
    verify,
};

#[derive(Debug)]
struct Refusal {
    code: &'static str,
    message: String,
    file: Option<Utf8PathBuf>,
    range: Option<TextRange>,
}
impl Refusal {
    fn new(
        code: &'static str,
        message: impl Into<String>,
        file: Option<Utf8PathBuf>,
        range: Option<TextRange>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            file,
            range,
        }
    }
    fn json(&self) -> Value {
        json!({"code":self.code,"message":self.message,"file":self.file,"range":self.range.map(range_json)})
    }
}

enum FlowError {
    Refused(Refusal),
    Failed(anyhow::Error),
}
impl From<anyhow::Error> for FlowError {
    fn from(value: anyhow::Error) -> Self {
        Self::Failed(value)
    }
}
impl From<std::io::Error> for FlowError {
    fn from(value: std::io::Error) -> Self {
        Self::Failed(value.into())
    }
}
fn refuse<T>(
    code: &'static str,
    message: impl Into<String>,
    file: Option<Utf8PathBuf>,
    range: Option<TextRange>,
) -> std::result::Result<T, FlowError> {
    Err(FlowError::Refused(Refusal::new(code, message, file, range)))
}

pub fn run(command: RemoveFunctionCommand) -> Result<i32> {
    match run_inner(&command) {
        Ok((status, plan, target)) => {
            emit(&command, status, Some(&plan), Some(&target), &[])?;
            Ok(0)
        }
        Err(FlowError::Refused(reason)) => {
            emit(&command, "refused", None, None, &[reason])?;
            Ok(3)
        }
        Err(FlowError::Failed(error)) => {
            if matches!(command.format, OutputFormat::Json) {
                println!(
                    "{}",
                    json!({"status":"error","target":null,"edits":[],"diagnostics":[{"code":"OPERATION_FAILED","message":format!("{error:#}"),"file":null,"range":null}]})
                );
                Ok(1)
            } else {
                Err(error)
            }
        }
    }
}

fn run_inner(
    command: &RemoveFunctionCommand,
) -> std::result::Result<(&'static str, RefactorPlan, Value), FlowError> {
    if command.dry_run == command.write {
        return refuse(
            "INVALID_MODE",
            "choose exactly one of --dry-run or --write",
            None,
            None,
        );
    }
    if command.line == 0 || command.column == 0 {
        return refuse(
            "INVALID_POSITION",
            "line and column are one-based",
            None,
            None,
        );
    }
    let project = Project::load(command.manifest_path.as_deref())?;
    let file = if command.file.is_absolute() {
        command.file.clone()
    } else {
        project.root.join(&command.file)
    };
    let file = match fs::canonicalize(&file) {
        Ok(path) => {
            Utf8PathBuf::from_path_buf(path).map_err(|_| anyhow!("non-UTF-8 target file"))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return refuse(
                "TARGET_NOT_FOUND",
                "target file does not exist",
                Some(file),
                None,
            )
        }
        Err(error) => return Err(error.into()),
    };
    if !project.rust_files.contains(&file) {
        return refuse(
            "OUTSIDE_WORKSPACE",
            "target must be a workspace Rust source file",
            Some(file),
            None,
        );
    }
    let files = analysis::parse_project_files(&project)?;
    let target_file = files
        .iter()
        .find(|item| item.path == file)
        .ok_or_else(|| anyhow!("target was not parsed"))?;
    let offset =
        line_col_offset(&target_file.source, command.line, command.column).ok_or_else(|| {
            FlowError::Refused(Refusal::new(
                "INVALID_POSITION",
                "position lies outside the file",
                Some(file.clone()),
                None,
            ))
        })?;
    let selected: Vec<_> = target_file
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::Fn::cast)
        .filter(|function| {
            function.name().is_some_and(|name| {
                name.syntax()
                    .text_range()
                    .contains(TextSize::from(offset as u32))
            })
        })
        .collect();
    if selected.len() != 1 {
        return refuse(
            "TARGET_NOT_FOUND",
            "position must point at exactly one function name",
            Some(file),
            None,
        );
    }
    let function = &selected[0];
    let function_range = function.syntax().text_range();
    if function.syntax().ancestors().skip(1).any(|node| {
        ast::Impl::cast(node.clone()).is_some()
            || ast::Trait::cast(node.clone()).is_some()
            || ast::Fn::cast(node).is_some()
    }) {
        return refuse(
            "NOT_FREE_FUNCTION",
            "target must be a free function",
            Some(file),
            Some(function_range),
        );
    }
    let name = function.name().unwrap().text().to_string();
    // Name-based matching is only accepted if no other workspace definition can own a call.
    let definitions = files
        .iter()
        .flat_map(|item| {
            item.tree
                .syntax()
                .descendants()
                .filter_map(ast::Fn::cast)
                .filter_map(|f| f.name().map(|n| n.text().to_string()))
                .collect::<Vec<_>>()
        })
        .filter(|candidate| *candidate == name)
        .count();
    if definitions != 1 {
        return refuse(
            "AMBIGUOUS_FUNCTION",
            format!("found {definitions} workspace functions named `{name}`"),
            Some(file),
            Some(function_range),
        );
    }

    let imports = analysis::collect_import_sites(&files);
    let mut plan = RefactorPlan::empty();
    plan.edits.push(TextEdit {
        file: file.clone(),
        range: function_range,
        replacement: String::new(),
    });
    let mut calls_removed = 0usize;
    let mut imports_removed = 0usize;
    for source_file in &files {
        for token in source_file
            .tree
            .syntax()
            .descendants_with_tokens()
            .filter_map(|element| element.into_token())
        {
            if token.kind() != SyntaxKind::IDENT || token.text() != name {
                continue;
            }
            let range = token.text_range();
            if source_file.path == file && function_range.contains_range(range) {
                continue;
            }
            if let Some(import) = imports.iter().find(|site| {
                site.file == source_file.path
                    && site.function_name == name
                    && site.name_range == range
            }) {
                plan.edits.push(TextEdit {
                    file: source_file.path.clone(),
                    range: import.use_range,
                    replacement: String::new(),
                });
                imports_removed += 1;
                continue;
            }
            let call = token
                .parent()
                .and_then(|node| node.ancestors().find_map(ast::CallExpr::cast));
            if let Some(call) = call {
                if call
                    .expr()
                    .is_some_and(|expr| expr.syntax().text_range().contains_range(range))
                {
                    let statement = call.syntax().parent().and_then(ast::ExprStmt::cast);
                    if let Some(statement) = statement.filter(|stmt| {
                        stmt.expr().is_some_and(|expr| {
                            expr.syntax().text_range() == call.syntax().text_range()
                        })
                    }) {
                        plan.edits.push(TextEdit {
                            file: source_file.path.clone(),
                            range: statement.syntax().text_range(),
                            replacement: String::new(),
                        });
                        calls_removed += 1;
                        continue;
                    }
                    return refuse(
                        "NON_STANDALONE_CALL",
                        "call is used as an expression; removing it requires a replacement value",
                        Some(source_file.path.clone()),
                        Some(call.syntax().text_range()),
                    );
                }
            }
            return refuse(
                "UNHANDLED_REFERENCE",
                "function is referenced outside a removable standalone call or simple import",
                Some(source_file.path.clone()),
                Some(range),
            );
        }
    }
    // Guard against duplicate/overlapping ranges before touching disk.
    let mut ranges: BTreeMap<&Utf8Path, Vec<TextRange>> = BTreeMap::new();
    for edit in &plan.edits {
        ranges.entry(&edit.file).or_default().push(edit.range);
    }
    for (_, ranges) in ranges.iter_mut() {
        ranges.sort_by_key(|range| range.start());
        if ranges
            .windows(2)
            .any(|pair| pair[0].end() > pair[1].start() || pair[0] == pair[1])
        {
            return refuse("CONFLICTING_EDITS", "removal edits overlap", None, None);
        }
    }
    let target = json!({"file":file,"name":name,"range":range_json(function_range),"calls_removed":calls_removed,"imports_removed":imports_removed});
    if !command.write {
        return Ok(("planned", plan, target));
    }
    let mut snapshot = BTreeMap::new();
    for edit in &plan.edits {
        snapshot
            .entry(edit.file.clone())
            .or_insert(fs::read_to_string(&edit.file)?);
    }
    let applied = (|| -> Result<()> {
        apply_plan(&plan)?;
        let edited_files = snapshot.keys().cloned().collect();
        verify::run_rustfmt_files(&project.manifest_path, &edited_files)?;
        verify::run_cargo_check(verify::CargoVerification {
            manifest_path: Some(&project.manifest_path),
            all_features: false,
            target: None,
        })?;
        Ok(())
    })();
    if let Err(error) = applied {
        for (path, original) in snapshot {
            fs::write(&path, original).with_context(|| format!("failed to restore {path}"))?;
        }
        return Err(FlowError::Failed(
            error.context("remove-function failed; original files restored"),
        ));
    }
    Ok(("applied", plan, target))
}

pub(crate) fn line_col_offset(source: &str, line: usize, column: usize) -> Option<usize> {
    let start = source
        .split_inclusive('\n')
        .take(line.checked_sub(1)?)
        .map(str::len)
        .sum::<usize>();
    let text = source.get(start..)?.split('\n').next()?;
    let offset = text
        .char_indices()
        .nth(column - 1)
        .map(|(offset, _)| offset)
        .or_else(|| (column == text.chars().count() + 1).then_some(text.len()))?;
    Some(start + offset)
}

pub(crate) fn range_json(range: TextRange) -> Value {
    json!({"start":u32::from(range.start()),"end":u32::from(range.end())})
}

fn emit(
    command: &RemoveFunctionCommand,
    status: &str,
    plan: Option<&RefactorPlan>,
    target: Option<&Value>,
    refusals: &[Refusal],
) -> Result<()> {
    let edits: Vec<_> = plan.into_iter().flat_map(|plan| &plan.edits).map(|edit| json!({"file":edit.file,"range":range_json(edit.range),"replacement":edit.replacement})).collect();
    let diagnostics: Vec<_> = refusals.iter().map(Refusal::json).collect();
    match command.format {
        OutputFormat::Json => println!(
            "{}",
            json!({"status":status,"target":target,"edits":edits,"diagnostics":diagnostics})
        ),
        OutputFormat::Text => {
            println!("{status}: {} edits", edits.len());
            for diagnostic in refusals {
                eprintln!("{}: {}", diagnostic.code, diagnostic.message);
            }
        }
    }
    Ok(())
}
