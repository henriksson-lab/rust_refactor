//! Introduce a numeric enum while retaining integer constants as boundary aliases.
use anyhow::{anyhow, Context, Result};
use camino::Utf8PathBuf;
use ra_ap_syntax::{
    ast::{self, AstNode, BinaryOp, CmpOp, HasName, HasVisibility},
    Edition, SourceFile,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};
use text_size::{TextRange, TextSize};

use super::remove_function::{line_col_offset, range_json};
use crate::{
    analysis,
    cli::{ConstantsToEnumCommand, EnumVisibility, OutputFormat},
    edits::{apply_edits_to_string, apply_plan, RefactorPlan, TextEdit},
    project::Project,
    semantic::SemanticProject,
    verify,
};

#[derive(Debug)]
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
    Err(FlowError::Refused(Refusal {
        code,
        message: message.into(),
        file,
        range,
    }))
}

#[derive(Clone)]
struct SelectedConst {
    name: String,
    variant: String,
    raw_type: String,
    value_source: String,
    value: i128,
    file: Utf8PathBuf,
    item_range: TextRange,
    name_range: TextRange,
    value_range: TextRange,
    visibility: String,
    workspace_unique: bool,
}

pub fn run(command: ConstantsToEnumCommand) -> Result<i32> {
    match run_inner(&command) {
        Ok((status, plan, target)) => {
            emit(&command, status, Some(&plan), Some(&target), None);
            Ok(0)
        }
        Err(FlowError::Refused(reason)) => {
            emit(&command, "refused", None, None, Some(&reason));
            Ok(3)
        }
        Err(FlowError::Failed(error)) if matches!(command.format, OutputFormat::Json) => {
            println!(
                "{}",
                json!({"status":"error","target":null,"edits":[],"diagnostics":[{"code":"OPERATION_FAILED","message":format!("{error:#}"),"file":null,"range":null}]})
            );
            Ok(1)
        }
        Err(FlowError::Failed(error)) => Err(error),
    }
}

