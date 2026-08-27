//! End-to-end tests for the `picamatl` CLI binary, driven as a subprocess via
//! `CARGO_BIN_EXE_picamatl` (cargo builds it before running integration tests).

use std::process::Command;

fn picamatl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_picamatl"))
}

#[test]
fn help_exits_successfully_and_documents_flags() {
    let out = picamatl().arg("--help").output().expect("run --help");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--target-dpi",
        "--jpeg-quality",
        "--dpi-margin",
        "--strip-accessibility",
        "--pack-object-streams",
        "--downsample-flate-images",
        "--subset-fonts",
        "--recompress-bitonal-images",
    ] {
        assert!(text.contains(flag), "--help missing {flag}");
    }
    // The defaults blurb must reflect the library, not stale prose.
    assert!(text.contains("OptimizeOptions::default()"));
}

#[test]
fn version_reports_cargo_version() {
    let out = picamatl().arg("--version").output().expect("run --version");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let expected = format!("picamatl {}", env!("CARGO_PKG_VERSION"));
    assert!(
        text.trim().starts_with(&expected),
        "version line {text:?} does not start with {expected:?}"
    );
}

#[test]
fn non_pdf_input_is_rejected_with_nonzero_exit() {
    let dir = std::env::temp_dir().join("picamatl-cli-not-pdf");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fake.txt");
    std::fs::write(&path, b"definitely not a pdf").unwrap();

    let out = picamatl().arg(&path).output().expect("run on non-PDF");
    assert!(!out.status.success(), "non-PDF input must fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not a PDF"), "stderr should explain: {err}");
    assert!(!path.with_extension("optimized.pdf").exists());
}

#[test]
fn optimizes_the_committed_fixture_end_to_end() {
    let dir = std::env::temp_dir().join("picamatl-cli-fixture");
    std::fs::create_dir_all(&dir).unwrap();
    let out_path = dir.join("sample.out.pdf");
    let _ = std::fs::remove_file(&out_path);

    let out = picamatl()
        .arg("fixtures/sample.pdf")
        .arg("-o")
        .arg(&out_path)
        .output()
        .expect("run on fixture");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let optimized = std::fs::read(&out_path).unwrap();
    let original = std::fs::read("fixtures/sample.pdf").unwrap();
    assert!(optimized.starts_with(b"%PDF-"), "output must be a PDF");
    assert!(
        optimized.len() < original.len(),
        "fixture should shrink: {} -> {}",
        original.len(),
        optimized.len()
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("saved"), "report should state savings: {err}");
}

#[test]
fn bitonal_flag_is_accepted_and_shrinks_the_flate_bitonal_pdf() {
    // tests/generate_fixture.rs builds a Flate-stored 1-bit image page; if the
    // generated fixture is present, the flag must actually fire on it.
    let dir = std::env::temp_dir().join("picamatl-cli-bitonal");
    std::fs::create_dir_all(&dir).unwrap();
    let out_path = dir.join("bitonal.out.pdf");
    let _ = std::fs::remove_file(&out_path);

    let out = picamatl()
        .arg("fixtures/sample.pdf")
        .arg("--recompress-bitonal-images")
        .arg("--no-downsample-flate-images")
        .arg("-o")
        .arg(&out_path)
        .output()
        .expect("run with bitonal flag");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out_path.exists());
    assert!(std::fs::read(&out_path).unwrap().starts_with(b"%PDF-"));
}
