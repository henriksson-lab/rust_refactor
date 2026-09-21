//! Find enum-like constant families from how code compares values.
use anyhow::Result;
use camino::Utf8PathBuf;
use ra_ap_syntax::ast::{
    self, ArithOp, AstNode, BinaryOp, CmpOp, HasArgList, HasName, HasVisibility,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    analysis::{self, ParsedFile},
    cli::{ConstantsStatsFormat, ConstantsToEnumStatsCommand},
    project::Project,
};

const MIN_AUTO_ALIAS_GROUP_SIZE: usize = 5;
const MAX_AUTO_ALIAS_LINE_GAP: usize = 5;

#[derive(Clone, Debug)]
struct ConstDef {
    name: String,
    file: Utf8PathBuf,
    line: usize,
    column: usize,
    raw_type: Option<String>,
    value: Option<String>,
    kind: &'static str,
    visibility: String,
    used_bitwise: bool,
    used_ordering: bool,
}

#[derive(Clone, Debug)]
struct FunctionDef {
    file: Utf8PathBuf,
    offset: usize,
    name: String,
    parameters: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct Evidence {
    kind: &'static str,
    file: Utf8PathBuf,
    scope: String,
    subject: String,
    line: usize,
    column: usize,
    constants: BTreeSet<String>,
}

#[derive(Clone, Debug, Default)]
struct Group {
    file: Utf8PathBuf,
    scope: String,
    subject: String,
    constants: BTreeMap<String, ConstDef>,
    evidence: Vec<Evidence>,
}

pub fn run(command: ConstantsToEnumStatsCommand) -> Result<()> {
    let project = Project::load(command.manifest_path.as_deref())?;
    let files = analysis::parse_project_files(&project)?;
    let mut inventory = collect_definitions(&files);
    mark_bitwise_constants(&files, &mut inventory);
    mark_ordering_constants(&files, &mut inventory);
    let definitions = enum_candidate_definitions(&inventory);
    let functions = collect_functions(&files);
    let observations = collect_groups(&files, &definitions, &functions);
    let mut groups = coalesce_families(split_dispatcher_observations(observations))
        .into_iter()
        .flat_map(split_dispatcher_observation)
        .collect::<Vec<_>>();
    // Only emit groups that can represent mutually exclusive enum variants.
    // Bit flags, numeric bounds, and mixed families remain in the complete CSV
    // inventory as unassigned constants, so a caller cannot accidentally apply
    // them as an enum group.
    groups.retain(|group| {
        group.constants.len() >= command.min_constants && group_kind(group) == "enum"
    });
    groups.sort_by(|a, b| {
        b.constants
            .len()
            .cmp(&a.constants.len())
            .then_with(|| b.evidence.len().cmp(&a.evidence.len()))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.scope.cmp(&b.scope))
            .then_with(|| a.subject.cmp(&b.subject))
    });

    match command.format {
        ConstantsStatsFormat::Json => emit_json(&project, &groups),
        ConstantsStatsFormat::Text => emit_text(&project, &groups),
        ConstantsStatsFormat::Csv => emit_csv(&project, &groups, &inventory),
    }
    Ok(())
}

fn collect_definitions(files: &[ParsedFile]) -> BTreeMap<String, Vec<ConstDef>> {
    let mut result: BTreeMap<String, Vec<ConstDef>> = BTreeMap::new();
    for file in files {
        for item in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::Const::cast)
        {
            let kind = if item
                .syntax()
                .ancestors()
                .skip(1)
                .any(|node| ast::Fn::cast(node).is_some())
            {
                "local"
            } else if item
                .syntax()
                .ancestors()
                .skip(1)
                .any(|node| ast::Impl::cast(node).is_some())
            {
                "associated"
            } else if item
                .syntax()
                .ancestors()
                .skip(1)
                .any(|node| ast::Trait::cast(node).is_some())
            {
                "trait"
            } else {
                "module"
            };
            let Some(name_node) = item.name() else {
                continue;
            };
            let name = name_node.text().to_string();
            let (line, column) =
                line_column(&file.source, name_node.syntax().text_range().start().into());
            let raw_type = item
                .ty()
                .map(|ty| ty.syntax().text().to_string().trim().to_owned());
            let value = item
                .syntax()
                .children()
                .find_map(ast::Expr::cast)
                .map(|expr| expr.syntax().text().to_string().trim().to_owned());
            let visibility = item
                .visibility()
                .map(|visibility| visibility.syntax().text().to_string())
                .unwrap_or_default();
            result.entry(name.clone()).or_default().push(ConstDef {
                name,
                file: file.path.clone(),
                line,
                column,
                raw_type,
                value,
                kind,
                visibility,
                used_bitwise: false,
                used_ordering: false,
            });
        }
    }
    result
}

fn numeric_module_definitions(
    inventory: &BTreeMap<String, Vec<ConstDef>>,
) -> BTreeMap<String, Vec<ConstDef>> {
    inventory
        .iter()
        .filter_map(|(name, definitions)| {
            let selected: Vec<_> = definitions
                .iter()
                .filter(|item| {
                    item.kind == "module" && item.raw_type.as_deref().is_some_and(is_integer_type)
                })
                .cloned()
                .collect();
            (!selected.is_empty()).then(|| (name.clone(), selected))
        })
        .collect()
}

fn enum_candidate_definitions(
    inventory: &BTreeMap<String, Vec<ConstDef>>,
) -> BTreeMap<String, Vec<ConstDef>> {
    numeric_module_definitions(inventory)
        .into_iter()
        .filter_map(|(name, definitions)| {
            let selected = definitions
                .into_iter()
                .filter(|item| !is_bitwise_constant(item) && !item.used_ordering)
                .collect::<Vec<_>>();
            (!selected.is_empty()).then_some((name, selected))
        })
        .collect()
}

fn collect_functions(files: &[ParsedFile]) -> BTreeMap<String, Vec<FunctionDef>> {
    let mut result: BTreeMap<String, Vec<FunctionDef>> = BTreeMap::new();
    for file in files {
        for function in file.tree.syntax().descendants().filter_map(ast::Fn::cast) {
            if function.syntax().ancestors().skip(1).any(|node| {
                ast::Impl::cast(node.clone()).is_some() || ast::Trait::cast(node).is_some()
            }) {
                continue;
            }
            let (Some(name), Some(parameters)) = (function.name(), function.param_list()) else {
                continue;
            };
            let parameters = parameters
                .params()
                .map(|parameter| {
                    parameter
                        .pat()
                        .map(|pattern| normalize(pattern.syntax().text().to_string()))
                        .unwrap_or_default()
                })
                .collect();
            let definition = FunctionDef {
                file: file.path.clone(),
                offset: function.syntax().text_range().start().into(),
                name: name.text().to_string(),
                parameters,
            };
            result
                .entry(definition.name.clone())
                .or_default()
                .push(definition);
        }
    }
    result
}

