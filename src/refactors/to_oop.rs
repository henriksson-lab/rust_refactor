use anyhow::{anyhow, bail, Result};
use camino::{Utf8Path, Utf8PathBuf};
use ra_ap_syntax::{
    ast::{self, AstNode, HasAttrs, HasGenericParams, HasName},
    Edition, SourceFile, SyntaxKind,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};
use text_size::{TextRange, TextSize};

use crate::{
    analysis,
    cli::{OutputFormat, ToOopCommand},
    edits::{apply_plan, RefactorPlan, TextEdit},
    project::Project,
    semantic::{SemanticDefinition, SemanticProject, SemanticReference},
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
    fn at(
        code: &'static str,
        message: impl Into<String>,
        file: &Utf8Path,
        range: TextRange,
    ) -> Self {
        Self::new(code, message, Some(file.to_owned()), Some(range))
    }
    fn json(&self) -> Value {
        json!({"code": self.code, "message": self.message, "file": self.file, "range": self.range.map(range_json)})
    }
}

struct Planned {
    plan: RefactorPlan,
    insertion_range: TextRange,
    insertion_body: String,
    new_impl: bool,
    target: Value,
    calls: Vec<(Utf8PathBuf, TextRange, usize)>,
    function_name: String,
    type_name: String,
    definition_file: Utf8PathBuf,
}

struct BatchPlanned {
    plan: RefactorPlan,
    targets: Vec<Planned>,
}

struct Selection {
    file: Utf8PathBuf,
    line: usize,
    column: usize,
}

pub fn run(command: ToOopCommand) -> Result<i32> {
    let result = run_inner(&command);
    match result {
        Ok((status, planned)) => {
            emit(&command, &status, planned.as_ref(), &[])?;
            Ok(0)
        }
        Err(FlowError::Refused(refusals)) => {
            emit(&command, "refused", None, &refusals)?;
            Ok(3)
        }
        Err(FlowError::Failed(error)) => {
            if matches!(command.format, OutputFormat::Json) {
                println!(
                    "{}",
                    json!({"status":"error", "target":null, "targets":[], "edits":[], "diagnostics":[{"code":"OPERATION_FAILED", "message":format!("{error:#}"), "file":null, "range":null}]})
                );
                Ok(1)
            } else {
                Err(error)
            }
        }
    }
}

enum FlowError {
    Refused(Vec<Refusal>),
    Failed(anyhow::Error),
}
impl From<anyhow::Error> for FlowError {
    fn from(value: anyhow::Error) -> Self {
        Self::Failed(value)
    }
}

fn refuse<T>(
    code: &'static str,
    message: impl Into<String>,
    file: Option<Utf8PathBuf>,
    range: Option<TextRange>,
) -> std::result::Result<T, FlowError> {
    Err(FlowError::Refused(vec![Refusal::new(
        code, message, file, range,
    )]))
}

