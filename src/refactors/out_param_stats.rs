//! Find `&mut` parameters that are used only as assignment destinations.
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use ra_ap_syntax::ast::{self, AstNode, BinaryOp, HasName, HasVisibility, UnaryOp};
use ra_ap_syntax::{Edition, SourceFile};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use crate::{
    analysis::ParsedFile,
    cli::{OutParamStatsCommand, ReturnStatsFormat},
    project::Project,
};

#[derive(Clone, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct WriteEvidence {
    line: usize,
    column: usize,
    target: String,
    kind: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct OutputParameter {
    name: String,
    r#type: String,
    write_count: usize,
    write_forms: Vec<&'static str>,
    review_note: String,
    writes: Vec<WriteEvidence>,
}

#[derive(Clone, Debug, Serialize)]
struct FunctionReport {
    function: String,
    file: Utf8PathBuf,
    line: usize,
    column: usize,
    kind: &'static str,
    visibility: String,
    output_parameters: Vec<OutputParameter>,
}

#[derive(Clone)]
struct Parameter {
    name: String,
    r#type: String,
}

#[derive(Default)]
struct UseSummary {
    writes: BTreeSet<WriteEvidence>,
    read_or_uncertain: bool,
    shadowed: bool,
}

pub fn run(command: OutParamStatsCommand) -> Result<()> {
    let project = Project::load(command.manifest_path.as_deref())?;
    let reports = scan_project(&project)?;
    match command.format {
        ReturnStatsFormat::Text => emit_text(&project, &reports),
        ReturnStatsFormat::Json => emit_json(&project, &reports)?,
        ReturnStatsFormat::Csv => emit_csv(&project, &reports),
    }
    Ok(())
}

fn scan_project(project: &Project) -> Result<Vec<FunctionReport>> {
    if project.rust_files.is_empty() {
        return Ok(Vec::new());
    }
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(16)
        .min(project.rust_files.len());
    let chunk_size = project.rust_files.len().div_ceil(worker_count);
    let mut reports = std::thread::scope(|scope| {
        let handles = project
            .rust_files
            .chunks(chunk_size)
            .map(|paths| {
                scope.spawn(move || {
                    let mut reports = Vec::new();
                    for path in paths {
                        let source = fs::read_to_string(path)
                            .with_context(|| format!("failed to read Rust source file {path}"))?;
                        let tree = SourceFile::parse(&source, Edition::CURRENT).tree();
                        let file = ParsedFile {
                            path: path.clone(),
                            source,
                            tree,
                        };
                        reports.extend(scan_file(&file));
                    }
                    Ok::<_, anyhow::Error>(reports)
                })
            })
            .collect::<Vec<_>>();
        let mut reports = Vec::new();
        for handle in handles {
            reports.extend(
                handle.join().map_err(|_| {
                    anyhow::anyhow!("an output-parameter analysis worker panicked")
                })??,
            );
        }
        Ok::<_, anyhow::Error>(reports)
    })?;
    reports.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));
    Ok(reports)
}

fn scan_file(file: &ParsedFile) -> Vec<FunctionReport> {
    file.tree
        .syntax()
        .descendants()
        .filter_map(ast::Fn::cast)
        .filter_map(|function| analyze_function(file, function))
        .collect()
}

