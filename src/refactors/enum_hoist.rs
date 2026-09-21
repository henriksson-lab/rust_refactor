//! Propagate an enum introduced by `constants-to-enum` through typed value flow.
//!
//! The planner follows value flow in both directions. Every selected parameter or named field,
//! and every compatible parameter, field, local, or return connected to it, is changed in one
//! atomic plan. Raw producers must be known variants, compatibility constants, or existing
//! validated enum values.
use anyhow::{anyhow, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use ra_ap_syntax::{
    ast::{self, AstNode, BinaryOp, CmpOp, HasArgList, HasAttrs, HasName},
    Edition, SourceFile,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
};
use text_size::{TextRange, TextSize};

use super::remove_function::{line_col_offset, range_json};
use crate::{
    analysis::{self, ParsedFile},
    cli::{EnumHoistCommand, EnumHoistStatsCommand, OutputFormat},
    edits::{apply_edits_to_string, apply_plan, RefactorPlan, TextEdit},
    project::Project,
    semantic::{SemanticDefinition, SemanticProject},
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

#[derive(Clone, Debug, Eq, PartialEq)]
enum PlaceKind {
    Parameter {
        function_name: String,
        function_name_range: TextRange,
        parameter_index: usize,
        method: bool,
    },
    Field {
        owner: String,
    },
    Local,
    Return {
        function_name_range: TextRange,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Place {
    id: String,
    file: Utf8PathBuf,
    name: String,
    name_range: TextRange,
    type_range: Option<TextRange>,
    kind: PlaceKind,
}

#[derive(Clone, Copy)]
enum SeedKind {
    Parameter,
    Field,
    Local,
    Return,
}

#[derive(Clone, Debug)]
struct EnumInfo {
    name: String,
    path: String,
    raw_type: String,
    variants: BTreeMap<SemanticDefinitionKey, String>,
    aliases: BTreeMap<SemanticDefinitionKey, String>,
    values: BTreeMap<i128, String>,
    from_raw: Option<SemanticDefinitionKey>,
    to_raw: Option<SemanticDefinitionKey>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SemanticDefinitionKey {
    file: Utf8PathBuf,
    start: u32,
    end: u32,
}

impl From<SemanticDefinition> for SemanticDefinitionKey {
    fn from(value: SemanticDefinition) -> Self {
        Self {
            file: value.file,
            start: value.name_range.start().into(),
            end: value.name_range.end().into(),
        }
    }
}

#[derive(Clone, Debug)]
struct Producer {
    place: Option<String>,
    replacement: Option<String>,
    range: TextRange,
    file: Utf8PathBuf,
}

struct StatsSubject {
    id: String,
    kind: String,
    subject: String,
    file: Utf8PathBuf,
    range: TextRange,
}

#[derive(Default)]
struct PlanBuilder {
    edits: BTreeMap<(Utf8PathBuf, u32, u32), TextEdit>,
    removed_conversions: usize,
    inserted_conversions: usize,
}

impl PlanBuilder {
    fn add(
        &mut self,
        file: &Utf8PathBuf,
        range: TextRange,
        replacement: impl Into<String>,
    ) -> std::result::Result<(), FlowError> {
        let mut replacement = replacement.into();
        let key = (
            file.clone(),
            u32::from(range.start()),
            u32::from(range.end()),
        );
        if let Some(existing) = self.edits.get(&key) {
            if existing.replacement == replacement {
                return Ok(());
            }
            return refuse(
                "CONFLICTING_EDITS",
                "the value flow requires two different rewrites at one location",
                Some(file.clone()),
                Some(range),
            );
        }
        let contained = self
            .edits
            .iter()
            .filter(|(_, edit)| {
                &edit.file == file && !edit.range.is_empty() && range.contains_range(edit.range)
            })
            .map(|(key, edit)| (key.clone(), edit.clone()))
            .collect::<Vec<_>>();
        if !contained.is_empty() {
            let source = fs::read_to_string(file)?;
            for (contained_key, edit) in contained {
                let original = text_at(&source, edit.range);
                let occurrences = replacement.match_indices(original).collect::<Vec<_>>();
                if occurrences.len() != 1 {
                    return refuse(
                        "CONFLICTING_EDITS",
                        "a containing rewrite could not uniquely compose an inner value rewrite",
                        Some(file.clone()),
                        Some(range),
                    );
                }
                let start = occurrences[0].0;
                replacement.replace_range(start..start + original.len(), &edit.replacement);
                self.edits.remove(&contained_key);
            }
        }
        for existing in self.edits.values().filter(|edit| &edit.file == file) {
            if existing.range.intersect(range).is_some()
                && !existing.range.is_empty()
                && !range.is_empty()
            {
                return refuse(
                    "CONFLICTING_EDITS",
                    "overlapping enum-hoist rewrites could not be composed",
                    Some(file.clone()),
                    Some(range),
                );
            }
        }
        self.edits.insert(
            key,
            TextEdit {
                file: file.clone(),
                range,
                replacement,
            },
        );
        Ok(())
    }

    fn covers(&self, file: &Utf8PathBuf, range: TextRange) -> bool {
        self.edits
            .values()
            .any(|edit| &edit.file == file && edit.range.contains_range(range))
    }
}

pub fn run(command: EnumHoistCommand) -> Result<i32> {
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

pub fn run_stats(command: EnumHoistStatsCommand) -> Result<()> {
    let project = Project::load(command.manifest_path.as_deref())?;
    let files = analysis::parse_project_files(&project)?;
    let semantic =
        SemanticProject::load_with(&project, command.all_features, command.target.as_deref())?;
    let enum_file = resolve_file(&project, &command.enum_file).map_err(flow_error)?;
    let info = collect_enum_info(
        &enum_file,
        &command.enum_name,
        &command.enum_name,
        &files,
        &semantic,
    )
    .map_err(flow_error)?;
    let places = collect_places(&files, &info.raw_type);
    let mut candidates: BTreeMap<String, (String, String, Utf8PathBuf, TextRange, usize)> =
        BTreeMap::new();
    for file in &files {
        for call in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::CallExpr::cast)
        {
            let Some(callee) = call.expr() else { continue };
            let Some(name) = callee
                .syntax()
                .descendants()
                .filter_map(ast::NameRef::cast)
                .last()
            else {
                continue;
            };
            if name.text() != "from_raw"
                || !definition_matches(
                    semantic.definition_at(&file.path, name.syntax().text_range())?,
                    info.from_raw.as_ref(),
                )
            {
                continue;
            }
            let Some(argument) = call.arg_list().and_then(|args| args.args().next()) else {
                continue;
            };
            let Some(subject) = stats_subject(&argument, &file.path, &places, &semantic, &files)?
            else {
                continue;
            };
            let entry = candidates.entry(subject.id).or_insert((
                subject.kind,
                subject.subject,
                subject.file,
                subject.range,
                0,
            ));
            entry.4 += 1;
        }
    }
    let mut values = candidates
        .into_values()
        .map(|(kind, subject, file, range, conversions)| {
            let parsed = files.iter().find(|parsed| parsed.path == file).unwrap();
            let (line, column) = line_column(&parsed.source, range.start().into());
            let references = semantic.references_to(&file, range).unwrap_or_default();
            json!({
                "kind":kind,
                "subject":subject,
                "file":relative(&project,&file),
                "line":line,
                "column":column,
                "selection":format!("{}:{line}:{column}",relative(&project,&file)),
                "from_raw_count":conversions,
                "reference_count":references.len()
            })
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right["from_raw_count"]
            .as_u64()
            .cmp(&left["from_raw_count"].as_u64())
            .then_with(|| left["file"].as_str().cmp(&right["file"].as_str()))
            .then_with(|| left["line"].as_u64().cmp(&right["line"].as_u64()))
    });
    match command.format {
        OutputFormat::Json => println!(
            "{}",
            json!({"candidate_count":values.len(),"candidates":values})
        ),
        OutputFormat::Text => {
            println!("CONVERSIONS  REFS  KIND       SUBJECT  SELECTION");
            for value in values {
                println!(
                    "{:<12} {:<5} {:<10} {:<20} {}",
                    value["from_raw_count"].as_u64().unwrap_or(0),
                    value["reference_count"].as_u64().unwrap_or(0),
                    value["kind"].as_str().unwrap_or(""),
                    value["subject"].as_str().unwrap_or(""),
                    value["selection"].as_str().unwrap_or("")
                );
            }
        }
    }
    Ok(())
}

fn stats_subject(
    expression: &ast::Expr,
    file: &Utf8PathBuf,
    places: &BTreeMap<String, Place>,
    semantic: &SemanticProject,
    files: &[ParsedFile],
) -> Result<Option<StatsSubject>> {
    if let Some(id) = expression_place(expression, file, places, semantic)?
        .or(call_return_place(expression, file, places, semantic)?)
    {
        let place = &places[&id];
        let kind = match place.kind {
            PlaceKind::Parameter { .. } => "parameter",
            PlaceKind::Field { .. } => "field",
            PlaceKind::Local => "local",
            PlaceKind::Return { .. } => "return",
        };
        return Ok(Some(StatsSubject {
            id,
            kind: kind.to_owned(),
            subject: place.name.clone(),
            file: place.file.clone(),
            range: place.name_range,
        }));
    }
    let ast::Expr::PathExpr(path) = expression else {
        return Ok(None);
    };
    let Some(name) = path
        .path()
        .and_then(|path| path.segment())
        .and_then(|segment| segment.name_ref())
    else {
        return Ok(None);
    };
    let Some(definition) = semantic.definition_at(file, name.syntax().text_range())? else {
        return Ok(None);
    };
    let Some(parsed) = files.iter().find(|parsed| parsed.path == definition.file) else {
        return Ok(None);
    };
    if parsed
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::LetStmt::cast)
        .any(|statement| {
            statement.pat().is_some_and(|pat| {
                pat.syntax()
                    .text_range()
                    .contains_range(definition.name_range)
            })
        })
    {
        return Ok(Some(StatsSubject {
            id: place_id(&definition.file, definition.name_range),
            kind: "local".to_owned(),
            subject: name.text().to_string(),
            file: definition.file,
            range: definition.name_range,
        }));
    }
    Ok(None)
}

fn flow_error(error: FlowError) -> anyhow::Error {
    match error {
        FlowError::Failed(error) => error,
        FlowError::Refused(reason) => anyhow!("{}: {}", reason.code, reason.message),
    }
}

fn run_inner(
    command: &EnumHoistCommand,
) -> std::result::Result<(&'static str, RefactorPlan, Value), FlowError> {
    if command.dry_run == command.write {
        return refuse(
            "INVALID_MODE",
            "choose exactly one of --dry-run or --write",
            None,
            None,
        );
    }
    if command.parameters.is_empty()
        && command.fields.is_empty()
        && command.locals.is_empty()
        && command.returns.is_empty()
    {
        return refuse(
            "NO_SEEDS",
            "select at least one --parameter, --field, --local, or --return",
            None,
            None,
        );
    }
    let project = Project::load(command.manifest_path.as_deref())?;
    let files = analysis::parse_project_files(&project)?;
    let file_map = files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect::<BTreeMap<_, _>>();
    let semantic =
        SemanticProject::load_with(&project, command.all_features, command.target.as_deref())?;
    let enum_file = resolve_file(&project, &command.enum_file)?;
    let enum_info = collect_enum_info(
        &enum_file,
        &command.enum_name,
        command.enum_path.as_deref().unwrap_or(&command.enum_name),
        &files,
        &semantic,
    )?;
    let places = collect_places(&files, &enum_info.raw_type);
    let mut selected = BTreeSet::new();
    for position in &command.parameters {
        selected.insert(select_place(
            &project,
            &file_map,
            &places,
            position,
            SeedKind::Parameter,
        )?);
    }
    for position in &command.fields {
        selected.insert(select_place(
            &project,
            &file_map,
            &places,
            position,
            SeedKind::Field,
        )?);
    }
    for position in &command.locals {
        selected.insert(select_place(
            &project,
            &file_map,
            &places,
            position,
            SeedKind::Local,
        )?);
    }
    for position in &command.returns {
        selected.insert(select_place(
            &project,
            &file_map,
            &places,
            position,
            SeedKind::Return,
        )?);
    }

    let mut queue = selected.iter().cloned().collect::<VecDeque<_>>();
    let mut producer_edits = Vec::new();
    while let Some(id) = queue.pop_front() {
        let place = places
            .get(&id)
            .ok_or_else(|| anyhow!("missing place {id}"))?;
        let producers = incoming_producers(place, &places, &file_map, &semantic, &enum_info)?;
        for producer in producers {
            if let Some(source) = producer.place {
                if selected.insert(source.clone()) {
                    queue.push_back(source);
                }
            }
            if let Some(replacement) = producer.replacement {
                producer_edits.push((producer.file, producer.range, replacement));
            }
        }
        for target in connected_consumers(place, &places, &file_map, &semantic, &enum_info)? {
            if selected.insert(target.clone()) {
                queue.push_back(target);
            }
        }
    }

    let mut builder = PlanBuilder::default();
    for id in &selected {
        let place = &places[id];
        if let Some(type_range) = place.type_range {
            builder.add(&place.file, type_range, enum_info.path.clone())?;
        }
    }
    for (file, range, replacement) in producer_edits {
        builder.add(&file, range, replacement)?;
    }
    for id in &selected {
        rewrite_place_uses(
            &places[id],
            &selected,
            &places,
            &file_map,
            &semantic,
            &enum_info,
            &mut builder,
        )?;
    }
    if builder.removed_conversions <= builder.inserted_conversions {
        return refuse(
            "NO_CONVERSION_REDUCTION",
            format!(
                "the plan removes {} conversions and inserts {}; no reduction was found",
                builder.removed_conversions, builder.inserted_conversions
            ),
            None,
            None,
        );
    }
    let plan = RefactorPlan {
        edits: builder.edits.into_values().collect(),
        diagnostics: Vec::new(),
    };
    validate_plan(&plan)?;
    let typed_places = selected
        .iter()
        .map(|id| {
            let place = &places[id];
            json!({"kind":match place.kind { PlaceKind::Parameter { .. } => "parameter", PlaceKind::Field { .. } => "field", PlaceKind::Local => "local", PlaceKind::Return { .. } => "return"},"name":place.name,"file":relative(&project,&place.file),"range":range_json(place.name_range)})
        })
        .collect::<Vec<_>>();
    let target = json!({
        "enum": enum_info.name,
        "raw_type": enum_info.raw_type,
        "typed_places": typed_places,
        "conversion_delta": {
            "before": builder.removed_conversions,
            "after": builder.inserted_conversions,
            "removed": builder.removed_conversions.saturating_sub(builder.inserted_conversions)
        }
    });
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
            error.context("enum-hoist failed; original files restored"),
        ));
    }
    Ok(("applied", plan, target))
}

fn collect_places(files: &[ParsedFile], raw_type: &str) -> BTreeMap<String, Place> {
    let mut places = BTreeMap::new();
    for file in files {
        for function in file.tree.syntax().descendants().filter_map(ast::Fn::cast) {
            let Some(function_name) = function.name() else {
                continue;
            };
            if let Some(parameters) = function.param_list() {
                let method = parameters.self_param().is_some();
                for (parameter_index, parameter) in parameters.params().enumerate() {
                    let (Some(ty), Some(pat)) = (parameter.ty(), parameter.pat()) else {
                        continue;
                    };
                    if ty.syntax().text().to_string().trim() != raw_type {
                        continue;
                    }
                    let Some(ident) = ast::IdentPat::cast(pat.syntax().clone()) else {
                        continue;
                    };
                    let Some(name) = ident.name() else {
                        continue;
                    };
                    let id = place_id(&file.path, name.syntax().text_range());
                    places.insert(
                        id.clone(),
                        Place {
                            id,
                            file: file.path.clone(),
                            name: name.text().to_string(),
                            name_range: name.syntax().text_range(),
                            type_range: Some(ty.syntax().text_range()),
                            kind: PlaceKind::Parameter {
                                function_name: function_name.text().to_string(),
                                function_name_range: function_name.syntax().text_range(),
                                parameter_index,
                                method,
                            },
                        },
                    );
                }
            }
            if let Some(ty) = function.ret_type().and_then(|ret| ret.ty()) {
                if ty.syntax().text().to_string().trim() == raw_type {
                    let name_range = function_name.syntax().text_range();
                    let id = return_place_id(&file.path, name_range);
                    places.insert(
                        id.clone(),
                        Place {
                            id,
                            file: file.path.clone(),
                            name: format!("{} return", function_name.text()),
                            name_range,
                            type_range: Some(ty.syntax().text_range()),
                            kind: PlaceKind::Return {
                                function_name_range: name_range,
                            },
                        },
                    );
                }
            }
        }
        for statement in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::LetStmt::cast)
        {
            let (Some(pattern), Some(_initializer)) = (statement.pat(), statement.initializer())
            else {
                continue;
            };
            let Some(ident) = ast::IdentPat::cast(pattern.syntax().clone()) else {
                continue;
            };
            let Some(name) = ident.name() else { continue };
            let type_range = match statement.ty() {
                Some(ty) if ty.syntax().text().to_string().trim() == raw_type => {
                    Some(ty.syntax().text_range())
                }
                Some(_) => continue,
                None => None,
            };
            let id = place_id(&file.path, name.syntax().text_range());
            places.insert(
                id.clone(),
                Place {
                    id,
                    file: file.path.clone(),
                    name: name.text().to_string(),
                    name_range: name.syntax().text_range(),
                    type_range,
                    kind: PlaceKind::Local,
                },
            );
        }
        for field in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::RecordField::cast)
        {
            let (Some(name), Some(ty)) = (field.name(), field.ty()) else {
                continue;
            };
            if ty.syntax().text().to_string().trim() != raw_type {
                continue;
            }
            let owner = field
                .syntax()
                .ancestors()
                .find_map(|node| ast::Struct::cast(node).and_then(|item| item.name()))
                .map(|name| name.text().to_string())
                .unwrap_or_default();
            let id = place_id(&file.path, name.syntax().text_range());
            places.insert(
                id.clone(),
                Place {
                    id,
                    file: file.path.clone(),
                    name: name.text().to_string(),
                    name_range: name.syntax().text_range(),
                    type_range: Some(ty.syntax().text_range()),
                    kind: PlaceKind::Field { owner },
                },
            );
        }
    }
    places
}