fn run_inner(
    command: &ConstantsToEnumCommand,
) -> std::result::Result<(&'static str, RefactorPlan, Value), FlowError> {
    if command.dry_run == command.write {
        return refuse(
            "INVALID_MODE",
            "choose exactly one of --dry-run or --write",
            None,
            None,
        );
    }
    if command.constants.len() < 2 {
        return refuse(
            "TOO_FEW_CONSTANTS",
            "select at least two constants",
            None,
            None,
        );
    }
    if command.matches.is_empty() && command.comparisons.is_empty() {
        return refuse(
            "NO_USE_SITES",
            "select at least one --match or --comparison",
            None,
            None,
        );
    }
    if !valid_ident(&command.enum_name) {
        return refuse(
            "INVALID_ENUM_NAME",
            "enum name is not a valid Rust identifier",
            None,
            None,
        );
    }
    let requested = parse_constant_specs(&command.constants)?;
    let project = Project::load(command.manifest_path.as_deref())?;
    let definition_file = resolve_file(&project, &command.file)?;
    let parsed = analysis::parse_source_file(&definition_file)?;

    if item_name_exists(&parsed.tree, &command.enum_name) {
        return refuse(
            "ENUM_NAME_CONFLICT",
            format!("an item named `{}` already exists", command.enum_name),
            Some(definition_file),
            None,
        );
    }
    let mut constants = Vec::new();
    for (name, variant) in requested {
        if !valid_ident(&variant) {
            return refuse(
                "INVALID_VARIANT",
                format!("`{variant}` is not a valid Rust variant identifier"),
                Some(definition_file.clone()),
                None,
            );
        }
        let found = parsed
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::Const::cast)
            .filter(|item| {
                item.syntax()
                    .parent()
                    .is_some_and(|parent| ast::SourceFile::cast(parent).is_some())
            })
            .filter(|item| {
                item.name()
                    .is_some_and(|candidate| candidate.text() == name)
            })
            .collect::<Vec<_>>();
        if found.len() != 1 {
            return refuse(
                "CONSTANT_NOT_FOUND",
                format!(
                    "expected one constant named `{name}`, found {}",
                    found.len()
                ),
                Some(definition_file.clone()),
                None,
            );
        }
        let item = &found[0];
        if item
            .syntax()
            .ancestors()
            .skip(1)
            .any(|node| ast::Impl::cast(node.clone()).is_some() || ast::Trait::cast(node).is_some())
        {
            return refuse(
                "NOT_CONST_ITEM",
                format!("`{name}` is not a free module constant"),
                Some(definition_file.clone()),
                Some(item.syntax().text_range()),
            );
        }
        let ty = item
            .ty()
            .ok_or_else(|| anyhow!("constant `{name}` has no type"))?;
        let raw_type = ty.syntax().text().to_string().trim().to_owned();
        if !matches!(
            raw_type.as_str(),
            "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize"
        ) {
            return refuse(
                "TYPE_MISMATCH",
                format!("`{name}` has unsupported type `{raw_type}`"),
                Some(definition_file.clone()),
                Some(ty.syntax().text_range()),
            );
        }
        let value_expr = item
            .syntax()
            .children()
            .find_map(ast::Expr::cast)
            .ok_or_else(|| anyhow!("constant `{name}` has no value"))?;
        let value_source = value_expr.syntax().text().to_string().trim().to_owned();
        let Some(value) = parse_integer(&value_source, &raw_type) else {
            return refuse(
                "UNSUPPORTED_CONST_VALUE",
                format!("`{name}` does not have a supported integer literal initializer"),
                Some(definition_file.clone()),
                Some(value_expr.syntax().text_range()),
            );
        };
        let visibility = item
            .visibility()
            .map(|v| v.syntax().text().to_string())
            .unwrap_or_default()
            .replace(char::is_whitespace, "");
        constants.push(SelectedConst {
            name,
            variant,
            raw_type,
            value_source,
            value,
            file: definition_file.clone(),
            item_range: item.syntax().text_range(),
            name_range: item.name().unwrap().syntax().text_range(),
            value_range: value_expr.syntax().text_range(),
            visibility,
            workspace_unique: false,
        });
    }
    constants.sort_by_key(|item| item.item_range.start());
    let raw_type = constants[0].raw_type.clone();
    if constants.iter().any(|item| item.raw_type != raw_type) {
        return refuse(
            "TYPE_MISMATCH",
            "selected constants do not share one integer type",
            Some(definition_file),
            None,
        );
    }
    let mut values: BTreeMap<i128, String> = BTreeMap::new();
    let mut variants: BTreeMap<String, i128> = BTreeMap::new();
    for item in &constants {
        if values
            .insert(item.value, item.variant.clone())
            .is_some_and(|variant| variant != item.variant)
        {
            return refuse(
                "DUPLICATE_VALUE",
                format!("value {} is assigned to different variants", item.value),
                Some(item.file.clone()),
                Some(item.value_range),
            );
        }
        if variants
            .insert(item.variant.clone(), item.value)
            .is_some_and(|value| value != item.value)
        {
            return refuse(
                "INVALID_VARIANT",
                format!("variant `{}` is assigned to different values", item.variant),
                Some(item.file.clone()),
                Some(item.name_range),
            );
        }
    }
    let visibility = enum_visibility(command.visibility, &constants)?;
    let enum_path = command.enum_path.as_deref().unwrap_or(&command.enum_name);
    if !valid_path(enum_path) {
        return refuse(
            "INVALID_ENUM_PATH",
            "--enum-path is not a valid Rust path",
            None,
            None,
        );
    }
    let generated = generate_enum(&command.enum_name, &visibility, &raw_type, &constants);
    let mut plan = RefactorPlan::empty();
    plan.edits.push(TextEdit {
        file: definition_file.clone(),
        range: TextRange::empty(constants[0].item_range.start()),
        replacement: generated,
    });
    for item in &constants {
        plan.edits.push(TextEdit {
            file: item.file.clone(),
            range: item.value_range,
            replacement: format!("{}::{}.to_raw()", command.enum_name, item.variant),
        });
    }

    let crosses_files = command
        .matches
        .iter()
        .chain(&command.comparisons)
        .try_fold(false, |needed, selection| {
            let (path, _, _) = parse_position(selection)?;
            Ok::<_, FlowError>(
                needed || resolve_file(&project, &Utf8PathBuf::from(path))? != definition_file,
            )
        })?;
    if crosses_files {
        let selected_names = constants
            .iter()
            .map(|item| item.name.clone())
            .collect::<BTreeSet<_>>();
        let mut definition_counts = BTreeMap::<String, usize>::new();
        for file in analysis::parse_project_files(&project)? {
            for item in file
                .tree
                .syntax()
                .descendants()
                .filter_map(ast::Const::cast)
            {
                if let Some(name) = item
                    .name()
                    .filter(|name| selected_names.contains(name.text().as_str()))
                {
                    *definition_counts
                        .entry(name.text().to_string())
                        .or_default() += 1;
                }
            }
        }
        for item in &mut constants {
            item.workspace_unique = definition_counts.get(&item.name) == Some(&1);
        }
    }
    let needs_semantic = crosses_files && constants.iter().any(|item| !item.workspace_unique);
    let semantic = needs_semantic
        .then(|| {
            SemanticProject::load_with(&project, command.all_features, command.target.as_deref())
        })
        .transpose()?;
    let mut converted_matches = Vec::new();
    let mut converted_match_ranges = Vec::new();
    for selection in &command.matches {
        let (selection_file, line, column) = parse_position(selection)?;
        let match_file = resolve_file(&project, &Utf8PathBuf::from(selection_file))?;
        let match_parsed = analysis::parse_source_file(&match_file)?;
        let offset = line_col_offset(&match_parsed.source, line, column).ok_or_else(|| {
            FlowError::Refused(Refusal {
                code: "MATCH_NOT_FOUND",
                message: "match position lies outside the file".into(),
                file: Some(match_file.clone()),
                range: None,
            })
        })?;
        let mut matches = match_parsed
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::MatchExpr::cast)
            .filter(|item| {
                item.syntax()
                    .text_range()
                    .contains(TextSize::from(offset as u32))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|item| u32::from(item.syntax().text_range().len()));
        let Some(match_expr) = matches.first() else {
            return refuse(
                "MATCH_NOT_FOUND",
                "position is not inside a match expression",
                Some(match_file),
                None,
            );
        };
        let replacement = rewrite_match(
            match_expr,
            &match_parsed.source,
            &match_file,
            enum_path,
            &constants,
            semantic.as_ref(),
        )?;
        plan.edits.push(TextEdit {
            file: match_file.clone(),
            range: match_expr.syntax().text_range(),
            replacement,
        });
        converted_match_ranges.push((match_file.clone(), match_expr.syntax().text_range()));
        converted_matches
            .push(json!({"file":match_file,"range":range_json(match_expr.syntax().text_range())}));
    }
    let mut converted_comparisons = Vec::new();
    for selection in &command.comparisons {
        let (selection_file, line, column) = parse_position(selection)?;
        let comparison_file = resolve_file(&project, &Utf8PathBuf::from(selection_file))?;
        let comparison_parsed = analysis::parse_source_file(&comparison_file)?;
        let offset = line_col_offset(&comparison_parsed.source, line, column).ok_or_else(|| {
            FlowError::Refused(Refusal {
                code: "COMPARISON_NOT_FOUND",
                message: "comparison position lies outside the file".into(),
                file: Some(comparison_file.clone()),
                range: None,
            })
        })?;
        let mut comparisons = comparison_parsed
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::BinExpr::cast)
            .filter(|item| {
                item.syntax()
                    .text_range()
                    .contains(TextSize::from(offset as u32))
            })
            .collect::<Vec<_>>();
        comparisons.sort_by_key(|item| u32::from(item.syntax().text_range().len()));
        let Some(comparison) = comparisons.first() else {
            return refuse(
                "COMPARISON_NOT_FOUND",
                "position is not inside a binary comparison",
                Some(comparison_file),
                None,
            );
        };
        if converted_match_ranges.iter().any(|(file, range)| {
            file == &comparison_file && range.contains_range(comparison.syntax().text_range())
        }) {
            continue;
        }
        let replacement = rewrite_comparison(
            comparison,
            &comparison_parsed.source,
            &comparison_file,
            enum_path,
            &constants,
            semantic.as_ref(),
        )?;
        plan.edits.push(TextEdit {
            file: comparison_file.clone(),
            range: comparison.syntax().text_range(),
            replacement,
        });
        converted_comparisons.push(
            json!({"file":comparison_file,"range":range_json(comparison.syntax().text_range())}),
        );
    }
    validate_plan(&plan)?;
    let target = json!({"enum_name":command.enum_name,"raw_type":raw_type,"constants":constants.iter().map(|item|json!({"name":item.name,"variant":item.variant,"value":item.value_source})).collect::<Vec<_>>(),"matches":converted_matches,"comparisons":converted_comparisons});
    if command.dry_run {
        return Ok(("planned", plan, target));
    }
    let originals = snapshot(&plan)?;
    let result = (|| -> Result<()> {
        apply_plan(&plan)?;
        verify::run_rustfmt_files(&project.manifest_path, &originals.keys().cloned().collect())?;
        verify::run_cargo_check(verify::CargoVerification {
            manifest_path: Some(&project.manifest_path),
            all_features: command.all_features,
            target: command.target.as_deref(),
        })
    })();
    if let Err(error) = result {
        restore(originals)?;
        return Err(FlowError::Failed(
            error.context("constants-to-enum failed; original files restored"),
        ));
    }
    Ok(("applied", plan, target))
}