fn mark_bitwise_constants(files: &[ParsedFile], inventory: &mut BTreeMap<String, Vec<ConstDef>>) {
    let definitions = numeric_module_definitions(inventory);
    let mut used = BTreeSet::new();
    for file in files {
        for binary in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::BinExpr::cast)
        {
            let bitwise = matches!(
                binary.op_kind(),
                Some(BinaryOp::ArithOp(
                    ArithOp::BitAnd | ArithOp::BitOr | ArithOp::BitXor
                )) | Some(BinaryOp::Assignment {
                    op: Some(ArithOp::BitAnd | ArithOp::BitOr | ArithOp::BitXor)
                })
            );
            if !bitwise {
                continue;
            }
            for path in binary
                .syntax()
                .descendants()
                .filter_map(ast::PathExpr::cast)
            {
                let expression = ast::Expr::PathExpr(path);
                if let Some(definition) = resolve_expr(&expression, &file.path, &definitions) {
                    used.insert(const_id(&definition));
                }
            }
        }
    }
    for definition in inventory.values_mut().flatten() {
        definition.used_bitwise = used.contains(&const_id(definition));
    }
}

fn mark_ordering_constants(files: &[ParsedFile], inventory: &mut BTreeMap<String, Vec<ConstDef>>) {
    let definitions = numeric_module_definitions(inventory);
    let mut used = BTreeSet::new();
    for file in files {
        for comparison in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::BinExpr::cast)
        {
            if !matches!(
                comparison.op_kind(),
                Some(BinaryOp::CmpOp(CmpOp::Ord { .. }))
            ) {
                continue;
            }
            let (Some(lhs), Some(rhs)) = (comparison.lhs(), comparison.rhs()) else {
                continue;
            };
            for expression in [lhs, rhs] {
                if let Some(definition) = resolve_expr(&expression, &file.path, &definitions) {
                    used.insert(const_id(&definition));
                }
            }
        }
    }
    for definition in inventory.values_mut().flatten() {
        definition.used_ordering = used.contains(&const_id(definition));
    }
}

fn path_expr_name(expr: &ast::Expr) -> Option<String> {
    let ast::Expr::PathExpr(path) = expr else {
        return None;
    };
    Some(path.path()?.segment()?.name_ref()?.text().to_string())
}

fn is_generic_wrapper(path: &str, name: &str) -> bool {
    matches!(name, "Some" | "None" | "Ok" | "Err")
        || [
            "Box::new",
            "Rc::new",
            "Arc::new",
            "Vec::from",
            "String::from",
            "Cow::from",
            "Option::from",
            "Result::from",
            "Into::into",
            "Default::default",
        ]
        .iter()
        .any(|wrapper| path == *wrapper || path.ends_with(&format!("::{wrapper}")))
}