fn select_place(
    project: &Project,
    files: &BTreeMap<Utf8PathBuf, &ParsedFile>,
    places: &BTreeMap<String, Place>,
    selection: &str,
    kind: SeedKind,
) -> std::result::Result<String, FlowError> {
    let (path, line, column) = parse_position(selection)?;
    let file = resolve_file(project, &Utf8PathBuf::from(path))?;
    let parsed = files
        .get(&file)
        .ok_or_else(|| anyhow!("file was not parsed"))?;
    let offset = line_col_offset(&parsed.source, line, column).ok_or_else(|| {
        FlowError::Refused(Refusal {
            code: "SELECTION_NOT_FOUND",
            message: "selection lies outside the source file".to_owned(),
            file: Some(file.clone()),
            range: None,
        })
    })?;
    let offset = TextSize::from(offset as u32);
    let matches = places
        .values()
        .filter(|place| {
            place.file == file
                && (place.name_range.contains(offset) || place.name_range.end() == offset)
                && matches!(
                    (&place.kind, kind),
                    (PlaceKind::Parameter { .. }, SeedKind::Parameter)
                        | (PlaceKind::Field { .. }, SeedKind::Field)
                        | (PlaceKind::Local, SeedKind::Local)
                        | (PlaceKind::Return { .. }, SeedKind::Return)
                )
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return refuse(
            "SELECTION_NOT_FOUND",
            match kind {
                SeedKind::Parameter => "position does not select one raw integer parameter",
                SeedKind::Field => "position does not select one raw integer named field",
                SeedKind::Local => "position does not select one simple local binding",
                SeedKind::Return => {
                    "position does not select one function with the raw integer return type"
                }
            },
            Some(file),
            None,
        );
    }
    Ok(matches[0].id.clone())
}

fn incoming_producers(
    place: &Place,
    places: &BTreeMap<String, Place>,
    files: &BTreeMap<Utf8PathBuf, &ParsedFile>,
    semantic: &SemanticProject,
    enum_info: &EnumInfo,
) -> std::result::Result<Vec<Producer>, FlowError> {
    match &place.kind {
        PlaceKind::Parameter {
            function_name_range,
            parameter_index,
            method,
            ..
        } => {
            let references = semantic.references_to(&place.file, *function_name_range)?;
            let mut producers = Vec::new();
            for reference in references {
                let parsed = files
                    .get(&reference.file)
                    .ok_or_else(|| anyhow!("reference file was not parsed"))?;
                if reference_in_use(parsed, reference.range) {
                    continue;
                }
                let expression = if *method {
                    method_argument(parsed, reference.range, *parameter_index)
                } else {
                    call_argument(parsed, reference.range, *parameter_index)
                };
                let Some(expression) = expression else {
                    return refuse(
                        "UNRESOLVED_FUNCTION_REFERENCE",
                        "a function reference is not a supported direct call",
                        Some(reference.file),
                        Some(reference.range),
                    );
                };
                producers.push(classify_producer(
                    &expression,
                    &reference.file,
                    places,
                    semantic,
                    enum_info,
                )?);
            }
            Ok(producers)
        }
        PlaceKind::Field { .. } => field_producers(place, places, files, semantic, enum_info),
        PlaceKind::Local => local_producers(place, places, files, semantic, enum_info),
        PlaceKind::Return { .. } => return_producers(place, places, files, semantic, enum_info),
    }
}

fn local_producers(
    place: &Place,
    places: &BTreeMap<String, Place>,
    files: &BTreeMap<Utf8PathBuf, &ParsedFile>,
    semantic: &SemanticProject,
    enum_info: &EnumInfo,
) -> std::result::Result<Vec<Producer>, FlowError> {
    let parsed = files
        .get(&place.file)
        .ok_or_else(|| anyhow!("local definition file was not parsed"))?;
    let statement = parsed
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::LetStmt::cast)
        .find(|statement| {
            statement
                .pat()
                .is_some_and(|pat| pat.syntax().text_range().contains_range(place.name_range))
        })
        .ok_or_else(|| anyhow!("local definition was not found"))?;
    let initializer = statement
        .initializer()
        .ok_or_else(|| anyhow!("local has no initializer"))?;
    let mut producers = vec![classify_producer(
        &initializer,
        &place.file,
        places,
        semantic,
        enum_info,
    )?];
    for reference in semantic.references_to(&place.file, place.name_range)? {
        let reference_file = files
            .get(&reference.file)
            .ok_or_else(|| anyhow!("local reference file was not parsed"))?;
        if let Some(binary) = ancestor_at::<ast::BinExpr>(reference_file, reference.range) {
            if binary
                .lhs()
                .is_some_and(|lhs| lhs.syntax().text_range().contains_range(reference.range))
            {
                if !matches!(binary.op_kind(), Some(BinaryOp::Assignment { op: None })) {
                    return refuse(
                        "UNHANDLED_LOCAL_WRITE",
                        "compound local assignment cannot be represented by an enum",
                        Some(reference.file),
                        Some(binary.syntax().text_range()),
                    );
                }
                let rhs = binary
                    .rhs()
                    .ok_or_else(|| anyhow!("assignment has no value"))?;
                producers.push(classify_producer(
                    &rhs,
                    &reference.file,
                    places,
                    semantic,
                    enum_info,
                )?);
            }
        }
    }
    Ok(producers)
}