fn run_inner(
    command: &ToOopCommand,
) -> std::result::Result<(String, Option<BatchPlanned>), FlowError> {
    if command.dry_run == command.write {
        return refuse(
            "INVALID_MODE",
            "choose exactly one of --dry-run or --write",
            None,
            None,
        );
    }
    let trace = std::env::var_os("RUST_REFACTOR_TRACE").is_some();
    let project = Project::load(command.manifest_path.as_deref())?;
    if trace {
        eprintln!("to-oop: Cargo workspace discovered");
    }
    let files = analysis::parse_project_files(&project)?;
    let function_name_counts = if command.fast {
        let mut counts = BTreeMap::new();
        for parsed in &files {
            for function in parsed.tree.syntax().descendants().filter_map(ast::Fn::cast) {
                if function.syntax().ancestors().skip(1).any(|node| {
                    ast::Impl::cast(node.clone()).is_some()
                        || ast::Trait::cast(node.clone()).is_some()
                        || ast::Fn::cast(node).is_some()
                }) {
                    continue;
                }
                if let Some(name) = function.name() {
                    *counts.entry(name.text().to_string()).or_insert(0usize) += 1;
                }
            }
        }
        counts
    } else {
        BTreeMap::new()
    };
    if trace {
        eprintln!("to-oop: {} Rust files parsed", files.len());
    }
    let selections = if let Some(struct_name) = &command.struct_name {
        let matches = super::oop_stats::collect(&files)
            .into_iter()
            .filter(|candidate| candidate.struct_name == *struct_name && candidate.selectable())
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return refuse(
                "NO_TARGETS",
                format!("no directly selectable free functions for struct `{struct_name}`"),
                None,
                None,
            );
        }
        matches
            .into_iter()
            .map(|candidate| Selection {
                file: candidate.file,
                line: candidate.line,
                column: candidate.column,
            })
            .collect()
    } else {
        parse_selections(command)?
    };
    let calls = analysis::collect_call_sites_light(&files);
    let imports = analysis::collect_import_sites(&files);
    let semantic = if command.fast {
        None
    } else {
        Some(SemanticProject::load_with(
            &project,
            command.all_features,
            command.target.as_deref(),
        )?)
    };
    if trace && semantic.is_some() {
        eprintln!("to-oop: rust-analyzer workspace loaded");
    }
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut combined = RefactorPlan::empty();
    let mut insertions: BTreeMap<(Utf8PathBuf, TextSize, String, bool), Vec<String>> =
        BTreeMap::new();
    for selection in selections {
        let (target_file, function, target) = resolve_selection(&project, &files, &selection)?;
        if trace {
            eprintln!("to-oop: planning {}", target["name"]);
        }
        let key = (
            target_file.path.clone(),
            u32::from(function.syntax().text_range().start()),
        );
        if !seen.insert(key) {
            return refuse(
                "DUPLICATE_TARGET",
                "the same function was selected more than once",
                Some(target_file.path.clone()),
                Some(function.syntax().text_range()),
            );
        }
        let planned = plan(
            &files,
            &calls,
            &imports,
            target_file,
            &function,
            semantic.as_ref(),
            &function_name_counts,
            target,
            command.all_features,
        )?;
        if !names.insert(planned.function_name.clone()) {
            return refuse(
                "DUPLICATE_METHOD_NAME",
                "batch targets must have distinct function names",
                Some(planned.definition_file.clone()),
                Some(function.syntax().text_range()),
            );
        }
        combined.edits.extend(
            planned
                .plan
                .edits
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != 1)
                .map(|(_, edit)| edit.clone()),
        );
        insertions
            .entry((
                planned.definition_file.clone(),
                planned.insertion_range.start(),
                planned.type_name.clone(),
                planned.new_impl,
            ))
            .or_default()
            .push(planned.insertion_body.clone());
        targets.push(planned);
    }
    for ((file, offset, type_name, new_impl), bodies) in insertions {
        let joined = bodies.join("\n");
        let replacement = if new_impl {
            format!("\n\nimpl {type_name} {{\n{joined}\n}}\n")
        } else {
            format!("\n{joined}\n")
        };
        combined.edits.push(TextEdit {
            file,
            range: TextRange::empty(offset),
            replacement,
        });
    }
    if let Err(error) = validate_edits(&project, &combined) {
        return refuse(
            "CONFLICTING_EDITS",
            format!("batch edits conflict: {error:#}"),
            None,
            None,
        );
    }
    let planned = BatchPlanned {
        plan: combined,
        targets,
    };
    if trace {
        eprintln!("to-oop: {} targets planned", planned.targets.len());
    }
    validate_batch_on_copy(
        &project,
        &planned,
        command.all_features,
        command.target.as_deref(),
        command.fast,
    )?;
    if trace {
        eprintln!("to-oop: edited-source validation passed");
    }

    if command.write {
        let snapshot = snapshot(&planned.plan)?;
        let applied = (|| -> Result<()> {
            apply_plan(&planned.plan)?;
            check_plan_files_parse(&planned.plan.edits)?;
            if !command.fast {
                let post_semantic = SemanticProject::load_with(
                    &project,
                    command.all_features,
                    command.target.as_deref(),
                )?;
                for target in &planned.targets {
                    validate_target_post(&post_semantic, target, &planned.plan.edits)?;
                }
            }
            let edited_files = planned
                .plan
                .edits
                .iter()
                .map(|edit| edit.file.clone())
                .collect();
            verify::run_rustfmt_files(&project.manifest_path, &edited_files)?;
            let options = verify::CargoVerification {
                manifest_path: Some(&project.manifest_path),
                all_features: command.all_features,
                target: command.target.as_deref(),
            };
            if !command.fast || command.check {
                verify::run_cargo_check(options)?;
            }
            Ok(())
        })();
        if let Err(error) = applied {
            restore(snapshot)?;
            return Err(FlowError::Failed(
                error.context("to-oop failed; original files restored"),
            ));
        }
        Ok(("applied".to_owned(), Some(planned)))
    } else {
        Ok(("planned".to_owned(), Some(planned)))
    }
}

fn parse_selections(command: &ToOopCommand) -> std::result::Result<Vec<Selection>, FlowError> {
    let mut result = Vec::new();
    if let Some(file) = &command.file {
        let Some(column) = command.column else {
            return refuse(
                "INVALID_POSITION",
                "--column is required with --file",
                Some(file.clone()),
                None,
            );
        };
        for &line in &command.line {
            result.push(Selection {
                file: file.clone(),
                line,
                column,
            });
        }
    }
    for raw in &command.selection {
        let mut fields = raw.rsplitn(3, ':');
        let column = fields.next().and_then(|s| s.parse::<usize>().ok());
        let line = fields.next().and_then(|s| s.parse::<usize>().ok());
        let file = fields.next().filter(|s| !s.is_empty());
        match (file, line, column) {
            (Some(file), Some(line), Some(column)) => result.push(Selection {
                file: Utf8PathBuf::from(file),
                line,
                column,
            }),
            _ => {
                return refuse(
                    "INVALID_SELECTION",
                    format!("expected FILE:LINE:COLUMN, found `{raw}`"),
                    None,
                    None,
                )
            }
        }
    }
    if result.is_empty() {
        return refuse(
            "NO_TARGETS",
            "provide --file with one or more --line values, or repeat --selection",
            None,
            None,
        );
    }
    if result.iter().any(|s| s.line == 0 || s.column == 0) {
        return refuse(
            "INVALID_POSITION",
            "line and column are one-based",
            None,
            None,
        );
    }
    Ok(result)
}