fn analyze_function(file: &ParsedFile, function: ast::Fn) -> Option<FunctionReport> {
    let (name, body) = (function.name()?, function.body()?);
    let parameters = function
        .param_list()?
        .params()
        .filter_map(|parameter| {
            let ast::Pat::IdentPat(pattern) = parameter.pat()? else {
                return None;
            };
            if !pattern.is_simple_ident() {
                return None;
            }
            let ty = parameter.ty()?;
            let ast::Type::RefType(reference) = &ty else {
                return None;
            };
            reference.mut_token()?;
            Some(Parameter {
                name: pattern.name()?.text().to_string(),
                r#type: normalize(&ty.syntax().text().to_string()),
            })
        })
        .collect::<Vec<_>>();
    if parameters.is_empty() {
        return None;
    }

    let mut uses = parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), UseSummary::default()))
        .collect::<BTreeMap<_, _>>();

    for binding in body
        .syntax()
        .descendants()
        .filter_map(ast::IdentPat::cast)
        .filter(|binding| belongs_to_function(binding.syntax(), &function))
    {
        if let Some(summary) = binding
            .name()
            .and_then(|binding| uses.get_mut(binding.text().as_str()))
        {
            summary.shadowed = true;
        }
    }

    for path in body
        .syntax()
        .descendants()
        .filter_map(ast::PathExpr::cast)
        .filter(|path| belongs_to_function(path.syntax(), &function))
    {
        let raw = normalize(&path.syntax().text().to_string());
        let Some(summary) = uses.get_mut(&raw) else {
            continue;
        };
        if let Some((target, kind)) = definite_assignment_target(&path) {
            let (line, column) = line_column(
                &file.source,
                usize::from(target.syntax().text_range().start()),
            );
            summary.writes.insert(WriteEvidence {
                line,
                column,
                target: truncate(&normalize(&target.syntax().text().to_string()), 120),
                kind,
            });
        } else {
            summary.read_or_uncertain = true;
        }
    }

    // Macro token trees are not expression ASTs. Treat a matching identifier
    // in one as an uncertain use, even if its spelling resembles an assignment.
    for macro_call in body
        .syntax()
        .descendants()
        .filter_map(ast::MacroCall::cast)
        .filter(|call| belongs_to_function(call.syntax(), &function))
    {
        for token in macro_call
            .syntax()
            .descendants_with_tokens()
            .filter_map(|it| it.into_token())
        {
            if let Some(summary) = uses.get_mut(token.text()) {
                summary.read_or_uncertain = true;
            }
        }
    }

    let output_parameters = parameters
        .into_iter()
        .filter_map(|parameter| {
            let summary = uses.remove(&parameter.name)?;
            (!summary.shadowed && !summary.read_or_uncertain && !summary.writes.is_empty()).then(|| {
                let write_forms = summary
                    .writes
                    .iter()
                    .map(|write| write.kind)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let review_note = if write_forms.contains(&"field") {
                    "field writes may preserve allocation or existing state; review before converting"
                        .to_owned()
                } else {
                    String::new()
                };
                OutputParameter {
                    name: parameter.name,
                    r#type: parameter.r#type,
                    write_count: summary.writes.len(),
                    write_forms,
                    review_note,
                    writes: summary.writes.into_iter().collect(),
                }
            })
        })
        .collect::<Vec<_>>();
    if output_parameters.is_empty() {
        return None;
    }

    let (line, column) = line_column(
        &file.source,
        usize::from(name.syntax().text_range().start()),
    );
    let kind = if function
        .syntax()
        .ancestors()
        .skip(1)
        .any(|node| ast::Trait::cast(node).is_some())
    {
        "trait_method"
    } else if function
        .syntax()
        .ancestors()
        .skip(1)
        .any(|node| ast::Impl::cast(node).is_some())
    {
        "method"
    } else {
        "free"
    };
    Some(FunctionReport {
        function: name.text().to_string(),
        file: file.path.clone(),
        line,
        column,
        kind,
        visibility: function
            .visibility()
            .map(|visibility| visibility.syntax().text().to_string())
            .unwrap_or_default(),
        output_parameters,
    })
}

fn belongs_to_function(node: &ra_ap_syntax::SyntaxNode, function: &ast::Fn) -> bool {
    node.ancestors()
        .skip(1)
        .find_map(ast::Fn::cast)
        .is_some_and(|owner| owner.syntax().text_range() == function.syntax().text_range())
}

fn definite_assignment_target(path: &ast::PathExpr) -> Option<(ast::Expr, &'static str)> {
    let assignment = path
        .syntax()
        .ancestors()
        .skip(1)
        .find_map(ast::BinExpr::cast)?;
    if !matches!(
        assignment.op_kind(),
        Some(BinaryOp::Assignment { op: None })
    ) {
        return None;
    }
    let lhs = assignment.lhs()?;
    if !lhs
        .syntax()
        .text_range()
        .contains_range(path.syntax().text_range())
    {
        return None;
    }
    let kind = mutated_place_kind(path.syntax(), &lhs)?;
    Some((lhs, kind))
}

fn mutated_place_kind(path: &ra_ap_syntax::SyntaxNode, lhs: &ast::Expr) -> Option<&'static str> {
    let mut current = path.clone();
    let mut reaches_pointee = false;
    let mut writes_field = false;
    while current != *lhs.syntax() {
        let parent = current.parent()?;
        if let Some(prefix) = ast::PrefixExpr::cast(parent.clone()) {
            if prefix.expr().is_none_or(|expr| expr.syntax() != &current)
                || prefix.op_kind() != Some(UnaryOp::Deref)
            {
                return None;
            }
            reaches_pointee = true;
        } else if let Some(field) = ast::FieldExpr::cast(parent.clone()) {
            if field.expr().is_none_or(|expr| expr.syntax() != &current) {
                return None;
            }
            reaches_pointee = true;
            writes_field = true;
        } else if ast::IndexExpr::cast(parent.clone()).is_some() {
            // Filling caller-owned storage is commonly an allocation-reuse
            // optimization, not a value returned through an output slot.
            return None;
        } else if ast::ParenExpr::cast(parent.clone()).is_some() {
            // Parentheses preserve a place expression.
        } else if ast::TupleExpr::cast(parent.clone()).is_some() && reaches_pointee {
            // Destructuring assignment can contain several independently
            // dereferenced output parameters.
        } else {
            return None;
        }
        current = parent;
    }
    reaches_pointee.then_some(if writes_field { "field" } else { "direct" })
}

