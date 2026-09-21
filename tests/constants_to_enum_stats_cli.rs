use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

#[test]
fn groups_match_arms_and_comparisons_by_subject_within_a_function() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='stats-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        "const MODE_A: i32 = 0;\nconst MODE_B: i32 = 1;\nconst MODE_C: i32 = 2;\nconst OTHER: i32 = 9;\n\nfn classify(mode: i32, other: i32) -> bool {\n    let value = match mode {\n        MODE_A | MODE_B => 1,\n        _ => 0,\n    };\n    if mode == MODE_C { return true; }\n    if other == OTHER { return false; }\n    value > 0\n}\n",
    )
    .unwrap();

    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "constants-to-enum-stats",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["group_count"], 1);
    let group = &data["groups"][0];
    assert_eq!(group["subject"], "mode");
    assert_eq!(group["scope"], "classify");
    assert_eq!(group["constant_count"], 3);
    assert_eq!(group["constants"][0]["name"], "MODE_A");
    assert_eq!(group["constants"][2]["name"], "MODE_C");
    assert_eq!(group["evidence"].as_array().unwrap().len(), 2);
    assert_eq!(group["evidence"][0]["kind"], "match");
    assert_eq!(group["evidence"][1]["kind"], "comparison");
}

#[test]
fn reports_repeated_if_comparisons_as_a_group() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='stats-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        "const OPEN: u8 = 1;\nconst CLOSED: u8 = 2;\nfn f(state: u8) -> bool { state == OPEN || CLOSED == state }\n",
    )
    .unwrap();
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "constants-to-enum-stats",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["group_count"], 1);
    assert_eq!(data["groups"][0]["constant_count"], 2);
    assert_eq!(data["groups"][0]["subject"], "state");
}

#[test]
fn csv_inventories_every_constant_and_leaves_ungrouped_rows_empty() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='csv-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        "const STATE_OPEN: i32 = 1;\nconst STATE_CLOSED: i32 = 2;\nconst LABEL: &str = \"state\";\nstruct S;\nimpl S { const LIMIT: usize = 4; }\nfn f(state: i32) { const LOCAL: i32 = 3; let _ = state == STATE_OPEN || state == STATE_CLOSED; }\n",
    )
    .unwrap();
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
    let csv = String::from_utf8(output.stdout).unwrap();
    assert_eq!(csv.lines().count(), 6);
    assert!(csv.starts_with("constant_name,file,line,column,kind,type,value,proposed_enum_group"));
    let open = csv
        .lines()
        .find(|line| line.starts_with("\"STATE_OPEN\""))
        .unwrap();
    assert!(open.contains("\"group_001\",\"enum\",\"State\",\"Open\""));
    let label = csv
        .lines()
        .find(|line| line.starts_with("\"LABEL\""))
        .unwrap();
    assert!(label.contains("\"module\",\"&str\",\"\"\"state\"\"\""));
    assert!(label.contains(",\"\",\"\",\"\",\"\","));
    assert!(csv
        .lines()
        .any(|line| line.starts_with("\"LIMIT\"") && line.contains("\"associated\"")));
    assert!(csv
        .lines()
        .any(|line| line.starts_with("\"LOCAL\"") && line.contains("\"local\"")));
}

#[test]
fn merges_sets_from_assignments_returns_and_call_arguments() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='flow-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        "const A: i32 = 1; const B: i32 = 2; const C: i32 = 3; const D: i32 = 4; const E: i32 = 5; const F: i32 = 6;\nfn assign(flag: bool) { let mut value = A; if flag { value = B; } let _ = value; }\nfn result(flag: bool) -> i32 { if flag { return C; } D }\nfn take(_: i32) {}\nfn calls() { take(E); take(F); }\n",
    )
    .unwrap();
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "constants-to-enum-stats",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["group_count"], 3, "{data:#}");
    let kinds: Vec<_> = data["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            group["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .map(|site| site["kind"].as_str().unwrap())
                .collect::<std::collections::BTreeSet<_>>()
        })
        .collect();
    assert!(kinds.iter().any(|set| set.contains("assignment")));
    assert!(kinds.iter().any(|set| set.contains("return")));
    assert!(kinds.iter().any(|set| set.contains("argument")));
}

#[test]
fn classifies_shift_based_sets_as_flags() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='flags-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        "const READ: u32 = 1 << 0; const WRITE: u32 = 1 << 1; fn f(value: u32) -> bool { value == READ || value == WRITE }\n",
    )
    .unwrap();
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "constants-to-enum-stats",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["group_count"], 0, "{data:#}");
}

#[test]
fn proposes_a_valid_name_from_a_shared_suffix() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='name-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        "const X_AXIS: i32 = 0; const Y_AXIS: i32 = 1; fn f(axis: i32) -> bool { axis == X_AXIS || axis == Y_AXIS }\n",
    )
    .unwrap();
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
    let csv = String::from_utf8(output.stdout).unwrap();
    assert!(csv
        .lines()
        .any(|line| line.starts_with("\"X_AXIS\"")
            && line.contains("\"group_001\",\"enum\",\"Axis\"")));
}

