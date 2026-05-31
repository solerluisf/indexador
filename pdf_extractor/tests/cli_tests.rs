use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("pdf_extractor_integration")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir.join("pdfs")).unwrap();
    dir
}

fn create_test_pdf(path: &PathBuf, body: &str) {
    use lopdf::*;

    let mut doc = Document::new();
    doc.version = "1.4".to_string();

    let catalog_id = doc.new_object_id();
    let font_id = doc.new_object_id();
    let pages_id = doc.new_object_id();
    let page_id = doc.new_object_id();
    let content_id = doc.new_object_id();

    let stream_data = format!(
        "BT /F1 12 Tf 100 700 Td ({}) Tj ET",
        body.chars()
            .map(|c| match c {
                '(' | ')' | '\\' => format!("\\{}", c),
                _ => c.to_string(),
            })
            .collect::<String>()
    );

    doc.objects.insert(
        font_id,
        Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Font".as_bytes().to_vec())),
            ("Subtype", Object::Name("Type1".as_bytes().to_vec())),
            ("BaseFont", Object::Name("Helvetica".as_bytes().to_vec())),
        ])),
    );

    doc.objects.insert(
        content_id,
        Object::Stream(Stream::new(
            Dictionary::from_iter([("Length", Object::Integer(stream_data.len() as i64))]),
            stream_data.as_bytes().to_vec(),
        )),
    );

    doc.objects.insert(
        page_id,
        Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Page".as_bytes().to_vec())),
            ("Parent", Object::Reference(pages_id)),
            ("MediaBox", Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(612),
                Object::Integer(792),
            ])),
            ("Contents", Object::Reference(content_id)),
            (
                "Resources",
                Object::Dictionary(Dictionary::from_iter([(
                    "Font",
                    Object::Dictionary(Dictionary::from_iter([(
                        "F1",
                        Object::Reference(font_id),
                    )])),
                )])),
            ),
        ])),
    );

    doc.objects.insert(
        pages_id,
        Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Pages".as_bytes().to_vec())),
            ("Kids", Object::Array(vec![Object::Reference(page_id)])),
            ("Count", Object::Integer(1)),
        ])),
    );

    doc.objects.insert(
        catalog_id,
        Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Catalog".as_bytes().to_vec())),
            ("Pages", Object::Reference(pages_id)),
        ])),
    );

    doc.trailer.set("Root", Object::Reference(catalog_id));
    doc.save(path).unwrap();
}

fn binary_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.push("target");
    path.push("debug");
    path.push("pdf_extractor.exe");
    path
}

