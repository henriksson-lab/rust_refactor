use anyhow::{bail, Context, Result};
use ra_ap_syntax::{ast, AstNode, Edition, SyntaxKind, SyntaxToken};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use text_size::TextRange;

use crate::{
    analysis::{self, CallSite, ImportCleanup, InlineAnalysis, InlineFunction, UnhandledReference},
    cli::InlineCommand,
    edits::{apply_plan, Diagnostic, DiagnosticLevel, RefactorPlan, TextEdit},
    project::Project,
    semantic::{SemanticProject, SemanticReference},
    verify,
};

pub fn run(command: InlineCommand) -> Result<()> {
    if !command.dry_run && !command.write {
        bail!("choose either --dry-run or --write");
    }

    if (command.all_features || command.target.is_some()) && !command.check && !command.test {
        bail!("--all-features and --target require --check or --test");
    }

    if command.keep_broken && !command.check && !command.test {
        bail!("--keep-broken requires --check or --test");
    }

    let project = Project::load(command.manifest_path.as_deref())?;
    let mut analysis = analysis::analyze_inline_functions(&project)?;
    let semantic = SemanticProject::load(&project)?;
    apply_semantic_reference_filter(&mut analysis, &semantic)?;
    let plan = plan_inline_analysis(&analysis);

    println!("workspace: {}", project.root);
    println!("manifest: {}", project.manifest_path);
    println!("inline targets: {}", analysis.targets.len());

    for target in &analysis.targets {
        println!(
            "- {} in {} ({:?}), resolved call candidates: {}",
            target.name, target.file, target.range, target.call_candidates
        );
    }

    println!("planned edits: {}", plan.edits.len());
    if command.dry_run {
        print_edit_preview(&plan);
    }
    for diagnostic in &plan.diagnostics {
        println!("{:?}: {}", diagnostic.level, diagnostic.message);
    }

    if command.write {
        let originals = snapshot_project_files(&project)?;
        apply_plan(&plan)?;

        if command.check || command.test {
            let verification = verify::CargoVerification {
                manifest_path: Some(&project.manifest_path),
                all_features: command.all_features,
                target: command.target.as_deref(),
            };

            if let Err(error) = verify::run_cargo_fmt(Some(&project.manifest_path))
                .and_then(|_| verify::run_cargo_check(verification))
                .and_then(|_| {
                    if command.test {
                        verify::run_cargo_test(verification)
                    } else {
                        Ok(())
                    }
                })
            {
                if command.keep_broken {
                    return Err(error).context("verification failed; kept edited files");
                } else {
                    restore_project_files(originals)?;
                    return Err(error).context("verification failed; restored original files");
                }
            }
        }
    }

    Ok(())
}

fn print_edit_preview(plan: &RefactorPlan) {
    for edit in &plan.edits {
        let replacement = edit
            .replacement
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let replacement = if replacement.is_empty() {
            "<delete>".to_owned()
        } else {
            replacement
        };

        println!("  edit {} {:?} -> {}", edit.file, edit.range, replacement);
    }
}

fn apply_semantic_reference_filter(
    analysis: &mut InlineAnalysis,
    semantic: &SemanticProject,
) -> Result<()> {
    let mut resolved_calls = Vec::new();

    for function in &analysis.functions {
        let references = semantic.references_to(&function.file, function.name_range)?;
        for reference in references {
            if let Some(call) = find_call_for_reference(&analysis.calls, &reference) {
                resolved_calls.push(call.clone());
            } else if let Some(import) =
                find_import_for_reference(&analysis.imports, &function.name, &reference)
            {
                analysis.import_cleanups.push(ImportCleanup {
                    function_name: function.name.clone(),
                    file: import.file.clone(),
                    range: import.use_range,
                });
            } else {
                analysis.unhandled_references.push(UnhandledReference {
                    function_name: function.name.clone(),
                    file: reference.file,
                    range: reference.range,
                });
            }
        }
    }

    analysis.calls = resolved_calls;

    for target in &mut analysis.targets {
        target.call_candidates = analysis
            .calls
            .iter()
            .filter(|call| call.callee == target.name)
            .count();
    }

    Ok(())
}

