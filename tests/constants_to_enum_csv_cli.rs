use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn fixture(source: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='csv-apply-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(project.path().join("src/lib.rs"), source).unwrap();
    project
}

fn discover(project: &TempDir) -> String {
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "constants-to-enum-stats",
            "--format",
            "csv",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

fn apply(project: &TempDir, table: &str) -> (i32, Value) {
    let table_path = project.path().join("constants.csv");
    fs::write(&table_path, table).unwrap();
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args(["constants-to-enum-csv", "--table"])
        .arg(&table_path)
        .args([
            "--group",
            "group_001",
            "--dry-run",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let value = serde_json::from_slice(&output.stdout).unwrap();
    (output.status.code().unwrap(), value)
}

#[test]
fn applies_a_reviewed_csv_group() {
    let source = "const MODE_A: i32 = 0;\nconst MODE_B: i32 = 1;\nfn width(mode: i32) -> i32 { match mode { MODE_A => 1, MODE_B => 2, _ => 0 } }\n";
    let project = fixture(source);
    let table = discover(&project);
    let (code, result) = apply(&project, &table);
    assert_eq!(code, 0, "{result:#}");
    assert_eq!(result["status"], "planned");
    assert_eq!(result["target"]["enum_name"], "Mode");
    assert_eq!(
        fs::read_to_string(project.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn bit_flags_are_not_emitted_as_an_applicable_group() {
    let project = fixture(
        "const FLAG_A: u32 = 1 << 0; const FLAG_B: u32 = 1 << 1; fn f(v: u32) -> bool { v == FLAG_A || v == FLAG_B }\n",
    );
    let table = discover(&project);
    assert!(table
        .lines()
        .skip(1)
        .all(|line| !line.contains("group_001")));
    assert!(table.contains("bitwise_use"));
    let (code, result) = apply(&project, &table);
    assert_eq!(code, 3);
    assert_eq!(result["diagnostics"][0]["code"], "GROUP_NOT_FOUND");
}

#[test]
fn refuses_group_without_convertible_match() {
    let project = fixture(
        "const STATE_A: i32 = 0; const STATE_B: i32 = 1; fn f(flag: bool) -> i32 { let mut state = STATE_A; if flag { state = STATE_B; } state }\n",
    );
    let table = discover(&project);
    let (code, result) = apply(&project, &table);
    assert_eq!(code, 3);
    assert_eq!(result["diagnostics"][0]["code"], "NO_CONVERTIBLE_USE");
}

#[test]
fn applies_a_group_discovered_from_comparisons() {
    let project = fixture(
        "const STATE_A: i32 = 0; const STATE_B: i32 = 1; fn f(state: i32) -> bool { state == STATE_A || STATE_B != state }\n",
    );
    let table = discover(&project);
    let (code, result) = apply(&project, &table);
    assert_eq!(code, 0, "{result:#}");
    assert_eq!(result["status"], "planned");
    assert_eq!(result["target"]["comparisons"].as_array().unwrap().len(), 2);
    let replacements: Vec<_> = result["edits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|edit| edit["replacement"].as_str())
        .collect();
    assert!(replacements
        .iter()
        .any(|text| text.contains("State::from_raw(state) == Some(State::A)")));
    assert!(replacements
        .iter()
        .any(|text| text.contains("State::from_raw(state) != Some(State::B)")));
}

#[test]
fn refuses_a_group_with_an_unreviewed_possible_sibling() {
    let project = fixture(
        "const MODE_A: i32 = 0; const MODE_B: i32 = 1; const MODE_C: i32 = 2; fn f(mode: i32) -> bool { mode == MODE_A || mode == MODE_B }\n",
    );
    let table = discover(&project);
    let (code, result) = apply(&project, &table);
    assert_eq!(code, 3);
    assert_eq!(
        result["diagnostics"][0]["code"],
        "POSSIBLE_MISSING_CONSTANT"
    );
    assert!(result["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("MODE_C"));
}

#[test]
fn discovers_and_applies_duplicate_values_as_aliases() {
    let project = fixture(
        "const MODE_A: i32 = 0; const MODE_ALIAS: i32 = 0; const MODE_B: i32 = 1; fn f(mode: i32) -> bool { mode == MODE_A || mode == MODE_ALIAS || mode == MODE_B }\n",
    );
    let table = discover(&project);
    let alias = table
        .lines()
        .find(|line| line.starts_with("\"MODE_ALIAS\""))
        .unwrap();
    assert!(alias.contains("\"MODE_A\""), "{alias}");
    let (code, result) = apply(&project, &table);
    assert_eq!(code, 0, "{result:#}");
}

#[test]
fn automatically_merges_one_nearby_equal_value_sibling() {
    let project = fixture(
        "const MODE_ALIAS: i32 = 0; const MODE_A: i32 = 0; const MODE_B: i32 = 1; const MODE_C: i32 = 2; const MODE_D: i32 = 3; const MODE_E: i32 = 4; fn f(mode: i32) -> bool { mode == MODE_A || mode == MODE_B || mode == MODE_C || mode == MODE_D || mode == MODE_E }\n",
    );
    let table = discover(&project);
    let alias = table
        .lines()
        .find(|line| line.starts_with("\"MODE_ALIAS\""))
        .unwrap();
    assert!(alias.contains("\"MODE_A\""), "{alias}");
    let (code, result) = apply(&project, &table);
    assert_eq!(code, 0, "{result:#}");
    assert_eq!(result["target"]["constants"].as_array().unwrap().len(), 6);
}
