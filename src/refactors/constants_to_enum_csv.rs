//! Load one reviewed enum family from the discovery CSV.
use anyhow::{Context, Result};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

use crate::cli::{ConstantsToEnumCommand, ConstantsToEnumCsvCommand, OutputFormat};

pub fn run(command: ConstantsToEnumCsvCommand) -> Result<i32> {
    if command.dry_run == command.write {
        return Ok(refuse(
            &command,
            "INVALID_MODE",
            "choose exactly one of --dry-run or --write",
        ));
    }
    let source = fs::read_to_string(&command.table)
        .with_context(|| format!("failed to read CSV table {}", command.table))?;
    let records = match parse_csv(&source) {
        Ok(records) => records,
        Err(message) => return Ok(refuse(&command, "INVALID_TABLE", &message)),
    };
    let Some(header) = records.first() else {
        return Ok(refuse(&command, "INVALID_TABLE", "CSV table is empty"));
    };
    let columns: BTreeMap<_, _> = header
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();
    let required = [
        "constant_name",
        "file",
        "type",
        "value",
        "proposed_enum_group",
        "proposed_group_kind",
        "proposed_enum_name",
        "proposed_variant",
        "alias_of",
        "match_selections",
        "comparison_selections",
        "possible_enum_groups",
    ];
    for name in required {
        if !columns.contains_key(name) {
            return Ok(refuse(
                &command,
                "INVALID_TABLE",
                &format!("CSV is missing `{name}` column"),
            ));
        }
    }
    let cell = |row: &[String], name: &str| -> String {
        columns
            .get(name)
            .and_then(|index| row.get(*index))
            .cloned()
            .unwrap_or_default()
    };
    let rows: Vec<_> = records
        .iter()
        .skip(1)
        .filter(|row| cell(row, "proposed_enum_group") == command.group)
        .collect();
    if rows.is_empty() {
        return Ok(refuse(
            &command,
            "GROUP_NOT_FOUND",
            &format!("no rows use group `{}`", command.group),
        ));
    }

    let kinds: BTreeSet<_> = rows
        .iter()
        .map(|row| cell(row, "proposed_group_kind"))
        .collect();
    if kinds.len() != 1 || kinds.first().is_none_or(|kind| kind != "enum") {
        return Ok(refuse(
            &command,
            "NOT_ENUM_GROUP",
            "selected rows must all have proposed_group_kind `enum`",
        ));
    }
    let files: BTreeSet<_> = rows.iter().map(|row| cell(row, "file")).collect();
    if files.len() != 1 || files.first().is_none_or(String::is_empty) {
        return Ok(refuse(
            &command,
            "MULTIPLE_DEFINITION_FILES",
            "selected constants must have one definition file",
        ));
    }
    let enum_names: BTreeSet<_> = rows
        .iter()
        .map(|row| cell(row, "proposed_enum_name"))
        .collect();
    if enum_names.len() != 1 || enum_names.first().is_none_or(String::is_empty) {
        return Ok(refuse(
            &command,
            "ENUM_NAME_MISMATCH",
            "selected rows must have one nonempty proposed_enum_name",
        ));
    }
    let mut constants = BTreeMap::new();
    for row in &rows {
        let name = cell(row, "constant_name");
        let variant = cell(row, "proposed_variant");
        if name.is_empty() || variant.is_empty() {
            return Ok(refuse(
                &command,
                "INVALID_CONSTANT_ROW",
                "constant and variant names must be nonempty",
            ));
        }
        if constants
            .insert(name.clone(), variant.clone())
            .is_some_and(|old| old != variant)
        {
            return Ok(refuse(
                &command,
                "CONFLICTING_CONSTANT_ROW",
                &format!("`{name}` has conflicting variants"),
            ));
        }
    }
    let selected_rows: BTreeMap<_, _> = rows
        .iter()
        .map(|row| (cell(row, "constant_name"), *row))
        .collect();
    for row in &rows {
        let alias = cell(row, "alias_of");
        if alias.is_empty() {
            continue;
        }
        let Some(canonical) = selected_rows.get(&alias) else {
            return Ok(refuse(
                &command,
                "INVALID_ALIAS",
                &format!(
                    "alias `{}` refers to unselected constant `{alias}`",
                    cell(row, "constant_name")
                ),
            ));
        };
        if !cell(canonical, "alias_of").is_empty()
            || cell(canonical, "value") != cell(row, "value")
            || cell(canonical, "type") != cell(row, "type")
            || cell(canonical, "proposed_variant") != cell(row, "proposed_variant")
        {
            return Ok(refuse(
                &command,
                "INVALID_ALIAS",
                &format!("alias `{}` must reference a canonical row with the same type, value, and variant", cell(row, "constant_name")),
            ));
        }
    }
    let definition_file = files.first().cloned().unwrap_or_default();
    let selected_types: BTreeSet<_> = rows.iter().map(|row| cell(row, "type")).collect();
    let selected_names: BTreeSet<_> = constants.keys().cloned().collect();
    for row in records.iter().skip(1) {
        if cell(row, "proposed_enum_group") == command.group
            || cell(row, "file") != definition_file
            || !selected_types.contains(&cell(row, "type"))
        {
            continue;
        }
        let name = cell(row, "constant_name");
        let suggestion = cell(row, "possible_enum_groups");
        let related = suggestion.split(';').any(|entry| {
            if entry == command.group {
                return true;
            }
            let Some(prefix) = entry.strip_prefix("prefix:") else {
                return false;
            };
            selected_names
                .iter()
                .filter(|selected| {
                    selected
                        .strip_prefix(prefix)
                        .is_some_and(|tail| tail.starts_with('_'))
                })
                .count()
                >= 2
        });
        if related {
            return Ok(refuse(
                &command,
                "POSSIBLE_MISSING_CONSTANT",
                &format!(
                    "unselected constant `{name}` may belong to `{}`; add it to the group or clear its possible_enum_groups cell after review",
                    command.group
                ),
            ));
        }
    }
    let matches: BTreeSet<_> = rows
        .iter()
        .flat_map(|row| {
            cell(row, "match_selections")
                .split(';')
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|selection| !selection.is_empty())
        .collect();
    let comparisons: BTreeSet<_> = rows
        .iter()
        .flat_map(|row| {
            cell(row, "comparison_selections")
                .split(';')
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|selection| !selection.is_empty())
        .collect();
    if matches.is_empty() && comparisons.is_empty() {
        return Ok(refuse(&command, "NO_CONVERTIBLE_USE", "group has no match or comparison selection; assignment, return, and argument evidence currently supports discovery only"));
    }

    super::constants_to_enum::run(ConstantsToEnumCommand {
        file: files.into_iter().next().unwrap().into(),
        enum_name: enum_names.into_iter().next().unwrap(),
        constants: constants
            .into_iter()
            .map(|(name, variant)| format!("{name}={variant}"))
            .collect(),
        matches: matches.into_iter().collect(),
        comparisons: comparisons.into_iter().collect(),
        enum_path: command.enum_path,
        visibility: command.visibility,
        dry_run: command.dry_run,
        write: command.write,
        all_features: command.all_features,
        target: command.target,
        manifest_path: command.manifest_path,
        format: command.format,
    })
}

fn refuse(command: &ConstantsToEnumCsvCommand, code: &str, message: &str) -> i32 {
    match command.format {
        OutputFormat::Json => println!(
            "{}",
            json!({"status":"refused","target":null,"edits":[],
            "diagnostics":[{"code":code,"message":message,"file":command.table,"range":null}]})
        ),
        OutputFormat::Text => eprintln!("{code}: {message}"),
    }
    3
}

fn parse_csv(source: &str) -> std::result::Result<Vec<Vec<String>>, String> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = source.chars().peekable();
    while let Some(character) = chars.next() {
        if quoted {
            if character == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(character);
            }
        } else {
            match character {
                '"' if field.is_empty() => quoted = true,
                ',' => record.push(std::mem::take(&mut field)),
                '\n' => {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
                '\r' if chars.peek() == Some(&'\n') => {}
                _ => field.push(character),
            }
        }
    }
    if quoted {
        return Err("CSV ends inside a quoted field".into());
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::parse_csv;

    #[test]
    fn parses_quotes_commas_and_newlines() {
        let rows = parse_csv("a,b\n\"x,y\",\"line1\nline2\"\n").unwrap();
        assert_eq!(rows[1], ["x,y", "line1\nline2"]);
    }
}