fn find_call_for_reference<'a>(
    calls: &'a [CallSite],
    reference: &SemanticReference,
) -> Option<&'a CallSite> {
    calls
        .iter()
        .find(|call| call.file == reference.file && call.callee_range == reference.range)
}

fn plan_inline_analysis(analysis: &InlineAnalysis) -> RefactorPlan {
    let mut plan = plan_inline_functions(&analysis.functions, &analysis.calls);
    add_unsupported_target_diagnostics(&analysis.targets, &analysis.functions, &mut plan);
    add_import_cleanup_edits(&analysis.import_cleanups, &mut plan);
    add_unhandled_reference_diagnostics(&analysis.unhandled_references, &mut plan);
    plan
}

fn find_import_for_reference<'a>(
    imports: &'a [analysis::ImportSite],
    function_name: &str,
    reference: &SemanticReference,
) -> Option<&'a analysis::ImportSite> {
    imports.iter().find(|import| {
        import.function_name == function_name
            && import.file == reference.file
            && import.name_range == reference.range
    })
}

fn add_import_cleanup_edits(imports: &[ImportCleanup], plan: &mut RefactorPlan) {
    let mut seen = BTreeSet::new();

    for import in imports {
        let key = (
            import.file.clone(),
            u32::from(import.range.start()),
            u32::from(import.range.end()),
        );

        if !seen.insert(key) {
            continue;
        }

        plan.edits.push(TextEdit {
            file: import.file.clone(),
            range: import.range,
            replacement: String::new(),
        });
    }
}

fn add_unhandled_reference_diagnostics(references: &[UnhandledReference], plan: &mut RefactorPlan) {
    for reference in references {
        plan.diagnostics.push(Diagnostic {
            level: DiagnosticLevel::Error,
            message: format!(
                "`{}` has a resolved reference that is not a supported function call",
                reference.function_name
            ),
            file: Some(reference.file.clone()),
            range: Some(reference.range),
        });
    }
}

fn add_unsupported_target_diagnostics(
    targets: &[analysis::InlineTarget],
    functions: &[InlineFunction],
    plan: &mut RefactorPlan,
) {
    for target in targets {
        if functions
            .iter()
            .any(|function| function.file == target.file && function.range == target.range)
        {
            continue;
        }

        plan.diagnostics.push(Diagnostic {
            level: DiagnosticLevel::Error,
            message: format!(
                "`{}` is annotated for inlining but is outside the supported subset",
                target.name
            ),
            file: Some(target.file.clone()),
            range: Some(target.range),
        });
    }
}

fn snapshot_project_files(project: &Project) -> Result<BTreeMap<camino::Utf8PathBuf, String>> {
    project
        .rust_files
        .iter()
        .map(|path| {
            let contents = fs::read_to_string(path)
                .with_context(|| format!("failed to snapshot {path} before applying edits"))?;
            Ok((path.clone(), contents))
        })
        .collect()
}

fn restore_project_files(originals: BTreeMap<camino::Utf8PathBuf, String>) -> Result<()> {
    for (path, contents) in originals {
        fs::write(&path, contents).with_context(|| format!("failed to restore {path}"))?;
    }

    Ok(())
}

fn plan_inline_functions(functions: &[InlineFunction], calls: &[CallSite]) -> RefactorPlan {
    let mut plan = RefactorPlan::empty();
    let mut by_name: BTreeMap<&str, Vec<&InlineFunction>> = BTreeMap::new();

    for function in functions {
        by_name.entry(&function.name).or_default().push(function);
    }

    for (name, functions) in by_name {
        if functions.len() != 1 {
            plan.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                message: format!(
                    "multiple inline targets named `{name}`; semantic resolution is required"
                ),
                file: None,
                range: None,
            });
            continue;
        }

        let function = functions[0];
        let matching_calls = calls
            .iter()
            .filter(|call| call.callee == function.name)
            .collect::<Vec<_>>();

        let mut target_failed = false;
        for call in &matching_calls {
            match inline_call(function, call) {
                Ok(replacement) => plan.edits.push(TextEdit {
                    file: call.file.clone(),
                    range: call.range,
                    replacement,
                }),
                Err(message) => {
                    target_failed = true;
                    plan.diagnostics.push(Diagnostic {
                        level: DiagnosticLevel::Error,
                        message,
                        file: Some(call.file.clone()),
                        range: Some(call.range),
                    });
                }
            }
        }

        if !target_failed {
            plan.edits.push(TextEdit {
                file: function.file.clone(),
                range: function.range,
                replacement: String::new(),
            });
        }
    }

    plan
}

