use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use ra_ap_syntax::{
    ast::{self, AstNode, HasArgList, HasAttrs, HasGenericParams, HasName, HasVisibility},
    Edition, SourceFile, SyntaxKind, SyntaxToken,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};
use text_size::{TextRange, TextSize};

use crate::project::Project;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineTarget {
    pub file: Utf8PathBuf,
    pub name: String,
    pub range: TextRange,
    pub call_candidates: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineFunction {
    pub file: Utf8PathBuf,
    pub name: String,
    pub range: TextRange,
    pub name_range: TextRange,
    pub params: Vec<String>,
    pub body_expr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub file: Utf8PathBuf,
    pub range: TextRange,
    pub callee_range: TextRange,
    pub callee: String,
    pub args: Vec<String>,
    pub identifiers_in_file: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDef {
    pub file: Utf8PathBuf,
    pub name: String,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineAnalysis {
    pub targets: Vec<InlineTarget>,
    pub functions: Vec<InlineFunction>,
    pub all_functions: Vec<FunctionDef>,
    pub calls: Vec<CallSite>,
    pub imports: Vec<ImportSite>,
    pub import_cleanups: Vec<ImportCleanup>,
    pub unhandled_references: Vec<UnhandledReference>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSite {
    pub function_name: String,
    pub file: Utf8PathBuf,
    pub name_range: TextRange,
    pub use_range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportCleanup {
    pub function_name: String,
    pub file: Utf8PathBuf,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnhandledReference {
    pub function_name: String,
    pub file: Utf8PathBuf,
    pub range: TextRange,
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedFile {
    pub(crate) path: Utf8PathBuf,
    pub(crate) source: String,
    pub(crate) tree: ast::SourceFile,
}

pub fn find_inline_targets(project: &Project) -> Result<Vec<InlineTarget>> {
    let parsed_files = parse_project_files(project)?;
    let mut targets = discover_annotated_functions(&parsed_files);
    let calls = collect_call_candidates(&parsed_files);

    for target in &mut targets {
        target.call_candidates = calls.get(&target.name).copied().unwrap_or(0);
    }

    Ok(targets)
}

pub fn analyze_inline_functions(project: &Project) -> Result<InlineAnalysis> {
    let parsed_files = parse_project_files(project)?;
    let mut targets = discover_annotated_functions(&parsed_files);
    let calls_by_name = collect_call_candidates(&parsed_files);

    for target in &mut targets {
        target.call_candidates = calls_by_name.get(&target.name).copied().unwrap_or(0);
    }

    Ok(InlineAnalysis {
        targets,
        functions: discover_inline_functions(&parsed_files),
        all_functions: discover_functions(&parsed_files),
        calls: collect_call_sites(&parsed_files),
        imports: collect_import_sites(&parsed_files),
        import_cleanups: Vec::new(),
        unhandled_references: Vec::new(),
    })
}

pub(crate) fn parse_project_files(project: &Project) -> Result<Vec<ParsedFile>> {
    project
        .rust_files
        .iter()
        .map(|path| {
            let source = fs::read_to_string(path)
                .with_context(|| format!("failed to read Rust source file {path}"))?;
            let parsed = SourceFile::parse(&source, Edition::CURRENT);

            Ok(ParsedFile {
                path: path.clone(),
                source,
                tree: parsed.tree(),
            })
        })
        .collect()
}

fn discover_inline_functions(files: &[ParsedFile]) -> Vec<InlineFunction> {
    let mut functions = Vec::new();

    for file in files {
        for node in file.tree.syntax().descendants() {
            let Some(function) = ast::Fn::cast(node) else {
                continue;
            };

            if !has_inline_annotation(&function) {
                continue;
            }

            if !is_free_function(&function) {
                continue;
            }

            let Some(name) = function.name() else {
                continue;
            };

            let Some(params) = simple_params(&function) else {
                continue;
            };

            let Some(body_expr) = simple_body_expr(&function, &file.source) else {
                continue;
            };

            functions.push(InlineFunction {
                file: file.path.clone(),
                name: name.text().to_string(),
                range: function.syntax().text_range(),
                name_range: name.syntax().text_range(),
                params,
                body_expr,
            });
        }
    }

    functions
}

fn is_free_function(function: &ast::Fn) -> bool {
    !function.syntax().ancestors().skip(1).any(|node| {
        ast::Impl::cast(node.clone()).is_some()
            || ast::Trait::cast(node.clone()).is_some()
            || ast::ExternBlock::cast(node.clone()).is_some()
            || ast::Fn::cast(node).is_some()
    })
}

fn discover_functions(files: &[ParsedFile]) -> Vec<FunctionDef> {
    let mut functions = Vec::new();

    for file in files {
        for node in file.tree.syntax().descendants() {
            let Some(function) = ast::Fn::cast(node) else {
                continue;
            };

            let Some(name) = function.name() else {
                continue;
            };

            functions.push(FunctionDef {
                file: file.path.clone(),
                name: name.text().to_string(),
                range: function.syntax().text_range(),
            });
        }
    }

    functions
}

fn discover_annotated_functions(files: &[ParsedFile]) -> Vec<InlineTarget> {
    let mut targets = Vec::new();

    for file in files {
        for node in file.tree.syntax().descendants() {
            let Some(function) = ast::Fn::cast(node) else {
                continue;
            };

            if !has_inline_annotation(&function) {
                continue;
            }

            let Some(name) = function.name() else {
                continue;
            };

            targets.push(InlineTarget {
                file: file.path.clone(),
                name: name.text().to_string(),
                range: function.syntax().text_range(),
                call_candidates: 0,
            });
        }
    }

    targets
}

fn has_inline_annotation(function: &ast::Fn) -> bool {
    function.attrs().any(|attr| {
        let Some(meta) = attr.meta() else {
            return false;
        };

        if meta.simple_name().as_deref() == Some("doinline") {
            return true;
        }

        meta.path()
            .map(|path| {
                path.syntax()
                    .text()
                    .to_string()
                    .replace(char::is_whitespace, "")
            })
            .as_deref()
            == Some("rust_refactor::inline")
    })
}

fn collect_call_candidates(files: &[ParsedFile]) -> BTreeMap<String, usize> {
    let mut calls = BTreeMap::new();

    for file in files {
        for node in file.tree.syntax().descendants() {
            let Some(call) = ast::CallExpr::cast(node) else {
                continue;
            };

            let Some(callee) = call.expr().and_then(callee_name) else {
                continue;
            };

            *calls.entry(callee).or_insert(0) += 1;
        }
    }

    calls
}

pub(crate) fn collect_call_sites(files: &[ParsedFile]) -> Vec<CallSite> {
    collect_call_sites_impl(files, true)
}

pub(crate) fn collect_call_sites_light(files: &[ParsedFile]) -> Vec<CallSite> {
    collect_call_sites_impl(files, false)
}

fn collect_call_sites_impl(files: &[ParsedFile], include_identifiers: bool) -> Vec<CallSite> {
    let mut calls = Vec::new();

    for file in files {
        let identifiers_in_file = if include_identifiers {
            collect_identifiers(file)
        } else {
            BTreeSet::new()
        };

        for node in file.tree.syntax().descendants() {
            let Some(call) = ast::CallExpr::cast(node) else {
                continue;
            };

            let Some(callee) = call.expr().and_then(callee_name) else {
                continue;
            };
            let Some(callee_range) = call.expr().and_then(callee_range) else {
                continue;
            };

            let Some(arg_list) = call.arg_list() else {
                continue;
            };

            calls.push(CallSite {
                file: file.path.clone(),
                range: call.syntax().text_range(),
                callee_range,
                callee,
                args: arg_list
                    .args()
                    .map(|arg| {
                        source_for(file, arg.syntax().text_range())
                            .trim()
                            .to_owned()
                    })
                    .collect(),
                identifiers_in_file: identifiers_in_file.clone(),
            });
        }

        // A macro's token tree is opaque to the Rust syntax parser. Scan its
        // tokens for ordinary call syntax so fast mode can rewrite calls in
        // assert_eq!, format!, and other expression macros as well.
        let seen = calls
            .iter()
            .filter(|call| call.file == file.path)
            .map(|call| call.callee_range.start())
            .collect::<BTreeSet<_>>();
        for call in collect_macro_calls(file, &identifiers_in_file) {
            if !seen.contains(&call.callee_range.start()) {
                calls.push(call);
            }
        }
    }

    calls
}

/// Identifiers in macro token trees are not `PathExpr` nodes. The caller uses
/// these ranges to refuse function-value references that cannot be rewritten.
pub(crate) fn collect_macro_identifiers(file: &ParsedFile, name: &str) -> Vec<TextRange> {
    macro_tokens(file)
        .into_iter()
        .filter(|token| token.kind() == SyntaxKind::IDENT && token.text() == name)
        .map(|token| token.text_range())
        .collect()
}

fn macro_tokens(file: &ParsedFile) -> Vec<SyntaxToken> {
    let mut tokens = BTreeMap::new();
    for macro_call in file
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::MacroCall::cast)
    {
        let Some(tree) = macro_call.token_tree() else {
            continue;
        };
        for token in tree
            .syntax()
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
        {
            if !token.kind().is_trivia() {
                tokens.insert(token.text_range().start(), token);
            }
        }
    }
    tokens.into_values().collect()
}

fn collect_macro_calls(file: &ParsedFile, identifiers: &BTreeSet<String>) -> Vec<CallSite> {
    let tokens = macro_tokens(file);
    let mut calls = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if token.kind() != SyntaxKind::IDENT
            || tokens.get(index + 1).is_none_or(|next| next.text() != "(")
            || index > 0 && matches!(tokens[index - 1].text(), "." | "!")
        {
            continue;
        }

        let mut path_start = index;
        while path_start >= 2
            && tokens[path_start - 1].text() == "::"
            && tokens[path_start - 2].kind() == SyntaxKind::IDENT
        {
            path_start -= 2;
        }
        let mut nesting = vec![")"];
        let mut arg_start = index + 2;
        let mut args = Vec::new();
        let mut closing = None;
        for cursor in index + 2..tokens.len() {
            let current = tokens[cursor].text();
            match current {
                "(" => nesting.push(")"),
                "[" => nesting.push("]"),
                "{" => nesting.push("}"),
                ")" | "]" | "}" => {
                    if nesting.pop() != Some(current) {
                        break;
                    }
                    if nesting.is_empty() {
                        if arg_start < cursor {
                            args.push(
                                source_for(
                                    file,
                                    TextRange::new(
                                        tokens[arg_start].text_range().start(),
                                        tokens[cursor - 1].text_range().end(),
                                    ),
                                )
                                .trim()
                                .to_owned(),
                            );
                        }
                        closing = Some(cursor);
                        break;
                    }
                }
                "," if nesting.len() == 1 => {
                    if arg_start < cursor {
                        args.push(
                            source_for(
                                file,
                                TextRange::new(
                                    tokens[arg_start].text_range().start(),
                                    tokens[cursor - 1].text_range().end(),
                                ),
                            )
                            .trim()
                            .to_owned(),
                        );
                    }
                    arg_start = cursor + 1;
                }
                _ => {}
            }
        }
        let Some(closing) = closing else { continue };
        calls.push(CallSite {
            file: file.path.clone(),
            range: TextRange::new(
                tokens[path_start].text_range().start(),
                tokens[closing].text_range().end(),
            ),
            callee_range: token.text_range(),
            callee: token.text().to_owned(),
            args,
            identifiers_in_file: identifiers.clone(),
        });
    }
    calls
}

pub(crate) fn collect_import_sites(files: &[ParsedFile]) -> Vec<ImportSite> {
    let mut imports = Vec::new();

    for file in files {
        for node in file.tree.syntax().descendants() {
            let Some(use_item) = ast::Use::cast(node) else {
                continue;
            };

            if use_item.visibility().is_some() || use_item.attrs().next().is_some() {
                continue;
            }

            let Some(use_tree) = use_item.use_tree() else {
                continue;
            };

            collect_import_sites_from_use_tree(file, &use_item, &use_tree, &mut imports);
        }
    }

    imports
}

fn collect_import_sites_from_use_tree(
    file: &ParsedFile,
    use_item: &ast::Use,
    use_tree: &ast::UseTree,
    imports: &mut Vec<ImportSite>,
) {
    if is_simple_imported_path(use_tree) {
        if let Some(name_ref) = use_tree.path().and_then(|path| path.segment()?.name_ref()) {
            let use_range = if use_tree.parent_use_tree_list().is_some() {
                grouped_import_delete_range(file, use_item, use_tree)
            } else {
                Some(use_item.syntax().text_range())
            };

            if let Some(use_range) = use_range {
                imports.push(ImportSite {
                    function_name: name_ref.text().to_string(),
                    file: file.path.clone(),
                    name_range: name_ref.syntax().text_range(),
                    use_range,
                });
            }
        }
    }

    if let Some(list) = use_tree.use_tree_list() {
        for child in list.use_trees() {
            collect_import_sites_from_use_tree(file, use_item, &child, imports);
        }
    }
}

fn is_simple_imported_path(use_tree: &ast::UseTree) -> bool {
    use_tree.is_simple_path()
        && use_tree.rename().is_none()
        && use_tree.use_tree_list().is_none()
        && use_tree.star_token().is_none()
}

fn grouped_import_delete_range(
    file: &ParsedFile,
    use_item: &ast::Use,
    use_tree: &ast::UseTree,
) -> Option<TextRange> {
    let list = use_tree.parent_use_tree_list()?;
    let item_count = list.use_trees().count();

    if item_count <= 1 {
        return Some(use_item.syntax().text_range());
    }

    let source = &file.source;
    let child_range = use_tree.syntax().text_range();
    let list_range = list.syntax().text_range();
    let child_start = u32::from(child_range.start()) as usize;
    let child_end = u32::from(child_range.end()) as usize;
    let list_start = u32::from(list_range.start()) as usize;
    let list_end = u32::from(list_range.end()) as usize;

    let mut forward = child_end;
    while forward < list_end && is_inline_space(source.as_bytes()[forward]) {
        forward += 1;
    }

    if source.as_bytes().get(forward) == Some(&b',') {
        forward += 1;
        while forward < list_end && is_inline_space(source.as_bytes()[forward]) {
            forward += 1;
        }
        return Some(byte_range(child_start, forward));
    }

    let mut backward = child_start;
    while backward > list_start && is_inline_space(source.as_bytes()[backward - 1]) {
        backward -= 1;
    }

    if backward > list_start && source.as_bytes().get(backward - 1) == Some(&b',') {
        return Some(byte_range(backward - 1, child_end));
    }

    None
}

fn is_inline_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn byte_range(start: usize, end: usize) -> TextRange {
    TextRange::new(TextSize::from(start as u32), TextSize::from(end as u32))
}

fn collect_identifiers(file: &ParsedFile) -> BTreeSet<String> {
    file.tree
        .syntax()
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.kind() == SyntaxKind::IDENT)
        .map(|token| token.text().to_owned())
        .collect()
}

fn callee_name(expr: ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::PathExpr(path_expr) => path_expr
            .path()?
            .segment()?
            .name_ref()
            .map(|name| name.text().to_string()),
        _ => None,
    }
}

fn callee_range(expr: ast::Expr) -> Option<TextRange> {
    match expr {
        ast::Expr::PathExpr(path_expr) => path_expr
            .path()?
            .segment()?
            .name_ref()
            .map(|name| name.syntax().text_range()),
        _ => None,
    }
}

fn simple_params(function: &ast::Fn) -> Option<Vec<String>> {
    let params = function.param_list()?;

    if params.self_param().is_some() || function.generic_param_list().is_some() {
        return None;
    }

    params
        .params()
        .map(|param| match param.pat()? {
            ast::Pat::IdentPat(ident) if ident.is_simple_ident() => {
                Some(ident.name()?.text().to_string())
            }
            _ => None,
        })
        .collect()
}

fn simple_body_expr(function: &ast::Fn, source: &str) -> Option<String> {
    if function.async_token().is_some()
        || function.const_token().is_some()
        || function.unsafe_token().is_some()
        || function.semicolon_token().is_some()
    {
        return None;
    }

    let body = function.body()?;
    let stmt_list = body.stmt_list()?;

    if contains_unsupported_body(&body)
        || contains_recursive_call(function, &body)
        || contains_param_shadowing(function, &body)
    {
        return None;
    }

    let tail_expr = stmt_list.tail_expr()?;

    if stmt_list.statements().next().is_none() {
        return Some(
            source_slice(source, tail_expr.syntax().text_range())
                .trim()
                .to_owned(),
        );
    }

    let inner_range = TextRange::new(
        stmt_list.l_curly_token()?.text_range().end(),
        stmt_list.r_curly_token()?.text_range().start(),
    );
    let inner = source_slice(source, inner_range).trim();

    if inner.is_empty() {
        return None;
    }

    Some(format!("({{ {inner} }})"))
}

fn contains_unsupported_body(body: &ast::BlockExpr) -> bool {
    body.syntax().descendants().any(|node| {
        matches!(
            node.kind(),
            SyntaxKind::RETURN_EXPR
                | SyntaxKind::TRY_EXPR
                | SyntaxKind::MACRO_EXPR
                | SyntaxKind::MACRO_CALL
                | SyntaxKind::LOOP_EXPR
                | SyntaxKind::WHILE_EXPR
                | SyntaxKind::FOR_EXPR
                | SyntaxKind::BREAK_EXPR
                | SyntaxKind::CONTINUE_EXPR
        )
    })
}

fn contains_recursive_call(function: &ast::Fn, body: &ast::BlockExpr) -> bool {
    let Some(name) = function.name().map(|name| name.text().to_string()) else {
        return true;
    };

    body.syntax().descendants().any(|node| {
        ast::CallExpr::cast(node)
            .and_then(|call| call.expr())
            .and_then(callee_name)
            .is_some_and(|callee| callee == name)
    })
}

fn contains_param_shadowing(function: &ast::Fn, body: &ast::BlockExpr) -> bool {
    let Some(params) = simple_params(function) else {
        return true;
    };
    let params = params.into_iter().collect::<BTreeSet<_>>();

    body.syntax().descendants().any(|node| {
        ast::IdentPat::cast(node)
            .and_then(|pat| pat.name())
            .is_some_and(|name| params.contains(name.text().as_str()))
    })
}

pub(crate) fn source_for(file: &ParsedFile, range: TextRange) -> &str {
    source_slice(&file.source, range)
}

pub(crate) fn source_slice(source: &str, range: TextRange) -> &str {
    let start = u32::from(range.start()) as usize;
    let end = u32::from(range.end()) as usize;
    &source[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_source(source: &str) -> Vec<InlineTarget> {
        let parsed = SourceFile::parse(source, Edition::CURRENT);
        let file = ParsedFile {
            path: Utf8PathBuf::from("src/lib.rs"),
            source: source.to_owned(),
            tree: parsed.tree(),
        };
        let mut targets = discover_annotated_functions(std::slice::from_ref(&file));
        let calls = collect_call_candidates(&[file]);

        for target in &mut targets {
            target.call_candidates = calls.get(&target.name).copied().unwrap_or(0);
        }

        targets
    }

    #[test]
    fn discovers_namespaced_inline_annotation() {
        let targets = scan_source(
            r#"
#[rust_refactor::inline]
fn helper(x: i32) -> i32 { x + 1 }

fn main() {
    let _ = helper(1);
    let _ = crate::helper(2);
}
"#,
        );

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "helper");
        assert_eq!(targets[0].call_candidates, 2);
    }

    #[test]
    fn discovers_fallback_doinline_annotation() {
        let targets = scan_source(
            r#"
#[doinline]
fn helper(x: i32) -> i32 { x + 1 }
"#,
        );

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "helper");
    }

    #[test]
    fn ignores_regular_functions() {
        let targets = scan_source("fn helper(x: i32) -> i32 { x + 1 }");

        assert!(targets.is_empty());
    }

    #[test]
    fn extracts_simple_inline_function() {
        let parsed = SourceFile::parse(
            r#"
#[rust_refactor::inline]
fn helper(x: i32, y: i32) -> i32 { x + y }
"#,
            Edition::CURRENT,
        );
        let file = ParsedFile {
            path: Utf8PathBuf::from("src/lib.rs"),
            source: parsed.syntax_node().to_string(),
            tree: parsed.tree(),
        };

        let functions = discover_inline_functions(&[file]);

        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].params, ["x", "y"]);
        assert_eq!(functions[0].body_expr, "x + y");
    }

    #[test]
    fn extracts_statement_body_as_block_expression() {
        let parsed = SourceFile::parse(
            r#"
#[rust_refactor::inline]
fn helper(x: i32) -> i32 {
    let y = x + 1;
    y * 2
}
"#,
            Edition::CURRENT,
        );
        let file = ParsedFile {
            path: Utf8PathBuf::from("src/lib.rs"),
            source: parsed.syntax_node().to_string(),
            tree: parsed.tree(),
        };

        let functions = discover_inline_functions(&[file]);

        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].body_expr, "({ let y = x + 1;\n    y * 2 })");
    }

    #[test]
    fn rejects_recursive_inline_function() {
        let parsed = SourceFile::parse(
            r#"
#[rust_refactor::inline]
fn helper(x: i32) -> i32 {
    helper(x)
}
"#,
            Edition::CURRENT,
        );
        let file = ParsedFile {
            path: Utf8PathBuf::from("src/lib.rs"),
            source: parsed.syntax_node().to_string(),
            tree: parsed.tree(),
        };

        assert!(discover_inline_functions(&[file]).is_empty());
    }

    #[test]
    fn rejects_body_that_shadows_parameter() {
        let parsed = SourceFile::parse(
            r#"
#[rust_refactor::inline]
fn helper(x: i32) -> i32 {
    let x = x + 1;
    x
}
"#,
            Edition::CURRENT,
        );
        let file = ParsedFile {
            path: Utf8PathBuf::from("src/lib.rs"),
            source: parsed.syntax_node().to_string(),
            tree: parsed.tree(),
        };

        assert!(discover_inline_functions(&[file]).is_empty());
    }

    #[test]
    fn collects_simple_private_import_sites() {
        let parsed = SourceFile::parse("use crate::helper;\n", Edition::CURRENT);
        let file = ParsedFile {
            path: Utf8PathBuf::from("src/lib.rs"),
            source: parsed.syntax_node().to_string(),
            tree: parsed.tree(),
        };

        let imports = collect_import_sites(&[file]);

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].function_name, "helper");
    }

    #[test]
    fn collects_grouped_import_sites() {
        let parsed = SourceFile::parse(
            r#"
use crate::{helper, other};
"#,
            Edition::CURRENT,
        );
        let file = ParsedFile {
            path: Utf8PathBuf::from("src/lib.rs"),
            source: parsed.syntax_node().to_string(),
            tree: parsed.tree(),
        };

        let imports = collect_import_sites(&[file]);

        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].function_name, "helper");
        assert_eq!(imports[1].function_name, "other");
    }

    #[test]
    fn ignores_public_and_renamed_import_sites() {
        let parsed = SourceFile::parse(
            r#"
pub use crate::exported;
use crate::helper as renamed;
"#,
            Edition::CURRENT,
        );
        let file = ParsedFile {
            path: Utf8PathBuf::from("src/lib.rs"),
            source: parsed.syntax_node().to_string(),
            tree: parsed.tree(),
        };

        assert!(collect_import_sites(&[file]).is_empty());
    }
}
