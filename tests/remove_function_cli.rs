use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"remove_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    for (name, source) in files {
        let path = dir.path().join("src").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    dir
}

fn run(dir: &TempDir, mode: &str) -> (i32, Value) {
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "remove-function",
            "--file",
            "src/lib.rs",
            "--line",
            "2",
            "--column",
            "8",
            mode,
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let data = serde_json::from_slice(&output.stdout).unwrap();
    (output.status.code().unwrap(), data)
}

#[test]
fn previews_removal_without_changing_files() {
    let source =
        "mod user;\npub fn discard(_: i32) {}\npub fn run() { discard(1); user::run(); }\n";
    let dir = fixture(&[
        ("lib.rs", source),
        (
            "user.rs",
            "use crate::discard;\npub fn run() { discard(2); }\n",
        ),
    ]);
    let (code, data) = run(&dir, "--dry-run");
    assert_eq!(code, 0, "{data}");
    assert_eq!(data["status"], "planned");
    assert_eq!(data["target"]["calls_removed"], 2);
    assert_eq!(data["target"]["imports_removed"], 1);
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn writes_removal_and_compiles() {
    let dir = fixture(&[
        (
            "lib.rs",
            "mod user;\npub fn discard(_: i32) {}\npub fn run() { discard(1); user::run(); }\n",
        ),
        (
            "user.rs",
            "use crate::discard;\npub fn run() { discard(2); }\n",
        ),
    ]);
    let (code, data) = run(&dir, "--write");
    assert_eq!(code, 0, "{data}");
    assert_eq!(data["status"], "applied");
    assert!(!fs::read_to_string(dir.path().join("src/lib.rs"))
        .unwrap()
        .contains("discard"));
    assert!(!fs::read_to_string(dir.path().join("src/user.rs"))
        .unwrap()
        .contains("discard"));
}

#[test]
fn refuses_value_call_and_keeps_files() {
    let source = "pub fn run() -> i32 { discard(1) }\npub fn discard(x: i32) -> i32 { x }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, data) = run(&dir, "--dry-run");
    assert_eq!(code, 3, "{data}");
    assert_eq!(data["diagnostics"][0]["code"], "NON_STANDALONE_CALL");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn refuses_function_pointer_reference() {
    let source =
        "pub fn run() { let callback = discard; callback(1); }\npub fn discard(_: i32) {}\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, data) = run(&dir, "--dry-run");
    assert_eq!(code, 3, "{data}");
    assert_eq!(data["diagnostics"][0]["code"], "UNHANDLED_REFERENCE");
}