fn resolve_selection<'a>(
    project: &Project,
    files: &'a [analysis::ParsedFile],
    selection: &Selection,
) -> std::result::Result<(&'a analysis::ParsedFile, ast::Fn, Value), FlowError> {
    let file = if selection.file.is_absolute() {
        selection.file.clone()
    } else {
        project.root.join(&selection.file)
    };
    let canonical = match fs::canonicalize(&file) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return refuse(
                "TARGET_NOT_FOUND",
                "target file does not exist",
                Some(file),
                None,
            )
        }
        Err(error) => return Err(FlowError::Failed(error.into())),
    };
    let file =
        Utf8PathBuf::from_path_buf(canonical).map_err(|_| anyhow!("non-UTF-8 target file"))?;
    if !project.rust_files.contains(&file) {
        return refuse(
            "OUTSIDE_WORKSPACE",
            "target file is not a workspace Rust source file",
            Some(file),
            None,
        );
    }
    let target_file = files
        .iter()
        .find(|item| item.path == file)
        .ok_or_else(|| anyhow!("target file not parsed"))?;
    let offset = line_col_offset(&target_file.source, selection.line, selection.column)
        .ok_or_else(|| {
            FlowError::Refused(vec![Refusal::new(
                "INVALID_POSITION",
                "position lies outside the file",
                Some(file.clone()),
                None,
            )])
        })?;
    let functions = target_file
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::Fn::cast)
        .filter(|f| {
            f.name().is_some_and(|name| {
                name.syntax()
                    .text_range()
                    .contains(TextSize::from(offset as u32))
            })
        })
        .collect::<Vec<_>>();
    if functions.len() != 1 {
        return refuse(
            "TARGET_NOT_FOUND",
            "position must point at exactly one function name",
            Some(file),
            None,
        );
    }
    let function = functions.into_iter().next().unwrap();
    let name = function
        .name()
        .ok_or_else(|| anyhow!("selected function has no name"))?
        .text()
        .to_string();
    let target =
        json!({"file":file, "name":name, "range":range_json(function.syntax().text_range())});
    Ok((target_file, function, target))
}

