//! Inventory C-style return conventions that can become `Result` or `Option`.
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use ra_ap_syntax::ast::{self, AstNode, ElseBranch, HasName, HasVisibility};
use ra_ap_syntax::{Edition, SourceFile};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use crate::{
    analysis::ParsedFile,
    cli::{ReturnStatsCommand, ReturnStatsFormat},
    project::Project,
};

#[derive(Clone, Debug, Serialize)]
struct ReturnSite {
    value: String,
    resolved_value: Option<i128>,
    category: &'static str,
    line: usize,
    column: usize,
}

#[derive(Clone, Debug, Serialize)]
struct FunctionReport {
    function: String,
    file: Utf8PathBuf,
    line: usize,
    column: usize,
    kind: &'static str,
    visibility: String,
    declared_return_type: String,
    suggested_return: String,
    confidence: String,
    reason: String,
    known_return_values: Vec<String>,
    negative_sentinels: Vec<String>,
    null_return_forms: Vec<String>,
    unknown_return_values: Vec<String>,
    return_site_count: usize,
    evidence: Vec<ReturnSite>,
}

#[derive(Default)]
struct ConstIndex {
    by_file: BTreeMap<(Utf8PathBuf, String), Option<i128>>,
    global: BTreeMap<String, Option<i128>>,
}

impl ConstIndex {
    fn get(&self, file: &Utf8Path, name: &str) -> Option<i128> {
        self.by_file
            .get(&(file.to_owned(), name.to_owned()))
            .copied()
            .flatten()
            .or_else(|| self.global.get(name).copied().flatten())
    }
}

pub fn run(command: ReturnStatsCommand) -> Result<()> {
    let project = Project::load(command.manifest_path.as_deref())?;
    let (constants, mut reports) = scan_project(&project)?;
    for report in &mut reports {
        resolve_workspace_constants(report, &constants);
        refresh_summary(report);
    }
    if command.candidates_only {
        reports.retain(|report| !report.suggested_return.is_empty());
    }
    reports.sort_by(|a, b| {
        b.suggested_return
            .is_empty()
            .cmp(&a.suggested_return.is_empty())
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    match command.format {
        ReturnStatsFormat::Text => emit_text(&project, &reports),
        ReturnStatsFormat::Json => emit_json(&project, &reports)?,
        ReturnStatsFormat::Csv => emit_csv(&project, &reports),
    }
    Ok(())
}

fn scan_project(project: &Project) -> Result<(ConstIndex, Vec<FunctionReport>)> {
    if project.rust_files.is_empty() {
        return Ok((ConstIndex::default(), Vec::new()));
    }
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(16)
        .min(project.rust_files.len());
    let chunk_size = project.rust_files.len().div_ceil(worker_count.max(1));
    let scans = std::thread::scope(|scope| {
        let handles = project
            .rust_files
            .chunks(chunk_size)
            .map(|paths| {
                scope.spawn(move || paths.iter().map(scan_file).collect::<Result<Vec<_>>>())
            })
            .collect::<Vec<_>>();
        let mut scans = Vec::with_capacity(project.rust_files.len());
        for handle in handles {
            scans.extend(
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("a return analysis worker panicked"))??,
            );
        }
        Ok::<_, anyhow::Error>(scans)
    })?;

    let mut index = ConstIndex::default();
    let mut reports = Vec::new();
    for (file, constants, file_reports) in scans {
        for (name, value) in constants {
            merge_value(&mut index.by_file, (file.clone(), name.clone()), value);
            merge_value(&mut index.global, name, value);
        }
        reports.extend(file_reports);
    }
    Ok((index, reports))
}

type FileScan = (Utf8PathBuf, Vec<(String, i128)>, Vec<FunctionReport>);

fn scan_file(path: &Utf8PathBuf) -> Result<FileScan> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read Rust source file {path}"))?;
    let tree = SourceFile::parse(&source, Edition::CURRENT).tree();
    let file = ParsedFile {
        path: path.clone(),
        source,
        tree,
    };
    let constants = constant_values(&file);
    let mut local = ConstIndex::default();
    for (name, value) in &constants {
        merge_value(&mut local.by_file, (path.clone(), name.clone()), *value);
        merge_value(&mut local.global, name.clone(), *value);
    }
    let reports = collect_reports(std::slice::from_ref(&file), &local);
    Ok((path.clone(), constants, reports))
}

fn constant_values(file: &ParsedFile) -> Vec<(String, i128)> {
    file.tree
        .syntax()
        .descendants()
        .filter_map(ast::Const::cast)
        .filter_map(|item| {
            let name = item.name()?;
            let expr = item.syntax().children().find_map(ast::Expr::cast)?;
            let value = parse_integer(&expr.syntax().text().to_string())?;
            Some((name.text().to_string(), value))
        })
        .collect()
}