fn inline_call(function: &InlineFunction, call: &CallSite) -> std::result::Result<String, String> {
    if function.params.len() != call.args.len() {
        return Err(format!(
            "`{}` expects {} arguments, found {} at call site",
            function.name,
            function.params.len(),
            call.args.len()
        ));
    }

    let usage_counts = count_param_uses(&function.body_expr, &function.params)?;

    let mut replacements = BTreeMap::new();
    let mut temp_bindings = Vec::new();
    let mut reserved_names = call.identifiers_in_file.clone();

    for (param, arg) in function.params.iter().zip(&call.args) {
        if usage_counts.get(param).copied().unwrap_or(0) > 1 && !is_simple_expr(arg) {
            let temp_name = fresh_temp_name(param, &mut reserved_names);
            temp_bindings.push((temp_name.clone(), arg.clone()));
            replacements.insert(param.as_str(), temp_name);
            continue;
        }

        replacements.insert(param.as_str(), parenthesize_if_needed(arg));
    }

    let body = substitute_params(&function.body_expr, &replacements)?;

    if temp_bindings.is_empty() {
        return Ok(body);
    }

    let bindings = temp_bindings
        .into_iter()
        .map(|(name, arg)| format!("let {name} = {arg};"))
        .collect::<Vec<_>>()
        .join(" ");

    Ok(format!("({{ {bindings} {body} }})"))
}

fn fresh_temp_name(param: &str, reserved_names: &mut BTreeSet<String>) -> String {
    let sanitized = param
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let base = format!("__rust_refactor_{sanitized}");

    for index in 0.. {
        let candidate = if index == 0 {
            base.clone()
        } else {
            format!("{base}_{index}")
        };

        if reserved_names.insert(candidate.clone()) {
            return candidate;
        }
    }

    unreachable!("unbounded fresh name search should always return")
}

fn count_param_uses(
    expr: &str,
    params: &[String],
) -> std::result::Result<BTreeMap<String, usize>, String> {
    let parsed = ast::Expr::parse(expr, Edition::CURRENT);
    if !parsed.errors().is_empty() {
        return Err("inline body expression does not parse".to_owned());
    }

    let params = params.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let mut counts = BTreeMap::new();

    for token in parsed
        .syntax_node()
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
    {
        if is_path_name_ref_token(&token) && params.contains(token.text()) {
            *counts.entry(token.text().to_owned()).or_insert(0) += 1;
        }
    }

    Ok(counts)
}

fn substitute_params(
    expr: &str,
    replacements: &BTreeMap<&str, String>,
) -> std::result::Result<String, String> {
    let parsed = ast::Expr::parse(expr, Edition::CURRENT);
    if !parsed.errors().is_empty() {
        return Err("inline body expression does not parse".to_owned());
    }

    let mut edits = parsed
        .syntax_node()
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .filter_map(|token| {
            if !is_path_name_ref_token(&token) {
                return None;
            }

            replacements
                .get(token.text())
                .map(|replacement| (token.text_range(), replacement.clone()))
        })
        .collect::<Vec<_>>();

    edits.sort_by_key(|(range, _)| u32::from(range.start()));

    let mut updated = expr.to_owned();
    for (range, replacement) in edits.into_iter().rev() {
        replace_range(&mut updated, range, &replacement);
    }

    Ok(updated)
}

fn is_path_name_ref_token(token: &SyntaxToken) -> bool {
    if token.kind() != SyntaxKind::IDENT {
        return false;
    }

    token
        .parent()
        .and_then(ast::NameRef::cast)
        .is_some_and(|name_ref| {
            name_ref
                .syntax()
                .ancestors()
                .any(|node| ast::PathExpr::cast(node).is_some())
        })
}