fn emit_text(project: &Project, reports: &[FunctionReport]) {
    println!("FUNCTION  OUTPUT_PARAMETERS  LOCATION  REVIEW_NOTE");
    for report in reports {
        let relative = report
            .file
            .strip_prefix(&project.root)
            .unwrap_or(&report.file);
        let parameters = report
            .output_parameters
            .iter()
            .map(|parameter| format!("{}: {}", parameter.name, parameter.r#type))
            .collect::<Vec<_>>()
            .join("; ");
        let notes = report
            .output_parameters
            .iter()
            .filter(|parameter| !parameter.review_note.is_empty())
            .map(|parameter| format!("{}: {}", parameter.name, parameter.review_note))
            .collect::<Vec<_>>()
            .join("; ");
        println!(
            "{:<32} {:<48} {}:{}:{}  {}",
            report.function, parameters, relative, report.line, report.column, notes
        );
    }
}

fn emit_json(project: &Project, reports: &[FunctionReport]) -> Result<()> {
    let rows = reports
        .iter()
        .map(|report| {
            let mut row = serde_json::to_value(report)?;
            row["file"] = serde_json::Value::String(
                report
                    .file
                    .strip_prefix(&project.root)
                    .unwrap_or(&report.file)
                    .to_string(),
            );
            Ok(row)
        })
        .collect::<Result<Vec<_>>>()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "function_count": rows.len(),
            "output_parameter_count": reports.iter().map(|row| row.output_parameters.len()).sum::<usize>(),
            "functions": rows,
        }))?
    );
    Ok(())
}

fn emit_csv(project: &Project, reports: &[FunctionReport]) {
    println!("function,file,line,column,kind,visibility,output_parameters,argument_names,argument_types,write_counts,write_forms,review_notes,write_evidence");
    for report in reports {
        let relative = report
            .file
            .strip_prefix(&project.root)
            .unwrap_or(&report.file);
        let parameter_list = report
            .output_parameters
            .iter()
            .map(|parameter| format!("{}: {}", parameter.name, parameter.r#type))
            .collect::<Vec<_>>()
            .join(";");
        let names = report
            .output_parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let types = report
            .output_parameters
            .iter()
            .map(|parameter| parameter.r#type.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let counts = report
            .output_parameters
            .iter()
            .map(|parameter| format!("{}={}", parameter.name, parameter.write_count))
            .collect::<Vec<_>>()
            .join(";");
        let forms = report
            .output_parameters
            .iter()
            .map(|parameter| format!("{}={}", parameter.name, parameter.write_forms.join("+")))
            .collect::<Vec<_>>()
            .join(";");
        let notes = report
            .output_parameters
            .iter()
            .filter(|parameter| !parameter.review_note.is_empty())
            .map(|parameter| format!("{}={}", parameter.name, parameter.review_note))
            .collect::<Vec<_>>()
            .join(";");
        let evidence = report
            .output_parameters
            .iter()
            .flat_map(|parameter| {
                parameter.writes.iter().map(|write| {
                    format!(
                        "{}@{}:{}:{}",
                        parameter.name, write.line, write.column, write.target
                    )
                })
            })
            .collect::<Vec<_>>()
            .join(";");
        let fields = [
            report.function.clone(),
            relative.to_string(),
            report.line.to_string(),
            report.column.to_string(),
            report.kind.to_owned(),
            report.visibility.clone(),
            parameter_list,
            names,
            types,
            counts,
            forms,
            notes,
            evidence,
        ];
        println!(
            "{}",
            fields
                .iter()
                .map(|field| csv_field(field))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut result = text
        .chars()
        .take(limit.saturating_sub(3))
        .collect::<String>();
    result.push_str("...");
    result
}

fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset];
    (
        before.bytes().filter(|byte| *byte == b'\n').count() + 1,
        before.rsplit('\n').next().unwrap_or("").chars().count() + 1,
    )
}

fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