fn plan(
    files: &[analysis::ParsedFile],
    calls: &[analysis::CallSite],
    imports: &[analysis::ImportSite],
    target_file: &analysis::ParsedFile,
    function: &ast::Fn,
    semantic: Option<&SemanticProject>,
    function_name_counts: &BTreeMap<String, usize>,
    target: Value,
    all_features: bool,
) -> std::result::Result<Planned, FlowError> {
    let file = &target_file.path;
    let range = function.syntax().text_range();
    let name = function
        .name()
        .ok_or_else(|| anyhow!("function has no name"))?
        .text()
        .to_string();
    let name_range = function.name().unwrap().syntax().text_range();
    let parent = function
        .syntax()
        .parent()
        .ok_or_else(|| anyhow!("function has no parent"))?;
    if function.generic_param_list().is_some()
        || function.async_token().is_some()
        || function.const_token().is_some()
        || !safe_attributes(function)
        || function.syntax().ancestors().skip(1).any(|n| {
            ast::Impl::cast(n.clone()).is_some()
                || ast::Trait::cast(n.clone()).is_some()
                || ast::Fn::cast(n).is_some()
        })
    {
        return refuse("UNSUPPORTED_FUNCTION", "target must be a non-generic free function without unsupported modifiers or attributes", Some(file.clone()), Some(range));
    }
    let params = function
        .param_list()
        .ok_or_else(|| anyhow!("function has no parameter list"))?;
    if params.self_param().is_some() {
        return refuse(
            "ALREADY_METHOD",
            "target already has a receiver",
            Some(file.clone()),
            Some(range),
        );
    }
    let Some(first) = params.params().next() else {
        return refuse(
            "NO_RECEIVER",
            "target has no first parameter",
            Some(file.clone()),
            Some(range),
        );
    };
    let Some(ast::Pat::IdentPat(binding)) = first.pat() else {
        return refuse(
            "UNSUPPORTED_PARAMETER",
            "first parameter must be a simple name",
            Some(file.clone()),
            Some(first.syntax().text_range()),
        );
    };
    if !binding.is_simple_ident() {
        return refuse(
            "UNSUPPORTED_PARAMETER",
            "first parameter must be a simple name",
            Some(file.clone()),
            Some(first.syntax().text_range()),
        );
    }
    let binding_name = binding
        .name()
        .ok_or_else(|| anyhow!("parameter has no name"))?
        .text()
        .to_string();
    let type_node = first.ty().ok_or_else(|| anyhow!("parameter has no type"))?;
    let type_text = type_node.syntax().text().to_string();
    let (receiver, type_name) = parse_receiver(&type_text).ok_or_else(|| {
        FlowError::Refused(vec![Refusal::at(
            "UNSUPPORTED_RECEIVER_TYPE",
            "first parameter must have type T, &T, or &mut T for a local struct",
            file,
            type_node.syntax().text_range(),
        )])
    })?;
    let type_ref_range =
        last_ident_range(&type_node).ok_or_else(|| anyhow!("type has no name reference"))?;
    let type_def = if let Some(semantic) = semantic {
        semantic.definition_at(file, type_ref_range)?
    } else {
        target_file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::Struct::cast)
            .find(|item| {
                item.name()
                    .is_some_and(|n| n.text().to_string() == type_name)
                    && item.syntax().parent() == Some(parent.clone())
            })
            .and_then(|item| item.name())
            .map(|name| SemanticDefinition {
                file: file.clone(),
                name_range: name.syntax().text_range(),
            })
    };
    let Some(type_def) = type_def else {
        return refuse(
            "TYPE_UNRESOLVED",
            "receiver type could not be resolved",
            Some(file.clone()),
            Some(type_ref_range),
        );
    };
    if std::env::var_os("RUST_REFACTOR_TRACE").is_some() {
        eprintln!("to-oop: receiver type resolved for {name}");
    }
    if type_def.file != *file {
        return refuse(
            "TYPE_NOT_LOCAL",
            "receiver struct must be in the same file and module",
            Some(file.clone()),
            Some(type_ref_range),
        );
    }
    let structures = target_file
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::Struct::cast)
        .filter(|s| {
            s.name()
                .is_some_and(|n| n.syntax().text_range() == type_def.name_range)
                && s.syntax().parent() == Some(parent.clone())
        })
        .collect::<Vec<_>>();
    if structures.len() != 1 {
        return refuse(
            "TYPE_NOT_LOCAL",
            "receiver type must resolve to a struct in the same module",
            Some(file.clone()),
            Some(type_ref_range),
        );
    }
    let structure = &structures[0];
    if structure.name().unwrap().text().to_string() != type_name {
        return refuse(
            "UNSUPPORTED_RECEIVER_TYPE",
            "qualified or aliased receiver types are not supported",
            Some(file.clone()),
            Some(type_ref_range),
        );
    }
    let body = function.body().ok_or_else(|| {
        FlowError::Refused(vec![Refusal::at(
            "NO_BODY",
            "target has no body",
            file,
            range,
        )])
    })?;
    let has_nested_item = body.syntax().descendants().any(|n| {
        matches!(
            n.kind(),
            SyntaxKind::FN | SyntaxKind::IMPL | SyntaxKind::ITEM_LIST
        )
    });
    let unsupported_macro = body
        .syntax()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::MACRO_CALL)
        .any(|call| {
            let text = call.text().to_string();
            let text = text.trim_start();
            !(text.starts_with("format!")
                || text.starts_with("vec!")
                || text.starts_with("cfg!")
                || text.starts_with("eprintln!"))
                || text.contains(&format!("{{{binding_name}"))
        });
    if has_nested_item || unsupported_macro {
        return refuse(
            "UNSUPPORTED_BODY",
            "body contains an unsupported macro or nested item",
            Some(file.clone()),
            Some(body.syntax().text_range()),
        );
    }
    if body
        .syntax()
        .descendants()
        .filter_map(ast::IdentPat::cast)
        .any(|p| {
            p.name()
                .is_some_and(|n| n.text().to_string() == binding_name)
        })
    {
        return refuse(
            "SHADOWED_RECEIVER",
            "body shadows the receiver parameter",
            Some(file.clone()),
            Some(body.syntax().text_range()),
        );
    }
    if body
        .syntax()
        .descendants_with_tokens()
        .filter_map(|n| n.into_token())
        .any(|t| t.text() == "Self" || t.text() == "self")
    {
        return refuse(
            "UNSUPPORTED_BODY",
            "body already uses Self or self",
            Some(file.clone()),
            Some(body.syntax().text_range()),
        );
    }
    for source_file in files {
        if !source_file.source.contains(&name) {
            continue;
        }
        for imp in source_file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::Impl::cast)
        {
            let same_method_name = imp
                .syntax()
                .descendants()
                .filter_map(ast::Fn::cast)
                .any(|f| f.name().is_some_and(|n| n.text().to_string() == name));
            if !same_method_name {
                continue;
            }
            let impl_type = imp.self_ty().and_then(|ty| last_ident_range(&ty));
            let impl_conflict = if let Some(semantic) = semantic {
                let impl_definition = match impl_type {
                    Some(range) => semantic.definition_at(&source_file.path, range)?,
                    None => None,
                };
                impl_definition.is_none() || impl_definition.as_ref() == Some(&type_def)
            } else {
                imp.self_ty()
                    .is_some_and(|ty| ty.syntax().text().to_string() == type_name)
            };
            if impl_conflict {
                return refuse(
                    "METHOD_CONFLICT",
                    "a method with this name may already exist for the receiver type",
                    Some(source_file.path.clone()),
                    Some(imp.syntax().text_range()),
                );
            }
        }
    }
    if std::env::var_os("RUST_REFACTOR_TRACE").is_some() {
        eprintln!("to-oop: method declarations checked for {name}");
    }
    let mut body_edits = vec![(first.syntax().text_range(), receiver.to_owned())];
    for path in body.syntax().descendants().filter_map(ast::PathExpr::cast) {
        if path
            .path()
            .is_some_and(|p| p.syntax().text().to_string() == binding_name)
        {
            body_edits.push((path.syntax().text_range(), "self".to_owned()));
        }
    }
    for token in body
        .syntax()
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
    {
        if token.kind() != SyntaxKind::IDENT || token.text() != binding_name {
            continue;
        }
        let inside_macro = token.parent().is_some_and(|parent| {
            parent
                .ancestors()
                .any(|node| node.kind() == SyntaxKind::TOKEN_TREE)
        });
        if inside_macro
            && !body_edits
                .iter()
                .any(|(range, _)| range.contains_range(token.text_range()))
        {
            body_edits.push((token.text_range(), "self".to_owned()));
        }
    }
    let moved = replace_subranges(
        analysis::source_slice(&target_file.source, range),
        range,
        &body_edits,
    )?;
    let existing_impl = target_file
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::Impl::cast)
        .find(|item| {
            item.syntax().parent() == Some(parent.clone())
                && item
                    .self_ty()
                    .is_some_and(|ty| ty.syntax().text().to_string() == type_name)
                && !item
                    .syntax()
                    .children_with_tokens()
                    .filter_map(|element| element.into_token())
                    .any(|token| token.kind() == SyntaxKind::FOR_KW)
                && item
                    .syntax()
                    .last_token()
                    .is_some_and(|token| token.kind() == SyntaxKind::R_CURLY)
        });
    let (insertion_range, new_impl) = if let Some(item) = existing_impl {
        (
            TextRange::empty(item.syntax().last_token().unwrap().text_range().start()),
            false,
        )
    } else {
        (
            TextRange::empty(structure.syntax().text_range().end()),
            true,
        )
    };
    let insertion = if new_impl {
        format!("\n\nimpl {type_name} {{\n{moved}\n}}\n")
    } else {
        format!("\n{moved}\n")
    };
    let mut edits = vec![
        TextEdit {
            file: file.clone(),
            range,
            replacement: String::new(),
        },
        TextEdit {
            file: file.clone(),
            range: insertion_range,
            replacement: insertion,
        },
    ];
    if std::env::var_os("RUST_REFACTOR_TRACE").is_some() {
        eprintln!("to-oop: syntax checks passed for {name}");
    }
    let refs = if let Some(semantic) = semantic {
        semantic.references_to(file, name_range).map_err(|error| {
            FlowError::Refused(vec![Refusal::at(
                "INCOMPLETE_REFERENCES",
                format!("cannot enumerate all references: {error:#}"),
                file,
                name_range,
            )])
        })?
    } else {
        let matching_defs = function_name_counts.get(&name).copied().unwrap_or(0);
        if matching_defs != 1 {
            return refuse(
                "AMBIGUOUS_FUNCTION",
                "fast mode needs a function name unique in the workspace",
                Some(file.clone()),
                Some(name_range),
            );
        }
        let mut refs = calls
            .iter()
            .filter(|call| call.callee == name)
            .map(|call| SemanticReference {
                file: call.file.clone(),
                range: call.callee_range,
            })
            .collect::<Vec<_>>();
        refs.extend(
            imports
                .iter()
                .filter(|import| import.function_name == name)
                .map(|import| SemanticReference {
                    file: import.file.clone(),
                    range: import.name_range,
                }),
        );
        // A unique function name still may be used as a value (for example,
        // as a callback). Those references cannot be rewritten as calls.
        // Include them so the ordinary reference classifier refuses them.
        let call_keys = refs
            .iter()
            .map(|reference| (reference.file.clone(), reference.range.start()))
            .collect::<BTreeSet<_>>();
        for source_file in files {
            if !source_file.source.contains(&name) {
                continue;
            }
            for path in source_file
                .tree
                .syntax()
                .descendants()
                .filter_map(ast::PathExpr::cast)
            {
                let Some(path_node) = path.path() else {
                    continue;
                };
                let path_text = path_node.syntax().text().to_string();
                if path_text != name && !path_text.ends_with(&format!("::{name}")) {
                    continue;
                }
                let Some(token) = path_node.syntax().last_token() else {
                    continue;
                };
                let range = token.text_range();
                if !call_keys.contains(&(source_file.path.clone(), range.start())) {
                    refs.push(SemanticReference {
                        file: source_file.path.clone(),
                        range,
                    });
                }
            }
            for range in analysis::collect_macro_identifiers(source_file, &name) {
                if !call_keys.contains(&(source_file.path.clone(), range.start())) {
                    refs.push(SemanticReference {
                        file: source_file.path.clone(),
                        range,
                    });
                }
            }
        }
        refs
    };
    if std::env::var_os("RUST_REFACTOR_TRACE").is_some() {
        let source = if semantic.is_some() {
            "semantic"
        } else {
            "source"
        };
        eprintln!(
            "to-oop: {} {source} references found for {name}",
            refs.len(),
        );
    }
    let resolved_reference_keys = refs
        .iter()
        .map(|reference| (reference.file.clone(), u32::from(reference.range.start())))
        .collect::<BTreeSet<_>>();
    let mut call_records = Vec::new();
    let mut seen_calls = BTreeSet::new();
    let mut seen_imports = BTreeSet::new();
    let mut blockers = Vec::new();
    for reference in refs {
        if let Some(call) = calls
            .iter()
            .find(|c| c.file == reference.file && c.callee_range == reference.range)
        {
            if !all_features
                && files
                    .iter()
                    .find(|f| f.path == call.file)
                    .is_some_and(|f| call_in_cfg(f, call.range))
            {
                blockers.push(Refusal::at(
                    "UNRESOLVED_CANDIDATE",
                    "call is conditionally compiled and cannot be checked in every configuration",
                    &call.file,
                    call.range,
                ));
                continue;
            }
            if let Some(semantic) = semantic {
                let definition = semantic.definition_at(&call.file, call.callee_range)?;
                if definition
                    .as_ref()
                    .is_none_or(|d| d.file != *file || d.name_range != name_range)
                {
                    blockers.push(Refusal::at(
                        "UNRESOLVED_CANDIDATE",
                        "call cannot be resolved to the target under the active configuration",
                        &call.file,
                        call.range,
                    ));
                    continue;
                }
            }
            if call.file == *file && range.contains_range(call.range) {
                blockers.push(Refusal::at(
                    "RECURSIVE_CALL",
                    "target calls itself inside its body",
                    &call.file,
                    call.range,
                ));
                continue;
            }
            if call.args.is_empty() {
                blockers.push(Refusal::at(
                    "CALL_HAS_NO_RECEIVER",
                    "call has no receiver argument",
                    &call.file,
                    call.range,
                ));
                continue;
            }
            if !seen_calls.insert((call.file.clone(), u32::from(call.range.start()))) {
                continue;
            }
            let receiver_expr = &call.args[0];
            let remaining = call.args[1..].join(", ");
            let receiver_expr = method_receiver(receiver_expr);
            let replacement = format!("{receiver_expr}.{name}({remaining})");
            let method_offset = replacement.rfind(&format!(".{name}(")).unwrap() + 1;
            edits.push(TextEdit {
                file: call.file.clone(),
                range: call.range,
                replacement,
            });
            call_records.push((call.file.clone(), call.range, method_offset));
        } else if let Some(import) = imports
            .iter()
            .find(|i| i.file == reference.file && i.name_range == reference.range)
        {
            if seen_imports.insert((import.file.clone(), u32::from(import.use_range.start()))) {
                edits.push(TextEdit {
                    file: import.file.clone(),
                    range: import.use_range,
                    replacement: String::new(),
                });
            }
        } else {
            blockers.push(Refusal::at(
                "UNHANDLED_REFERENCE",
                "resolved reference is not a supported direct call or private import",
                &reference.file,
                reference.range,
            ));
        }
    }
    // Inactive cfg branches do not appear in rust-analyzer reference results.
    // Refuse a same-spelled call whose meaning cannot be established.
    if let Some(semantic) = semantic {
        for call in calls.iter().filter(|c| {
            c.callee == name && !seen_calls.contains(&(c.file.clone(), u32::from(c.range.start())))
        }) {
            let definition = semantic.definition_at(&call.file, call.callee_range)?;
            if definition.is_none()
                || definition
                    .as_ref()
                    .is_some_and(|d| d.file == *file && d.name_range == name_range)
            {
                blockers.push(Refusal::at(
                    "UNRESOLVED_CANDIDATE",
                    "same-name call cannot be resolved under the active configuration",
                    &call.file,
                    call.range,
                ));
            }
        }
    }
    for source_file in files {
        if !source_file.source.contains(&name) {
            continue;
        }
        for use_item in source_file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::Use::cast)
        {
            for name_ref in use_item
                .syntax()
                .descendants()
                .filter_map(ast::NameRef::cast)
            {
                if name_ref.text().to_string() != name
                    || resolved_reference_keys.contains(&(
                        source_file.path.clone(),
                        u32::from(name_ref.syntax().text_range().start()),
                    ))
                {
                    continue;
                }
                if let Some(semantic) = semantic {
                    let definition = semantic
                        .definition_at(&source_file.path, name_ref.syntax().text_range())?;
                    if definition.is_none()
                        || definition
                            .as_ref()
                            .is_some_and(|d| d.file == *file && d.name_range == name_range)
                    {
                        blockers.push(Refusal::at(
                            "UNRESOLVED_IMPORT",
                            "same-name import cannot be resolved under the active configuration",
                            &source_file.path,
                            name_ref.syntax().text_range(),
                        ));
                    }
                }
            }
        }
    }
    if !blockers.is_empty() {
        return Err(FlowError::Refused(blockers));
    }
    if std::env::var_os("RUST_REFACTOR_TRACE").is_some() {
        eprintln!("to-oop: references classified for {name}");
    }
    let plan = RefactorPlan {
        edits,
        diagnostics: Vec::new(),
    };
    Ok(Planned {
        plan,
        insertion_range,
        insertion_body: moved,
        new_impl,
        target,
        calls: call_records,
        function_name: name,
        type_name,
        definition_file: file.clone(),
    })
}

