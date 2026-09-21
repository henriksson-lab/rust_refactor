use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn fixture(source: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(project.path().join("src/lib.rs"), source).unwrap();
    project
}

fn run(project: &TempDir, mode: &str, extra: &[&str]) -> (std::process::ExitStatus, Value) {
    let mut command = Command::cargo_bin("rust-refactor").unwrap();
    command.args([
        "constants-to-enum",
        "--manifest-path",
        project.path().join("Cargo.toml").to_str().unwrap(),
        "--file",
        "src/lib.rs",
        "--enum-name",
        "SliceMode",
        "--constant",
        "SLICE_MODE_BYTE=Byte",
        "--constant",
        "SLICE_MODE_SHORT=Short",
        "--constant",
        "SLICE_MODE_FLOAT=Float",
        "--match",
        "src/lib.rs:6:5",
        mode,
        "--format",
        "json",
    ]);
    command.args(extra);
    let output = command.output().unwrap();
    let json = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status, json)
}

const SOURCE: &str = "pub const SLICE_MODE_BYTE: i32 = 0;\n\
pub const SLICE_MODE_SHORT: i32 = 1;\n\
pub const SLICE_MODE_FLOAT: i32 = 2;\n\
\n\
pub fn width(raw: i32) -> usize {\n\
    match raw {\n\
        SLICE_MODE_BYTE => 1,\n\
        SLICE_MODE_SHORT => 2,\n\
        SLICE_MODE_FLOAT => 4,\n\
        _ => 0,\n\
    }\n\
}\n";

#[test]
fn introduces_enum_and_converts_selected_match() {
    let project = fixture(SOURCE);
    let (status, preview) = run(&project, "--dry-run", &[]);
    assert!(status.success(), "{preview:#}");
    assert_eq!(preview["status"], "planned");
    assert_eq!(preview["target"]["raw_type"], "i32");
    assert_eq!(
        fs::read_to_string(project.path().join("src/lib.rs")).unwrap(),
        SOURCE
    );

    let (status, applied) = run(&project, "--write", &[]);
    assert!(status.success());
    assert_eq!(applied["status"], "applied");
    let updated = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("pub enum SliceMode"));
    assert!(updated.contains("SLICE_MODE_BYTE: i32 = SliceMode::Byte.to_raw()"));
    assert!(updated.contains("match SliceMode::from_raw(raw)"));
    assert!(updated.contains("Some(SliceMode::Short) => 2"));
}

#[test]
fn refuses_literal_pattern_in_selected_match() {
    let project = fixture(&SOURCE.replace("SLICE_MODE_SHORT => 2", "1 => 2"));
    let (status, result) = run(&project, "--dry-run", &[]);
    assert_eq!(status.code(), Some(3));
    assert_eq!(result["status"], "refused");
    assert_eq!(result["diagnostics"][0]["code"], "UNSUPPORTED_PATTERN");
}

#[test]
fn refuses_duplicate_discriminants() {
    let project =
        fixture(&SOURCE.replace("SLICE_MODE_SHORT: i32 = 1", "SLICE_MODE_SHORT: i32 = 0"));
    let (status, result) = run(&project, "--dry-run", &[]);
    assert_eq!(status.code(), Some(3));
    assert_eq!(result["diagnostics"][0]["code"], "DUPLICATE_VALUE");
}

#[test]
fn accepts_duplicate_discriminants_as_one_variant_alias() {
    let project = fixture(
        "const MODE_A: i32 = 0;\nconst MODE_ALIAS: i32 = 0;\nconst MODE_B: i32 = 1;\nfn f(mode: i32) -> bool { mode == MODE_A || mode == MODE_ALIAS || mode == MODE_B }\n",
    );
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "constants-to-enum",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
            "--file",
            "src/lib.rs",
            "--enum-name",
            "Mode",
            "--constant",
            "MODE_A=A",
            "--constant",
            "MODE_ALIAS=A",
            "--constant",
            "MODE_B=B",
            "--comparison",
            "src/lib.rs:4:28",
            "--comparison",
            "src/lib.rs:4:46",
            "--comparison",
            "src/lib.rs:4:68",
            "--dry-run",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{result:#}");
    let generated = result["edits"][0]["replacement"].as_str().unwrap();
    assert_eq!(generated.matches("    A = 0,").count(), 1);
}

#[test]
fn refuses_match_without_unknown_value_arm() {
    let project = fixture(&SOURCE.replace("_ => 0,", ""));
    let (status, result) = run(&project, "--dry-run", &[]);
    assert_eq!(status.code(), Some(3));
    assert_eq!(result["diagnostics"][0]["code"], "MISSING_CATCH_ALL");
}

#[test]
fn failed_check_restores_all_edits() {
    let broken = format!("{SOURCE}\ncompile_error!(\"existing failure\");\n");
    let project = fixture(&broken);
    let (status, result) = run(&project, "--write", &[]);
    assert_eq!(status.code(), Some(1));
    assert_eq!(result["status"], "error");
    assert_eq!(
        fs::read_to_string(project.path().join("src/lib.rs")).unwrap(),
        broken
    );
}

#[test]
fn converts_imported_constants_with_explicit_enum_path() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/modes.rs"),
        "pub const MODE_A: i32 = 0;\npub const MODE_B: i32 = 1;\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        "pub mod modes;\nuse modes::{MODE_A, MODE_B};\npub fn value(raw: i32) -> i32 {\n    match raw {\n        MODE_A => 10,\n        MODE_B => 20,\n        _ => 0,\n    }\n}\n",
    )
    .unwrap();
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "constants-to-enum",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
            "--file",
            "src/modes.rs",
            "--enum-name",
            "Mode",
            "--enum-path",
            "crate::modes::Mode",
            "--constant",
            "MODE_A=A",
            "--constant",
            "MODE_B=B",
            "--match",
            "src/lib.rs:4:5",
            "--dry-run",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let replacement = result["edits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|edit| edit["replacement"].as_str())
        .find(|text| text.starts_with("match "))
        .unwrap();
    assert!(replacement.contains("Some(crate::modes::Mode::A)"));
}

#[test]
fn writes_selected_equality_comparisons_and_compiles() {
    let project = fixture(
        "const MODE_A: i32 = 0;\nconst MODE_B: i32 = 1;\nfn selected(mode: i32) -> bool { mode == MODE_A || MODE_B != mode }\n",
    );
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "constants-to-enum",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
            "--file",
            "src/lib.rs",
            "--enum-name",
            "Mode",
            "--constant",
            "MODE_A=A",
            "--constant",
            "MODE_B=B",
            "--comparison",
            "src/lib.rs:3:35",
            "--comparison",
            "src/lib.rs:3:53",
            "--write",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let updated = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("Mode::from_raw(mode) == Some(Mode::A)"));
    assert!(updated.contains("Mode::from_raw(mode) != Some(Mode::B)"));
}