fn merge_value<K: Ord>(map: &mut BTreeMap<K, Option<i128>>, key: K, value: i128) {
    map.entry(key)
        .and_modify(|old| {
            if old.is_some_and(|old| old != value) {
                *old = None;
            }
        })
        .or_insert(Some(value));
}

fn collect_reports(files: &[ParsedFile], constants: &ConstIndex) -> Vec<FunctionReport> {
    let mut reports = Vec::new();
    for file in files {
        for function in file.tree.syntax().descendants().filter_map(ast::Fn::cast) {
            let (Some(name), Some(body)) = (function.name(), function.body()) else {
                continue;
            };
            let (line, column) = line_column(
                &file.source,
                usize::from(name.syntax().text_range().start()),
            );
            let mut expressions = Vec::new();
            for returned in body
                .syntax()
                .descendants()
                .filter_map(ast::ReturnExpr::cast)
                .filter(|returned| return_belongs_to(returned, &function))
            {
                if let Some(expr) = returned.expr() {
                    collect_terminal_expressions(expr, &mut expressions);
                } else {
                    expressions.push(ast::Expr::ReturnExpr(returned));
                }
            }
            if let Some(tail) = body.stmt_list().and_then(|list| list.tail_expr()) {
                if !matches!(tail, ast::Expr::ReturnExpr(_)) {
                    collect_terminal_expressions(tail, &mut expressions);
                }
            }

            let mut evidence = expressions
                .into_iter()
                .map(|expr| classify_site(file, &expr, constants))
                .collect::<Vec<_>>();
            evidence.sort_by_key(|site| (site.line, site.column));
            evidence
                .dedup_by(|a, b| a.line == b.line && a.column == b.column && a.value == b.value);
            let declared_return_type = function
                .ret_type()
                .and_then(|ret| ret.ty())
                .map(|ty| normalize_space(&ty.syntax().text().to_string()))
                .unwrap_or_else(|| "()".to_owned());
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
            let mut report = FunctionReport {
                function: name.text().to_string(),
                file: file.path.clone(),
                line,
                column,
                kind,
                visibility: function
                    .visibility()
                    .map(|visibility| visibility.syntax().text().to_string())
                    .unwrap_or_default(),
                declared_return_type,
                suggested_return: String::new(),
                confidence: String::new(),
                reason: String::new(),
                known_return_values: Vec::new(),
                negative_sentinels: Vec::new(),
                null_return_forms: Vec::new(),
                unknown_return_values: Vec::new(),
                return_site_count: evidence.len(),
                evidence,
            };
            refresh_summary(&mut report);
            reports.push(report);
        }
    }
    reports
}

fn resolve_workspace_constants(report: &mut FunctionReport, constants: &ConstIndex) {
    for site in &mut report.evidence {
        if site.category != "symbolic" || site.resolved_value.is_some() {
            continue;
        }
        let compact = site.value.replace(char::is_whitespace, "");
        let Some(name) = compact.rsplit("::").next().filter(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        }) else {
            continue;
        };
        if let Some(value) = constants.get(&report.file, name) {
            site.resolved_value = Some(value);
            site.category = "integer";
        }
    }
}

fn refresh_summary(report: &mut FunctionReport) {
    (report.suggested_return, report.confidence, report.reason) = suggestion(
        &report.function,
        &report.declared_return_type,
        &report.evidence,
    );
    report.known_return_values = distinct_values(
        report
            .evidence
            .iter()
            .filter(|site| site.category != "unknown")
            .map(display_value),
    );
    report.negative_sentinels = distinct_values(
        report
            .evidence
            .iter()
            .filter(|site| site.resolved_value.is_some_and(|value| value < 0))
            .map(display_value),
    );
    report.null_return_forms = distinct_values(
        report
            .evidence
            .iter()
            .filter(|site| site.category == "null")
            .map(|site| site.value.clone()),
    );
    report.unknown_return_values = distinct_values(
        report
            .evidence
            .iter()
            .filter(|site| site.category == "unknown")
            .map(|site| site.value.clone()),
    );
    report.return_site_count = report.evidence.len();
}

fn return_belongs_to(returned: &ast::ReturnExpr, function: &ast::Fn) -> bool {
    for ancestor in returned.syntax().ancestors().skip(1) {
        if ast::ClosureExpr::cast(ancestor.clone()).is_some()
            || ast::BlockExpr::cast(ancestor.clone())
                .is_some_and(|block| block.async_token().is_some() || block.gen_token().is_some())
        {
            return false;
        }
        if let Some(owner) = ast::Fn::cast(ancestor) {
            return owner.syntax().text_range() == function.syntax().text_range();
        }
    }
    false
}