fn call_in_cfg(file: &analysis::ParsedFile, range: TextRange) -> bool {
    let Some(call) = file
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::CallExpr::cast)
        .find(|call| call.syntax().text_range() == range)
    else {
        return false;
    };
    call.syntax().ancestors().any(|node| {
        node.children().filter_map(ast::Attr::cast).any(|attr| {
            let text = attr.syntax().text().to_string();
            text.starts_with("#[cfg(") || text.starts_with("#[cfg_attr(")
        })
    })
}

fn safe_attributes(function: &ast::Fn) -> bool {
    function.attrs().all(|attr| {
        let text = attr.syntax().text().to_string();
        text.starts_with("///")
            || text.starts_with("//!")
            || text.starts_with("#[doc")
            || matches!(text.as_str(), "#[inline]" | "#[cold]" | "#[must_use]")
    })
}

fn parse_receiver(input: &str) -> Option<(&'static str, String)> {
    let text = input.trim();
    let (receiver, name) = if let Some(rest) = text.strip_prefix("&mut ") {
        ("&mut self", rest)
    } else if let Some(rest) = text.strip_prefix('&') {
        ("&self", rest.trim())
    } else {
        ("self", text)
    };
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || name.chars().next()?.is_ascii_digit()
    {
        return None;
    }
    Some((receiver, name.to_owned()))
}

