use std::path::PathBuf;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_handy-tts"))
        .arg("--check-vocabulary")
        .args(args)
        .output()
        .expect("run handy-tts --check-vocabulary")
}

fn scratch_file(name: &str, json: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("handy-tts-{}-{name}.json", std::process::id()));
    std::fs::write(&path, json).expect("write the scratch vocabulary");
    path
}

#[test]
fn check_vocabulary_exit_code() {
    let builtin = run(&[]);
    let stdout = String::from_utf8_lossy(&builtin.stdout);
    assert!(builtin.status.success(), "built-in check failed: {stdout}");
    assert!(stdout.contains("PASS tests/lib.sh"));
    assert!(stdout.contains(", 0 failed"));

    let good = scratch_file(
        "good",
        r#"{"version":1,"terms":{"pueue":"pweh"},"examples":[{"text":"pueue","spoken":"pweh"}]}"#,
    );
    let merged = run(&[good.to_str().expect("utf-8 path")]);
    let merged_out = String::from_utf8_lossy(&merged.stdout);
    assert!(merged.status.success(), "merged check failed: {merged_out}");
    assert!(merged_out.contains("PASS pueue"));

    let wrong = scratch_file(
        "wrong",
        r#"{"version":1,"examples":[{"text":"lib.sh","spoken":"lib shush"}]}"#,
    );
    let failed = run(&[wrong.to_str().expect("utf-8 path")]);
    let failed_out = String::from_utf8_lossy(&failed.stdout);
    assert_eq!(failed.status.code(), Some(1), "output: {failed_out}");
    assert!(failed_out.contains("FAIL lib.sh: expected \"lib shush\" got \"lib dot S H\""));

    let broken = scratch_file("broken", "{ not json");
    let unparsed = run(&[broken.to_str().expect("utf-8 path")]);
    assert!(!unparsed.status.success());
    assert!(String::from_utf8_lossy(&unparsed.stderr).contains("parse"));

    for path in [good, wrong, broken] {
        std::fs::remove_file(path).expect("remove the scratch vocabulary");
    }
}

#[test]
fn check_vocabulary_rejects_unsafe_replacements() {
    let cases = [
        (
            "unsafe-term",
            r#"{"version":1,"terms":{"customtool":"custom/tool"},"examples":[{"text":"customtool","spoken":"custom/tool"}]}"#,
            "customtool",
        ),
        (
            "unsafe-symbol",
            r#"{"version":1,"symbols":{"/":"a/b"}}"#,
            "\"/\"",
        ),
        (
            "unsafe-extension",
            r#"{"version":1,"extensions":{"sh":"S-H"}}"#,
            "\"sh\"",
        ),
        (
            "unsafe-regex",
            r#"{"version":1,"terms":{"^v(.+)$":{"say":"v/$1","match":"regex"}}}"#,
            "^v(.+)$",
        ),
    ];
    for (name, json, key) in cases {
        let path = scratch_file(name, json);
        let output = run(&[path.to_str().expect("utf-8 path")]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{name} must fail the check; stdout: {stdout}"
        );
        assert!(!stdout.contains("PASS customtool"), "{name}: {stdout}");
        assert!(stderr.contains(key), "{name} must name {key}: {stderr}");
        assert!(
            stderr.contains("letters, digits and spaces"),
            "{name}: {stderr}"
        );
        std::fs::remove_file(path).expect("remove the scratch vocabulary");
    }
}

#[test]
fn check_vocabulary_edge_examples_pass() {
    let path = scratch_file(
        "edges",
        r#"{"version":1,"examples":[
            {"text":"file.q","spoken":"file dot Q"},
            {"text":"a.b","spoken":"a dot B"},
            {"text":"backup.7z","spoken":"backup dot 7 Z"},
            {"text":"lib.sh...","spoken":"lib dot S H..."},
            {"text":"lib.sh?!","spoken":"lib dot S H?!"},
            {"text":"**lib.sh.**","spoken":"lib dot S H."},
            {"text":"don't","spoken":"don't"},
            {"text":"well-known","spoken":"well-known"},
            {"text":"1,000","spoken":"1,000"},
            {"text":"\"done.\"","spoken":"\"done.\""}]}"#,
    );
    let output = run(&[path.to_str().expect("utf-8 path")]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "edge examples failed: {stdout}");
    assert!(stdout.contains("PASS **lib.sh.**"));
    std::fs::remove_file(path).expect("remove the scratch vocabulary");
}
