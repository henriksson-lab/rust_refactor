use assert_cmd::Command;
use std::fs;
use tempfile::TempDir;

fn fixture(helper: &str, lib: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(project.path().join("src/helper.rs"), helper).unwrap();
    fs::write(project.path().join("src/lib.rs"), lib).unwrap();
    project
}

fn command(project: &TempDir, mode: &str) -> serde_json::Value {
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "simplify-wrapper",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
            "--file",
            "src/helper.rs",
            "--line",
            "1",
            "--column",
            "8",
            mode,
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    if result["status"] == "refused" {
        assert_eq!(output.status.code(), Some(3));
    } else {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    result
}

#[test]
fn rewrites_drop_wrapper_across_files_and_removes_import() {
    let helper = "pub fn consume(value: Vec<u8>) { drop(value); }\n";
    let lib = "mod helper;\nuse helper::consume;\npub fn run() { consume(vec![1, 2]); }\n";
    let project = fixture(helper, lib);

    let preview = command(&project, "--dry-run");
    assert_eq!(preview["status"], "planned");
    assert_eq!(preview["target"]["pattern"], "drop");
    assert_eq!(preview["target"]["calls_replaced"], 1);
    assert_eq!(
        fs::read_to_string(project.path().join("src/helper.rs")).unwrap(),
        helper
    );

    let fast = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "simplify-wrapper",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
            "--file",
            "src/helper.rs",
            "--line",
            "1",
            "--column",
            "8",
            "--dry-run",
            "--fast",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(fast.status.success());
    let fast: serde_json::Value = serde_json::from_slice(&fast.stdout).unwrap();
    assert_eq!(fast["target"]["calls_replaced"], 1);

    let result = command(&project, "--write");
    assert_eq!(result["status"], "applied");
    let updated = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("::core::mem::drop(vec![1, 2])"));
    assert!(!updated.contains("use helper::consume"));
    assert!(!fs::read_to_string(project.path().join("src/helper.rs"))
        .unwrap()
        .contains("fn consume"));
}

#[test]
fn refuses_function_value_reference() {
    let project = fixture(
        "pub fn consume(value: Vec<u8>) { drop(value); }\n",
        "mod helper;\nuse helper::consume;\npub fn run() { let f = consume; f(vec![1]); }\n",
    );
    let result = command(&project, "--dry-run");
    assert_eq!(result["status"], "refused");
    assert_eq!(result["diagnostics"][0]["code"], "UNHANDLED_REFERENCE");
}

#[test]
fn refuses_a_different_function_named_drop() {
    let project = fixture(
        "pub fn consume(value: Vec<u8>) { drop(value); }\nfn drop(_value: Vec<u8>) {}\n",
        "mod helper;\npub fn run() { helper::consume(vec![1]); }\n",
    );
    let result = command(&project, "--dry-run");
    assert_eq!(result["status"], "refused");
    assert_eq!(result["diagnostics"][0]["code"], "UNRESOLVED_DROP");
}