fn collect_groups(
    files: &[ParsedFile],
    definitions: &BTreeMap<String, Vec<ConstDef>>,
    functions: &BTreeMap<String, Vec<FunctionDef>>,
) -> Vec<Group> {
    let mut groups: BTreeMap<(Utf8PathBuf, usize, String), Group> = BTreeMap::new();
    for file in files {
        for matched in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::MatchExpr::cast)
        {
            let Some(subject_expr) = matched.expr() else {
                continue;
            };
            let subject = subject_identity(&subject_expr, file);
            let Some((scope_offset, scope)) = containing_scope(matched.syntax()) else {
                continue;
            };
            let mut names = BTreeSet::new();
            if let Some(arms) = matched.match_arm_list() {
                for path in arms.syntax().descendants().filter_map(ast::PathPat::cast) {
                    if let Some(name) = path
                        .path()
                        .and_then(|path| path.segment())
                        .and_then(|seg| seg.name_ref())
                    {
                        names.insert(name.text().to_string());
                    }
                }
                // Bare constants and new bindings share the same syntax here.
                // A known module constant is useful discovery evidence; the
                // conversion command performs the semantic validation later.
                for binding in arms.syntax().descendants().filter_map(ast::IdentPat::cast) {
                    if let Some(name) = binding.name() {
                        let text = name.text().to_string();
                        if definitions.contains_key(&text) {
                            names.insert(text);
                        }
                    }
                }
            }
            let resolved = resolve_names(&names, &file.path, definitions);
            if resolved.is_empty() {
                continue;
            }
            let offset: usize = matched.syntax().text_range().start().into();
            let (line, column) = line_column(&file.source, offset);
            add_evidence(
                &mut groups,
                file,
                scope_offset,
                scope,
                subject,
                "match",
                line,
                column,
                resolved,
            );
        }

        for comparison in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::BinExpr::cast)
        {
            if !matches!(
                comparison.op_kind(),
                Some(BinaryOp::CmpOp(CmpOp::Eq { .. }))
            ) {
                continue;
            }
            let (Some(lhs), Some(rhs)) = (comparison.lhs(), comparison.rhs()) else {
                continue;
            };
            let lhs_const = resolve_expr(&lhs, &file.path, definitions);
            let rhs_const = resolve_expr(&rhs, &file.path, definitions);
            let (subject_expr, constant) = match (lhs_const, rhs_const) {
                (Some(_), Some(_)) | (None, None) => continue,
                (Some(definition), None) => (rhs, definition),
                (None, Some(definition)) => (lhs, definition),
            };
            let Some((scope_offset, scope)) = containing_scope(comparison.syntax()) else {
                continue;
            };
            let offset: usize = comparison.syntax().text_range().start().into();
            let (line, column) = line_column(&file.source, offset);
            add_evidence(
                &mut groups,
                file,
                scope_offset,
                scope,
                subject_identity(&subject_expr, file),
                "comparison",
                line,
                column,
                vec![constant],
            );
        }

        for statement in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::LetStmt::cast)
        {
            let (Some(pattern), Some(initializer)) = (statement.pat(), statement.initializer())
            else {
                continue;
            };
            let ast::Pat::IdentPat(binding) = pattern else {
                continue;
            };
            let Some(name) = binding.name() else { continue };
            if let Some(constant) = resolve_expr(&initializer, &file.path, definitions) {
                record_single_use(
                    &mut groups,
                    file,
                    statement.syntax(),
                    format!(
                        "{}\0binding@{}",
                        name.text(),
                        usize::from(name.syntax().text_range().start())
                    ),
                    "assignment",
                    constant,
                );
            }
        }
        for assignment in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::BinExpr::cast)
        {
            if !matches!(
                assignment.op_kind(),
                Some(BinaryOp::Assignment { op: None })
            ) {
                continue;
            }
            let (Some(lhs), Some(rhs)) = (assignment.lhs(), assignment.rhs()) else {
                continue;
            };
            if let Some(constant) = resolve_expr(&rhs, &file.path, definitions) {
                record_single_use(
                    &mut groups,
                    file,
                    assignment.syntax(),
                    subject_identity(&lhs, file),
                    "assignment",
                    constant,
                );
            }
        }

        for returned in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::ReturnExpr::cast)
        {
            let Some(expr) = returned.expr() else {
                continue;
            };
            if let Some(constant) = resolve_expr(&expr, &file.path, definitions) {
                record_single_use(
                    &mut groups,
                    file,
                    returned.syntax(),
                    "return".into(),
                    "return",
                    constant,
                );
            }
        }
        for function in file.tree.syntax().descendants().filter_map(ast::Fn::cast) {
            let Some(tail) = function.body().and_then(|body| body.tail_expr()) else {
                continue;
            };
            if let Some(constant) = resolve_expr(&tail, &file.path, definitions) {
                record_single_use(
                    &mut groups,
                    file,
                    tail.syntax(),
                    "return".into(),
                    "return",
                    constant,
                );
            }
        }

        for call in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::CallExpr::cast)
        {
            let Some(callee) = call.expr() else { continue };
            if !matches!(callee, ast::Expr::PathExpr(_)) {
                continue;
            }
            let Some(arguments) = call.arg_list() else {
                continue;
            };
            let arity = arguments.args().count();
            let callee_text = normalize(callee.syntax().text().to_string());
            let Some(callee_name) = path_expr_name(&callee) else {
                continue;
            };
            if is_generic_wrapper(&callee_text, &callee_name) {
                continue;
            }
            let Some(candidates) = functions.get(&callee_name) else {
                continue;
            };
            let matching: Vec<_> = candidates
                .iter()
                .filter(|function| function.parameters.len() == arity)
                .collect();
            let [function] = matching.as_slice() else {
                continue;
            };
            for (index, argument) in arguments.args().enumerate() {
                let Some(constant) = resolve_expr(&argument, &file.path, definitions) else {
                    continue;
                };
                let parameter = function.parameters.get(index).cloned().unwrap_or_default();
                let subject = format!("parameter {} ({})", index + 1, parameter);
                let offset: usize = argument.syntax().text_range().start().into();
                let (line, column) = line_column(&file.source, offset);
                add_evidence_at(
                    &mut groups,
                    file,
                    function.file.clone(),
                    function.offset,
                    format!("function {}", function.name),
                    subject,
                    "argument",
                    line,
                    column,
                    vec![constant],
                );
            }
        }
    }
    groups.into_values().collect()
}

fn record_single_use(
    groups: &mut BTreeMap<(Utf8PathBuf, usize, String), Group>,
    file: &ParsedFile,
    syntax: &ra_ap_syntax::SyntaxNode,
    subject: String,
    kind: &'static str,
    constant: ConstDef,
) {
    let Some((scope_offset, scope)) = containing_scope(syntax) else {
        return;
    };
    let offset: usize = syntax.text_range().start().into();
    let (line, column) = line_column(&file.source, offset);
    add_evidence(
        groups,
        file,
        scope_offset,
        scope,
        subject,
        kind,
        line,
        column,
        vec![constant],
    );
}

#[allow(clippy::too_many_arguments)]
fn add_evidence(
    groups: &mut BTreeMap<(Utf8PathBuf, usize, String), Group>,
    file: &ParsedFile,
    scope_offset: usize,
    scope: String,
    subject: String,
    kind: &'static str,
    line: usize,
    column: usize,
    definitions: Vec<ConstDef>,
) {
    add_evidence_at(
        groups,
        file,
        file.path.clone(),
        scope_offset,
        scope,
        subject,
        kind,
        line,
        column,
        definitions,
    );
}

#[allow(clippy::too_many_arguments)]
fn add_evidence_at(
    groups: &mut BTreeMap<(Utf8PathBuf, usize, String), Group>,
    file: &ParsedFile,
    key_file: Utf8PathBuf,
    scope_offset: usize,
    scope: String,
    subject: String,
    kind: &'static str,
    line: usize,
    column: usize,
    definitions: Vec<ConstDef>,
) {
    let key = (key_file.clone(), scope_offset, subject.clone());
    let shown_subject = subject.split('\0').next().unwrap_or(&subject).to_owned();
    let group = groups.entry(key).or_insert_with(|| Group {
        file: key_file,
        scope,
        subject: shown_subject,
        ..Group::default()
    });
    let names = definitions.iter().map(|item| item.name.clone()).collect();
    for definition in definitions {
        group
            .constants
            .entry(const_id(&definition))
            .or_insert(definition);
    }
    group.evidence.push(Evidence {
        kind,
        file: file.path.clone(),
        scope: group.scope.clone(),
        subject: group.subject.clone(),
        line,
        column,
        constants: names,
    });
}

fn coalesce_families(groups: Vec<Group>) -> Vec<Group> {
    // Each observation is a set. A constant appearing in two observations is
    // an edge between them, so union-find gives the transitive components in
    // almost linear time without repeatedly comparing whole sets.
    let mut parents: Vec<usize> = (0..groups.len()).collect();
    let mut ranks = vec![0_u8; groups.len()];
    let mut first_observation: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, group) in groups.iter().enumerate() {
        for constant in group.constants.keys() {
            if let Some(previous) = first_observation.insert(constant, index) {
                union_components(&mut parents, &mut ranks, index, previous);
            }
        }
    }

    let roots: Vec<_> = (0..groups.len())
        .map(|index| find_component(&mut parents, index))
        .collect();
    let mut components: BTreeMap<usize, Group> = BTreeMap::new();
    for (root, group) in roots.into_iter().zip(groups) {
        if let Some(family) = components.get_mut(&root) {
            for (name, definition) in group.constants {
                family.constants.entry(name).or_insert(definition);
            }
            family.evidence.extend(group.evidence);
        } else {
            components.insert(root, group);
        }
    }
    let mut families: Vec<_> = components.into_values().collect();
    for family in &mut families {
        family
            .evidence
            .sort_by(|a, b| (&a.file, a.line, a.column).cmp(&(&b.file, b.line, b.column)));
    }
    families
}

