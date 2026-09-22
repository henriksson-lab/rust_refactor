use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn fixture(source: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='return-stats-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(project.path().join("src/lib.rs"), source).unwrap();
    project
}

fn json_report(project: &TempDir, extra: &[&str]) -> Value {
    let mut command = Command::cargo_bin("rust-refactor").unwrap();
    command.args(["return-stats", "--format", "json", "--manifest-path"]);
    command.arg(project.path().join("Cargo.toml"));
    command.args(extra);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn finds_negative_sentinels_and_resolves_simple_constants() {
    let project = fixture(
        r#"
mod remote;
const FAILURE: i32 = -1;

pub fn status(ok: bool) -> i32 {
    if ok { 0 } else { FAILURE }
}

fn early(ok: bool) -> i32 {
    if !ok { return -2i32; }
    4
}

fn ordinary() -> i32 { 12 }

fn matched(value: i32) -> i32 {
    match value { 0 => -3, _ => 5 }
}

fn remote_status(ok: bool) -> i32 {
    if ok { 0 } else { crate::remote::REMOTE_FAILURE }
}

fn compare_values(left: i32, right: i32) -> i32 {
    if left < right { -1 } else if left > right { 1 } else { 0 }
}
"#,
    );
    fs::write(
        project.path().join("src/remote.rs"),
        "pub const REMOTE_FAILURE: i32 = -9;\n",
    )
    .unwrap();
    let report = json_report(&project, &[]);
    assert_eq!(report["candidate_count"], 4);
    let functions = report["functions"].as_array().unwrap();
    let status = functions
        .iter()
        .find(|row| row["function"] == "status")
        .unwrap();
    assert_eq!(status["suggested_return"], "Result");
    assert_eq!(status["confidence"], "high");
    assert_eq!(status["negative_sentinels"][0], "FAILURE=-1");
    assert!(status["known_return_values"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == "0"));
    let early = functions
        .iter()
        .find(|row| row["function"] == "early")
        .unwrap();
    assert_eq!(early["negative_sentinels"][0], "-2i32=-2");
    assert_eq!(early["return_site_count"], 2);
    let ordinary = functions
        .iter()
        .find(|row| row["function"] == "ordinary")
        .unwrap();
    assert_eq!(ordinary["suggested_return"], "");
    let matched = functions
        .iter()
        .find(|row| row["function"] == "matched")
        .unwrap();
    assert_eq!(matched["negative_sentinels"][0], "-3");
    assert_eq!(matched["return_site_count"], 2);
    let remote = functions
        .iter()
        .find(|row| row["function"] == "remote_status")
        .unwrap();
    assert_eq!(
        remote["negative_sentinels"][0],
        "crate::remote::REMOTE_FAILURE=-9"
    );
    let comparison = functions
        .iter()
        .find(|row| row["function"] == "compare_values")
        .unwrap();
    assert_eq!(comparison["suggested_return"], "");
    assert_eq!(comparison["reason"], "ordered_comparison");
}

#[test]
fn finds_null_returns_and_ignores_nested_return_scopes() {
    let project = fixture(
        r#"
fn lookup(pointer: *mut u8) -> *mut u8 {
    if pointer.is_null() { std::ptr::null_mut() } else { pointer }
}

fn cast_null() -> *const u8 { 0 as *const u8 }

fn nested() -> i32 {
    let callback = || { return -7; };
    let future = async { return -8; };
    let _ = (callback, future);
    1
}

fn already(value: bool) -> Option<i32> {
    if value { Some(1) } else { None }
}
"#,
    );
    let report = json_report(&project, &[]);
    assert_eq!(report["candidate_count"], 2);
    let functions = report["functions"].as_array().unwrap();
    let lookup = functions
        .iter()
        .find(|row| row["function"] == "lookup")
        .unwrap();
    assert_eq!(lookup["suggested_return"], "Option");
    assert_eq!(lookup["confidence"], "high");
    assert_eq!(lookup["null_return_forms"][0], "std::ptr::null_mut()");
    let cast_null = functions
        .iter()
        .find(|row| row["function"] == "cast_null")
        .unwrap();
    assert_eq!(cast_null["suggested_return"], "Option");
    let nested = functions
        .iter()
        .find(|row| row["function"] == "nested")
        .unwrap();
    assert_eq!(nested["suggested_return"], "");
    assert_eq!(nested["known_return_values"][0], "1");
    let already = functions
        .iter()
        .find(|row| row["function"] == "already")
        .unwrap();
    assert_eq!(already["reason"], "already_option_or_result");
}

#[test]
fn csv_has_review_columns_and_candidate_filter() {
    let project = fixture("fn bad() -> i32 { -1 }\nfn good() -> i32 { 0 }\n");
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "return-stats",
            "--format",
            "csv",
            "--candidates-only",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let csv = String::from_utf8(output.stdout).unwrap();
    assert!(csv.starts_with("function,file,line,column,kind,visibility,declared_return_type,suggested_return,confidence,reason,known_return_values,negative_sentinels,null_return_forms,unknown_return_values,return_site_count,evidence"));
    assert_eq!(csv.lines().count(), 2);
    assert!(csv.lines().nth(1).unwrap().starts_with("\"bad\","));
    assert!(csv.contains("\"Result\",\"medium\",\"negative_error_sentinel\""));
}