fn return_producers(
    place: &Place,
    places: &BTreeMap<String, Place>,
    files: &BTreeMap<Utf8PathBuf, &ParsedFile>,
    semantic: &SemanticProject,
    enum_info: &EnumInfo,
) -> std::result::Result<Vec<Producer>, FlowError> {
    let parsed = files
        .get(&place.file)
        .ok_or_else(|| anyhow!("return definition file was not parsed"))?;
    let function = parsed
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::Fn::cast)
        .find(|function| {
            function
                .name()
                .is_some_and(|name| name.syntax().text_range() == place.name_range)
        })
        .ok_or_else(|| anyhow!("returning function was not found"))?;
    if function.async_token().is_some() {
        return refuse(
            "UNSUPPORTED_RETURN",
            "async function return propagation is not supported",
            Some(place.file.clone()),
            Some(place.name_range),
        );
    }
    let body = function
        .body()
        .ok_or_else(|| anyhow!("returning function has no body"))?;
    let mut expressions = body
        .syntax()
        .descendants()
        .filter_map(ast::ReturnExpr::cast)
        .filter(|returned| {
            returned
                .syntax()
                .ancestors()
                .skip(1)
                .find_map(ast::Fn::cast)
                .and_then(|owner| owner.name())
                .is_some_and(|name| name.syntax().text_range() == place.name_range)
        })
        .map(|returned| {
            returned.expr().ok_or_else(|| {
                FlowError::Refused(Refusal {
                    code: "UNSUPPORTED_RETURN",
                    message: "a bare return cannot produce the selected enum".to_owned(),
                    file: Some(place.file.clone()),
                    range: Some(returned.syntax().text_range()),
                })
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if let Some(tail) = body.stmt_list().and_then(|list| list.tail_expr()) {
        if !matches!(tail, ast::Expr::ReturnExpr(_)) {
            expressions.push(tail);
        }
    }
    if expressions.is_empty() {
        return refuse(
            "UNSUPPORTED_RETURN",
            "the function has no supported value-producing return expression",
            Some(place.file.clone()),
            Some(place.name_range),
        );
    }
    expressions
        .into_iter()
        .map(|expression| classify_producer(&expression, &place.file, places, semantic, enum_info))
        .collect()
}

fn field_producers(
    place: &Place,
    places: &BTreeMap<String, Place>,
    files: &BTreeMap<Utf8PathBuf, &ParsedFile>,
    semantic: &SemanticProject,
    enum_info: &EnumInfo,
) -> std::result::Result<Vec<Producer>, FlowError> {
    let parsed_definition = files
        .get(&place.file)
        .ok_or_else(|| anyhow!("field definition file was not parsed"))?;
    if let Some(structure) = parsed_definition
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::Struct::cast)
        .find(|structure| {
            structure.field_list().is_some_and(|fields| {
                fields
                    .syntax()
                    .text_range()
                    .contains_range(place.name_range)
            })
        })
    {
        for attribute in structure.attrs() {
            let text = attribute.syntax().text().to_string();
            if text.starts_with("#[repr(") && text.contains('C') {
                return refuse(
                    "REPRESENTATION_CONFLICT",
                    "a field in a repr(C) struct cannot be changed automatically",
                    Some(place.file.clone()),
                    Some(place.name_range),
                );
            }
            if text.starts_with("#[derive(")
                && text
                    .trim_start_matches("#[derive(")
                    .trim_end_matches(")]")
                    .split(',')
                    .any(|derive| derive.trim().ends_with("Default"))
            {
                return refuse(
                    "UNHANDLED_FIELD_WRITE",
                    "a derived Default implementation would need a reviewed enum default",
                    Some(place.file.clone()),
                    Some(place.name_range),
                );
            }
        }
    }
    let references = semantic.references_to(&place.file, place.name_range)?;
    let mut producers = Vec::new();
    for reference in references {
        let parsed = files
            .get(&reference.file)
            .ok_or_else(|| anyhow!("reference file was not parsed"))?;
        if let Some(field) = ancestor_at::<ast::RecordExprField>(parsed, reference.range) {
            let Some(expression) = field.expr() else {
                return refuse(
                    "UNHANDLED_FIELD_WRITE",
                    "shorthand struct field initialization is not supported yet",
                    Some(reference.file),
                    Some(field.syntax().text_range()),
                );
            };
            producers.push(classify_producer(
                &expression,
                &reference.file,
                places,
                semantic,
                enum_info,
            )?);
            continue;
        }
        if let Some(field_expr) = ancestor_at::<ast::FieldExpr>(parsed, reference.range) {
            if let Some(binary) = field_expr.syntax().parent().and_then(ast::BinExpr::cast) {
                if binary.lhs().is_some_and(|lhs| {
                    lhs.syntax().text_range() == field_expr.syntax().text_range()
                }) {
                    if !matches!(binary.op_kind(), Some(BinaryOp::Assignment { op: None })) {
                        return refuse(
                            "UNHANDLED_FIELD_WRITE",
                            "compound field assignment cannot be represented by an enum",
                            Some(reference.file),
                            Some(binary.syntax().text_range()),
                        );
                    }
                    let rhs = binary
                        .rhs()
                        .ok_or_else(|| anyhow!("assignment has no value"))?;
                    producers.push(classify_producer(
                        &rhs,
                        &reference.file,
                        places,
                        semantic,
                        enum_info,
                    )?);
                }
            }
        }
    }
    Ok(producers)
}

fn classify_producer(
    expression: &ast::Expr,
    file: &Utf8PathBuf,
    places: &BTreeMap<String, Place>,
    semantic: &SemanticProject,
    enum_info: &EnumInfo,
) -> std::result::Result<Producer, FlowError> {
    let expression = unwrap_parens(expression.clone());
    if let Some((receiver, _method_range)) =
        enum_to_raw_receiver(&expression, file, semantic, enum_info)?
    {
        return Ok(Producer {
            place: expression_place(&receiver, file, places, semantic)?,
            replacement: Some(slice_expr(&receiver)),
            range: expression.syntax().text_range(),
            file: file.clone(),
        });
    }
    if let ast::Expr::MethodCallExpr(call) = &expression {
        if call.name_ref().is_some_and(|name| name.text() == "to_raw") {
            if let Some(receiver) = call.receiver() {
                if expression_is_validated_enum(&receiver, file, semantic, enum_info)?
                    || enum_variant(&receiver, file, semantic, enum_info)?.is_some()
                {
                    return Ok(Producer {
                        place: expression_place(&receiver, file, places, semantic)?,
                        replacement: Some(slice_expr(&receiver)),
                        range: expression.syntax().text_range(),
                        file: file.clone(),
                    });
                }
            }
        }
    }
    if let Some(variant) = enum_variant(&expression, file, semantic, enum_info)? {
        let replacement = format!("{}::{variant}", enum_info.path);
        let current = slice_expr(&expression);
        return Ok(Producer {
            place: None,
            replacement: (current != replacement).then_some(replacement),
            range: expression.syntax().text_range(),
            file: file.clone(),
        });
    }
    if let Some(place) = call_return_place(&expression, file, places, semantic)? {
        return Ok(Producer {
            place: Some(place),
            replacement: None,
            range: expression.syntax().text_range(),
            file: file.clone(),
        });
    }
    if let Some(place) = expression_place(&expression, file, places, semantic)? {
        return Ok(Producer {
            place: Some(place),
            replacement: None,
            range: expression.syntax().text_range(),
            file: file.clone(),
        });
    }
    if expression_is_validated_enum(&expression, file, semantic, enum_info)? {
        return Ok(Producer {
            place: None,
            replacement: None,
            range: expression.syntax().text_range(),
            file: file.clone(),
        });
    }
    refuse(
        "UNSUPPORTED_ARGUMENT_SOURCE",
        format!(
            "`{}` is not a known enum variant, compatibility constant, validated enum value, parameter, field, local, or function return",
            slice_expr(&expression)
        ),
        Some(file.clone()),
        Some(expression.syntax().text_range()),
    )
}

fn connected_consumers(
    place: &Place,
    places: &BTreeMap<String, Place>,
    files: &BTreeMap<Utf8PathBuf, &ParsedFile>,
    semantic: &SemanticProject,
    enum_info: &EnumInfo,
) -> std::result::Result<Vec<String>, FlowError> {
    if matches!(place.kind, PlaceKind::Return { .. }) {
        return return_consumers(place, places, files, semantic, enum_info);
    }
    let references = semantic.references_to(&place.file, place.name_range)?;
    let mut connected = BTreeSet::new();
    for reference in references {
        let parsed = files
            .get(&reference.file)
            .ok_or_else(|| anyhow!("reference file was not parsed"))?;
        if inside_enum_conversion(
            parsed,
            reference.range,
            &reference.file,
            semantic,
            enum_info,
        )? {
            continue;
        }
        if let Some(target) = argument_target(parsed, reference.range, places, semantic)? {
            connected.insert(target);
            continue;
        }
        if let Some(target) = assigned_place_target(parsed, reference.range, places, semantic)? {
            connected.insert(target);
            continue;
        }
        if let Some(target) = initializer_target(parsed, reference.range, places, semantic)? {
            connected.insert(target);
            continue;
        }
        if let Some(target) = return_target(parsed, reference.range, places)? {
            connected.insert(target);
        }
    }
    Ok(connected.into_iter().collect())
}

fn return_consumers(
    place: &Place,
    places: &BTreeMap<String, Place>,
    files: &BTreeMap<Utf8PathBuf, &ParsedFile>,
    semantic: &SemanticProject,
    enum_info: &EnumInfo,
) -> std::result::Result<Vec<String>, FlowError> {
    let mut connected = BTreeSet::new();
    for reference in semantic.references_to(&place.file, place.name_range)? {
        let parsed = files
            .get(&reference.file)
            .ok_or_else(|| anyhow!("return reference file was not parsed"))?;
        if reference_in_use(parsed, reference.range) {
            continue;
        }
        let Some(call) = direct_call_at_reference(parsed, reference.range) else {
            return refuse(
                "UNRESOLVED_FUNCTION_REFERENCE",
                "a returning function reference is not a supported direct call",
                Some(reference.file),
                Some(reference.range),
            );
        };
        if inside_enum_conversion(
            parsed,
            call.syntax().text_range(),
            &reference.file,
            semantic,
            enum_info,
        )? {
            continue;
        }
        let range = call.syntax().text_range();
        for target in flow_targets(parsed, range, places, semantic)? {
            connected.insert(target);
        }
    }
    Ok(connected.into_iter().collect())
}

fn rewrite_place_uses(
    place: &Place,
    selected: &BTreeSet<String>,
    places: &BTreeMap<String, Place>,
    files: &BTreeMap<Utf8PathBuf, &ParsedFile>,
    semantic: &SemanticProject,
    enum_info: &EnumInfo,
    builder: &mut PlanBuilder,
) -> std::result::Result<(), FlowError> {
    if matches!(place.kind, PlaceKind::Return { .. }) {
        return rewrite_return_uses(place, selected, places, files, semantic, enum_info, builder);
    }
    let references = semantic.references_to(&place.file, place.name_range)?;
    for reference in references {
        let parsed = files
            .get(&reference.file)
            .ok_or_else(|| anyhow!("reference file was not parsed"))?;
        if builder.covers(&reference.file, reference.range) {
            continue;
        }
        if ancestor_at::<ast::MacroCall>(parsed, reference.range).is_some() {
            return refuse(
                "MACRO_REFERENCE",
                "a selected value is referenced inside a macro",
                Some(reference.file),
                Some(reference.range),
            );
        }
        if let Some(reference_expr) = ancestor_at::<ast::RefExpr>(parsed, reference.range) {
            if reference_expr
                .expr()
                .is_some_and(|expr| expr.syntax().text_range().contains_range(reference.range))
            {
                return refuse(
                    "RAW_VALUE_OBSERVED",
                    "a selected value is borrowed directly; the raw reference cannot be recreated from a temporary conversion",
                    Some(reference.file),
                    Some(reference_expr.syntax().text_range()),
                );
            }
        }
        if let Some(call) = enum_from_raw_call_at(
            parsed,
            reference.range,
            &reference.file,
            semantic,
            enum_info,
        )? {
            rewrite_from_raw_consumer(parsed, &reference.file, &call, enum_info, builder)?;
            continue;
        }
        if enum_to_raw_call_at(
            parsed,
            reference.range,
            &reference.file,
            semantic,
            enum_info,
        )?
        .is_some()
        {
            continue;
        }
        if let Some(binary) = ancestor_at::<ast::BinExpr>(parsed, reference.range) {
            if let Some(replacement) =
                rewrite_direct_comparison(&binary, &reference.file, semantic, enum_info)?
            {
                builder.add(&reference.file, binary.syntax().text_range(), replacement)?;
                continue;
            }
            if binary
                .lhs()
                .is_some_and(|lhs| lhs.syntax().text_range().contains_range(reference.range))
                && matches!(binary.op_kind(), Some(BinaryOp::Assignment { .. }))
            {
                continue;
            }
        }
        if let Some(target) = argument_target(parsed, reference.range, places, semantic)? {
            if selected.contains(&target) {
                continue;
            }
        }
        if let Some(target) = assigned_place_target(parsed, reference.range, places, semantic)? {
            if selected.contains(&target) {
                continue;
            }
        }
        if let Some(target) = initializer_target(parsed, reference.range, places, semantic)? {
            if selected.contains(&target) {
                continue;
            }
        }
        if let Some(target) = return_target(parsed, reference.range, places)? {
            if selected.contains(&target) {
                continue;
            }
        }
        if is_record_field_name(parsed, reference.range) {
            continue;
        }
        if direct_enum_compatible_use(
            parsed,
            reference.range,
            &reference.file,
            semantic,
            enum_info,
        )? {
            continue;
        }
        builder.add(
            &reference.file,
            reference.range,
            format!("{}.to_raw()", text_at(&parsed.source, reference.range)),
        )?;
        builder.inserted_conversions += 1;
    }
    Ok(())
}

fn rewrite_return_uses(
    place: &Place,
    selected: &BTreeSet<String>,
    places: &BTreeMap<String, Place>,
    files: &BTreeMap<Utf8PathBuf, &ParsedFile>,
    semantic: &SemanticProject,
    enum_info: &EnumInfo,
    builder: &mut PlanBuilder,
) -> std::result::Result<(), FlowError> {
    for reference in semantic.references_to(&place.file, place.name_range)? {
        let parsed = files
            .get(&reference.file)
            .ok_or_else(|| anyhow!("return reference file was not parsed"))?;
        if reference_in_use(parsed, reference.range) {
            continue;
        }
        let Some(call) = direct_call_at_reference(parsed, reference.range) else {
            return refuse(
                "UNRESOLVED_FUNCTION_REFERENCE",
                "a returning function reference is not a supported direct call",
                Some(reference.file),
                Some(reference.range),
            );
        };
        let range = call.syntax().text_range();
        if builder.covers(&reference.file, range) {
            continue;
        }
        if let Some(from_raw) =
            enum_from_raw_call_at(parsed, range, &reference.file, semantic, enum_info)?
        {
            rewrite_from_raw_consumer(parsed, &reference.file, &from_raw, enum_info, builder)?;
            continue;
        }
        if let Some(binary) = ancestor_at::<ast::BinExpr>(parsed, range) {
            if rewrite_return_comparison(
                &binary,
                range,
                &reference.file,
                semantic,
                enum_info,
                builder,
            )? {
                continue;
            }
        }
        let targets = flow_targets(parsed, range, places, semantic)?;
        if targets.iter().any(|target| selected.contains(target)) {
            continue;
        }
        if direct_enum_compatible_use(parsed, range, &reference.file, semantic, enum_info)? {
            continue;
        }
        builder.add(&reference.file, TextRange::empty(range.end()), ".to_raw()")?;
        builder.inserted_conversions += 1;
    }
    Ok(())
}

// Remaining syntax and semantic helpers are deliberately small and conservative. They only
// accept shapes covered by the command's tests; every unknown reference refuses or becomes an
// explicit raw sink.
fn call_argument(file: &ParsedFile, range: TextRange, index: usize) -> Option<ast::Expr> {
    let call = ancestor_at::<ast::CallExpr>(file, range)?;
    let callee = call.expr()?;
    callee
        .syntax()
        .text_range()
        .contains_range(range)
        .then_some(())?;
    call.arg_list()?.args().nth(index)
}

fn method_argument(file: &ParsedFile, range: TextRange, index: usize) -> Option<ast::Expr> {
    let call = ancestor_at::<ast::MethodCallExpr>(file, range)?;
    call.name_ref()?
        .syntax()
        .text_range()
        .contains_range(range)
        .then_some(())?;
    call.arg_list()?.args().nth(index)
}

fn argument_target(
    file: &ParsedFile,
    range: TextRange,
    places: &BTreeMap<String, Place>,
    semantic: &SemanticProject,
) -> Result<Option<String>> {
    let source_range = flow_source_range(file, range);
    for call in ancestors_at::<ast::CallExpr>(file, range) {
        let arguments = call
            .arg_list()
            .map(|args| args.args().collect::<Vec<_>>())
            .unwrap_or_default();
        if let Some(index) = arguments
            .iter()
            .position(|arg| unwrap_parens(arg.clone()).syntax().text_range() == source_range)
        {
            let Some(callee) = call.expr() else {
                return Ok(None);
            };
            let Some(name_ref) = callee
                .syntax()
                .descendants()
                .filter_map(ast::NameRef::cast)
                .last()
            else {
                return Ok(None);
            };
            let Some(definition) =
                semantic.definition_at(&file.path, name_ref.syntax().text_range())?
            else {
                return Ok(None);
            };
            if let Some(place) = parameter_for_function(places, &definition, index, false) {
                return Ok(Some(place));
            }
        }
    }
    for call in ancestors_at::<ast::MethodCallExpr>(file, range) {
        let arguments = call
            .arg_list()
            .map(|args| args.args().collect::<Vec<_>>())
            .unwrap_or_default();
        if let Some(index) = arguments
            .iter()
            .position(|arg| unwrap_parens(arg.clone()).syntax().text_range() == source_range)
        {
            let Some(name_ref) = call.name_ref() else {
                return Ok(None);
            };
            let Some(definition) =
                semantic.definition_at(&file.path, name_ref.syntax().text_range())?
            else {
                return Ok(None);
            };
            if let Some(place) = parameter_for_function(places, &definition, index, true) {
                return Ok(Some(place));
            }
        }
    }
    Ok(None)
}

fn parameter_for_function(
    places: &BTreeMap<String, Place>,
    definition: &SemanticDefinition,
    index: usize,
    method: bool,
) -> Option<String> {
    places.values().find_map(|place| match &place.kind {
        PlaceKind::Parameter {
            function_name_range,
            parameter_index,
            method: is_method,
            ..
        } if place.file == definition.file
            && *function_name_range == definition.name_range
            && *parameter_index == index
            && *is_method == method =>
        {
            Some(place.id.clone())
        }
        _ => None,
    })
}

fn assigned_place_target(
    file: &ParsedFile,
    range: TextRange,
    places: &BTreeMap<String, Place>,
    semantic: &SemanticProject,
) -> Result<Option<String>> {
    let source_range = flow_source_range(file, range);
    for binary in ancestors_at::<ast::BinExpr>(file, range) {
        if !matches!(binary.op_kind(), Some(BinaryOp::Assignment { op: None }))
            || !binary
                .rhs()
                .is_some_and(|rhs| unwrap_parens(rhs).syntax().text_range() == source_range)
        {
            continue;
        }
        let name_ref = match binary.lhs() {
            Some(ast::Expr::FieldExpr(field)) => field.name_ref(),
            Some(ast::Expr::PathExpr(path)) => path
                .path()
                .and_then(|path| path.segment())
                .and_then(|segment| segment.name_ref()),
            _ => None,
        };
        let Some(name_ref) = name_ref else { continue };
        let Some(definition) =
            semantic.definition_at(&file.path, name_ref.syntax().text_range())?
        else {
            continue;
        };
        if let Some(place) = place_for_definition(places, &definition) {
            return Ok(Some(place));
        }
    }
    Ok(None)
}

fn initializer_target(
    file: &ParsedFile,
    range: TextRange,
    places: &BTreeMap<String, Place>,
    _semantic: &SemanticProject,
) -> Result<Option<String>> {
    let source_range = flow_source_range(file, range);
    for statement in ancestors_at::<ast::LetStmt>(file, range) {
        if !statement
            .initializer()
            .is_some_and(|expr| unwrap_parens(expr).syntax().text_range() == source_range)
        {
            continue;
        }
        let Some(name) = statement
            .pat()
            .and_then(|pat| ast::IdentPat::cast(pat.syntax().clone()))
            .and_then(|ident| ident.name())
        else {
            continue;
        };
        if let Some(place) = places.get(&place_id(&file.path, name.syntax().text_range())) {
            return Ok(Some(place.id.clone()));
        }
    }
    Ok(None)
}

fn return_target(
    file: &ParsedFile,
    range: TextRange,
    places: &BTreeMap<String, Place>,
) -> Result<Option<String>> {
    let source_range = flow_source_range(file, range);
    for returned in ancestors_at::<ast::ReturnExpr>(file, range) {
        if returned
            .expr()
            .is_some_and(|expr| unwrap_parens(expr).syntax().text_range() == source_range)
        {
            if let Some(function) = returned
                .syntax()
                .ancestors()
                .skip(1)
                .find_map(ast::Fn::cast)
            {
                if let Some(name) = function.name() {
                    return Ok(places
                        .get(&return_place_id(&file.path, name.syntax().text_range()))
                        .map(|place| place.id.clone()));
                }
            }
        }
    }
    for function in ancestors_at::<ast::Fn>(file, range) {
        let Some(tail) = function
            .body()
            .and_then(|body| body.stmt_list())
            .and_then(|list| list.tail_expr())
        else {
            continue;
        };
        if unwrap_parens(tail).syntax().text_range() == source_range {
            if let Some(name) = function.name() {
                return Ok(places
                    .get(&return_place_id(&file.path, name.syntax().text_range()))
                    .map(|place| place.id.clone()));
            }
        }
    }
    Ok(None)
}

fn flow_source_range(file: &ParsedFile, range: TextRange) -> TextRange {
    if ancestors_at::<ast::CallExpr>(file, range)
        .into_iter()
        .any(|call| call.syntax().text_range() == range)
        || ancestors_at::<ast::MethodCallExpr>(file, range)
            .into_iter()
            .any(|call| call.syntax().text_range() == range)
    {
        return range;
    }
    if let Some(call) = direct_call_at_reference(file, range) {
        if range.contains_range(call.syntax().text_range())
            || call.syntax().text_range().contains_range(range)
        {
            return call.syntax().text_range();
        }
    }
    if let Some(field) = ancestor_at::<ast::FieldExpr>(file, range) {
        return field.syntax().text_range();
    }
    if let Some(path) = ancestor_at::<ast::PathExpr>(file, range) {
        return path.syntax().text_range();
    }
    range
}

fn flow_targets(
    file: &ParsedFile,
    range: TextRange,
    places: &BTreeMap<String, Place>,
    semantic: &SemanticProject,
) -> Result<Vec<String>> {
    let mut targets = BTreeSet::new();
    if let Some(target) = argument_target(file, range, places, semantic)? {
        targets.insert(target);
    }
    if let Some(target) = assigned_place_target(file, range, places, semantic)? {
        targets.insert(target);
    }
    if let Some(target) = initializer_target(file, range, places, semantic)? {
        targets.insert(target);
    }
    if let Some(target) = return_target(file, range, places)? {
        targets.insert(target);
    }
    Ok(targets.into_iter().collect())
}

fn direct_call_at_reference(file: &ParsedFile, range: TextRange) -> Option<ast::Expr> {
    for call in ancestors_at::<ast::CallExpr>(file, range) {
        if call
            .expr()
            .is_some_and(|callee| callee.syntax().text_range().contains_range(range))
        {
            return Some(ast::Expr::CallExpr(call));
        }
    }
    for call in ancestors_at::<ast::MethodCallExpr>(file, range) {
        if call
            .name_ref()
            .is_some_and(|name| name.syntax().text_range().contains_range(range))
        {
            return Some(ast::Expr::MethodCallExpr(call));
        }
    }
    None
}

fn expression_place(
    expression: &ast::Expr,
    file: &Utf8PathBuf,
    places: &BTreeMap<String, Place>,
    semantic: &SemanticProject,
) -> Result<Option<String>> {
    let name_ref = match expression {
        ast::Expr::PathExpr(path) => path
            .path()
            .and_then(|path| path.segment())
            .and_then(|segment| segment.name_ref()),
        ast::Expr::FieldExpr(field) => field.name_ref(),
        _ => None,
    };
    let Some(name_ref) = name_ref else {
        return Ok(None);
    };
    let Some(definition) = semantic.definition_at(file, name_ref.syntax().text_range())? else {
        return Ok(None);
    };
    Ok(place_for_definition(places, &definition))
}

fn call_return_place(
    expression: &ast::Expr,
    file: &Utf8PathBuf,
    places: &BTreeMap<String, Place>,
    semantic: &SemanticProject,
) -> Result<Option<String>> {
    let name_ref = match expression {
        ast::Expr::CallExpr(call) => call.expr().and_then(|callee| {
            callee
                .syntax()
                .descendants()
                .filter_map(ast::NameRef::cast)
                .last()
        }),
        ast::Expr::MethodCallExpr(call) => call.name_ref(),
        _ => None,
    };
    let Some(name_ref) = name_ref else {
        return Ok(None);
    };
    let Some(definition) = semantic.definition_at(file, name_ref.syntax().text_range())? else {
        return Ok(None);
    };
    Ok(return_for_function(places, &definition))
}

fn place_for_definition(
    places: &BTreeMap<String, Place>,
    definition: &SemanticDefinition,
) -> Option<String> {
    places
        .values()
        .find(|place| place.file == definition.file && place.name_range == definition.name_range)
        .map(|place| place.id.clone())
}

fn return_for_function(
    places: &BTreeMap<String, Place>,
    definition: &SemanticDefinition,
) -> Option<String> {
    places.values().find_map(|place| match place.kind {
        PlaceKind::Return {
            function_name_range,
        } if place.file == definition.file && function_name_range == definition.name_range => {
            Some(place.id.clone())
        }
        _ => None,
    })
}

fn collect_enum_info(
    enum_file: &Utf8PathBuf,
    enum_name: &str,
    enum_path: &str,
    files: &[ParsedFile],
    semantic: &SemanticProject,
) -> std::result::Result<EnumInfo, FlowError> {
    let parsed = files
        .iter()
        .find(|file| &file.path == enum_file)
        .ok_or_else(|| anyhow!("enum file was not parsed"))?;
    let candidates = parsed
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::Enum::cast)
        .filter(|item| item.name().is_some_and(|name| name.text() == enum_name))
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        return refuse(
            "ENUM_NOT_FOUND",
            format!(
                "expected one enum named `{enum_name}`, found {}",
                candidates.len()
            ),
            Some(enum_file.clone()),
            None,
        );
    }
    let enumeration = &candidates[0];
    let raw_type = enumeration
        .attrs()
        .find_map(|attr| {
            let text = attr.syntax().text().to_string();
            text.strip_prefix("#[repr(")
                .and_then(|text| text.strip_suffix(")]"))
                .map(str::to_owned)
        })
        .filter(|raw| {
            matches!(
                raw.as_str(),
                "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize"
            )
        })
        .ok_or_else(|| {
            FlowError::Refused(Refusal {
                code: "UNSUPPORTED_ENUM",
                message: "the enum needs one primitive integer #[repr(...)]".into(),
                file: Some(enum_file.clone()),
                range: Some(enumeration.syntax().text_range()),
            })
        })?;
    let mut variants = BTreeMap::new();
    let mut values = BTreeMap::new();
    for variant in enumeration
        .variant_list()
        .into_iter()
        .flat_map(|list| list.variants())
    {
        let Some(name) = variant.name() else { continue };
        variants.insert(
            definition_key(enum_file, name.syntax().text_range()),
            name.text().to_string(),
        );
        if let Some(expr) = variant.const_arg().and_then(|argument| argument.expr()) {
            if let Some(value) = parse_integer(&expr.syntax().text().to_string(), &raw_type) {
                values.insert(value, name.text().to_string());
            }
        }
    }
    let mut from_raw = None;
    let mut to_raw = None;
    for item in parsed
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::Impl::cast)
    {
        if item
            .self_ty()
            .is_none_or(|ty| ty.syntax().text().to_string().trim() != enum_name)
        {
            continue;
        }
        for function in item
            .assoc_item_list()
            .into_iter()
            .flat_map(|list| list.assoc_items())
            .filter_map(|item| match item {
                ast::AssocItem::Fn(function) => Some(function),
                _ => None,
            })
        {
            let Some(name) = function.name() else {
                continue;
            };
            let key = definition_key(enum_file, name.syntax().text_range());
            match name.text().as_str() {
                "from_raw" => from_raw = Some(key),
                "to_raw" => to_raw = Some(key),
                _ => {}
            }
        }
    }
    let mut aliases = BTreeMap::new();
    for file in files {
        for item in file
            .tree
            .syntax()
            .descendants()
            .filter_map(ast::Const::cast)
        {
            let (Some(name), Some(expr)) = (
                item.name(),
                item.syntax().children().find_map(ast::Expr::cast),
            ) else {
                continue;
            };
            if let Some(variant) = syntactic_to_raw_variant(&expr, enum_name) {
                aliases.insert(
                    definition_key(&file.path, name.syntax().text_range()),
                    variant,
                );
            }
        }
    }
    // Ensure rust-analyzer can resolve the enum itself before planning cross-file edits.
    let enum_name_range = enumeration.name().unwrap().syntax().text_range();
    let _ = semantic.references_to(enum_file, enum_name_range)?;
    Ok(EnumInfo {
        name: enum_name.to_owned(),
        path: enum_path.to_owned(),
        raw_type,
        variants,
        aliases,
        values,
        from_raw,
        to_raw,
    })
}