#[test]
fn test_full_pipeline_with_real_pdfs() {
    let dir = test_dir("full_pipeline");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "Hello World from PDF one");
    create_test_pdf(
        &pdf_dir.join("doc2.pdf"),
        "Second document with unique content",
    );

    assert!(pdf_dir.join("doc1.pdf").exists());
    assert!(pdf_dir.join("doc2.pdf").exists());

    let status = Command::new(binary_path())
        .args([
            "-i",
            pdf_dir.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
            "-d",
            db_path.to_str().unwrap(),
            "-l",
            log_path.to_str().unwrap(),
        ])
        .status()
        .expect("Failed to run pdf_extractor");

    assert!(status.success(), "Binary exited with failure");

    assert!(output_path.exists(), "Output JSONL not created");
    assert!(db_path.exists(), "SQLite DB not created");
    assert!(log_path.exists(), "Log file not created");

    let mut content = String::new();
    std::fs::File::open(&output_path)
        .unwrap()
        .read_to_string(&mut content)
        .unwrap();

    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2);

    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["ocr_flag"], false);
    assert_eq!(first["language"], serde_json::Value::Null);
    assert!(first["checksum"].as_str().unwrap().len() == 16);
    assert!(first["path"].as_str().unwrap().contains("doc1.pdf"));
    assert!(first["text"].as_str().unwrap().contains("Hello World"));

    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert!(second["path"].as_str().unwrap().contains("doc2.pdf"));
    assert!(second["text"].as_str().unwrap().contains("Second document"));

    let log_content = std::fs::read_to_string(&log_path).unwrap();
    assert!(log_content.contains("Extraction complete"));
    assert!(log_content.contains("docs_processed"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_resumable_skip_unchanged_files() {
    let dir = test_dir("resumable");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "Stable content");

    let binary = binary_path();

    let run = |args: &[&str]| -> std::process::Output {
        Command::new(&binary)
            .args(args)
            .output()
            .expect("Failed to run pdf_extractor")
    };

    let args: &[&str] = &[
        "-i",
        pdf_dir.to_str().unwrap(),
        "-o",
        output_path.to_str().unwrap(),
        "-d",
        db_path.to_str().unwrap(),
        "-l",
        log_path.to_str().unwrap(),
    ];

    let first = run(args);
    assert!(first.status.success());

    let first_content = std::fs::read_to_string(&output_path).unwrap();
    assert_eq!(first_content.lines().count(), 1);

    let second = run(args);
    assert!(second.status.success());

    let second_content = std::fs::read_to_string(&output_path).unwrap();
    assert_eq!(
        second_content.lines().count(),
        1,
        "Second run should not reprocess unchanged file"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_empty_pdf_marked_ocr() {
    let dir = test_dir("ocr_flag");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    create_test_pdf(&pdf_dir.join("blank.pdf"), "");

    let status = Command::new(binary_path())
        .args([
            "-i",
            pdf_dir.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
            "-d",
            db_path.to_str().unwrap(),
            "-l",
            log_path.to_str().unwrap(),
        ])
        .status()
        .unwrap();

    assert!(status.success());

    let content = std::fs::read_to_string(&output_path).unwrap();
    let record: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
    assert!(
        record["ocr_flag"].as_bool().unwrap(),
        "Empty PDF should be marked for OCR"
    );
    assert_eq!(record["text"], "");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Alternative flow: mixed valid / invalid PDFs ---

#[test]
fn test_mixed_valid_and_invalid_pdfs() {
    let dir = test_dir("mixed");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    create_test_pdf(&pdf_dir.join("valid.pdf"), "Extracted text");
    std::fs::write(pdf_dir.join("corrupt.pdf"), b"not a real pdf").unwrap();
    create_test_pdf(&pdf_dir.join("also_valid.pdf"), "Another one");

    let out = Command::new(binary_path())
        .args([
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    let content = std::fs::read_to_string(&output_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2, "Only valid PDFs should produce records");

    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("\"docs_errored\":1"));

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Alternative flow: all PDFs error ---

#[test]
fn test_all_pdfs_fail_extraction() {
    let dir = test_dir("all_fail");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    std::fs::write(pdf_dir.join("bad1.pdf"), b"garbage").unwrap();
    std::fs::write(pdf_dir.join("bad2.pdf"), b"corrupt").unwrap();

    let out = Command::new(binary_path())
        .args([
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    let content = std::fs::read_to_string(&output_path).unwrap();
    assert!(content.is_empty(), "No valid PDFs → no JSONL records");

    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("\"docs_errored\":2"));

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Alternative flow: directory with no PDFs ---

#[test]
fn test_no_pdf_files_in_directory() {
    let dir = test_dir("no_pdf");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();
    std::fs::write(pdf_dir.join("data.csv"), b"a,b").unwrap();

    let out = Command::new(binary_path())
        .args([
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    let content = std::fs::read_to_string(&output_path).unwrap();
    assert_eq!(content.trim(), "", "No PDFs → empty JSONL");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Alternative flow: PDFs in subdirectories ---

#[test]
fn test_pdfs_in_subdirectories() {
    let dir = test_dir("subdirs");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    std::fs::create_dir_all(pdf_dir.join("sub")).unwrap();
    create_test_pdf(&pdf_dir.join("root.pdf"), "Root level");
    create_test_pdf(&pdf_dir.join("sub").join("nested.pdf"), "Nested level");

    let out = Command::new(binary_path())
        .args([
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    let content = std::fs::read_to_string(&output_path).unwrap();
    assert_eq!(content.lines().count(), 2);
    assert!(content.contains("Root level"));
    assert!(content.contains("Nested level"));

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Alternative flow: unicode content preserved ---

#[test]
fn test_unicode_content_preserved() {
    let dir = test_dir("unicode");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    create_test_pdf(&pdf_dir.join("unicode.pdf"), "Hello World ASCII text");

    let out = Command::new(binary_path())
        .args([
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    let content = std::fs::read_to_string(&output_path).unwrap();
    let record: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
    let text = record["text"].as_str().unwrap();
    assert!(text.contains("Hello World"));
    assert!(text.contains("ASCII"));

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Alternative flow: resume after partial failure (mix of states) ---

#[test]
fn test_resume_after_partial_failure() {
    let dir = test_dir("resume_after_fail");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    let args = || -> Vec<String> {
        vec![
            "-i".into(), pdf_dir.to_str().unwrap().into(),
            "-o".into(), output_path.to_str().unwrap().into(),
            "-d".into(), db_path.to_str().unwrap().into(),
            "-l".into(), log_path.to_str().unwrap().into(),
        ]
    };

    create_test_pdf(&pdf_dir.join("good.pdf"), "I am valid");
    std::fs::write(pdf_dir.join("bad.pdf"), b"nope").unwrap();
    create_test_pdf(&pdf_dir.join("also_good.pdf"), "Me too");

    // First run: good.pdf + bad.pdf + also_good.pdf
    let r1 = Command::new(binary_path()).args(args()).output().unwrap();
    assert!(r1.status.success());

    let content1 = std::fs::read_to_string(&output_path).unwrap();
    assert_eq!(content1.lines().count(), 2, "2 valid PDFs on first run");

    // Change one file to force re-extraction
    create_test_pdf(&pdf_dir.join("also_good.pdf"), "Updated content");
    std::fs::write(pdf_dir.join("new.pdf"), b"also corrupt").unwrap();

    // Second run: also_good.pdf (changed) + new.pdf (corrupt) + nothing else
    let r2 = Command::new(binary_path()).args(args()).output().unwrap();
    assert!(r2.status.success());

    let content2 = std::fs::read_to_string(&output_path).unwrap();
    let lines2: Vec<&str> = content2.lines().collect();
    assert_eq!(lines2.len(), 3, "Third line appended for the updated file");

    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("\"docs_errored\":1") || log.contains("\"docs_errored\":2"),
        "Expected at least 1 error in logs");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Error flow: invalid directory path ---

#[test]
fn test_invalid_input_directory() {
    let dir = test_dir("invalid_dir");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    let out = Command::new(binary_path())
        .args([
            "-i", r"C:\NONEXISTENT_DIR_FOR_TEST_99999",
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!out.status.success(), "Binary should fail on invalid input dir");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Error flow: input is a file, not a directory ---

#[test]
fn test_input_is_file_not_directory() {
    let dir = std::env::temp_dir()
        .join("pdf_extractor_integration_input_is_file");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pdf_path = dir.join("not_a_dir");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    std::fs::write(&pdf_path, b"this is a file, not a dir").unwrap();

    let out = Command::new(binary_path())
        .args([
            "-i", pdf_path.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!out.status.success(), "Binary should fail when input is a file");

    std::fs::remove_dir_all(&dir).unwrap();
}
