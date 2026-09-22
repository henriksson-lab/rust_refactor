use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn fixture(source: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='out-param-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(project.path().join("src/lib.rs"), source).unwrap();
    project
}

fn run_json(project: &TempDir) -> Value {
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args(["out-param-stats", "--format", "json", "--manifest-path"])
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
fn finds_mutable_references_used_only_as_assignment_destinations() {
    let project = fixture(
        r#"
pub struct Pair { pub left: i32, pub right: i32 }

pub fn scalar(value: i32, output: &mut i32) {
    *output = value;
}

fn fields(output: &mut Pair) {
    output.left = 1;
    output.right = 2;
}

fn indexed(output: &mut [i32], other: &mut i32) {
    output[0] = 3;
    *other = 4;
    let _read = *other;
}

fn destructured(a: &mut i32, b: &mut i32) {
    (*a, *b) = (1, 2);
}
"#,
    );
    let report = run_json(&project);
    assert_eq!(report["function_count"], 3);
    assert_eq!(report["output_parameter_count"], 4);
    let rows = report["functions"].as_array().unwrap();
    let scalar = rows.iter().find(|row| row["function"] == "scalar").unwrap();
    assert_eq!(scalar["output_parameters"][0]["name"], "output");
    assert_eq!(scalar["output_parameters"][0]["type"], "&mut i32");
    assert_eq!(scalar["output_parameters"][0]["write_count"], 1);
    let fields = rows.iter().find(|row| row["function"] == "fields").unwrap();
    assert_eq!(fields["output_parameters"][0]["write_count"], 2);
    assert_eq!(fields["output_parameters"][0]["write_forms"][0], "field");
    assert!(fields["output_parameters"][0]["review_note"]
        .as_str()
        .unwrap()
        .contains("preserve allocation or existing state"));
    assert!(!rows.iter().any(|row| row["function"] == "indexed"));
}

#[test]
fn rejects_reads_compound_assignments_calls_macros_shadowing_and_unused_parameters() {
    let project = fixture(
        r#"
fn consume(_: &mut i32) {}

fn read(output: &mut i32) { let _ = *output; }
fn read_after_write(output: &mut i32) { *output = 1; let _ = *output; }
fn compound(output: &mut i32) { *output += 1; }
fn passed(output: &mut i32) { consume(output); }
fn macro_use(output: &mut i32) { println!("{}", *output); }
fn unused(_output: &mut i32) {}
fn binding_only(mut output: &mut i32) {
    let mut local = 1;
    output = &mut local;
}
fn shadowed(output: &mut i32) {
    *output = 1;
    { let output = &mut 0; *output = 2; }
}
"#,
    );
    let report = run_json(&project);
    assert_eq!(report["function_count"], 0);
    assert_eq!(report["output_parameter_count"], 0);
}

#[test]
fn csv_contains_function_and_argument_lists() {
    let project = fixture("fn fill(a: &mut i32, b: &'static mut u8) { *a = 1; *b = 2; }\n");
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args(["out-param-stats", "--format", "csv", "--manifest-path"])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let csv = String::from_utf8(output.stdout).unwrap();
    assert!(csv.starts_with("function,file,line,column,kind,visibility,output_parameters,argument_names,argument_types,write_counts,write_forms,review_notes,write_evidence"));
    assert_eq!(csv.lines().count(), 2);
    assert!(csv.contains("\"a: &mut i32;b: &'static mut u8\""));
    assert!(csv.contains("\"a;b\",\"&mut i32;&'static mut u8\""));
    assert!(csv.contains("\"a=1;b=1\""));
    assert!(csv.contains("\"a=direct;b=direct\",\"\""));
}