fn enum_variant(
    expression: &ast::Expr,
    file: &Utf8PathBuf,
    semantic: &SemanticProject,
    info: &EnumInfo,
) -> Result<Option<String>> {
    let name_ref = match expression {
        ast::Expr::PathExpr(path) => path
            .path()
            .and_then(|path| path.segment())
            .and_then(|segment| segment.name_ref()),
        _ => None,
    };
    if let Some(name_ref) = name_ref {
        if let Some(definition) = semantic.definition_at(file, name_ref.syntax().text_range())? {
            let key = SemanticDefinitionKey::from(definition);
            if let Some(variant) = info.variants.get(&key).or_else(|| info.aliases.get(&key)) {
                return Ok(Some(variant.clone()));
            }
        }
    }
    if let ast::Expr::Literal(literal) = expression {
        if let Some(value) = parse_integer(&literal.syntax().text().to_string(), &info.raw_type) {
            return Ok(info.values.get(&value).cloned());
        }
    }
    Ok(None)
}

fn enum_to_raw_receiver(
    expression: &ast::Expr,
    file: &Utf8PathBuf,
    semantic: &SemanticProject,
    info: &EnumInfo,
) -> Result<Option<(ast::Expr, TextRange)>> {
    let ast::Expr::MethodCallExpr(call) = expression else {
        return Ok(None);
    };
    let Some(name) = call.name_ref() else {
        return Ok(None);
    };
    if name.text() != "to_raw" {
        return Ok(None);
    }
    if !definition_matches(
        semantic.definition_at(file, name.syntax().text_range())?,
        info.to_raw.as_ref(),
    ) {
        return Ok(None);
    }
    Ok(call
        .receiver()
        .map(|receiver| (receiver, name.syntax().text_range())))
}