fn split_dispatcher_observations(groups: Vec<Group>) -> Vec<Group> {
    groups
        .into_iter()
        .flat_map(split_dispatcher_observation)
        .collect()
}

fn split_dispatcher_observation(group: Group) -> Vec<Group> {
    if group.constants.len() < 4 {
        return vec![group];
    }
    let tokenized: Vec<_> = group
        .constants
        .iter()
        .map(|(id, definition)| (id, definition.name.split('_').collect::<Vec<_>>()))
        .collect();
    let buckets = (1..=3).find_map(|depth| {
        let mut buckets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (id, tokens) in &tokenized {
            if tokens.len() <= depth {
                continue;
            }
            buckets
                .entry(tokens[..depth].join("_"))
                .or_default()
                .insert((*id).clone());
        }
        let strong = buckets
            .into_iter()
            .filter(|(_, members)| members.len() >= 2)
            .collect::<BTreeMap<_, _>>();
        (strong.len() >= 2).then_some(strong)
    });
    let Some(buckets) = buckets else {
        return vec![group];
    };
    let assigned = buckets.values().flatten().cloned().collect::<BTreeSet<_>>();
    let remainder = group
        .constants
        .keys()
        .filter(|id| !assigned.contains(*id))
        .cloned()
        .collect::<BTreeSet<_>>();
    buckets
        .into_values()
        .chain((!remainder.is_empty()).then_some(remainder))
        .map(|members| {
            let constants = group
                .constants
                .iter()
                .filter(|(id, _)| members.contains(*id))
                .map(|(id, definition)| (id.clone(), definition.clone()))
                .collect::<BTreeMap<_, _>>();
            let names = constants
                .values()
                .map(|definition| definition.name.clone())
                .collect::<BTreeSet<_>>();
            let evidence = group
                .evidence
                .iter()
                .cloned()
                .map(|mut evidence| {
                    evidence.constants.retain(|name| names.contains(name));
                    evidence
                })
                .collect();
            Group {
                constants,
                evidence,
                ..group.clone()
            }
        })
        .collect()
}

fn find_component(parents: &mut [usize], item: usize) -> usize {
    if parents[item] != item {
        parents[item] = find_component(parents, parents[item]);
    }
    parents[item]
}

fn union_components(parents: &mut [usize], ranks: &mut [u8], left: usize, right: usize) {
    let left_root = find_component(parents, left);
    let right_root = find_component(parents, right);
    if left_root == right_root {
        return;
    }
    match ranks[left_root].cmp(&ranks[right_root]) {
        std::cmp::Ordering::Less => parents[left_root] = right_root,
        std::cmp::Ordering::Greater => parents[right_root] = left_root,
        std::cmp::Ordering::Equal => {
            parents[right_root] = left_root;
            ranks[left_root] += 1;
        }
    }
}

fn const_id(definition: &ConstDef) -> String {
    format!("{}\0{}", definition.file, definition.name)
}

fn declaration_id(definition: &ConstDef) -> String {
    format!(
        "{}\0{}\0{}\0{}",
        definition.file, definition.line, definition.column, definition.name
    )
}

fn is_integer_type(raw: &str) -> bool {
    matches!(
        raw,
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize"
    )
}

fn resolve_expr(
    expr: &ast::Expr,
    use_file: &Utf8PathBuf,
    definitions: &BTreeMap<String, Vec<ConstDef>>,
) -> Option<ConstDef> {
    match expr {
        ast::Expr::PathExpr(path_expr) => {
            let name = path_expr.path()?.segment()?.name_ref()?.text().to_string();
            resolve_name(&name, use_file, definitions)
        }
        ast::Expr::ParenExpr(parenthesized) => {
            resolve_expr(&parenthesized.expr()?, use_file, definitions)
        }
        ast::Expr::CastExpr(cast) => resolve_expr(&cast.expr()?, use_file, definitions),
        _ => None,
    }
}

fn resolve_names(
    names: &BTreeSet<String>,
    use_file: &Utf8PathBuf,
    definitions: &BTreeMap<String, Vec<ConstDef>>,
) -> Vec<ConstDef> {
    names
        .iter()
        .filter_map(|name| resolve_name(name, use_file, definitions))
        .collect()
}

fn resolve_name(
    name: &str,
    use_file: &Utf8PathBuf,
    definitions: &BTreeMap<String, Vec<ConstDef>>,
) -> Option<ConstDef> {
    let candidates = definitions.get(name)?;
    let local: Vec<_> = candidates
        .iter()
        .filter(|item| &item.file == use_file)
        .collect();
    if local.len() == 1 {
        return Some(local[0].clone());
    }
    (candidates.len() == 1).then(|| candidates[0].clone())
}

fn containing_scope(node: &ra_ap_syntax::SyntaxNode) -> Option<(usize, String)> {
    if let Some(function) = node.ancestors().find_map(ast::Fn::cast) {
        let offset = function.syntax().text_range().start().into();
        let name = function
            .name()
            .map(|name| name.text().to_string())
            .unwrap_or_else(|| "<function>".into());
        Some((offset, name))
    } else {
        Some((0, "<module>".into()))
    }
}

