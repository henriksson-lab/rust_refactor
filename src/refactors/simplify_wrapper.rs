//! Remove a selected transparent wrapper by replacing its resolved calls.
use anyhow::{anyhow, Context, Result};
use camino::Utf8PathBuf;
use ra_ap_syntax::{
    ast::{self, AstNode, HasArgList, HasName},
    SyntaxKind,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs};
use text_size::{TextRange, TextSize};

use crate::{
    analysis,
    cli::{OutputFormat, SimplifyWrapperCommand},
    edits::{apply_plan, RefactorPlan, TextEdit},
    project::Project,
    semantic::{SemanticProject, SemanticReference},
    verify,
};

use super::remove_function::{line_col_offset, range_json};

struct Refusal {
    code: &'static str,
    message: String,
    file: Option<Utf8PathBuf>,
    range: Option<TextRange>,
}

enum FlowError {
    Refused(Refusal),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for FlowError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

impl From<std::io::Error> for FlowError {
    fn from(error: std::io::Error) -> Self {
        Self::Failed(error.into())
    }
}

fn refuse<T>(
    code: &'static str,
    message: impl Into<String>,
    file: Option<Utf8PathBuf>,
    range: Option<TextRange>,
) -> std::result::Result<T, FlowError> {
    Err(FlowError::Refused(Refusal {
        code,
        message: message.into(),
        file,
        range,
    }))
}

pub fn run(command: SimplifyWrapperCommand) -> Result<i32> {
    match run_inner(&command) {
        Ok((status, plan, target)) => {
            emit(&command, status, Some(&plan), Some(&target), None);
            Ok(0)
        }
        Err(FlowError::Refused(reason)) => {
            emit(&command, "refused", None, None, Some(&reason));
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
    command: &SimplifyWrapperCommand,
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
    let input = if command.file.is_absolute() {
        command.file.clone()
    } else {
        project.root.join(&command.file)
    };
    let file = match fs::canonicalize(&input) {
        Ok(path) => {
            Utf8PathBuf::from_path_buf(path).map_err(|_| anyhow!("non-UTF-8 target file"))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return refuse(
                "TARGET_NOT_FOUND",
                "target file does not exist",
                Some(input),
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
    let target_file = analysis::parse_source_file(&file)?;
    let offset =
        line_col_offset(&target_file.source, command.line, command.column).ok_or_else(|| {
            FlowError::Refused(Refusal {
                code: "INVALID_POSITION",
                message: "position lies outside the file".to_owned(),
                file: Some(file.clone()),
                range: None,
            })
        })?;
    let selected = target_file
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
        .collect::<Vec<_>>();
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
    let files = analysis::parse_project_files_matching(&project, &name)?;
    let Some((drop_range, drop_callee)) = drop_wrapper_call(function) else {
        return refuse(
            "UNSUPPORTED_BODY",
            "selected function must only call drop on its single owned parameter",
            Some(file),
            Some(function_range),
        );
    };

    let semantic = if command.fast {
        None
    } else {
        Some(SemanticProject::load(&project)?)
    };
    let drop_definition = semantic
        .as_ref()
        .map(|semantic| semantic.definition_at(&file, drop_range))
        .transpose()?
        .flatten();
    let resolved_core_drop = drop_definition.as_ref().is_some_and(|definition| {
        definition
            .file
            .as_str()
            .replace('\\', "/")
            .contains("/library/core/src/mem/")
    });
    let explicit_core_drop = matches!(
        drop_callee.as_str(),
        "core::mem::drop" | "::core::mem::drop" | "std::mem::drop" | "::std::mem::drop"
    );
    let unshadowed_prelude_drop = drop_callee == "drop"
        && drop_definition.is_none()
        && target_file
            .tree
            .syntax()
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|token| token.text() == "drop")
            .count()
            == 1
        && !target_file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::Use::cast)
            .any(|item| item.syntax().text().to_string().contains('*'));
    if !(resolved_core_drop
        || explicit_core_drop && drop_definition.is_none()
        || unshadowed_prelude_drop)
    {
        return refuse(
            "UNRESOLVED_DROP",
            "could not prove that the wrapper calls core::mem::drop",
            Some(file),
            Some(drop_range),
        );
    }

    let name_range = function.name().unwrap().syntax().text_range();
    let references = if let Some(semantic) = &semantic {
        semantic.references_to(&file, name_range)?
    } else {
        let definitions = files
            .iter()
            .flat_map(|source| {
                source
                    .tree
                    .syntax()
                    .descendants()
                    .filter_map(ast::Fn::cast)
                    .collect::<Vec<_>>()
            })
            .filter(|candidate| candidate.name().is_some_and(|item| item.text() == name))
            .count();
        if definitions != 1 {
            return refuse(
                "AMBIGUOUS_FUNCTION",
                format!("fast mode found {definitions} functions named `{name}`"),
                Some(file),
                Some(function_range),
            );
        }
        files
            .iter()
            .flat_map(|source| {
                source
                    .tree
                    .syntax()
                    .descendants_with_tokens()
                    .filter_map(|element| element.into_token())
                    .filter(|token| token.kind() == SyntaxKind::IDENT && token.text() == name)
                    .map(|token| SemanticReference {
                        file: source.path.clone(),
                        range: token.text_range(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    let imports = analysis::collect_import_sites(&files);
    let calls = analysis::collect_call_sites_light(&files);
    let mut plan = RefactorPlan::empty();
    plan.edits.push(TextEdit {
        file: file.clone(),
        range: function_range,
        replacement: String::new(),
    });
    let mut calls_replaced = 0usize;
    let mut imports_removed = 0usize;
    for reference in references {
        if reference.file == file && function_range.contains_range(reference.range) {
            continue;
        }
        if let Some(import) = imports.iter().find(|site| {
            site.file == reference.file
                && site.name_range == reference.range
                && site.function_name == name
        }) {
            plan.edits.push(TextEdit {
                file: reference.file,
                range: import.use_range,
                replacement: String::new(),
            });
            imports_removed += 1;
            continue;
        }
        if let Some(call) = calls
            .iter()
            .find(|site| site.file == reference.file && site.callee_range == reference.range)
        {
            if call.args.len() != 1 {
                return refuse(
                    "UNSUPPORTED_CALL",
                    "drop wrapper call must have exactly one argument",
                    Some(reference.file),
                    Some(call.range),
                );
            }
            plan.edits.push(TextEdit {
                file: reference.file,
                range: call.range,
                replacement: format!("::core::mem::drop({})", call.args[0]),
            });
            calls_replaced += 1;
            continue;
        }
        return refuse(
            "UNHANDLED_REFERENCE",
            "function is referenced outside a direct call or simple import",
            Some(reference.file),
            Some(reference.range),
        );
    }

    let mut ranges: BTreeMap<&Utf8PathBuf, Vec<TextRange>> = BTreeMap::new();
    for edit in &plan.edits {
        ranges.entry(&edit.file).or_default().push(edit.range);
    }
    for file_ranges in ranges.values_mut() {
        file_ranges.sort_by_key(|range| range.start());
        if file_ranges
            .windows(2)
            .any(|pair| pair[0].end() > pair[1].start())
        {
            return refuse("CONFLICTING_EDITS", "planned edits overlap", None, None);
        }
    }
    let target = json!({"file":file,"name":name,"range":range_json(function_range),"pattern":"drop","calls_replaced":calls_replaced,"imports_removed":imports_removed});
    if command.dry_run {
        return Ok(("planned", plan, target));
    }

    let mut originals = BTreeMap::new();
    for edit in &plan.edits {
        originals
            .entry(edit.file.clone())
            .or_insert(fs::read_to_string(&edit.file)?);
    }
    let applied = (|| -> Result<()> {
        apply_plan(&plan)?;
        let edited_files = originals.keys().cloned().collect();
        verify::run_rustfmt_files(&project.manifest_path, &edited_files)?;
        verify::run_cargo_check(verify::CargoVerification {
            manifest_path: Some(&project.manifest_path),
            all_features: false,
            target: None,
        })?;
        Ok(())
    })();
    if let Err(error) = applied {
        for (path, original) in originals {
            fs::write(&path, original).with_context(|| format!("failed to restore {path}"))?;
        }
        return Err(FlowError::Failed(
            error.context("simplify-wrapper failed; original files restored"),
        ));
    }
    Ok(("applied", plan, target))
}

fn drop_wrapper_call(function: &ast::Fn) -> Option<(TextRange, String)> {
    if function.async_token().is_some()
        || function.const_token().is_some()
        || function.unsafe_token().is_some()
        || function
            .ret_type()
            .is_some_and(|ret| ret.syntax().text().to_string().trim() != "-> ()")
    {
        return None;
    }
    let params = function.param_list()?.params().collect::<Vec<_>>();
    if params.len() != 1
        || params[0]
            .ty()?
            .syntax()
            .text()
            .to_string()
            .trim_start()
            .starts_with('&')
    {
        return None;
    }
    let ast::Pat::IdentPat(param) = params[0].pat()? else {
        return None;
    };
    if !param.is_simple_ident() {
        return None;
    }
    let param_name = param.name()?.text().to_string();
    let body = function.body()?;
    let statements = body.stmt_list()?.statements().collect::<Vec<_>>();
    let call = if statements.is_empty() {
        ast::CallExpr::cast(body.stmt_list()?.tail_expr()?.syntax().clone())?
    } else if statements.len() == 1 && body.stmt_list()?.tail_expr().is_none() {
        let statement = ast::ExprStmt::cast(statements[0].syntax().clone())?;
        ast::CallExpr::cast(statement.expr()?.syntax().clone())?
    } else {
        return None;
    };
    let callee = call.expr()?;
    let callee_text = callee
        .syntax()
        .text()
        .to_string()
        .replace(char::is_whitespace, "");
    if !matches!(
        callee_text.as_str(),
        "drop" | "core::mem::drop" | "::core::mem::drop" | "std::mem::drop" | "::std::mem::drop"
    ) {
        return None;
    }
    let args = call.arg_list()?.args().collect::<Vec<_>>();
    if args.len() != 1 || args[0].syntax().text().to_string().trim() != param_name {
        return None;
    }
    let name_ref = callee
        .syntax()
        .descendants()
        .filter_map(ast::NameRef::cast)
        .last()?;
    Some((name_ref.syntax().text_range(), callee_text))
}

fn emit(
    command: &SimplifyWrapperCommand,
    status: &str,
    plan: Option<&RefactorPlan>,
    target: Option<&Value>,
    refusal: Option<&Refusal>,
) {
    let edits = plan
        .into_iter()
        .flat_map(|plan| &plan.edits)
        .map(|edit| json!({"file":edit.file,"range":range_json(edit.range),"replacement":edit.replacement}))
        .collect::<Vec<_>>();
    let diagnostics = refusal
        .into_iter()
        .map(|reason| json!({"code":reason.code,"message":reason.message,"file":reason.file,"range":reason.range.map(range_json)}))
        .collect::<Vec<_>>();
    match command.format {
        OutputFormat::Json => println!(
            "{}",
            json!({"status":status,"target":target,"edits":edits,"diagnostics":diagnostics})
        ),
        OutputFormat::Text => {
            println!("{status}: {} edits", edits.len());
            if let Some(reason) = refusal {
                eprintln!("{}: {}", reason.code, reason.message);
            }
        }
    }
}