fn enum_from_raw_call_at(
    parsed: &ParsedFile,
    range: TextRange,
    file: &Utf8PathBuf,
    semantic: &SemanticProject,
    info: &EnumInfo,
) -> Result<Option<ast::CallExpr>> {
    for call in ancestors_at::<ast::CallExpr>(parsed, range) {
        let Some(callee) = call.expr() else { continue };
        let Some(name) = callee
            .syntax()
            .descendants()
            .filter_map(ast::NameRef::cast)
            .last()
        else {
            continue;
        };
        if name.text() == "from_raw"
            && definition_matches(
                semantic.definition_at(file, name.syntax().text_range())?,
                info.from_raw.as_ref(),
            )
            && call.arg_list().is_some_and(|args| {
                args.args().count() == 1
                    && args
                        .args()
                        .next()
                        .unwrap()
                        .syntax()
                        .text_range()
                        .contains_range(range)
            })
        {
            return Ok(Some(call));
        }
    }
    Ok(None)
}

fn enum_to_raw_call_at(
    parsed: &ParsedFile,
    range: TextRange,
    file: &Utf8PathBuf,
    semantic: &SemanticProject,
    info: &EnumInfo,
) -> Result<Option<ast::MethodCallExpr>> {
    for call in ancestors_at::<ast::MethodCallExpr>(parsed, range) {
        let Some(name) = call.name_ref() else {
            continue;
        };
        if name.text() == "to_raw"
            && definition_matches(
                semantic.definition_at(file, name.syntax().text_range())?,
                info.to_raw.as_ref(),
            )
            && call
                .receiver()
                .is_some_and(|receiver| receiver.syntax().text_range().contains_range(range))
        {
            return Ok(Some(call));
        }
    }
    Ok(None)
}