fn collect_terminal_expressions(expr: ast::Expr, output: &mut Vec<ast::Expr>) {
    match expr {
        ast::Expr::IfExpr(if_expr) => {
            if let Some(branch) = if_expr.then_branch() {
                collect_block_tail(branch, output);
            }
            match if_expr.else_branch() {
                Some(ElseBranch::Block(block)) => collect_block_tail(block, output),
                Some(ElseBranch::IfExpr(next)) => {
                    collect_terminal_expressions(ast::Expr::IfExpr(next), output)
                }
                None => output.push(ast::Expr::IfExpr(if_expr)),
            }
        }
        ast::Expr::MatchExpr(match_expr) => {
            if let Some(arms) = match_expr.match_arm_list() {
                for arm in arms.arms() {
                    if let Some(expr) = arm.expr() {
                        collect_terminal_expressions(expr, output);
                    }
                }
            }
        }
        ast::Expr::BlockExpr(block) => collect_block_tail(block, output),
        ast::Expr::ParenExpr(paren) => {
            if let Some(inner) = paren.expr() {
                collect_terminal_expressions(inner, output);
            }
        }
        ast::Expr::ReturnExpr(returned) => {
            if let Some(inner) = returned.expr() {
                collect_terminal_expressions(inner, output);
            }
        }
        other => output.push(other),
    }
}

fn collect_block_tail(block: ast::BlockExpr, output: &mut Vec<ast::Expr>) {
    match block.stmt_list().and_then(|list| list.tail_expr()) {
        Some(expr) => collect_terminal_expressions(expr, output),
        None => output.push(ast::Expr::BlockExpr(block)),
    }
}

fn classify_site(file: &ParsedFile, expr: &ast::Expr, constants: &ConstIndex) -> ReturnSite {
    let raw = normalize_space(&expr.syntax().text().to_string());
    let compact: String = raw
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let path_name = match expr {
        ast::Expr::PathExpr(_) => compact.rsplit("::").next(),
        _ => None,
    };
    let resolved_value =
        parse_integer(&raw).or_else(|| path_name.and_then(|name| constants.get(&file.path, name)));
    let category = if is_null_form(&compact) {
        "null"
    } else if resolved_value.is_some() {
        "integer"
    } else if matches!(compact.as_str(), "true" | "false") {
        "boolean"
    } else if compact == "()" || matches!(expr, ast::Expr::BlockExpr(_)) {
        "unit"
    } else if constructor(&compact, "Some") {
        "some"
    } else if compact == "None" {
        "none"
    } else if constructor(&compact, "Ok") {
        "ok"
    } else if constructor(&compact, "Err") {
        "err"
    } else if path_name.is_some() {
        "symbolic"
    } else {
        "unknown"
    };
    let (line, column) = line_column(
        &file.source,
        usize::from(expr.syntax().text_range().start()),
    );
    ReturnSite {
        value: truncate(&raw, 160),
        resolved_value,
        category,
        line,
        column,
    }
}

fn suggestion(
    function_name: &str,
    return_type: &str,
    sites: &[ReturnSite],
) -> (String, String, String) {
    let compact: String = return_type
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if compact.starts_with("Option<") || compact.starts_with("Result<") {
        return (
            String::new(),
            String::new(),
            "already_option_or_result".to_owned(),
        );
    }
    let nulls = sites.iter().filter(|site| site.category == "null").count();
    let negatives = sites
        .iter()
        .filter(|site| site.resolved_value.is_some_and(|value| value < 0))
        .count();
    let numeric_values = sites
        .iter()
        .filter_map(|site| site.resolved_value)
        .collect::<BTreeSet<_>>();
    let lower_name = function_name.to_ascii_lowercase();
    if numeric_values == BTreeSet::from([-1, 0, 1])
        && (lower_name.contains("compare") || lower_name.contains("cmp"))
    {
        return (
            String::new(),
            String::new(),
            "ordered_comparison".to_owned(),
        );
    }
    let other_values = sites
        .iter()
        .filter(|site| {
            site.category != "null"
                && !site.resolved_value.is_some_and(|value| value < 0)
                && site.category != "unit"
        })
        .count();
    if nulls > 0 {
        let confidence = if other_values > 0 { "high" } else { "medium" };
        let reason = if compact.starts_with("*mut") || compact.starts_with("*const") {
            "raw_pointer_returns_null"
        } else {
            "returns_null"
        };
        return (
            "Option".to_owned(),
            confidence.to_owned(),
            reason.to_owned(),
        );
    }
    if negatives > 0 {
        let confidence = if other_values > 0 { "high" } else { "medium" };
        return (
            "Result".to_owned(),
            confidence.to_owned(),
            "negative_error_sentinel".to_owned(),
        );
    }
    (String::new(), String::new(), String::new())
}