fn rewrite_comparison(
    comparison: &ast::BinExpr,
    source: &str,
    file: &Utf8PathBuf,
    enum_path: &str,
    constants: &[SelectedConst],
    semantic: Option<&SemanticProject>,
) -> std::result::Result<String, FlowError> {
    let negated = match comparison.op_kind() {
        Some(BinaryOp::CmpOp(CmpOp::Eq { negated })) => negated,
        _ => {
            return refuse(
                "UNSUPPORTED_COMPARISON",
                "selected comparison must use == or !=",
                Some(file.clone()),
                Some(comparison.syntax().text_range()),
            )
        }
    };
    let (Some(lhs), Some(rhs)) = (comparison.lhs(), comparison.rhs()) else {
        return Err(anyhow!("comparison has missing operand").into());
    };
    let lhs_constant = selected_from_expr(&lhs, file, constants, semantic)?;
    let rhs_constant = selected_from_expr(&rhs, file, constants, semantic)?;
    let (raw, selected) = match (lhs_constant, rhs_constant) {
        (Some(_), Some(_)) => {
            return refuse(
                "TWO_CONSTANT_COMPARISON",
                "both comparison operands are selected constants",
                Some(file.clone()),
                Some(comparison.syntax().text_range()),
            )
        }
        (None, None) => {
            return refuse(
                "CONSTANT_NOT_FOUND",
                "comparison does not contain a selected constant",
                Some(file.clone()),
                Some(comparison.syntax().text_range()),
            )
        }
        (Some(selected), None) => (&rhs, selected),
        (None, Some(selected)) => (&lhs, selected),
    };
    let operator = if negated { "!=" } else { "==" };
    Ok(format!(
        "{enum_path}::from_raw({}) {operator} Some({enum_path}::{})",
        slice(source, raw.syntax().text_range()),
        selected.variant
    ))
}