fn inside_enum_conversion(
    parsed: &ParsedFile,
    range: TextRange,
    file: &Utf8PathBuf,
    semantic: &SemanticProject,
    info: &EnumInfo,
) -> Result<bool> {
    Ok(
        enum_from_raw_call_at(parsed, range, file, semantic, info)?.is_some()
            || enum_to_raw_call_at(parsed, range, file, semantic, info)?.is_some(),
    )
}

fn rewrite_from_raw_consumer(
    parsed: &ParsedFile,
    file: &Utf8PathBuf,
    call: &ast::CallExpr,
    info: &EnumInfo,
    builder: &mut PlanBuilder,
) -> std::result::Result<(), FlowError> {
    let argument = call
        .arg_list()
        .and_then(|args| args.args().next())
        .ok_or_else(|| anyhow!("from_raw has no argument"))?;
    if let Some(match_expr) = call
        .syntax()
        .ancestors()
        .find_map(ast::MatchExpr::cast)
        .filter(|item| {
            item.expr()
                .is_some_and(|expr| expr.syntax().text_range() == call.syntax().text_range())
        })
    {
        let replacement = rewrite_option_match(parsed, &match_expr, &argument, info)?;
        builder.add(file, match_expr.syntax().text_range(), replacement)?;
        builder.removed_conversions += 1;
        return Ok(());
    }
    if let Some(binary) = call.syntax().parent().and_then(ast::BinExpr::cast) {
        if let Some(replacement) = rewrite_option_comparison(parsed, &binary, call, &argument, info)
        {
            builder.add(file, binary.syntax().text_range(), replacement)?;
            builder.removed_conversions += 1;
            return Ok(());
        }
    }
    builder.add(
        file,
        call.syntax().text_range(),
        format!("Some({})", slice_expr(&argument)),
    )?;
    builder.removed_conversions += 1;
    Ok(())
}