fn display_value(site: &ReturnSite) -> String {
    match site.resolved_value {
        Some(value) if site.value != value.to_string() => format!("{}={value}", site.value),
        _ => site.value.clone(),
    }
}

fn distinct_values(values: impl Iterator<Item = String>) -> Vec<String> {
    values.collect::<BTreeSet<_>>().into_iter().collect()
}

fn parse_integer(raw: &str) -> Option<i128> {
    let mut text: String = raw
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    loop {
        if text.starts_with('(') && text.ends_with(')') && encloses_whole_expression(&text) {
            text = text[1..text.len() - 1].to_owned();
        } else {
            break;
        }
    }
    if let Some((before, _)) = text.split_once("as") {
        text = before.to_owned();
    }
    let negative = text.starts_with('-');
    if negative || text.starts_with('+') {
        text.remove(0);
    }
    text.retain(|character| character != '_');
    let (radix, digits) = if let Some(rest) = text.strip_prefix("0x") {
        (16, rest)
    } else if let Some(rest) = text.strip_prefix("0o") {
        (8, rest)
    } else if let Some(rest) = text.strip_prefix("0b") {
        (2, rest)
    } else {
        (10, text.as_str())
    };
    let digit_len = digits
        .chars()
        .take_while(|character| character.is_digit(radix))
        .count();
    if digit_len == 0 {
        return None;
    }
    let suffix = &digits[digit_len..];
    if !suffix.is_empty()
        && !matches!(
            suffix,
            "i8" | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
        )
    {
        return None;
    }
    let value = i128::from_str_radix(&digits[..digit_len], radix).ok()?;
    Some(if negative { -value } else { value })
}

fn encloses_whole_expression(text: &str) -> bool {
    let mut depth = 0usize;
    for (index, character) in text.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 && index + character.len_utf8() != text.len() {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

fn is_null_form(compact: &str) -> bool {
    compact == "NULL"
        || compact == "null()"
        || compact == "null_mut()"
        || compact.ends_with("::null()")
        || compact.ends_with("::null_mut()")
        || compact.contains("::null::<")
        || compact.contains("::null_mut::<")
        || compact.starts_with("0as*mut")
        || compact.starts_with("0as*const")
}

fn constructor(compact: &str, name: &str) -> bool {
    compact.starts_with(&format!("{name}("))
        || compact.starts_with(&format!("{name}::<"))
        || compact.contains(&format!("::{name}("))
        || compact.contains(&format!("::{name}::<"))
}

fn normalize_space(text: &str) -> String {
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

fn emit_text(project: &Project, reports: &[FunctionReport]) {
    println!("SUGGEST  CONFIDENCE  FUNCTION  RETURN_TYPE  VALUES  LOCATION");
    for report in reports {
        let relative = report
            .file
            .strip_prefix(&project.root)
            .unwrap_or(&report.file);
        println!(
            "{:<8} {:<10} {:<28} {:<18} {:<28} {}:{}:{}",
            dash(&report.suggested_return),
            dash(&report.confidence),
            report.function,
            report.declared_return_type,
            dash(&report.known_return_values.join(";")),
            relative,
            report.line,
            report.column
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
            "candidate_count": reports.iter().filter(|row| !row.suggested_return.is_empty()).count(),
            "functions": rows,
        }))?
    );
    Ok(())
}

fn emit_csv(project: &Project, reports: &[FunctionReport]) {
    println!("function,file,line,column,kind,visibility,declared_return_type,suggested_return,confidence,reason,known_return_values,negative_sentinels,null_return_forms,unknown_return_values,return_site_count,evidence");
    for report in reports {
        let relative = report
            .file
            .strip_prefix(&project.root)
            .unwrap_or(&report.file);
        let evidence = report
            .evidence
            .iter()
            .map(|site| format!("{}:{}:{}", site.line, site.column, display_value(site)))
            .collect::<Vec<_>>()
            .join(";");
        let fields = [
            report.function.clone(),
            relative.to_string(),
            report.line.to_string(),
            report.column.to_string(),
            report.kind.to_owned(),
            report.visibility.clone(),
            report.declared_return_type.clone(),
            report.suggested_return.clone(),
            report.confidence.clone(),
            report.reason.clone(),
            report.known_return_values.join(";"),
            report.negative_sentinels.join(";"),
            report.null_return_forms.join(";"),
            report.unknown_return_values.join(";"),
            report.return_site_count.to_string(),
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

fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn dash(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}