fn selected_from_expr<'a>(
    expr: &ast::Expr,
    file: &Utf8PathBuf,
    constants: &'a [SelectedConst],
    semantic: Option<&SemanticProject>,
) -> std::result::Result<Option<&'a SelectedConst>, FlowError> {
    let path_expr = match expr {
        ast::Expr::PathExpr(path) => path.clone(),
        ast::Expr::ParenExpr(paren) => {
            return selected_from_expr(
                &paren
                    .expr()
                    .ok_or_else(|| anyhow!("empty parenthesized expression"))?,
                file,
                constants,
                semantic,
            )
        }
        ast::Expr::CastExpr(cast) => {
            return selected_from_expr(
                &cast
                    .expr()
                    .ok_or_else(|| anyhow!("cast has no expression"))?,
                file,
                constants,
                semantic,
            )
        }
        _ => return Ok(None),
    };
    let Some(name) = path_expr
        .path()
        .and_then(|path| path.segment())
        .and_then(|segment| segment.name_ref())
    else {
        return Ok(None);
    };
    resolve_selected_constant(
        &name.text().to_string(),
        name.syntax().text_range(),
        file,
        constants,
        semantic,
    )
}

fn rewrite_match(
    match_expr: &ast::MatchExpr,
    source: &str,
    file: &Utf8PathBuf,
    enum_path: &str,
    constants: &[SelectedConst],
    semantic: Option<&SemanticProject>,
) -> std::result::Result<String, FlowError> {
    let expr = match_expr
        .expr()
        .ok_or_else(|| anyhow!("match has no discriminant"))?;
    let arms = match_expr
        .match_arm_list()
        .ok_or_else(|| anyhow!("match has no arms"))?;
    let mut replacements = vec![(
        expr.syntax().text_range(),
        format!(
            "{enum_path}::from_raw({})",
            slice(source, expr.syntax().text_range())
        ),
    )];
    let mut catch_all = false;
    for arm in arms.arms() {
        let pat = arm
            .pat()
            .ok_or_else(|| anyhow!("match arm has no pattern"))?;
        collect_pattern_edits(
            &pat,
            file,
            enum_path,
            constants,
            semantic,
            &mut replacements,
            &mut catch_all,
        )?;
    }
    if !catch_all {
        return refuse(
            "MISSING_CATCH_ALL",
            "selected match needs a `_` catch-all arm",
            Some(file.clone()),
            Some(match_expr.syntax().text_range()),
        );
    }
    let whole = match_expr.syntax().text_range();
    let mut output = slice(source, whole).to_owned();
    replacements.sort_by_key(|(range, _)| range.start());
    for (range, replacement) in replacements.into_iter().rev() {
        let start = u32::from(range.start() - whole.start()) as usize;
        let end = u32::from(range.end() - whole.start()) as usize;
        output.replace_range(start..end, &replacement);
    }
    Ok(output)
}