fn rewrite_option_match(
    parsed: &ParsedFile,
    item: &ast::MatchExpr,
    argument: &ast::Expr,
    info: &EnumInfo,
) -> std::result::Result<String, FlowError> {
    let mut replacements = vec![(
        item.expr().unwrap().syntax().text_range(),
        slice_expr(argument),
    )];
    let arms = item
        .match_arm_list()
        .ok_or_else(|| anyhow!("match has no arms"))?;
    for arm in arms.arms() {
        let Some(pattern) = arm.pat() else { continue };
        let text = pattern.syntax().text().to_string();
        if text.trim() == "None" {
            return refuse(
                "INVALID_VALUE_POLICY_REQUIRED",
                "an explicit None arm observes invalid raw values",
                Some(parsed.path.clone()),
                Some(pattern.syntax().text_range()),
            );
        }
        collect_some_pattern_rewrites(&pattern, info, &mut replacements)?;
    }
    replace_within(&parsed.source, item.syntax().text_range(), replacements)
}

fn collect_some_pattern_rewrites(
    pattern: &ast::Pat,
    info: &EnumInfo,
    replacements: &mut Vec<(TextRange, String)>,
) -> std::result::Result<(), FlowError> {
    let text = pattern.syntax().text().to_string();
    if let Some(inner) = text
        .trim()
        .strip_prefix("Some(")
        .and_then(|text| text.strip_suffix(')'))
    {
        if inner.contains(&info.name) {
            replacements.push((pattern.syntax().text_range(), inner.to_owned()));
            return Ok(());
        }
    }
    if let ast::Pat::OrPat(or) = pattern {
        for child in or.pats() {
            collect_some_pattern_rewrites(&child, info, replacements)?;
        }
    }
    if let ast::Pat::ParenPat(paren) = pattern {
        if let Some(child) = paren.pat() {
            collect_some_pattern_rewrites(&child, info, replacements)?;
        }
    }
    Ok(())
}

fn rewrite_option_comparison(
    _parsed: &ParsedFile,
    binary: &ast::BinExpr,
    call: &ast::CallExpr,
    argument: &ast::Expr,
    info: &EnumInfo,
) -> Option<String> {
    let operator = match binary.op_kind()? {
        BinaryOp::CmpOp(ra_ap_syntax::ast::CmpOp::Eq { negated }) => {
            if negated {
                "!="
            } else {
                "=="
            }
        }
        _ => return None,
    };
    let (lhs, rhs) = (binary.lhs()?, binary.rhs()?);
    let other = if lhs.syntax().text_range() == call.syntax().text_range() {
        rhs
    } else if rhs.syntax().text_range() == call.syntax().text_range() {
        lhs
    } else {
        return None;
    };
    let text = other.syntax().text().to_string();
    let inner = text.trim().strip_prefix("Some(")?.strip_suffix(')')?;
    if !inner.contains(&info.name) {
        return None;
    }
    Some(format!("{} {operator} {inner}", slice_expr(argument)))
}

fn rewrite_direct_comparison(
    binary: &ast::BinExpr,
    file: &Utf8PathBuf,
    semantic: &SemanticProject,
    info: &EnumInfo,
) -> Result<Option<String>> {
    let operator = match binary.op_kind() {
        Some(BinaryOp::CmpOp(ra_ap_syntax::ast::CmpOp::Eq { negated })) => {
            if negated {
                "!="
            } else {
                "=="
            }
        }
        _ => return Ok(None),
    };
    let (Some(lhs), Some(rhs)) = (binary.lhs(), binary.rhs()) else {
        return Ok(None);
    };
    let left = enum_variant(&lhs, file, semantic, info)?;
    let right = enum_variant(&rhs, file, semantic, info)?;
    match (left, right) {
        (Some(variant), None) => Ok(Some(format!(
            "{}::{variant} {operator} {}",
            info.path,
            slice_expr(&rhs)
        ))),
        (None, Some(variant)) => Ok(Some(format!(
            "{} {operator} {}::{variant}",
            slice_expr(&lhs),
            info.path
        ))),
        _ => Ok(None),
    }
}