fn json_stats(source: &str) -> Value {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='focused-stats-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(project.path().join("src/lib.rs"), source).unwrap();
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "constants-to-enum-stats",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn resolves_arguments_to_a_unique_function_parameter() {
    let data = json_stats(
        "const OPEN: i32 = 1; const CLOSED: i32 = 2;\nfn accept(state: i32) {}\nfn f() { accept(OPEN); accept(CLOSED); }\n",
    );
    assert_eq!(data["group_count"], 1, "{data:#}");
    assert_eq!(data["groups"][0]["constant_count"], 2);
    assert!(data["groups"][0]["subject"]
        .as_str()
        .unwrap()
        .contains("parameter 1"));
}

#[test]
fn ignores_ambiguous_callees_and_generic_wrappers() {
    let data = json_stats(
        "const A: i32 = 1; const B: i32 = 2;\nmod one { pub fn take(_: i32) {} }\nmod two { pub fn take(_: i32) {} }\nfn f() { one::take(A); two::take(B); let _ = Some(A); let _ = Some(B); }\n",
    );
    assert_eq!(data["group_count"], 0, "{data:#}");
}

#[test]
fn splits_lexical_families_in_one_dispatcher() {
    let data = json_stats(
        "const MENU_OPEN: i32 = 1; const MENU_CLOSE: i32 = 2; const VIEW_TOP: i32 = 3; const VIEW_BOTTOM: i32 = 4;\nfn f(command: i32) { match command { MENU_OPEN | MENU_CLOSE | VIEW_TOP | VIEW_BOTTOM => {}, _ => {} } }\n",
    );
    assert_eq!(data["group_count"], 2, "{data:#}");
    assert!(data["groups"]
        .as_array()
        .unwrap()
        .iter()
        .all(|group| group["constant_count"] == 2));
}

#[test]
fn splits_lexical_families_connected_by_a_generic_bridge() {
    let data = json_stats(
        "const MENU_OPEN: i32 = 1; const MENU_CLOSE: i32 = 2; const SHARED: i32 = 3; const VIEW_TOP: i32 = 4; const VIEW_BOTTOM: i32 = 5;\nfn left(_: i32) {} fn right(_: i32) {}\nfn f() { left(MENU_OPEN); left(MENU_CLOSE); left(SHARED); right(SHARED); right(VIEW_TOP); right(VIEW_BOTTOM); }\n",
    );
    assert_eq!(data["group_count"], 2, "{data:#}");
    assert!(data["groups"]
        .as_array()
        .unwrap()
        .iter()
        .all(|group| group["constant_count"] == 2));
}

#[test]
fn keeps_shadowed_bindings_as_separate_subjects() {
    let data = json_stats(
        "const OUTER_A: i32 = 1; const OUTER_B: i32 = 2; const INNER_A: i32 = 3; const INNER_B: i32 = 4;\nfn f() { let mode = OUTER_A; let _ = mode == OUTER_B; { let mode = INNER_A; let _ = mode == INNER_B; } }\n",
    );
    assert_eq!(data["group_count"], 2, "{data:#}");
}

#[test]
fn classifies_literal_constants_as_flags_from_bitwise_uses() {
    let data = json_stats(
        "const READ: u32 = 1; const WRITE: u32 = 2;\nfn accept(_: u32) {}\nfn f(flags: u32) { accept(READ); accept(WRITE); let _ = flags & READ; let _ = flags | WRITE; }\n",
    );
    assert_eq!(data["group_count"], 0, "{data:#}");
}

#[test]
fn rules_out_constants_used_as_ordering_bounds() {
    let data = json_stats(
        "const MIN_LEVEL: i32 = 1; const MAX_LEVEL: i32 = 9;\nfn accept(_: i32) {}\nfn f(level: i32) { accept(MIN_LEVEL); accept(MAX_LEVEL); let _ = level >= MIN_LEVEL && level <= MAX_LEVEL; }\n",
    );
    assert_eq!(data["group_count"], 0, "{data:#}");
}

#[test]
fn ordering_bound_does_not_poison_enum_members_seen_at_the_same_sink() {
    let data = json_stats(
        "const STATE_A: i32 = 1; const STATE_B: i32 = 2; const STATE_LIMIT: i32 = 9;\nfn accept(_: i32) {}\nfn f(value: i32) { accept(STATE_A); accept(STATE_B); accept(STATE_LIMIT); let _ = value < STATE_LIMIT; }\n",
    );
    assert_eq!(data["group_count"], 1, "{data:#}");
    assert_eq!(data["groups"][0]["constant_count"], 2, "{data:#}");
    assert_eq!(data["groups"][0]["constants"][0]["name"], "STATE_A");
    assert_eq!(data["groups"][0]["constants"][1]["name"], "STATE_B");
}

#[test]
fn classifies_equal_value_alias_confidence() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='alias-stats-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        "const FIRST_INDEX: i32 = 0; const OTHER_INDEX: i32 = 0;\nconst MODE_A: i32 = 1; const MODE_ALIAS: i32 = 1; const MODE_B: i32 = 2;\nconst DATA_CHAR: i32 = 0; const DATA_BYTE: i32 = 0; const DATA_SHORT: i32 = 1; const DATA_LONG: i32 = 2; const DATA_FLOAT: i32 = 3; const DATA_DOUBLE: i32 = 4;\nconst CODE_ALT_A: i32 = 0; const CODE_ALT_B: i32 = 1; const CODE_ALT_C: i32 = 2; const CODE_BASE_A: i32 = 0; const CODE_BASE_B: i32 = 1; const CODE_BASE_C: i32 = 2; const CODE_BASE_D: i32 = 3; const CODE_BASE_E: i32 = 4;\nfn f(index: i32) -> bool { index == FIRST_INDEX || index == OTHER_INDEX } fn g(mode: i32) -> bool { mode == MODE_A || mode == MODE_B }\nfn data(v: i32) -> bool { v == DATA_BYTE || v == DATA_SHORT || v == DATA_LONG || v == DATA_FLOAT || v == DATA_DOUBLE }\nfn code(v: i32) -> bool { v == CODE_BASE_A || v == CODE_BASE_B || v == CODE_BASE_C || v == CODE_BASE_D || v == CODE_BASE_E }\n",
    )
    .unwrap();
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
    let csv = String::from_utf8(output.stdout).unwrap();
    for line in csv.lines().filter(|line| line.contains("_INDEX\"")) {
        let fields = line.split(',').collect::<Vec<_>>();
        assert_eq!(fields[11], "\"\"", "{line}");
    }
    let weak_alias = csv
        .lines()
        .find(|line| line.starts_with("\"MODE_ALIAS\""))
        .unwrap()
        .split(',')
        .collect::<Vec<_>>();
    assert_eq!(weak_alias[11], "\"\"");
    assert_eq!(weak_alias[12], "\"MODE_A\"");

    let promoted_alias = csv
        .lines()
        .find(|line| line.starts_with("\"DATA_CHAR\""))
        .unwrap()
        .split(',')
        .collect::<Vec<_>>();
    assert_ne!(promoted_alias[7], "\"\"");
    assert_eq!(promoted_alias[11], "\"DATA_BYTE\"");
    assert_eq!(promoted_alias[12], "\"\"");

    let parallel_family = csv
        .lines()
        .find(|line| line.starts_with("\"CODE_ALT_A\""))
        .unwrap()
        .split(',')
        .collect::<Vec<_>>();
    assert_eq!(parallel_family[7], "\"\"");
    assert_eq!(parallel_family[11], "\"\"");
    assert_eq!(parallel_family[12], "\"CODE_BASE_A\"");
}