fn collect_pattern_edits(
    pat: &ast::Pat,
    file: &Utf8PathBuf,
    enum_path: &str,
    constants: &[SelectedConst],
    semantic: Option<&SemanticProject>,
    edits: &mut Vec<(TextRange, String)>,
    catch_all: &mut bool,
) -> std::result::Result<(), FlowError> {
    match pat {
        ast::Pat::WildcardPat(_) => {
            *catch_all = true;
            Ok(())
        }
        ast::Pat::ParenPat(item) => collect_pattern_edits(
            &item
                .pat()
                .ok_or_else(|| anyhow!("empty parenthesized pattern"))?,
            file,
            enum_path,
            constants,
            semantic,
            edits,
            catch_all,
        ),
        ast::Pat::OrPat(item) => {
            for child in item.pats() {
                collect_pattern_edits(
                    &child, file, enum_path, constants, semantic, edits, catch_all,
                )?;
            }
            Ok(())
        }
        ast::Pat::PathPat(item) => {
            let path = item
                .path()
                .ok_or_else(|| anyhow!("path pattern has no path"))?;
            let name_ref = path
                .segment()
                .and_then(|segment| segment.name_ref())
                .ok_or_else(|| anyhow!("path pattern has no name"))?;
            let selected = resolve_selected_constant(
                &name_ref.text().to_string(),
                name_ref.syntax().text_range(),
                file,
                constants,
                semantic,
            )?;
            let Some(selected) = selected else {
                return refuse(
                    "FOREIGN_CONSTANT",
                    format!(
                        "pattern `{}` does not resolve to a selected constant",
                        path.syntax().text()
                    ),
                    Some(file.clone()),
                    Some(path.syntax().text_range()),
                );
            };
            edits.push((
                path.syntax().text_range(),
                format!("Some({enum_path}::{})", selected.variant),
            ));
            Ok(())
        }
        ast::Pat::IdentPat(item)
            if item.ref_token().is_none() && item.mut_token().is_none() && item.pat().is_none() =>
        {
            let name = item
                .name()
                .ok_or_else(|| anyhow!("identifier pattern has no name"))?;
            let selected = resolve_selected_constant(
                &name.text().to_string(),
                name.syntax().text_range(),
                file,
                constants,
                semantic,
            )?;
            let Some(selected) = selected else {
                return refuse(
                    "FOREIGN_CONSTANT",
                    format!(
                        "pattern `{}` does not resolve to a selected constant",
                        name.text()
                    ),
                    Some(file.clone()),
                    Some(item.syntax().text_range()),
                );
            };
            edits.push((
                item.syntax().text_range(),
                format!("Some({enum_path}::{})", selected.variant),
            ));
            Ok(())
        }
        _ => refuse(
            "UNSUPPORTED_PATTERN",
            "match patterns may contain only selected constants, `|`, parentheses, and `_`",
            Some(file.clone()),
            Some(pat.syntax().text_range()),
        ),
    }
}

