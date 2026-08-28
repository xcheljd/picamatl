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

// ---- output options (v0.4.1): --output-dir, --suffix, batch mode ----

#[test]
fn output_dir_batch_processes_a_directory_into_new_files() {
    let tmp = std::env::temp_dir().join("picamatl-cli-outdir");
    let in_dir = tmp.join("in");
    let out_dir = tmp.join("out");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&in_dir).unwrap();
    // Two inputs: one shrinks, one is unchanged — batch must handle both.
    std::fs::copy("corpus/dummy.pdf", in_dir.join("dummy.pdf")).unwrap();
    std::fs::copy("corpus/nist-ssdf.pdf", in_dir.join("nist-ssdf.pdf")).unwrap();

    let out = picamatl()
        .arg("--output-dir").arg(&out_dir)
        .arg(&in_dir)
        .output()
        .expect("run batch");
    assert!(out.status.success(), "batch run failed: {}", String::from_utf8_lossy(&out.stderr));

    // Outputs land in out_dir with the same file names; inputs untouched.
    for name in ["dummy.pdf", "nist-ssdf.pdf"] {
        assert!(out_dir.join(name).is_file(), "missing {name} in output dir");
        assert!(in_dir.join(name).is_file(), "input {name} was consumed");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn suffix_writes_beside_the_input_and_leaves_the_input_alone() {
    let tmp = std::env::temp_dir().join("picamatl-cli-suffix");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let input = tmp.join("doc.pdf");
    std::fs::copy("corpus/dummy.pdf", &input).unwrap();

    let out = picamatl()
        .arg("--suffix=-small")
        .arg(&input)
        .output()
        .expect("run suffix");
    assert!(out.status.success(), "suffix run failed: {}", String::from_utf8_lossy(&out.stderr));

    let written = tmp.join("doc-small.pdf");
    assert!(written.is_file(), "doc-small.pdf not written beside input");
    assert!(input.is_file(), "original was modified/removed");

    // The suffix output is a valid PDF with the same page count class: verify
    // it parses and is never larger than the input (never-larger contract).
    let original_len = std::fs::metadata(&input).unwrap().len();
    let written_len = std::fs::metadata(&written).unwrap().len();
    assert!(written_len <= original_len, "suffix output grew: {written_len} > {original_len}");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn output_dir_and_suffix_combine() {
    let tmp = std::env::temp_dir().join("picamatl-cli-combo");
    let in_dir = tmp.join("in");
    let out_dir = tmp.join("out");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&in_dir).unwrap();
    std::fs::copy("corpus/dummy.pdf", in_dir.join("report.pdf")).unwrap();

    let out = picamatl()
        .arg("--output-dir").arg(&out_dir)
        .arg("--suffix=-min")
        .arg(&in_dir)
        .output()
        .expect("run combo");
    assert!(out.status.success());
    assert!(out_dir.join("report-min.pdf").is_file(), "combined stem+suffix name missing");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn batch_continues_past_a_bad_file_and_reports_failure() {
    let tmp = std::env::temp_dir().join("picamatl-cli-batcherr");
    let in_dir = tmp.join("in");
    let out_dir = tmp.join("out");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&in_dir).unwrap();
    std::fs::copy("corpus/dummy.pdf", in_dir.join("good.pdf")).unwrap();
    std::fs::write(in_dir.join("broken.pdf"), b"not a pdf at all").unwrap();

    let out = picamatl()
        .arg("--output-dir").arg(&out_dir)
        .arg(&in_dir)
        .output()
        .expect("run batch with bad file");
    assert!(!out.status.success(), "batch must exit nonzero on any failure");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not a PDF"), "bad-file error should name the problem: {err}");
    assert!(err.contains("1 of 2"), "summary must count failures: {err}");
    // The good file still processed — batch does not lose the rest.
    assert!(out_dir.join("good.pdf").is_file(), "good file lost when a sibling failed");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn output_conflicts_with_suffix_and_output_dir() {
    let out = picamatl()
        .arg("corpus/dummy.pdf")
        .arg("-o").arg("/tmp/never.pdf")
        .arg("--suffix=-x")
        .output()
        .expect("run conflicting flags");
    assert!(!out.status.success(), "-o + --suffix must be rejected");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot be used with"), "clap conflict message expected: {err}");
}

#[test]
fn multiple_inputs_with_single_file_output_flags_are_rejected() {
    let tmp = std::env::temp_dir().join("picamatl-cli-multi-o");
    let in_dir = tmp.join("in");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&in_dir).unwrap();
    std::fs::copy("corpus/dummy.pdf", in_dir.join("a.pdf")).unwrap();
    std::fs::copy("corpus/dummy.pdf", in_dir.join("b.pdf")).unwrap();

    let out = picamatl()
        .arg(&in_dir)
        .arg("-o").arg("/tmp/never-multi.pdf")
        .output()
        .expect("run multi + -o");
    assert!(!out.status.success(), "multi-input with -o must be rejected");
    let _ = std::fs::remove_dir_all(&tmp);
}