fn subject_identity(expr: &ast::Expr, file: &ParsedFile) -> String {
    if let ast::Expr::FieldExpr(field) = expr {
        let receiver = field
            .expr()
            .map(|expr| subject_identity(&expr, file))
            .unwrap_or_else(|| "<receiver>".into());
        let field_name = field
            .name_ref()
            .map(|name| name.text().to_string())
            .or_else(|| field.index_token().map(|token| token.text().to_string()))
            .unwrap_or_else(|| "?".into());
        let shown_receiver = receiver.split('\0').next().unwrap_or(&receiver);
        return format!("{shown_receiver}.{field_name}\0{receiver}.{field_name}");
    }
    let ast::Expr::PathExpr(path) = expr else {
        return normalize(expr.syntax().text().to_string());
    };
    let Some(name) = path
        .path()
        .and_then(|path| path.segment())
        .and_then(|segment| segment.name_ref())
    else {
        return normalize(expr.syntax().text().to_string());
    };
    let name_text = name.text().to_string();
    let use_offset: usize = expr.syntax().text_range().start().into();
    let block_ranges: Vec<_> = expr
        .syntax()
        .ancestors()
        .filter_map(ast::BlockExpr::cast)
        .map(|block| block.syntax().text_range())
        .collect();
    if let Some(function) = expr.syntax().ancestors().find_map(ast::Fn::cast) {
        let mut bindings = function
            .syntax()
            .descendants()
            .filter_map(ast::LetStmt::cast)
            .filter_map(|statement| {
                let ast::Pat::IdentPat(binding) = statement.pat()? else {
                    return None;
                };
                let binding_name = binding.name()?;
                let offset: usize = binding_name.syntax().text_range().start().into();
                let declaration_block = statement
                    .syntax()
                    .ancestors()
                    .find_map(ast::BlockExpr::cast)?;
                (binding_name.text() == name_text
                    && offset < use_offset
                    && block_ranges.contains(&declaration_block.syntax().text_range()))
                .then_some(offset)
            })
            .collect::<Vec<_>>();
        bindings.sort_unstable();
        if let Some(offset) = bindings.last() {
            return format!("{name_text}\0binding@{offset}");
        }
        if let Some(parameter) = function
            .param_list()
            .into_iter()
            .flat_map(|parameters| parameters.params())
            .find(|parameter| {
                parameter.pat().is_some_and(|pattern| {
                    normalize(pattern.syntax().text().to_string()) == name_text
                })
            })
        {
            return format!(
                "{name_text}\0parameter@{}",
                usize::from(parameter.syntax().text_range().start())
            );
        }
    }
    format!("{name_text}\0{}::{name_text}", file.path)
}

fn normalize(text: String) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset];
    (
        before.bytes().filter(|byte| *byte == b'\n').count() + 1,
        before.rsplit('\n').next().unwrap_or("").chars().count() + 1,
    )
}

fn relative<'a>(project: &'a Project, path: &'a Utf8PathBuf) -> &'a camino::Utf8Path {
    path.strip_prefix(&project.root).unwrap_or(path)
}

fn emit_json(project: &Project, groups: &[Group]) {
    let values: Vec<_> = groups.iter().map(|group| {
        let definition_files: BTreeSet<_> = group.constants.values().map(|item| relative(project, &item.file).to_string()).collect();
        let raw_types: BTreeSet<_> = group.constants.values().filter_map(|item| item.raw_type.clone()).collect();
        let subjects: BTreeSet<_> = group.evidence.iter().map(|site| site.subject.clone()).collect();
        let scopes: BTreeSet<_> = group.evidence.iter().map(|site| site.scope.clone()).collect();
        json!({
            "constant_count": group.constants.len(),
            "group_kind": group_kind(group),
            "subject": group.subject,
            "scope": group.scope,
            "subjects": subjects,
            "scopes": scopes,
            "file": relative(project, &group.file),
            "definition_file": (definition_files.len() == 1).then(|| definition_files.first().cloned()).flatten(),
            "shared_raw_type": (raw_types.len() == 1).then(|| raw_types.first().cloned()).flatten(),
            "constants": group.constants.values().map(|item| json!({
                "name": item.name, "type": item.raw_type, "value": item.value,
                "file": relative(project, &item.file), "line": item.line, "column": item.column
            })).collect::<Vec<_>>(),
            "evidence": group.evidence.iter().map(|site| json!({
                "kind": site.kind, "file": relative(project, &site.file), "line": site.line,
                "column": site.column, "selection": format!("{}:{}:{}", relative(project, &site.file), site.line, site.column),
                "scope": site.scope, "subject": site.subject, "constants": site.constants
            })).collect::<Vec<_>>()
        })
    }).collect();
    println!("{}", json!({"group_count": groups.len(), "groups": values}));
}