fn replace_range(source: &mut String, range: TextRange, replacement: &str) {
    let start = u32::from(range.start()) as usize;
    let end = u32::from(range.end()) as usize;
    source.replace_range(start..end, replacement);
}

fn is_simple_expr(expr: &str) -> bool {
    let parsed = ast::Expr::parse(expr, Edition::CURRENT);
    if !parsed.errors().is_empty() {
        return false;
    }

    matches!(
        parsed.tree(),
        ast::Expr::Literal(_) | ast::Expr::PathExpr(_) | ast::Expr::ParenExpr(_)
    )
}

fn parenthesize_if_needed(expr: &str) -> String {
    if is_simple_expr(expr) {
        expr.to_owned()
    } else {
        format!("({expr})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use text_size::TextSize;

    fn range(start: u32, end: u32) -> TextRange {
        TextRange::new(TextSize::from(start), TextSize::from(end))
    }

    #[test]
    fn substitutes_simple_arguments() {
        let function = InlineFunction {
            file: Utf8PathBuf::from("src/lib.rs"),
            name: "helper".to_owned(),
            range: range(0, 10),
            name_range: range(3, 9),
            params: vec!["x".to_owned(), "y".to_owned()],
            body_expr: "x + y".to_owned(),
        };
        let call = CallSite {
            file: Utf8PathBuf::from("src/main.rs"),
            range: range(20, 32),
            callee_range: range(20, 26),
            callee: "helper".to_owned(),
            args: vec!["a + b".to_owned(), "c".to_owned()],
            arg_ranges: Vec::new(),
            identifiers_in_file: BTreeSet::new(),
        };

        assert_eq!(inline_call(&function, &call).unwrap(), "(a + b) + c");
    }

    #[test]
    fn introduces_temp_for_duplicated_complex_argument_evaluation() {
        let function = InlineFunction {
            file: Utf8PathBuf::from("src/lib.rs"),
            name: "square".to_owned(),
            range: range(0, 10),
            name_range: range(3, 9),
            params: vec!["x".to_owned()],
            body_expr: "x * x".to_owned(),
        };
        let call = CallSite {
            file: Utf8PathBuf::from("src/main.rs"),
            range: range(20, 32),
            callee_range: range(20, 26),
            callee: "square".to_owned(),
            args: vec!["next()".to_owned()],
            arg_ranges: Vec::new(),
            identifiers_in_file: BTreeSet::new(),
        };

        assert_eq!(
            inline_call(&function, &call).unwrap(),
            "({ let __rust_refactor_x = next(); __rust_refactor_x * __rust_refactor_x })"
        );
    }

    #[test]
    fn fresh_temp_avoids_existing_names() {
        let mut names = BTreeSet::from(["__rust_refactor_x".to_owned()]);

        assert_eq!(fresh_temp_name("x", &mut names), "__rust_refactor_x_1");
    }

    #[test]
    fn substitution_preserves_field_names() {
        let replacements = BTreeMap::from([("x", "value".to_owned())]);

        assert_eq!(
            substitute_params("x.field + other.x", &replacements).unwrap(),
            "value.field + other.x"
        );
    }

    #[test]
    fn refuses_to_plan_when_same_name_inline_target_exists() {
        let inline_function = InlineFunction {
            file: Utf8PathBuf::from("src/lib.rs"),
            name: "helper".to_owned(),
            range: range(0, 10),
            name_range: range(3, 9),
            params: vec!["x".to_owned()],
            body_expr: "x + 1".to_owned(),
        };
        let other_function = InlineFunction {
            file: Utf8PathBuf::from("src/other.rs"),
            name: "helper".to_owned(),
            range: range(20, 30),
            name_range: range(23, 29),
            params: vec!["x".to_owned()],
            body_expr: "x - 1".to_owned(),
        };

        let plan = plan_inline_functions(&[inline_function, other_function], &[]);

        assert!(plan.has_errors());
        assert!(plan.edits.is_empty());
    }
}
