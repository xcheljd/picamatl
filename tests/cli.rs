//! End-to-end tests for the `picamatl` CLI binary, driven as a subprocess via
//! `CARGO_BIN_EXE_picamatl` (cargo builds it before running integration tests).

use std::path::{Path, PathBuf};
use std::process::Command;

use lopdf::{dictionary, Document, Object, StringFormat};

fn picamatl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_picamatl"))
}

/// Write `fixtures/sample.pdf` with a conforming signature field bolted on
/// (`/FT /Sig` -> `/V` -> a `/ByteRange`-carrying dictionary) into a fresh
/// temp dir, and return the path. The base fixture is the one the CLI is
/// already known to shrink, which is what makes the warning legitimate: a
/// passthrough must not print it.
fn signed_fixture(dir_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(dir_name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut doc = Document::load("fixtures/sample.pdf").expect("load fixture");
    let sig_id = doc.add_object(dictionary! {
        "Type" => Object::Name(b"Sig".to_vec()),
        "Filter" => Object::Name(b"Adobe.PPKLite".to_vec()),
        "SubFilter" => Object::Name(b"adbe.pkcs7.detached".to_vec()),
        "ByteRange" => Object::Array(vec![
            Object::Integer(0), Object::Integer(10),
            Object::Integer(20), Object::Integer(10),
        ]),
        "Contents" => Object::String(b"fake".to_vec(), StringFormat::Hexadecimal),
    });
    let field_id = doc.add_object(dictionary! {
        "FT" => Object::Name(b"Sig".to_vec()),
        "T" => Object::String(b"Signature1".to_vec(), StringFormat::Literal),
        "V" => Object::Reference(sig_id),
    });
    doc.catalog_mut().unwrap().set(
        "AcroForm",
        Object::Dictionary(dictionary! {
            "SigFlags" => Object::Integer(3),
            "Fields" => Object::Array(vec![Object::Reference(field_id)]),
        }),
    );
    let path = dir.join("signed.pdf");
    doc.save(&path).expect("write signed fixture");
    path
}

/// Run the CLI on `input`, returning `(stderr, output bytes)`.
fn run_on(input: &Path, out: &Path, extra: &[&str]) -> (String, Vec<u8>) {
    let mut cmd = picamatl();
    cmd.arg(input).arg("-o").arg(out);
    for arg in extra {
        cmd.arg(arg);
    }
    let result = cmd.output().expect("run picamatl");
    assert!(
        result.status.success(),
        "picamatl failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    (
        String::from_utf8_lossy(&result.stderr).into_owned(),
        std::fs::read(out).expect("read output"),
    )
}

#[test]
fn signature_warning_prints_by_default_and_silence_suppresses_it() {
    let input = signed_fixture("picamatl-cli-signed");
    let original = std::fs::read(&input).unwrap();
    let dir = input.parent().unwrap();

    let (warned, out) = run_on(&input, &dir.join("warn.pdf"), &[]);
    assert!(
        out.len() < original.len(),
        "the signed fixture must actually shrink, or the warning would be \
         about a passthrough: {} -> {}",
        original.len(),
        out.len()
    );
    assert!(
        warned.contains("1 digital signature detected"),
        "default run must warn: {warned}"
    );
    assert!(
        warned.contains("the /ByteRange digest is invalidated"),
        "singular grammar, with both spaces: {warned}"
    );
    assert!(
        warned.contains("signature field (object"),
        "the warning must name what it found: {warned}"
    );

    let (silenced, silent_out) =
        run_on(&input, &dir.join("silent.pdf"), &["--on-signature=silence"]);
    assert!(
        !silenced.contains("digital signature"),
        "--on-signature silence must suppress the warning: {silenced}"
    );
    assert_eq!(
        out, silent_out,
        "the flag decides who is told, never what is produced"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_passthrough_run_does_not_warn_about_signatures() {
    // Feed picamatl its own output back in. Nothing is left to win, so the
    // bytes come straight back — no re-serialization, no invalidated digest,
    // and therefore no warning. This used to print the warning and the
    // "wrote a byte-for-byte copy" note on the same run, which cannot both
    // be true.
    let input = signed_fixture("picamatl-cli-signed-passthrough");
    let dir = input.parent().unwrap().to_path_buf();
    let once = dir.join("once.pdf");
    let (_, first) = run_on(&input, &once, &[]);

    let twice = dir.join("twice.pdf");
    let (stderr, second) = run_on(&once, &twice, &[]);
    assert_eq!(second, first, "second pass must be a byte-for-byte copy");
    assert!(
        stderr.contains("byte-for-byte copy"),
        "expected the passthrough note: {stderr}"
    );
    assert!(
        !stderr.contains("digital signature"),
        "a passthrough invalidates nothing and must not warn: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unsigned_document_never_mentions_signatures() {
    let dir = std::env::temp_dir().join("picamatl-cli-unsigned");
    std::fs::create_dir_all(&dir).unwrap();
    let (stderr, _) = run_on(Path::new("fixtures/sample.pdf"), &dir.join("out.pdf"), &[]);
    assert!(
        !stderr.contains("digital signature"),
        "no signature, no warning: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
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
        "--on-signature",
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
    let optimized = std::fs::read(&out_path).unwrap();
    assert!(optimized.starts_with(b"%PDF-"));

    // The name of this test promises a size reduction, so assert one.
    // `--no-downsample-flate-images` is what makes this specific: with the
    // resampling route switched off, the bitonal G4 pass is the only thing
    // that can move the 1-bit page, so a non-shrinking output means the flag
    // did not fire.
    let original = std::fs::read("fixtures/sample.pdf").unwrap();
    assert!(
        optimized.len() < original.len(),
        "bitonal recompression should shrink the fixture: {} -> {} bytes",
        original.len(),
        optimized.len()
    );
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
    std::fs::copy(
        "fixtures/forms/filled-acroform.pdf",
        in_dir.join("form.pdf"),
    )
    .unwrap();
    std::fs::copy("fixtures/sample.pdf", in_dir.join("images.pdf")).unwrap();

    let out = picamatl()
        .arg("--output-dir")
        .arg(&out_dir)
        .arg(&in_dir)
        .output()
        .expect("run batch");
    assert!(
        out.status.success(),
        "batch run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Outputs land in out_dir with the same file names; inputs untouched.
    for name in ["form.pdf", "images.pdf"] {
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
    std::fs::copy("fixtures/forms/filled-acroform.pdf", &input).unwrap();

    let out = picamatl()
        .arg("--suffix=-small")
        .arg(&input)
        .output()
        .expect("run suffix");
    assert!(
        out.status.success(),
        "suffix run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let written = tmp.join("doc-small.pdf");
    assert!(written.is_file(), "doc-small.pdf not written beside input");
    assert!(input.is_file(), "original was modified/removed");

    // The suffix output is a valid PDF with the same page count class: verify
    // it parses and is never larger than the input (never-larger contract).
    let original_len = std::fs::metadata(&input).unwrap().len();
    let written_len = std::fs::metadata(&written).unwrap().len();
    assert!(
        written_len <= original_len,
        "suffix output grew: {written_len} > {original_len}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn output_dir_and_suffix_combine() {
    let tmp = std::env::temp_dir().join("picamatl-cli-combo");
    let in_dir = tmp.join("in");
    let out_dir = tmp.join("out");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&in_dir).unwrap();
    std::fs::copy(
        "fixtures/forms/filled-acroform.pdf",
        in_dir.join("report.pdf"),
    )
    .unwrap();

    let out = picamatl()
        .arg("--output-dir")
        .arg(&out_dir)
        .arg("--suffix=-min")
        .arg(&in_dir)
        .output()
        .expect("run combo");
    assert!(out.status.success());
    assert!(
        out_dir.join("report-min.pdf").is_file(),
        "combined stem+suffix name missing"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn batch_continues_past_a_bad_file_and_reports_failure() {
    let tmp = std::env::temp_dir().join("picamatl-cli-batcherr");
    let in_dir = tmp.join("in");
    let out_dir = tmp.join("out");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&in_dir).unwrap();
    std::fs::copy(
        "fixtures/forms/filled-acroform.pdf",
        in_dir.join("good.pdf"),
    )
    .unwrap();
    std::fs::write(in_dir.join("broken.pdf"), b"not a pdf at all").unwrap();

    let out = picamatl()
        .arg("--output-dir")
        .arg(&out_dir)
        .arg(&in_dir)
        .output()
        .expect("run batch with bad file");
    assert!(
        !out.status.success(),
        "batch must exit nonzero on any failure"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("not a PDF"),
        "bad-file error should name the problem: {err}"
    );
    assert!(err.contains("1 of 2"), "summary must count failures: {err}");
    // The good file still processed — batch does not lose the rest.
    assert!(
        out_dir.join("good.pdf").is_file(),
        "good file lost when a sibling failed"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn output_conflicts_with_suffix_and_output_dir() {
    let out = picamatl()
        .arg("fixtures/forms/filled-acroform.pdf")
        .arg("-o")
        .arg("/tmp/never.pdf")
        .arg("--suffix=-x")
        .output()
        .expect("run conflicting flags");
    assert!(!out.status.success(), "-o + --suffix must be rejected");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("cannot be used with"),
        "clap conflict message expected: {err}"
    );
}

#[test]
fn a_single_failing_input_reports_exactly_one_error() {
    let missing = std::env::temp_dir().join("picamatl-cli-does-not-exist.pdf");
    let _ = std::fs::remove_file(&missing);

    let out = picamatl()
        .arg(&missing)
        .output()
        .expect("run on a missing file");
    assert!(!out.status.success(), "a missing input must exit nonzero");

    let err = String::from_utf8_lossy(&out.stderr);
    let errors: Vec<&str> = err.lines().filter(|l| l.starts_with("error:")).collect();
    // The per-file line says everything; a "1 of 1 inputs failed" summary
    // after it would be a second, vaguer report of the same fact.
    assert_eq!(errors.len(), 1, "one error line expected, got: {err:?}");
    assert!(
        errors[0].starts_with("error: cannot read "),
        "the surviving line must be the specific one: {err:?}"
    );
}

#[test]
fn a_batch_with_one_failing_input_still_summarizes_the_count() {
    let tmp = std::env::temp_dir().join("picamatl-cli-batch-partial");
    let in_dir = tmp.join("in");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&in_dir).unwrap();
    std::fs::copy("fixtures/sample.pdf", in_dir.join("good.pdf")).unwrap();
    // A .pdf that is not a PDF: collected as an input, rejected on sniffing.
    std::fs::write(in_dir.join("bad.pdf"), b"not a pdf at all").unwrap();

    let out = picamatl()
        .arg(&in_dir)
        .arg("--output-dir")
        .arg(tmp.join("out"))
        .output()
        .expect("run a partially failing batch");
    assert!(!out.status.success(), "a failed member must exit nonzero");

    let err = String::from_utf8_lossy(&out.stderr);
    // Two inputs, so the count genuinely summarizes something the per-file
    // lines do not, and the plural is spelled out rather than "input(s)".
    assert!(
        err.contains("error: 1 of 2 inputs failed"),
        "batch summary expected: {err:?}"
    );
    assert!(
        !err.contains("input(s)"),
        "parenthetical plural should be gone: {err:?}"
    );
    // The good file still ran: batch semantics are unchanged.
    assert!(tmp.join("out").join("good.pdf").exists());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn multiple_inputs_with_single_file_output_flags_are_rejected() {
    let tmp = std::env::temp_dir().join("picamatl-cli-multi-o");
    let in_dir = tmp.join("in");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&in_dir).unwrap();
    std::fs::copy("fixtures/forms/filled-acroform.pdf", in_dir.join("a.pdf")).unwrap();
    std::fs::copy("fixtures/forms/filled-acroform.pdf", in_dir.join("b.pdf")).unwrap();

    let out = picamatl()
        .arg(&in_dir)
        .arg("-o")
        .arg("/tmp/never-multi.pdf")
        .output()
        .expect("run multi + -o");
    assert!(
        !out.status.success(),
        "multi-input with -o must be rejected"
    );

    // Pin the exact wording, not just the exit code. This string is assembled
    // across a `\` line continuation, which eats the newline AND the next
    // line's indent — it shipped once with 14 literal spaces inside it, and
    // nothing in the suite noticed.
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        err.trim_end(),
        "error: --output/--force apply to a single input; \
         for multiple files use --output-dir and/or --suffix",
        "exact rejection text (watch the internal spacing): {err:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn force_replaces_the_input_in_place_and_leaves_no_temp_file() {
    let dir = std::env::temp_dir().join("picamatl-cli-force-atomic");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("doc.pdf");
    let original = std::fs::read("fixtures/sample.pdf").unwrap();
    std::fs::write(&input, &original).unwrap();

    let result = picamatl()
        .arg(&input)
        .arg("--force")
        .output()
        .expect("run picamatl");
    assert!(
        result.status.success(),
        "--force run failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let written = std::fs::read(&input).unwrap();
    assert!(
        written.len() < original.len(),
        "--force must write the smaller output over the input"
    );
    Document::load_mem(&written).expect("the replaced file must be a valid PDF");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "doc.pdf")
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temp file may be left behind: {leftovers:?}"
    );
}

#[test]
fn help_does_not_promise_pattern_expansion() {
    let result = picamatl().arg("--help").output().expect("run picamatl");
    let help = String::from_utf8_lossy(&result.stdout);
    assert!(
        !help.contains("glob pattern"),
        "the CLI does not expand patterns itself, so --help must not offer them"
    );
}

/// A JPEG whose progressive frame header declares 65535 x 65535 x 4
/// components, in a few-hundred-byte PDF. The lossless Huffman pass used to
/// size its coefficient state from that header (about 2 GB) before reading
/// any scan data; under a memory limit that allocation aborts the process,
/// which no in-library fallback can catch. Run under a 1 GiB address-space
/// cap, the way a container or serverless host would run it.
#[cfg(target_os = "linux")]
#[test]
fn a_huge_progressive_jpeg_header_does_not_abort_under_a_memory_limit() {
    fn seg(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, marker];
        v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }
    let mut sof = vec![8, 0xFF, 0xFF, 0xFF, 0xFF, 4];
    for id in 1..=4u8 {
        sof.extend_from_slice(&[id, 0x11, 0]);
    }
    let mut dht = vec![0x00, 1];
    dht.extend_from_slice(&[0u8; 15]);
    dht.push(0);
    let mut sos = vec![4];
    for id in 1..=4u8 {
        sos.extend_from_slice(&[id, 0x00]);
    }
    sos.extend_from_slice(&[0, 0, 0]);
    let mut jpeg = vec![0xFF, 0xD8];
    let mut dqt = vec![0u8];
    dqt.extend_from_slice(&[1u8; 64]);
    jpeg.extend(seg(0xDB, &dqt));
    jpeg.extend(seg(0xC2, &sof));
    jpeg.extend(seg(0xC4, &dht));
    jpeg.extend(seg(0xDA, &sos));
    jpeg.extend_from_slice(&[0u8; 16]);
    jpeg.extend_from_slice(&[0xFF, 0xD9]);

    let mut doc = Document::with_version("1.5");
    let img = doc.add_object(lopdf::Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 64,
            "Height" => 64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
        },
        jpeg,
    ));
    let content = doc.add_object(lopdf::Stream::new(
        dictionary! {},
        b"q 50 0 0 50 0 0 cm /Im0 Do Q".to_vec(),
    ));
    let pages = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages,
        "Contents" => content,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => img } },
    });
    doc.objects.insert(
        pages,
        Object::Dictionary(
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
        ),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    doc.trailer.set("Root", catalog);

    let dir = std::env::temp_dir().join("picamatl-cli-jpeg-sof-bomb");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("bomb.pdf");
    let output = dir.join("out.pdf");
    doc.save(&input).unwrap();

    let result = Command::new("sh")
        .arg("-c")
        .arg("ulimit -v 1048576 && exec \"$0\" -o \"$2\" \"$1\"")
        .arg(env!("CARGO_BIN_EXE_picamatl"))
        .arg(&input)
        .arg(&output)
        .output()
        .expect("run picamatl under a memory limit");
    assert!(
        result.status.success(),
        "picamatl must not abort under a 1 GiB limit: {:?}\n{}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read(&output).unwrap(),
        std::fs::read(&input).unwrap()
    );
}