#[test]
fn csv_separates_duplicate_declarations_from_equal_value_aliases() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='duplicate-stats-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        "pub mod canonical { pub const MODE_A: i32 = 2; pub const MODE_B: i32 = 3; pub const OTHER: i32 = 2; pub const STATE_A: i32 = 7; }\nmod copied { const MODE_A: i32 = 2 as i32; const STATE_A: i32 = 7; }\nmod conflict { const MODE_A: i32 = 4; }\n",
    )
    .unwrap();

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
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let csv = String::from_utf8(output.stdout).unwrap();
    let rows = csv
        .lines()
        .map(|line| line.split(',').collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(rows[0][20], "duplicate_kind");
    assert_eq!(rows[0][23], "duplicate_name_conflict");

    let mode_rows = rows
        .iter()
        .skip(1)
        .filter(|row| row[0] == "\"MODE_A\"")
        .collect::<Vec<_>>();
    assert_eq!(mode_rows.len(), 3);
    assert_eq!(
        mode_rows
            .iter()
            .filter(|row| row[20] == "\"normalized\"")
            .count(),
        2
    );
    assert!(mode_rows.iter().all(|row| row[23] == "\"true\""));
    assert!(mode_rows
        .iter()
        .filter(|row| row[20] == "\"normalized\"")
        .all(|row| row[22].contains("src/lib.rs:1:")));
    assert!(mode_rows.iter().any(|row| row[20] == "\"conflict\""));

    let state_rows = rows
        .iter()
        .skip(1)
        .filter(|row| row[0] == "\"STATE_A\"")
        .collect::<Vec<_>>();
    assert!(state_rows.iter().all(|row| row[20] == "\"exact\""));
    assert!(state_rows.iter().all(|row| row[23] == "\"\""));

    let other = rows
        .iter()
        .skip(1)
        .find(|row| row[0] == "\"OTHER\"")
        .unwrap();
    assert_eq!(other[20], "\"\"");
    assert_eq!(other[21], "\"\"");
}