fn resolve_selected_constant<'a>(
    name: &str,
    range: TextRange,
    file: &Utf8PathBuf,
    constants: &'a [SelectedConst],
    semantic: Option<&SemanticProject>,
) -> std::result::Result<Option<&'a SelectedConst>, FlowError> {
    if constants.first().is_some_and(|item| &item.file == file) {
        return Ok(constants.iter().find(|item| item.name == name));
    }
    if let Some(item) = constants
        .iter()
        .find(|item| item.name == name && item.workspace_unique)
    {
        return Ok(Some(item));
    }
    let Some(semantic) = semantic else {
        return Ok(None);
    };
    let definition = semantic.definition_at(file, range)?;
    Ok(definition.and_then(|definition| {
        constants
            .iter()
            .find(|item| item.file == definition.file && item.name_range == definition.name_range)
    }))
}

fn parse_constant_specs(specs: &[String]) -> std::result::Result<Vec<(String, String)>, FlowError> {
    let mut out = Vec::new();
    let mut names = BTreeSet::new();
    for spec in specs {
        let Some((name, variant)) = spec.split_once('=') else {
            return refuse(
                "INVALID_CONSTANT",
                format!("`{spec}` must be CONSTANT=Variant"),
                None,
                None,
            );
        };
        if !valid_ident(name) || !names.insert(name.to_owned()) {
            return refuse(
                "INVALID_CONSTANT",
                format!("invalid or repeated constant `{name}`"),
                None,
                None,
            );
        }
        out.push((name.to_owned(), variant.to_owned()));
    }
    Ok(out)
}
fn parse_position(value: &str) -> std::result::Result<(&str, usize, usize), FlowError> {
    let mut parts = value.rsplitn(3, ':');
    let column = parts.next().and_then(|v| v.parse().ok());
    let line = parts.next().and_then(|v| v.parse().ok());
    let file = parts.next();
    match (file, line, column) {
        (Some(f), Some(l), Some(c)) if l > 0 && c > 0 => Ok((f, l, c)),
        _ => refuse(
            "INVALID_POSITION",
            format!("`{value}` must be FILE:LINE:COLUMN"),
            None,
            None,
        ),
    }
}
fn resolve_file(
    project: &Project,
    path: &Utf8PathBuf,
) -> std::result::Result<Utf8PathBuf, FlowError> {
    let joined = if path.is_absolute() {
        path.clone()
    } else {
        project.root.join(path)
    };
    let canonical = fs::canonicalize(&joined).map_err(|e| anyhow!(e))?;
    let file = Utf8PathBuf::from_path_buf(canonical).map_err(|_| anyhow!("non-UTF-8 path"))?;
    if !project.rust_files.contains(&file) {
        return refuse(
            "OUTSIDE_WORKSPACE",
            "file is outside the Cargo workspace",
            Some(file),
            None,
        );
    }
    Ok(file)
}
fn enum_visibility(
    override_: Option<EnumVisibility>,
    constants: &[SelectedConst],
) -> std::result::Result<String, FlowError> {
    if let Some(value) = override_ {
        return Ok(match value {
            EnumVisibility::Private => "",
            EnumVisibility::PubCrate => "pub(crate) ",
            EnumVisibility::Pub => "pub ",
        }
        .into());
    }
    let first = &constants[0].visibility;
    if constants.iter().any(|item| &item.visibility != first) {
        return refuse(
            "VISIBILITY_MISMATCH",
            "selected constants have different visibility; pass --visibility",
            Some(constants[0].file.clone()),
            None,
        );
    }
    Ok(match first.as_str() {
        "" => "".into(),
        "pub" => "pub ".into(),
        "pub(crate)" => "pub(crate) ".into(),
        other => {
            return refuse(
                "VISIBILITY_MISMATCH",
                format!("unsupported visibility `{other}`; pass --visibility"),
                Some(constants[0].file.clone()),
                None,
            )
        }
    })
}
fn generate_enum(name: &str, visibility: &str, raw: &str, constants: &[SelectedConst]) -> String {
    let mut seen = BTreeSet::new();
    let canonical = constants
        .iter()
        .filter(|item| seen.insert(item.variant.clone()))
        .collect::<Vec<_>>();
    let variants = canonical
        .iter()
        .map(|item| format!("    {} = {},", item.variant, item.value_source))
        .collect::<Vec<_>>()
        .join("\n");
    let conversions = canonical
        .iter()
        .map(|item| {
            format!(
                "            {} => Some(Self::{}),",
                item.value_source, item.variant
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("#[repr({raw})]\n#[derive(Clone, Copy, Debug, Eq, PartialEq)]\n{visibility}enum {name} {{\n{variants}\n}}\n\nimpl {name} {{\n    pub const fn from_raw(value: {raw}) -> Option<Self> {{\n        match value {{\n{conversions}\n            _ => None,\n        }}\n    }}\n\n    pub const fn to_raw(self) -> {raw} {{\n        self as {raw}\n    }}\n}}\n\n")
}
fn parse_integer(source: &str, raw: &str) -> Option<i128> {
    let mut s = source.trim();
    while s.starts_with('(') && s.ends_with(')') {
        s = s[1..s.len() - 1].trim();
    }
    if let Some(x) = s.strip_suffix(raw) {
        s = x;
    }
    let clean = s.replace('_', "");
    let (negative, digits) = clean
        .strip_prefix('-')
        .map_or((false, clean.as_str()), |v| (true, v));
    let (radix, digits) = if let Some(v) = digits.strip_prefix("0x") {
        (16, v)
    } else if let Some(v) = digits.strip_prefix("0o") {
        (8, v)
    } else if let Some(v) = digits.strip_prefix("0b") {
        (2, v)
    } else {
        (10, digits)
    };
    let value = i128::from_str_radix(digits, radix).ok()?;
    Some(if negative { -value } else { value })
}
fn valid_ident(value: &str) -> bool {
    let parsed = SourceFile::parse(&format!("enum __Check {{ {value} }}"), Edition::CURRENT);
    parsed.errors().is_empty()
}
fn valid_path(value: &str) -> bool {
    let parsed = SourceFile::parse(
        &format!("fn __check() {{ let _ = {value}::from_raw(0); }}"),
        Edition::CURRENT,
    );
    parsed.errors().is_empty()
}
fn item_name_exists(file: &ast::SourceFile, name: &str) -> bool {
    file.syntax().children().any(|node| {
        ast::Enum::cast(node.clone())
            .and_then(|x| x.name())
            .is_some_and(|x| x.text() == name)
            || ast::Struct::cast(node.clone())
                .and_then(|x| x.name())
                .is_some_and(|x| x.text() == name)
            || ast::Union::cast(node.clone())
                .and_then(|x| x.name())
                .is_some_and(|x| x.text() == name)
            || ast::Trait::cast(node)
                .and_then(|x| x.name())
                .is_some_and(|x| x.text() == name)
    })
}
fn slice(source: &str, range: TextRange) -> &str {
    &source[u32::from(range.start()) as usize..u32::from(range.end()) as usize]
}
fn validate_plan(plan: &RefactorPlan) -> std::result::Result<(), FlowError> {
    let mut by_file: BTreeMap<&Utf8PathBuf, Vec<&TextEdit>> = BTreeMap::new();
    for edit in &plan.edits {
        by_file.entry(&edit.file).or_default().push(edit);
    }
    for (file, edits) in by_file {
        let source = fs::read_to_string(file)?;
        let updated = apply_edits_to_string(&source, &edits).map_err(FlowError::Failed)?;
        let parsed = SourceFile::parse(&updated, Edition::CURRENT);
        if !parsed.errors().is_empty() {
            return refuse(
                "GENERATED_CODE_INVALID",
                format!("generated source does not parse: {}", parsed.errors()[0]),
                Some(file.clone()),
                None,
            );
        }
    }
    Ok(())
}
fn snapshot(plan: &RefactorPlan) -> Result<BTreeMap<Utf8PathBuf, String>> {
    let mut map = BTreeMap::new();
    for edit in &plan.edits {
        if !map.contains_key(&edit.file) {
            map.insert(edit.file.clone(), fs::read_to_string(&edit.file)?);
        }
    }
    Ok(map)
}
fn restore(originals: BTreeMap<Utf8PathBuf, String>) -> Result<()> {
    for (path, text) in originals {
        fs::write(&path, text).with_context(|| format!("failed to restore {path}"))?;
    }
    Ok(())
}
fn emit(
    command: &ConstantsToEnumCommand,
    status: &str,
    plan: Option<&RefactorPlan>,
    target: Option<&Value>,
    refusal: Option<&Refusal>,
) {
    let edits = plan
        .into_iter()
        .flat_map(|p| &p.edits)
        .map(|e| json!({"file":e.file,"range":range_json(e.range),"replacement":e.replacement}))
        .collect::<Vec<_>>();
    let diagnostics=refusal.into_iter().map(|r|json!({"code":r.code,"message":r.message,"file":r.file,"range":r.range.map(range_json)})).collect::<Vec<_>>();
    match command.format {
        OutputFormat::Json => println!(
            "{}",
            json!({"status":status,"target":target,"edits":edits,"diagnostics":diagnostics})
        ),
        OutputFormat::Text => {
            println!("{status}: {} edits", edits.len());
            if let Some(r) = refusal {
                eprintln!("{}: {}", r.code, r.message);
            }
        }
    }
}