fn emit_text(project: &Project, groups: &[Group]) {
    println!("Found {} candidate groups.", groups.len());
    println!("CONSTANTS  KIND   MATCHES  OTHER_SITES  SCOPE  SUBJECT  FILE");
    for group in groups {
        let matches = group
            .evidence
            .iter()
            .filter(|site| site.kind == "match")
            .count();
        let comparisons = group.evidence.len() - matches;
        println!(
            "{:<10} {:<6} {:<8} {:<12} {:<24} {:<24} {}",
            group.constants.len(),
            group_kind(group),
            matches,
            comparisons,
            group.scope,
            group.subject,
            relative(project, &group.file)
        );
        println!(
            "          {}",
            group
                .constants
                .values()
                .map(|item| item.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn emit_csv(project: &Project, groups: &[Group], inventory: &BTreeMap<String, Vec<ConstDef>>) {
    println!("constant_name,file,line,column,kind,type,value,proposed_enum_group,proposed_group_kind,proposed_enum_name,proposed_variant,alias_of,possible_alias_of,evidence_count,subjects,match_selections,comparison_selections,evidence_selections,possible_enum_groups,unassigned_reason,duplicate_kind,duplicate_group,canonical_candidate,duplicate_name_conflict");
    let mut assignments: BTreeMap<String, CsvProposal> = BTreeMap::new();
    let mut group_prefixes = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        let group_id = format!("group_{:03}", index + 1);
        let enum_name = suggested_enum_name(group);
        let subjects = group
            .evidence
            .iter()
            .map(|site| format!("{}::{}", site.scope, site.subject))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(";");
        let selections = group
            .evidence
            .iter()
            .map(|site| {
                format!(
                    "{}:{}:{}",
                    relative(project, &site.file),
                    site.line,
                    site.column
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        let matches = group
            .evidence
            .iter()
            .filter(|site| site.kind == "match")
            .map(|site| {
                format!(
                    "{}:{}:{}",
                    relative(project, &site.file),
                    site.line,
                    site.column
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        let comparisons = group
            .evidence
            .iter()
            .filter(|site| site.kind == "comparison")
            .map(|site| {
                format!(
                    "{}:{}:{}",
                    relative(project, &site.file),
                    site.line,
                    site.column
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        let prefix = common_name_prefix(group);
        if !prefix.is_empty() {
            let prefix_text = prefix.join("_");
            let types: BTreeSet<_> = group
                .constants
                .values()
                .filter_map(|item| item.raw_type.clone())
                .collect();
            let files: BTreeSet<_> = group
                .constants
                .values()
                .map(|item| item.file.clone())
                .collect();
            if types.len() == 1 && files.len() == 1 {
                group_prefixes.push((
                    files.first().unwrap().clone(),
                    types.first().unwrap().clone(),
                    prefix_text,
                    group_id.clone(),
                ));
            }
        }
        let mut canonical_values: BTreeMap<
            (Option<String>, Option<String>),
            Vec<(String, String)>,
        > = BTreeMap::new();
        for definition in group.constants.values() {
            let proposed_variant = suggested_variant(&definition.name, &prefix);
            let value_key = (definition.raw_type.clone(), definition.value.clone());
            let (variant, alias_of) = if definition.value.is_some() {
                let compatible = canonical_values.get(&value_key).and_then(|candidates| {
                    candidates.iter().find(|(canonical_name, _)| {
                        alias_names_compatible(canonical_name, &definition.name)
                    })
                });
                if let Some((canonical_name, canonical_variant)) = compatible {
                    (canonical_variant.clone(), canonical_name.clone())
                } else {
                    canonical_values
                        .entry(value_key)
                        .or_default()
                        .push((definition.name.clone(), proposed_variant.clone()));
                    (proposed_variant, String::new())
                }
            } else {
                (proposed_variant, String::new())
            };
            assignments.insert(
                const_id(definition),
                CsvProposal {
                    group: group_id.clone(),
                    group_kind: group_kind(group).to_owned(),
                    enum_name: enum_name.clone(),
                    variant,
                    alias_of,
                    evidence_count: group.evidence.len().to_string(),
                    subjects: subjects.clone(),
                    matches: matches.clone(),
                    comparisons: comparisons.clone(),
                    selections: selections.clone(),
                },
            );
        }
    }
    let possible_aliases = merge_equal_value_aliases(inventory, &mut assignments);
    let duplicates = classify_duplicate_declarations(project, inventory, &assignments);

    let lexical_counts = lexical_prefix_counts(inventory);

    let mut constants: Vec<_> = inventory.values().flatten().collect();
    constants.sort_by(|a, b| (&a.file, a.line, a.column).cmp(&(&b.file, b.line, b.column)));
    for item in constants {
        let empty = CsvProposal::default();
        let assignment = assignments.get(&const_id(item)).unwrap_or(&empty);
        let duplicate = duplicates
            .get(&declaration_id(item))
            .cloned()
            .unwrap_or_default();
        let (possible_groups, reason) = if assignment.group.is_empty() {
            (
                possible_groups(item, &group_prefixes, &lexical_counts),
                unassigned_reason(item).to_owned(),
            )
        } else {
            (String::new(), String::new())
        };
        let fields = [
            item.name.clone(),
            relative(project, &item.file).to_string(),
            item.line.to_string(),
            item.column.to_string(),
            item.kind.to_owned(),
            item.raw_type.clone().unwrap_or_default(),
            item.value.clone().unwrap_or_default(),
            assignment.group.clone(),
            assignment.group_kind.clone(),
            assignment.enum_name.clone(),
            assignment.variant.clone(),
            assignment.alias_of.clone(),
            possible_aliases
                .get(&const_id(item))
                .cloned()
                .unwrap_or_default(),
            assignment.evidence_count.clone(),
            assignment.subjects.clone(),
            assignment.matches.clone(),
            assignment.comparisons.clone(),
            assignment.selections.clone(),
            possible_groups,
            reason,
            duplicate.kind,
            duplicate.group,
            duplicate.canonical_candidate,
            duplicate.name_conflict,
        ];
        println!("{}", fields.map(|field| csv_field(&field)).join(","));
    }
}

#[derive(Clone, Default)]
struct DuplicateProposal {
    kind: String,
    group: String,
    canonical_candidate: String,
    name_conflict: String,
}

/// Find copied declarations without conflating them with different constants that happen to
/// have the same numeric value. A name may contain one repeated compatible cluster and other,
/// conflicting declarations; `duplicate_name_conflict` preserves that warning on every row.
fn classify_duplicate_declarations(
    project: &Project,
    inventory: &BTreeMap<String, Vec<ConstDef>>,
    assignments: &BTreeMap<String, CsvProposal>,
) -> BTreeMap<String, DuplicateProposal> {
    let mut result = BTreeMap::new();
    let mut next_group = 1_usize;
    for definitions in inventory.values().filter(|items| items.len() > 1) {
        let mut clusters: BTreeMap<(Option<String>, Option<String>), Vec<&ConstDef>> =
            BTreeMap::new();
        for definition in definitions {
            clusters
                .entry((
                    definition.raw_type.clone(),
                    normalized_duplicate_value(definition),
                ))
                .or_default()
                .push(definition);
        }
        let has_name_conflict = clusters.len() > 1;
        for cluster in clusters.values().filter(|cluster| cluster.len() > 1) {
            let group = format!("duplicate_{next_group:03}");
            next_group += 1;
            let exact = cluster.iter().all(|definition| {
                definition.raw_type == cluster[0].raw_type && definition.value == cluster[0].value
            });
            let canonical = canonical_duplicate(cluster, inventory, assignments);
            let canonical_candidate = format!(
                "{}:{}:{}",
                relative(project, &canonical.file),
                canonical.line,
                canonical.column
            );
            for definition in cluster {
                result.insert(
                    declaration_id(definition),
                    DuplicateProposal {
                        kind: if exact { "exact" } else { "normalized" }.to_owned(),
                        group: group.clone(),
                        canonical_candidate: canonical_candidate.clone(),
                        name_conflict: if has_name_conflict {
                            "true".to_owned()
                        } else {
                            String::new()
                        },
                    },
                );
            }
        }
        if has_name_conflict {
            for definition in definitions {
                result
                    .entry(declaration_id(definition))
                    .or_insert_with(|| DuplicateProposal {
                        kind: "conflict".to_owned(),
                        name_conflict: "true".to_owned(),
                        ..DuplicateProposal::default()
                    });
            }
        }
    }
    result
}

fn canonical_duplicate<'a>(
    cluster: &[&'a ConstDef],
    inventory: &BTreeMap<String, Vec<ConstDef>>,
    assignments: &BTreeMap<String, CsvProposal>,
) -> &'a ConstDef {
    let family_prefix = cluster[0]
        .name
        .rsplit_once('_')
        .map(|(prefix, _)| format!("{prefix}_"));
    let family_density = |candidate: &ConstDef| {
        inventory
            .values()
            .flatten()
            .filter(|definition| {
                definition.file == candidate.file
                    && definition.raw_type == candidate.raw_type
                    && family_prefix
                        .as_deref()
                        .is_some_and(|prefix| definition.name.starts_with(prefix))
            })
            .count()
    };
    let visibility_rank = |definition: &ConstDef| match definition.visibility.as_str() {
        "pub" => 3,
        value if value.starts_with("pub(") => 2,
        "" => 1,
        _ => 0,
    };
    let kind_rank = |definition: &ConstDef| match definition.kind {
        "module" => 3,
        "associated" => 2,
        "trait" => 1,
        _ => 0,
    };
    let evidence = |definition: &ConstDef| {
        assignments
            .get(&const_id(definition))
            .and_then(|proposal| proposal.evidence_count.parse::<usize>().ok())
            .unwrap_or(0)
    };
    let mut candidates = cluster.to_vec();
    candidates.sort_by(|left, right| {
        visibility_rank(right)
            .cmp(&visibility_rank(left))
            .then_with(|| kind_rank(right).cmp(&kind_rank(left)))
            .then_with(|| family_density(right).cmp(&family_density(left)))
            .then_with(|| evidence(right).cmp(&evidence(left)))
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.column.cmp(&right.column))
    });
    candidates[0]
}

fn normalized_duplicate_value(definition: &ConstDef) -> Option<String> {
    let value = definition.value.as_deref()?;
    let raw_type = definition.raw_type.as_deref()?;
    if is_integer_type(raw_type) {
        if let Some(value) = parse_duplicate_integer(value, raw_type) {
            return Some(format!("integer:{value}"));
        }
    }
    Some(format!("source:{}", value.trim()))
}

fn parse_duplicate_integer(source: &str, raw_type: &str) -> Option<i128> {
    let mut source = source.trim();
    loop {
        let Some(inner) = source.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
            break;
        };
        source = inner.trim();
    }
    if let Some((before, cast)) = source.rsplit_once(" as ") {
        if cast.trim() == raw_type {
            source = before.trim();
            while let Some(inner) = source.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
                source = inner.trim();
            }
        }
    }
    if let Some(without_suffix) = source.strip_suffix(raw_type) {
        source = without_suffix.trim_end_matches('_');
    }
    let clean = source.replace('_', "");
    let (negative, digits) = clean
        .strip_prefix('-')
        .map_or((false, clean.as_str()), |digits| (true, digits));
    let (radix, digits) = if let Some(digits) = digits.strip_prefix("0x") {
        (16, digits)
    } else if let Some(digits) = digits.strip_prefix("0o") {
        (8, digits)
    } else if let Some(digits) = digits.strip_prefix("0b") {
        (2, digits)
    } else {
        (10, digits)
    };
    let value = i128::from_str_radix(digits, radix).ok()?;
    Some(if negative { -value } else { value })
}

fn merge_equal_value_aliases(
    inventory: &BTreeMap<String, Vec<ConstDef>>,
    assignments: &mut BTreeMap<String, CsvProposal>,
) -> BTreeMap<String, String> {
    let assigned = inventory
        .values()
        .flatten()
        .filter_map(|definition| {
            assignments
                .get(&const_id(definition))
                .filter(|proposal| proposal.group_kind == "enum")
                .map(|proposal| (definition.clone(), proposal.clone()))
        })
        .collect::<Vec<_>>();
    let group_sizes = assignments
        .values()
        .fold(BTreeMap::new(), |mut counts, proposal| {
            *counts.entry(proposal.group.clone()).or_insert(0_usize) += 1;
            counts
        });
    let mut candidates_by_family: BTreeMap<(Utf8PathBuf, String, String), Vec<_>> = BTreeMap::new();
    for definition in inventory.values().flatten() {
        let id = const_id(definition);
        if assignments.contains_key(&id) || definition.value.is_none() {
            continue;
        }
        let candidates = assigned
            .iter()
            .filter(|(canonical, _)| {
                canonical.file == definition.file
                    && canonical.raw_type == definition.raw_type
                    && canonical.value == definition.value
                    && alias_names_compatible(&canonical.name, &definition.name)
            })
            .collect::<Vec<_>>();
        let identities = candidates
            .iter()
            .map(|(_, proposal)| (&proposal.group, &proposal.variant))
            .collect::<BTreeSet<_>>();
        if identities.len() != 1 {
            continue;
        }
        let (canonical, proposal) = candidates
            .into_iter()
            .min_by_key(|(canonical, _)| canonical.line.abs_diff(definition.line))
            .unwrap();
        let prefix = definition
            .name
            .split('_')
            .next()
            .unwrap_or_default()
            .to_owned();
        candidates_by_family
            .entry((definition.file.clone(), prefix, proposal.group.clone()))
            .or_default()
            .push((id, definition.clone(), canonical.clone(), proposal.clone()));
    }

    let mut suggestions = BTreeMap::new();
    for ((_, _, group), candidates) in candidates_by_family {
        let promote = candidates.len() == 1
            && group_sizes.get(&group).copied().unwrap_or(0) >= MIN_AUTO_ALIAS_GROUP_SIZE;
        for (id, definition, canonical, proposal) in candidates {
            if promote && canonical.line.abs_diff(definition.line) <= MAX_AUTO_ALIAS_LINE_GAP {
                let mut alias = proposal;
                alias.alias_of = canonical.name;
                assignments.insert(id, alias);
            } else {
                suggestions.insert(id, canonical.name);
            }
        }
    }
    suggestions
}

fn alias_names_compatible(left: &str, right: &str) -> bool {
    left == right
        || left
            .split('_')
            .next()
            .zip(right.split('_').next())
            .is_some_and(|(left, right)| left == right)
}

#[derive(Clone, Default)]
struct CsvProposal {
    group: String,
    group_kind: String,
    enum_name: String,
    variant: String,
    alias_of: String,
    evidence_count: String,
    subjects: String,
    matches: String,
    comparisons: String,
    selections: String,
}

fn group_kind(group: &Group) -> &'static str {
    let bitwise = group
        .constants
        .values()
        .filter(|item| is_bitwise_constant(item))
        .count();
    if bitwise * 2 >= group.constants.len() && bitwise > 0 {
        "flags"
    } else if bitwise > 0 {
        "mixed"
    } else if group.constants.values().any(|item| item.used_ordering) {
        "limits"
    } else if group
        .constants
        .values()
        .filter(|item| has_limit_marker(&item.name))
        .count()
        * 2
        >= group.constants.len()
    {
        "limits"
    } else {
        "enum"
    }
}

fn is_bitwise_constant(item: &ConstDef) -> bool {
    item.used_bitwise
        || item
            .value
            .as_deref()
            .is_some_and(|value| value.contains("<<") || value.contains(" | "))
}

fn has_limit_marker(name: &str) -> bool {
    name.split('_').any(|token| {
        matches!(
            token,
            "MAX"
                | "MIN"
                | "SIZE"
                | "LEN"
                | "LENGTH"
                | "COUNT"
                | "NUM"
                | "SHIFT"
                | "MASK"
                | "WIDTH"
                | "HEIGHT"
                | "LIMIT"
        )
    })
}

type PrefixKey = (Utf8PathBuf, String, String);

fn lexical_prefix_counts(
    inventory: &BTreeMap<String, Vec<ConstDef>>,
) -> BTreeMap<PrefixKey, usize> {
    let mut members: BTreeMap<PrefixKey, BTreeSet<String>> = BTreeMap::new();
    for item in inventory.values().flatten().filter(|item| {
        item.kind == "module" && item.raw_type.as_deref().is_some_and(is_integer_type)
    }) {
        let Some(raw_type) = &item.raw_type else {
            continue;
        };
        let tokens: Vec<_> = item.name.split('_').collect();
        for count in 1..=tokens.len().saturating_sub(1).min(2) {
            members
                .entry((
                    item.file.clone(),
                    raw_type.clone(),
                    tokens[..count].join("_"),
                ))
                .or_default()
                .insert(item.name.clone());
        }
    }
    members
        .into_iter()
        .map(|(key, names)| (key, names.len()))
        .collect()
}

fn possible_groups(
    item: &ConstDef,
    group_prefixes: &[(Utf8PathBuf, String, String, String)],
    lexical_counts: &BTreeMap<PrefixKey, usize>,
) -> String {
    if item.kind != "module" || !item.raw_type.as_deref().is_some_and(is_integer_type) {
        return String::new();
    }
    let raw_type = item.raw_type.as_deref().unwrap_or_default();
    let mut suggestions = BTreeSet::new();
    for (file, ty, prefix, group) in group_prefixes {
        if file == &item.file
            && ty == raw_type
            && item
                .name
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.starts_with('_'))
        {
            suggestions.insert(group.clone());
        }
    }
    let tokens: Vec<_> = item.name.split('_').collect();
    for count in (1..=tokens.len().saturating_sub(1).min(2)).rev() {
        let prefix = tokens[..count].join("_");
        let key = (item.file.clone(), raw_type.to_owned(), prefix.clone());
        let threshold = if count == 1 { 4 } else { 3 };
        if lexical_counts.get(&key).copied().unwrap_or(0) >= threshold {
            suggestions.insert(format!("prefix:{prefix}"));
            break;
        }
    }
    suggestions.into_iter().collect::<Vec<_>>().join(";")
}

fn unassigned_reason(item: &ConstDef) -> &'static str {
    if item.kind != "module" {
        return "non_module_constant";
    }
    if !item.raw_type.as_deref().is_some_and(is_integer_type) {
        return "unsupported_type";
    }
    if is_bitwise_constant(item) {
        return "bitwise_use";
    }
    if item.used_ordering {
        return "ordering_comparison";
    }
    "no_strong_set"
}

fn common_name_prefix(group: &Group) -> Vec<String> {
    let mut names = group
        .constants
        .values()
        .map(|item| item.name.split('_').map(str::to_owned).collect::<Vec<_>>());
    let Some(first) = names.next() else {
        return Vec::new();
    };
    let mut count = first.len();
    for name in names {
        count = count.min(name.len());
        while count > 0 && first[..count] != name[..count] {
            count -= 1;
        }
    }
    // Leave at least one token for each variant.
    if group
        .constants
        .values()
        .any(|item| item.name.split('_').count() == count)
    {
        count = count.saturating_sub(1);
    }
    first[..count].to_vec()
}

fn suggested_enum_name(group: &Group) -> String {
    let prefix = common_name_prefix(group);
    if !prefix.is_empty() {
        return pascal_case(&prefix.join("_"));
    }
    let suffix = common_name_suffix(group);
    if !suffix.is_empty() {
        return pascal_case(&suffix.join("_"));
    }
    for subject in group.evidence.iter().map(|site| site.subject.as_str()) {
        for part in subject
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .filter(|part| !part.is_empty())
            .rev()
        {
            if part
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic())
                && !matches!(part, "argument" | "call" | "return")
            {
                return pascal_case(part);
            }
        }
    }
    "Value".into()
}

fn common_name_suffix(group: &Group) -> Vec<String> {
    let names: Vec<Vec<_>> = group
        .constants
        .values()
        .map(|item| item.name.split('_').map(str::to_owned).collect())
        .collect();
    let Some(first) = names.first() else {
        return Vec::new();
    };
    let mut count = first.len();
    for name in &names[1..] {
        count = count.min(name.len());
        while count > 0 && first[first.len() - count..] != name[name.len() - count..] {
            count -= 1;
        }
    }
    if names.iter().any(|name| name.len() == count) {
        count = count.saturating_sub(1);
    }
    first[first.len() - count..].to_vec()
}

fn suggested_variant(name: &str, prefix: &[String]) -> String {
    let tokens: Vec<_> = name.split('_').collect();
    let remainder = tokens.get(prefix.len()..).unwrap_or(&tokens);
    let mut result = pascal_case(&remainder.join("_"));
    if result.is_empty() {
        result = pascal_case(name);
    }
    if result.starts_with(|character: char| character.is_ascii_digit()) {
        result.insert(0, 'V');
    }
    result
}

fn pascal_case(raw: &str) -> String {
    raw.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let lower = part.to_ascii_lowercase();
            let mut characters = lower.chars();
            characters
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + characters.as_str())
                .unwrap_or_default()
        })
        .collect()
}

fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