fn rewrite_return_comparison(
    binary: &ast::BinExpr,
    call_range: TextRange,
    file: &Utf8PathBuf,
    semantic: &SemanticProject,
    info: &EnumInfo,
    builder: &mut PlanBuilder,
) -> std::result::Result<bool, FlowError> {
    if !matches!(binary.op_kind(), Some(BinaryOp::CmpOp(CmpOp::Eq { .. }))) {
        return Ok(false);
    }
    let (Some(lhs), Some(rhs)) = (binary.lhs(), binary.rhs()) else {
        return Ok(false);
    };
    let other = if lhs.syntax().text_range().contains_range(call_range) {
        rhs
    } else if rhs.syntax().text_range().contains_range(call_range) {
        lhs
    } else {
        return Ok(false);
    };
    let Some(variant) = enum_variant(&other, file, semantic, info)? else {
        return Ok(false);
    };
    builder.add(
        file,
        other.syntax().text_range(),
        format!("{}::{variant}", info.path),
    )?;
    Ok(true)
}

fn direct_enum_compatible_use(
    parsed: &ParsedFile,
    range: TextRange,
    file: &Utf8PathBuf,
    semantic: &SemanticProject,
    info: &EnumInfo,
) -> Result<bool> {
    if let Some(binary) = ancestor_at::<ast::BinExpr>(parsed, range) {
        if rewrite_direct_comparison(&binary, file, semantic, info)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn expression_is_validated_enum(
    expression: &ast::Expr,
    file: &Utf8PathBuf,
    semantic: &SemanticProject,
    info: &EnumInfo,
) -> Result<bool> {
    let ast::Expr::PathExpr(path) = expression else {
        return Ok(false);
    };
    let Some(name) = path
        .path()
        .and_then(|path| path.segment())
        .and_then(|segment| segment.name_ref())
    else {
        return Ok(false);
    };
    let Some(definition) = semantic.definition_at(file, name.syntax().text_range())? else {
        return Ok(false);
    };
    // A local bound by `let Some(name) = Enum::from_raw(...)` is already enum-typed.
    let parsed = analysis::parse_source_file(&definition.file)?;
    let Some(let_stmt) = parsed
        .tree
        .syntax()
        .descendants()
        .filter_map(ast::LetStmt::cast)
        .find(|stmt| {
            stmt.pat().is_some_and(|pat| {
                pat.syntax()
                    .text_range()
                    .contains_range(definition.name_range)
            })
        })
    else {
        return Ok(false);
    };
    let pattern = let_stmt
        .pat()
        .map(|pat| pat.syntax().text().to_string())
        .unwrap_or_default();
    let initializer = let_stmt
        .initializer()
        .map(|expr| expr.syntax().text().to_string())
        .unwrap_or_default();
    Ok(pattern.trim().starts_with("Some(")
        && initializer.contains(&format!("{}::from_raw", info.name)))
}

fn syntactic_to_raw_variant(expression: &ast::Expr, enum_name: &str) -> Option<String> {
    let ast::Expr::MethodCallExpr(call) = expression else {
        return None;
    };
    if call.name_ref()?.text() != "to_raw" {
        return None;
    }
    let receiver = call.receiver()?;
    let ast::Expr::PathExpr(path) = receiver else {
        return None;
    };
    let text = path.path()?.syntax().text().to_string();
    let (prefix, variant) = text.rsplit_once("::")?;
    (prefix == enum_name || prefix.ends_with(&format!("::{enum_name}"))).then(|| variant.to_owned())
}

fn definition_matches(
    actual: Option<SemanticDefinition>,
    expected: Option<&SemanticDefinitionKey>,
) -> bool {
    actual.zip(expected).is_some_and(|(actual, expected)| {
        definition_key(&actual.file, actual.name_range) == *expected
    })
}

fn definition_key(file: &Utf8PathBuf, range: TextRange) -> SemanticDefinitionKey {
    SemanticDefinitionKey {
        file: file.clone(),
        start: range.start().into(),
        end: range.end().into(),
    }
}

fn reference_in_use(file: &ParsedFile, range: TextRange) -> bool {
    ancestor_at::<ast::Use>(file, range).is_some()
}
fn is_record_field_name(file: &ParsedFile, range: TextRange) -> bool {
    ancestor_at::<ast::RecordExprField>(file, range).is_some_and(|field| {
        field
            .name_ref()
            .is_some_and(|name| name.syntax().text_range().contains_range(range))
    })
}

fn ancestor_at<N: AstNode>(file: &ParsedFile, range: TextRange) -> Option<N> {
    ancestors_at::<N>(file, range).into_iter().next()
}
fn ancestors_at<N: AstNode>(file: &ParsedFile, range: TextRange) -> Vec<N> {
    let token = file
        .tree
        .syntax()
        .token_at_offset(range.start())
        .right_biased();
    token
        .into_iter()
        .flat_map(|token| token.parent_ancestors())
        .filter_map(N::cast)
        .collect()
}

fn unwrap_parens(mut expression: ast::Expr) -> ast::Expr {
    loop {
        if let ast::Expr::ParenExpr(paren) = &expression {
            if let Some(inner) = paren.expr() {
                expression = inner;
                continue;
            }
        }
        return expression;
    }
}
fn slice_expr(expression: &ast::Expr) -> String {
    expression.syntax().text().to_string()
}
fn text_at(source: &str, range: TextRange) -> &str {
    &source[u32::from(range.start()) as usize..u32::from(range.end()) as usize]
}

fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset.min(source.len())];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    (line, offset - line_start + 1)
}
fn replace_within(
    source: &str,
    whole: TextRange,
    mut replacements: Vec<(TextRange, String)>,
) -> std::result::Result<String, FlowError> {
    replacements.sort_by_key(|(range, _)| range.start());
    for pair in replacements.windows(2) {
        if pair[0].0.end() > pair[1].0.start() {
            return refuse(
                "CONFLICTING_EDITS",
                "overlapping expression rewrites",
                None,
                Some(whole),
            );
        }
    }
    let mut output = text_at(source, whole).to_owned();
    for (range, replacement) in replacements.into_iter().rev() {
        let start = u32::from(range.start() - whole.start()) as usize;
        let end = u32::from(range.end() - whole.start()) as usize;
        output.replace_range(start..end, &replacement)
    }
    Ok(output)
}

fn parse_integer(source: &str, raw: &str) -> Option<i128> {
    let mut source = source.trim();
    while source.starts_with('(') && source.ends_with(')') {
        source = source[1..source.len() - 1].trim()
    }
    if let Some(value) = source.strip_suffix(raw) {
        source = value.trim_end_matches('_')
    }
    let clean = source.replace('_', "");
    let (negative, digits) = clean
        .strip_prefix('-')
        .map_or((false, clean.as_str()), |digits| (true, digits));
    let (radix, digits) = if let Some(v) = digits.strip_prefix("0x") {
        (16, v)
    } else if let Some(v) = digits.strip_prefix("0o") {
        (8, v)
    } else if let Some(v) = digits.strip_prefix("0b") {
        (2, v)
    } else {
        (10, digits)
    };
    i128::from_str_radix(digits, radix)
        .ok()
        .map(|value| if negative { -value } else { value })
}

fn parse_position(value: &str) -> std::result::Result<(&str, usize, usize), FlowError> {
    let mut parts = value.rsplitn(3, ':');
    let column = parts.next().and_then(|v| v.parse().ok());
    let line = parts.next().and_then(|v| v.parse().ok());
    let file = parts.next();
    match (file, line, column) {
        (Some(file), Some(line), Some(column)) if line > 0 && column > 0 => {
            Ok((file, line, column))
        }
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
    let canonical = fs::canonicalize(&joined).map_err(|error| anyhow!(error))?;
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
fn place_id(file: &Utf8Path, range: TextRange) -> String {
    format!("{}\0{}", file, u32::from(range.start()))
}
fn return_place_id(file: &Utf8Path, range: TextRange) -> String {
    format!("{}\0{}\0return", file, u32::from(range.start()))
}
fn relative<'a>(project: &'a Project, path: &'a Utf8PathBuf) -> &'a Utf8Path {
    path.strip_prefix(&project.root).unwrap_or(path)
}

fn validate_plan(plan: &RefactorPlan) -> std::result::Result<(), FlowError> {
    let mut by_file: BTreeMap<&Utf8PathBuf, Vec<&TextEdit>> = BTreeMap::new();
    for edit in &plan.edits {
        by_file.entry(&edit.file).or_default().push(edit)
    }
    for (file, edits) in by_file {
        let source = fs::read_to_string(file)?;
        let updated = apply_edits_to_string(&source, &edits)?;
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
    command: &EnumHoistCommand,
    status: &str,
    plan: Option<&RefactorPlan>,
    target: Option<&Value>,
    refusal: Option<&Refusal>,
) {
    let edits=plan.into_iter().flat_map(|plan|&plan.edits).map(|edit|json!({"file":edit.file,"range":range_json(edit.range),"replacement":edit.replacement})).collect::<Vec<_>>();
    let diagnostics=refusal.into_iter().map(|reason|json!({"code":reason.code,"message":reason.message,"file":reason.file,"range":reason.range.map(range_json)})).collect::<Vec<_>>();
    match command.format {
        OutputFormat::Json => println!(
            "{}",
            json!({"status":status,"target":target,"edits":edits,"diagnostics":diagnostics})
        ),
        OutputFormat::Text => {
            println!("{status}: {} edits", edits.len());
            if let Some(reason) = refusal {
                eprintln!("{}: {}", reason.code, reason.message)
            }
        }
    }
}