fn method_receiver(argument: &str) -> String {
    let expression = argument.trim();
    let name = expression
        .strip_prefix("&mut ")
        .or_else(|| expression.strip_prefix('&'))
        .unwrap_or(expression)
        .trim();
    if !name.is_empty()
        && !name.chars().next().unwrap().is_ascii_digit()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        name.to_owned()
    } else {
        format!("({expression})")
    }
}

fn last_ident_range(ty: &ast::Type) -> Option<TextRange> {
    ty.syntax()
        .descendants()
        .filter_map(ast::NameRef::cast)
        .last()
        .map(|n| n.syntax().text_range())
}

fn replace_subranges(
    source: &str,
    base: TextRange,
    edits: &[(TextRange, String)],
) -> Result<String> {
    let mut result = source.to_owned();
    let mut sorted = edits.to_vec();
    sorted.sort_by_key(|e| e.0.start());
    for pair in sorted.windows(2) {
        if pair[0].0.end() > pair[1].0.start() {
            bail!("overlapping body edits");
        }
    }
    for (range, text) in sorted.into_iter().rev() {
        let start = u32::from(range.start() - base.start()) as usize;
        let end = u32::from(range.end() - base.start()) as usize;
        result.replace_range(start..end, &text);
    }
    Ok(result)
}

fn validate_edits(project: &Project, plan: &RefactorPlan) -> Result<()> {
    let mut by_file: BTreeMap<&Utf8PathBuf, Vec<&TextEdit>> = BTreeMap::new();
    for edit in &plan.edits {
        by_file.entry(&edit.file).or_default().push(edit);
    }
    for (path, mut edits) in by_file {
        if !project.rust_files.contains(path) {
            bail!("edit outside workspace: {path}");
        }
        let source = fs::read_to_string(path)?;
        edits.sort_by_key(|e| e.range.start());
        for edit in &edits {
            let start = u32::from(edit.range.start()) as usize;
            let end = u32::from(edit.range.end()) as usize;
            if end > source.len()
                || !source.is_char_boundary(start)
                || !source.is_char_boundary(end)
            {
                bail!("invalid edit range in {path}");
            }
        }
        for pair in edits.windows(2) {
            if pair[0].range.end() > pair[1].range.start() {
                bail!("overlapping edits in {path}");
            }
        }
    }
    Ok(())
}

fn snapshot(plan: &RefactorPlan) -> Result<BTreeMap<Utf8PathBuf, String>> {
    plan.edits
        .iter()
        .map(|edit| &edit.file)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|p| Ok((p.clone(), fs::read_to_string(p)?)))
        .collect()
}
fn restore(contents: BTreeMap<Utf8PathBuf, String>) -> Result<()> {
    for (path, text) in contents {
        fs::write(path, text)?;
    }
    Ok(())
}

fn validate_batch_on_copy(
    project: &Project,
    planned: &BatchPlanned,
    all_features: bool,
    target: Option<&str>,
    fast: bool,
) -> std::result::Result<(), FlowError> {
    if fast {
        let mut by_file: BTreeMap<&Utf8PathBuf, Vec<&TextEdit>> = BTreeMap::new();
        for edit in &planned.plan.edits {
            by_file.entry(&edit.file).or_default().push(edit);
        }
        for (path, edits) in by_file {
            let source = fs::read_to_string(path).map_err(anyhow::Error::from)?;
            let updated = crate::edits::apply_edits_to_string(&source, &edits)?;
            let parsed = SourceFile::parse(&updated, Edition::CURRENT);
            if !parsed.errors().is_empty() {
                return Err(anyhow!("rewritten file does not parse: {path}").into());
            }
        }
        return Ok(());
    }
    let temp = tempfile::tempdir().map_err(anyhow::Error::from)?;
    copy_workspace(&project.root, temp.path())?;
    if std::env::var_os("RUST_REFACTOR_TRACE").is_some() {
        eprintln!("to-oop: temporary workspace copied");
    }
    let copy_root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf())
        .map_err(|_| anyhow!("temporary path is not UTF-8"))?;
    let copy_project = Project::load(Some(&copy_root.join("Cargo.toml")))?;
    let copy_plan = RefactorPlan {
        edits: planned
            .plan
            .edits
            .iter()
            .map(|e| TextEdit {
                file: remap(&e.file, &project.root, &copy_root).unwrap(),
                range: e.range,
                replacement: e.replacement.clone(),
            })
            .collect(),
        diagnostics: Vec::new(),
    };
    apply_plan(&copy_plan)?;
    check_plan_files_parse(&copy_plan.edits)?;
    let post_semantic = SemanticProject::load_with(&copy_project, all_features, target)?;
    if std::env::var_os("RUST_REFACTOR_TRACE").is_some() {
        eprintln!("to-oop: temporary rust-analyzer workspace loaded");
    }
    for original in &planned.targets {
        let copied = Planned {
            plan: RefactorPlan::empty(),
            insertion_range: original.insertion_range,
            insertion_body: original.insertion_body.clone(),
            new_impl: original.new_impl,
            target: original.target.clone(),
            calls: original
                .calls
                .iter()
                .map(|(p, r, o)| (remap(p, &project.root, &copy_root).unwrap(), *r, *o))
                .collect(),
            function_name: original.function_name.clone(),
            type_name: original.type_name.clone(),
            definition_file: remap(&original.definition_file, &project.root, &copy_root)?,
        };
        validate_target_post(&post_semantic, &copied, &copy_plan.edits).map_err(|e| {
            let (location_file, location_range) = original
                .calls
                .first()
                .map(|(file, range, _)| (file.clone(), Some(*range)))
                .unwrap_or_else(|| (original.definition_file.clone(), None));
            FlowError::Refused(vec![Refusal::new(
                "POST_EDIT_SEMANTICS",
                format!(
                    "rewritten code could not be validated for `{}`: {e:#}",
                    original.function_name
                ),
                Some(location_file),
                location_range,
            )])
        })?;
    }
    Ok(())
}

fn copy_workspace(from: &Utf8Path, to: &std::path::Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(from).into_iter().filter_entry(|e| {
        e.depth() == 0
            || !matches!(
                e.file_name().to_str(),
                Some("target" | ".git" | ".tmp" | ".claude" | ".agents" | ".codex")
            )
    }) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(from)?;
        let destination = to.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&destination)?;
        } else if entry.file_type().is_file() {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

fn remap(path: &Utf8Path, old_root: &Utf8Path, new_root: &Utf8Path) -> Result<Utf8PathBuf> {
    Ok(new_root.join(path.strip_prefix(old_root)?))
}

fn check_plan_files_parse(all_edits: &[TextEdit]) -> Result<()> {
    for path in all_edits
        .iter()
        .map(|edit| &edit.file)
        .collect::<BTreeSet<_>>()
    {
        let source = fs::read_to_string(path)?;
        let parsed = SourceFile::parse(&source, Edition::CURRENT);
        if !parsed.errors().is_empty() {
            bail!("rewritten file does not parse: {path}");
        }
    }
    Ok(())
}

fn validate_target_post(
    semantic: &SemanticProject,
    planned: &Planned,
    all_edits: &[TextEdit],
) -> Result<()> {
    let source = fs::read_to_string(&planned.definition_file)?;
    let parsed = SourceFile::parse(&source, Edition::CURRENT);
    if !parsed.errors().is_empty() {
        bail!("rewritten definition file does not parse");
    }
    let method = parsed
        .tree()
        .syntax()
        .descendants()
        .filter_map(ast::Fn::cast)
        .find(|f| {
            f.name()
                .is_some_and(|n| n.text().to_string() == planned.function_name)
                && f.syntax().ancestors().any(|n| ast::Impl::cast(n).is_some())
        })
        .ok_or_else(|| anyhow!("new method was not found"))?;
    let method_name_range = method.name().unwrap().syntax().text_range();
    let expected = SemanticDefinition {
        file: planned.definition_file.clone(),
        name_range: method_name_range,
    };
    let mut expected_references = BTreeSet::new();
    for (path, old_range, method_offset) in &planned.calls {
        let new_start = shifted_start(path, old_range.start(), all_edits)?;
        let start = new_start + TextSize::from(*method_offset as u32);
        let range = TextRange::at(start, TextSize::from(planned.function_name.len() as u32));
        let definition = semantic.definition_at(path, range)?;
        if definition.as_ref() != Some(&expected) {
            bail!("rewritten call at {path}:{range:?} does not resolve to new method");
        }
        expected_references.insert((
            path.clone(),
            u32::from(range.start()),
            u32::from(range.end()),
        ));
    }
    let actual_references = semantic
        .references_to(&planned.definition_file, method_name_range)?
        .into_iter()
        .map(|reference| {
            (
                reference.file,
                u32::from(reference.range.start()),
                u32::from(reference.range.end()),
            )
        })
        .collect::<BTreeSet<_>>();
    if actual_references != expected_references {
        bail!("post-edit method references differ from the planned call sites");
    }
    Ok(())
}

fn shifted_start(path: &Utf8Path, start: TextSize, edits: &[TextEdit]) -> Result<TextSize> {
    let mut offset = i64::from(u32::from(start));
    for edit in edits
        .iter()
        .filter(|e| e.file == path && e.range.end() <= start && e.range.start() != start)
    {
        offset += edit.replacement.len() as i64 - i64::from(u32::from(edit.range.len()));
    }
    if offset < 0 {
        bail!("invalid shifted offset");
    }
    Ok(TextSize::from(offset as u32))
}

fn line_col_offset(source: &str, line: usize, column: usize) -> Option<usize> {
    let mut start = 0;
    for _ in 1..line {
        start = source[start..].find('\n').map(|n| start + n + 1)?;
    }
    let slice = source.get(start..)?.split('\n').next()?;
    if column > slice.chars().count() + 1 {
        return None;
    }
    Some(
        start
            + slice
                .char_indices()
                .nth(column - 1)
                .map(|(i, _)| i)
                .unwrap_or(slice.len()),
    )
}

fn range_json(range: TextRange) -> Value {
    json!({"start":u32::from(range.start()), "end":u32::from(range.end())})
}

fn emit(
    command: &ToOopCommand,
    status: &str,
    planned: Option<&BatchPlanned>,
    refusals: &[Refusal],
) -> Result<()> {
    let targets = planned
        .map(|p| {
            p.targets
                .iter()
                .map(|target| target.target.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let target = if targets.len() == 1 {
        targets[0].clone()
    } else {
        Value::Null
    };
    let edits: Vec<Value> = planned.map(|p| p.plan.edits.iter().map(|e| json!({"file":e.file,"range":range_json(e.range),"replacement":e.replacement})).collect()).unwrap_or_default();
    let diagnostics: Vec<Value> = refusals.iter().map(Refusal::json).collect();
    let value = json!({"status":status,"target":target,"targets":targets,"edits":edits,"diagnostics":diagnostics});
    match command.format {
        OutputFormat::Json => println!("{value}"),
        OutputFormat::Text => {
            println!("to-oop: {status}");
            if let Some(p) = planned {
                println!(
                    "{} edits for {} functions",
                    p.plan.edits.len(),
                    p.targets.len()
                );
            }
            for refusal in refusals {
                println!("{}: {}", refusal.code, refusal.message);
            }
        }
    }
    Ok(())
}
