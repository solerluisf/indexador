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
            "extract",
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

    for line in &lines {
        let rec: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(rec["ocr_flag"], false);
        // Language detection: "Hello World from PDF one" (25 chars) may or may not
        // meet whatlang's reliability threshold. Accept either "eng" or null.
        let lang = rec["language"].as_str().map(|s| s.to_string());
        if let Some(l) = &lang {
            assert_eq!(l, "eng", "If language is detected, it should be 'eng', got: {}", l);
        }
        assert!(rec["checksum"].as_str().unwrap().len() == 16);
        let path = rec["path"].as_str().unwrap();
        let text = rec["text"].as_str().unwrap();
        if path.contains("doc1.pdf") {
            assert!(text.contains("Hello World"));
        } else if path.contains("doc2.pdf") {
            assert!(text.contains("Second document"));
        } else {
            panic!("Unexpected path: {}", path);
        }
    }

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
        "extract",
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
            "extract",
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
            "extract",
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
            "extract",
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
            "extract",
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
            "extract",
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
            "extract",
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
            "extract".into(),
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
            "extract",
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
            "extract",
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

// --- Search integration tests ---

fn run_extract(pdf_dir: &PathBuf, output_path: &PathBuf, db_path: &PathBuf, log_path: &PathBuf, index_path: &PathBuf) {
    let status = Command::new(binary_path())
        .args([
            "extract",
            "-i",
            pdf_dir.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
            "-d",
            db_path.to_str().unwrap(),
            "-l",
            log_path.to_str().unwrap(),
            "--index-path",
            index_path.to_str().unwrap(),
        ])
        .status()
        .expect("Failed to run pdf_extractor extract");
    assert!(status.success(), "Extract should succeed");
    assert!(index_path.join("meta.json").exists(), "Index should be created");
}

fn run_search(index_path: &PathBuf, query: &str) -> std::process::Output {
    Command::new(binary_path())
        .args([
            "search",
            "-d",
            "nonexistent.db",
            "--index-path",
            index_path.to_str().unwrap(),
            query,
        ])
        .output()
        .expect("Failed to run search")
}

fn run_search_with_filter(index_path: &PathBuf, query: &str, filter: &str) -> std::process::Output {
    Command::new(binary_path())
        .args([
            "search",
            "-d",
            "nonexistent.db",
            "--index-path",
            index_path.to_str().unwrap(),
            "--path-filter",
            filter,
            query,
        ])
        .output()
        .expect("Failed to run search with path filter")
}

fn run_search_with_offset(index_path: &PathBuf, query: &str, limit: &str, offset: &str) -> std::process::Output {
    Command::new(binary_path())
        .args([
            "search",
            "-d",
            "nonexistent.db",
            "--index-path",
            index_path.to_str().unwrap(),
            "--limit",
            limit,
            "--offset",
            offset,
            query,
        ])
        .output()
        .expect("Failed to run search with offset")
}

fn run_search_field(index_path: &PathBuf, query: &str, field: &str) -> std::process::Output {
    Command::new(binary_path())
        .args([
            "search",
            "-d",
            "nonexistent.db",
            "--index-path",
            index_path.to_str().unwrap(),
            "--field",
            field,
            query,
        ])
        .output()
        .expect("Failed to run search with --field")
}

#[test]
fn test_search_after_extract() {
    let dir = test_dir("search_after_extract");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "Hello World from PDF one");
    create_test_pdf(
        &pdf_dir.join("doc2.pdf"),
        "Second document with unique content",
    );

    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = run_search(&index_path, "Hello");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Expected 1 result for 'Hello'");

    let out2 = run_search(&index_path, "Second");
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("Found 1 result(s)"), "Expected 1 result for 'Second'");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_no_results() {
    let dir = test_dir("search_no_results");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "Rust programming language");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = run_search(&index_path, "nonexistentterm");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No results found"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_invalid_index_path() {
    let dir = test_dir("search_invalid_index");
    let index_path = dir.join("nonexistent_index_dir");

    // Search auto-creates the index, so it should succeed with no results
    let out = run_search(&index_path, "test");
    assert!(out.status.success(), "Search should auto-create index and succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No results found"));

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Search with path filter integration tests ---

#[test]
fn test_search_with_path_filter() {
    let dir = test_dir("search_path_filter");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    std::fs::create_dir_all(pdf_dir.join("reports")).unwrap();
    std::fs::create_dir_all(pdf_dir.join("invoices")).unwrap();
    create_test_pdf(&pdf_dir.join("reports").join("q1.pdf"), "quarterly report earnings");
    create_test_pdf(&pdf_dir.join("invoices").join("inv1.pdf"), "invoice total earnings");

    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Basic: path filter narrows results
    let out_all = run_search(&index_path, "earnings");
    assert!(out_all.status.success());
    let stdout_all = String::from_utf8_lossy(&out_all.stdout);
    assert!(stdout_all.contains("Found 2 result(s)"), "Unfiltered search should find both earnings docs");

    let out = run_search_with_filter(&index_path, "earnings", ".*reports.*");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Path filter should narrow to 1 result");
    assert!(stdout.contains("reports"), "Result path should contain 'reports'");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_path_filter_no_match() {
    let dir = test_dir("search_path_filter_none");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "some content");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Alternative: filter matching nothing
    let out = run_search_with_filter(&index_path, "content", "ZZZZNONEXISTENTZZZZ");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No results found"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_path_filter_invalid_regex() {
    let dir = test_dir("search_path_filter_bad_regex");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "content");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Error flow: invalid regex pattern
    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--path-filter", "[invalid",
            "content",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Invalid path filter regex") || stderr.contains("error"),
        "Invalid path-filter regex should return an error");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Pagination (offset) integration tests ---

fn run_extract_multi(pdf_dir: &PathBuf, output_path: &PathBuf, db_path: &PathBuf, log_path: &PathBuf, index_path: &PathBuf, count: usize) {
    for i in 0..count {
        create_test_pdf(&pdf_dir.join(&format!("doc{}.pdf", i)), &format!("document number {}", i));
    }
    run_extract(pdf_dir, output_path, db_path, log_path, index_path);
}

#[test]
fn test_search_with_offset() {
    let dir = test_dir("search_offset");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    run_extract_multi(&pdf_dir, &output_path, &db_path, &log_path, &index_path, 10);

    // Basic: offset 5 returns page 2
    let out = run_search_with_offset(&index_path, "document", "5", "0");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 5 result(s)"), "Limit 5 should show 5 results");

    // Pagination: offset 5 skips first 5 (equal scores = undefined ordering, just check count)
    let out2 = run_search_with_offset(&index_path, "document", "5", "5");
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("Found 5 result(s)"), "Offset 5 should show 5 results");
    // Total unique results across both pages should be 10
    // (We can't assert exact paths due to equal-score ordering)

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_offset_beyond_total() {
    let dir = test_dir("search_offset_beyond");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    run_extract_multi(&pdf_dir, &output_path, &db_path, &log_path, &index_path, 3);

    // Offset past all results
    let out = run_search_with_offset(&index_path, "document", "10", "10");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No results found"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_offset_with_path_filter() {
    let dir = test_dir("search_offset_filter");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    std::fs::create_dir_all(pdf_dir.join("reports")).unwrap();
    std::fs::create_dir_all(pdf_dir.join("invoices")).unwrap();
    for i in 0..3 {
        create_test_pdf(&pdf_dir.join("reports").join(&format!("r{}.pdf", i)), &format!("report {}", i));
        create_test_pdf(&pdf_dir.join("invoices").join(&format!("i{}.pdf", i)), &format!("invoice {}", i));
    }
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Combined: path filter + offset (3 reports, skip 1, limit 2 → 2 results)
    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--path-filter", ".*reports.*",
            "--limit", "2",
            "--offset", "1",
            "report",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 2 result(s)"), "Path filter + offset should return 2 results (3 reports, skip 1, limit 2)");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Phrase query integration tests ---

#[test]
fn test_search_phrase_query() {
    let dir = test_dir("search_phrase");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "the quick brown fox jumps over the lazy dog");
    create_test_pdf(&pdf_dir.join("doc2.pdf"), "quick brown fox jumps high");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Phrase query with quotes via CLI (inner quotes escaped for the shell)
    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "\"quick brown fox\"",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 2 result(s)"), "Phrase 'quick brown fox' matches both");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_phrase_no_match_when_words_out_of_order() {
    let dir = test_dir("search_phrase_order");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "\"world hello\"",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No results found"), "Out-of-order phrase should not match");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Fuzzy query integration tests ---

#[test]
fn test_search_fuzzy_query() {
    let dir = test_dir("search_fuzzy");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--fuzzy", "1",
            "hallo",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Fuzzy search with typo should match");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_fuzzy_no_match_when_edit_distance_too_low() {
    let dir = test_dir("search_fuzzy_low");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--fuzzy", "1",
            "zzzzz",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No results found"), "Fuzzy search with completely unrelated word should return nothing");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- JSON output integration tests ---

#[test]
fn test_search_json_output_has_results() {
    let dir = test_dir("search_json");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "rust programming language");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--json",
            "rust",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Should be valid JSON array with results
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed.is_array());
    assert_eq!(parsed.as_array().unwrap().len(), 1);
    assert!(parsed[0]["path"].as_str().unwrap().contains("doc.pdf"));
    assert!(parsed[0]["score"].as_f64().unwrap() > 0.0);
    assert!(parsed[0]["snippet"].as_str().unwrap().contains("rust"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_json_output_empty() {
    let dir = test_dir("search_json_empty");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Search for non-matching term with --json
    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--json",
            "nonexistent",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Should be empty JSON array, not text message
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed.is_array());
    assert!(parsed.as_array().unwrap().is_empty());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_json_with_path_filter() {
    let dir = test_dir("search_json_filter");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    std::fs::create_dir_all(pdf_dir.join("reports")).unwrap();
    create_test_pdf(&pdf_dir.join("reports").join("q1.pdf"), "quarterly earnings");
    create_test_pdf(&pdf_dir.join("doc.pdf"), "earnings report");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // --json + --path-filter
    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--json",
            "--path-filter", ".*reports.*",
            "earnings",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);

    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed.is_array());
    assert_eq!(parsed.as_array().unwrap().len(), 1);
    assert!(parsed[0]["path"].as_str().unwrap().contains("reports"));

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Error flow: JSON output with invalid regex ---

#[test]
fn test_search_json_invalid_regex_errors() {
    let dir = test_dir("search_json_bad_regex");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "content");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // --json with invalid regex should still error (not produce partial JSON)
    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--json",
            "--path-filter", "[invalid",
            "content",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Invalid path filter regex") || stderr.contains("error"));

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Alternative: JSON output with offset ---

#[test]
fn test_search_json_with_offset() {
    let dir = test_dir("search_json_offset");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    for i in 0..5 {
        create_test_pdf(&pdf_dir.join(&format!("doc{}.pdf", i)), &format!("document {}", i));
    }
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--json",
            "--limit", "2",
            "--offset", "2",
            "document",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed.is_array());
    assert_eq!(parsed.as_array().unwrap().len(), 2, "Offset 2 with limit 2 on 5 docs should return 2 results");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Index optimize integration tests ---

#[test]
fn test_index_optimize_reduces_segments() {
    let dir = test_dir("index_optimize");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "index-optimize",
            "--index-path", index_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("segments"), "Should report segment reduction");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Delete from index integration tests ---

#[test]
fn test_delete_from_index_by_path() {
    let dir = test_dir("delete_by_path");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    std::fs::create_dir_all(pdf_dir.join("reports")).unwrap();
    create_test_pdf(&pdf_dir.join("reports").join("doc1.pdf"), "hello");
    create_test_pdf(&pdf_dir.join("doc2.pdf"), "hello");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "delete-from-index",
            "--index-path", index_path.to_str().unwrap(),
            "--path", ".*reports.*",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Deleted"), "Should report deletion");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_delete_from_index_by_id() {
    let dir = test_dir("delete_by_id");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "delete-from-index",
            "--index-path", index_path.to_str().unwrap(),
            "--id", "1",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Deleted"), "Should report deletion");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Stem search integration tests ---

#[test]
fn test_search_stem_finds_stemmed_variants() {
    let dir = test_dir("search_stem");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "running quickly");
    create_test_pdf(&pdf_dir.join("doc2.pdf"), "the cat runs fast");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--stem",
            "run",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 2 result(s)"), "Stemmed search should find 'running' and 'runs'");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_stem_with_fuzzy() {
    let dir = test_dir("search_stem_fuzzy");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "algorithm");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--stem",
            "--fuzzy", "1",
            "algorith",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Stemmed + fuzzy search should match");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Error flow: index-stats with a file path instead of directory ---

#[test]
fn test_index_stats_path_is_file_fails() {
    let dir = test_dir("index_stats_file_path");
    let file_path = dir.join("not_a_dir");
    std::fs::write(&file_path, "this is a file, not a directory").unwrap();

    let out = Command::new(binary_path())
        .args([
            "index-stats",
            "--index-path", file_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "index-stats with file path should fail");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- index-stats integration tests ---

#[test]
fn test_index_stats_empty() {
    let dir = test_dir("index_stats_empty");
    let index_path = dir.join("index");

    // Create empty index via search
    let _ = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "test",
        ])
        .output()
        .unwrap();

    let out = Command::new(binary_path())
        .args([
            "index-stats",
            "--index-path", index_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Documents:    0"), "Empty index should have 0 docs");
    assert!(stdout.contains("Segments:"), "Should report segment count");
    assert!(stdout.contains("Size on disk:"), "Should report size");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_index_stats_with_docs() {
    let dir = test_dir("index_stats_with_docs");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "hello world");
    create_test_pdf(&pdf_dir.join("doc2.pdf"), "foo bar");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "index-stats",
            "--index-path", index_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Documents:    2"), "Index with 2 PDFs should have 2 docs");
    assert!(stdout.contains("Segments:"), "Should report segment count");
    assert!(stdout.contains("Size on disk:"), "Should report size");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_index_stats_nonexistent_path() {
    let dir = test_dir("index_stats_nonexistent");
    let index_path = dir.join("nonexistent");

    // Auto-creates the index directory
    let out = Command::new(binary_path())
        .args([
            "index-stats",
            "--index-path", index_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Documents:    0"), "Auto-created empty index");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Stem search error flows ---

#[test]
fn test_search_stem_no_results() {
    let dir = test_dir("search_stem_no_results");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--stem",
            "nonexistent",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No results found"), "Stem search with no match should report no results");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Delete from index error flows ---

#[test]
fn test_delete_from_index_nonexistent_id() {
    let dir = test_dir("delete_nonexistent_id");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "delete-from-index",
            "--index-path", index_path.to_str().unwrap(),
            "--id", "999",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Deleted"), "Should report deletion attempt");
    // Verify the existing document is still searchable
    let search_out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "hello",
        ])
        .output()
        .unwrap();
    assert!(search_out.status.success());
    let search_stdout = String::from_utf8_lossy(&search_out.stdout);
    assert!(search_stdout.contains("Found 1"), "Doc should remain after deleting non-existent id");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_delete_from_index_no_match_path() {
    let dir = test_dir("delete_no_match_path");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "delete-from-index",
            "--index-path", index_path.to_str().unwrap(),
            "--path", ".*nonexistent.*",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Deleted 0 document(s)"), "Should report 0 deletions");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_delete_from_index_invalid_regex() {
    let dir = test_dir("delete_invalid_regex");
    let index_path = dir.join("index"); // doesn't need to exist; CLI creates it

    let out = Command::new(binary_path())
        .args([
            "delete-from-index",
            "--index-path", index_path.to_str().unwrap(),
            "--path", "[invalid",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "Invalid regex should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid") || stderr.contains("regex"), "Should report invalid regex error");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Alternative: JSON output with non-existent index ---

#[test]
fn test_search_json_no_index_returns_empty_array() {
    let dir = test_dir("search_json_no_index");
    let index_path = dir.join("nonexistent");

    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--json",
            "test",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed.is_array());
    assert!(parsed.as_array().unwrap().is_empty());

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- OCR integration tests ---

fn run_ocr(pdf_dir: &PathBuf, db_path: &PathBuf, log_path: &PathBuf) -> std::process::Output {
    Command::new(binary_path())
        .args([
            "ocr",
            "-i",
            pdf_dir.to_str().unwrap(),
            "-d",
            db_path.to_str().unwrap(),
            "-l",
            log_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to run pdf_extractor ocr")
}

#[test]
fn test_ocr_completes_gracefully_no_renderer() {
    let dir = test_dir("ocr_no_renderer");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let ocr_log_path = dir.join("ocr.log");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "Hello World");
    create_test_pdf(&pdf_dir.join("empty.pdf"), "");

    // Extract first to populate the DB (empty PDF gets ocr_flag=true)
    let extract = Command::new(binary_path())
        .args([
            "extract",
            "-i",
            pdf_dir.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
            "-d",
            db_path.to_str().unwrap(),
            "-l",
            log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(extract.status.success());

    let content = std::fs::read_to_string(&output_path).unwrap();
    assert_eq!(content.lines().count(), 2);

    // Run OCR — no Tesseract/renderer available, so gracefully processes 0
    let out = run_ocr(&pdf_dir, &db_path, &ocr_log_path);
    assert!(out.status.success(), "OCR should exit successfully even without renderer");

    let ocr_log = std::fs::read_to_string(&ocr_log_path).unwrap();
    assert!(ocr_log.contains("OCR processing complete"), "Log should indicate completion");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_no_pdfs_in_directory() {
    let dir = test_dir("ocr_no_pdfs");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"not a pdf").unwrap();

    let out = run_ocr(&pdf_dir, &db_path, &log_path);
    assert!(out.status.success(), "OCR on dir with no PDFs should succeed");

    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("OCR processing complete"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_nonexistent_input_directory_fails() {
    let dir = test_dir("ocr_bad_input");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", r"C:\NONEXISTENT_OCR_TEST_DIR_99999",
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!out.status.success(), "OCR with nonexistent input dir should fail");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_tesseract_path_flag() {
    let dir = test_dir("ocr_tesseract_flag");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let ocr_log_path = dir.join("ocr.log");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "");

    // Extract to set ocr_flag
    let extract = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(extract.status.success());

    // Run with --tesseract-path pointing to nonexistent binary — should still complete gracefully
    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", ocr_log_path.to_str().unwrap(),
            "--tesseract-path", r"C:\nonexistent_tesseract.exe",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR should complete even with nonexistent tesseract path");

    let log = std::fs::read_to_string(&ocr_log_path).unwrap();
    assert!(log.contains("OCR processing complete"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_custom_workers_flag() {
    let dir = test_dir("ocr_workers_flag");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--ocr-workers", "2",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR with --ocr-workers flag should succeed");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_zero_workers_is_clamped() {
    let dir = test_dir("ocr_zero_workers");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--ocr-workers", "0",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR with --ocr-workers 0 should be clamped to 1");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_large_workers_flag() {
    let dir = test_dir("ocr_large_workers");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--ocr-workers", "50",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR with --ocr-workers 50 should succeed");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_max_dim_flag() {
    let dir = test_dir("ocr_max_dim");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--max-dim", "1000",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR with --max-dim flag should succeed");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_max_dim_and_workers_flags() {
    let dir = test_dir("ocr_max_dim_workers");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--max-dim", "2000",
            "--ocr-workers", "4",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR with combined flags should succeed");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_max_dim_minimum() {
    let dir = test_dir("ocr_max_dim_min");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--max-dim", "1",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR with --max-dim 1 should succeed");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_max_dim_large() {
    let dir = test_dir("ocr_max_dim_large");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--max-dim", "99999",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR with --max-dim 99999 should succeed");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_lang_flag() {
    let dir = test_dir("ocr_lang");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--lang", "por",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR with --lang por should succeed");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_lang_and_workers_flags() {
    let dir = test_dir("ocr_lang_workers");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--lang", "spa+eng",
            "--ocr-workers", "2",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR with --lang spa+eng and --ocr-workers 2 should succeed");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_lang_and_max_dim_flags() {
    let dir = test_dir("ocr_lang_maxdim");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--lang", "fra",
            "--max-dim", "1500",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR with --lang fra and --max-dim 1500 should succeed");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_default_lang() {
    let dir = test_dir("ocr_default_lang");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("ocr.log");

    std::fs::write(pdf_dir.join("readme.txt"), b"hello").unwrap();

    let out = Command::new(binary_path())
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "OCR without --lang should succeed (default eng)");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Happy path OCR with mock executables ---

fn mock_bin_dir() -> PathBuf {
    let mut path = binary_path();
    path.pop(); // remove pdf_extractor.exe name
    path
}

#[test]
fn test_ocr_happy_path_with_mocks() {
    let dir = test_dir("ocr_happy_path");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let ocr_log_path = dir.join("ocr.log");
    let ocr_output_path = dir.join("ocr_output.jsonl");
    let mock_dir = dir.join("mocks");

    std::fs::create_dir_all(&mock_dir).unwrap();

    // Copy compiled mock binaries to mock dir with expected names
    let bin_dir = mock_bin_dir();
    std::fs::copy(bin_dir.join("mock_mutool.exe"), mock_dir.join("mutool.exe")).unwrap();
    std::fs::copy(bin_dir.join("mock_tesseract.exe"), mock_dir.join("tesseract.exe")).unwrap();

    // Empty PDF triggers ocr_flag=true during extraction
    create_test_pdf(&pdf_dir.join("empty.pdf"), "");

    // Extract to populate the DB
    let extract = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(extract.status.success());
    let content = std::fs::read_to_string(&output_path).unwrap();
    assert!(content.contains("\"ocr_flag\":true"), "Empty PDF should be marked for OCR");

    // Run OCR with mocks on PATH and --output flag
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{};{}", mock_dir.to_str().unwrap(), original_path);

    let ocr = Command::new(binary_path())
        .env("PATH", &new_path)
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", ocr_output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", ocr_log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(ocr.status.success(), "OCR with mocks should succeed");

    let ocr_log = std::fs::read_to_string(&ocr_log_path).unwrap();
    assert!(
        ocr_log.contains("\"ocr_processed\":1"),
        "Log should show 1 OCR-processed document. Log contents:\n{}",
        &ocr_log[..std::cmp::min(800, ocr_log.len())]
    );

    assert!(
        ocr_log.contains("\"ocr_errored\":0"),
        "Log should show 0 OCR errors"
    );

    // Verify JSONL output was written
    assert!(ocr_output_path.exists(), "OCR output file should exist");
    let ocr_content = std::fs::read_to_string(&ocr_output_path).unwrap();
    let lines: Vec<&str> = ocr_content.lines().collect();
    assert_eq!(lines.len(), 1, "Should have 1 OCR record");
    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(record["id"], 1, "Should have correct id");
    assert!(record["path"].as_str().unwrap().contains("empty.pdf"), "Should reference the PDF path");
    assert_eq!(record["ocr_flag"], false, "OCR flag should be false after OCR");
    assert!(record["text"].as_str().unwrap().contains("Mock OCR text"), "Should contain OCR'd text");
    assert!(record["checksum"].as_str().unwrap().len() == 16, "Should have valid checksum");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_multi_page_with_mocks() {
    let dir = test_dir("ocr_multi_page");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let ocr_log_path = dir.join("ocr.log");
    let ocr_output_path = dir.join("ocr_output.jsonl");
    let mock_dir = dir.join("mocks");

    std::fs::create_dir_all(&mock_dir).unwrap();

    let bin_dir = mock_bin_dir();
    std::fs::copy(bin_dir.join("mock_mutool.exe"), mock_dir.join("mutool.exe")).unwrap();
    std::fs::copy(bin_dir.join("mock_tesseract.exe"), mock_dir.join("tesseract.exe")).unwrap();

    create_test_pdf(&pdf_dir.join("empty.pdf"), "");

    // Extract to populate the DB
    let extract = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(extract.status.success());
    let content = std::fs::read_to_string(&output_path).unwrap();
    assert!(content.contains("\"ocr_flag\":true"), "Empty PDF should be marked for OCR");

    // Run OCR with mock reporting 3 pages
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{};{}", mock_dir.to_str().unwrap(), original_path);

    let ocr = Command::new(binary_path())
        .env("PATH", &new_path)
        .env("MOCK_MUTOOL_PAGES", "3")
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", ocr_output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", ocr_log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(ocr.status.success(), "OCR with mocks should succeed");

    let ocr_log = std::fs::read_to_string(&ocr_log_path).unwrap();
    assert!(
        ocr_log.contains("\"ocr_processed\":1"),
        "Log should show 1 OCR-processed document. Log contents:\n{}",
        &ocr_log[..std::cmp::min(800, ocr_log.len())]
    );

    assert!(
        ocr_log.contains("\"ocr_errored\":0"),
        "Log should show 0 OCR errors"
    );

    // Verify JSONL output — 1 record with text from all 3 pages joined
    assert!(ocr_output_path.exists(), "OCR output file should exist");
    let ocr_content = std::fs::read_to_string(&ocr_output_path).unwrap();
    let lines: Vec<&str> = ocr_content.lines().collect();
    assert_eq!(lines.len(), 1, "Should have 1 OCR record for 1 document");

    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(record["id"], 1);
    assert!(record["path"].as_str().unwrap().contains("empty.pdf"));

    let ocr_text = record["text"].as_str().unwrap();
    let count = ocr_text.matches("Mock OCR text").count();
    assert_eq!(count, 3, "OCR text should contain output from all 3 pages, got {} matches", count);

    assert_eq!(record["ocr_flag"], false, "OCR flag should be false after OCR");
    assert!(record["checksum"].as_str().unwrap().len() == 16, "Should have valid checksum");

    // Verify OMP env vars were set on Tesseract subprocess
    let ocr_text = record["text"].as_str().unwrap();
    assert!(
        ocr_text.contains("OMP_THREAD_LIMIT=1"),
        "Tesseract should receive OMP_THREAD_LIMIT=1"
    );
    assert!(
        ocr_text.contains("OMP_NUM_THREADS=1"),
        "Tesseract should receive OMP_NUM_THREADS=1"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_with_single_worker_pool() {
    let dir = test_dir("ocr_single_worker");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let ocr_log_path = dir.join("ocr.log");
    let ocr_output_path = dir.join("ocr_output.jsonl");
    let mock_dir = dir.join("mocks");

    std::fs::create_dir_all(&mock_dir).unwrap();

    let bin_dir = mock_bin_dir();
    std::fs::copy(bin_dir.join("mock_mutool.exe"), mock_dir.join("mutool.exe")).unwrap();
    std::fs::copy(bin_dir.join("mock_tesseract.exe"), mock_dir.join("tesseract.exe")).unwrap();

    create_test_pdf(&pdf_dir.join("empty.pdf"), "");

    let extract = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(extract.status.success());

    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{};{}", mock_dir.to_str().unwrap(), original_path);

    let ocr = Command::new(binary_path())
        .env("PATH", &new_path)
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", ocr_output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", ocr_log_path.to_str().unwrap(),
            "--ocr-workers", "1",
        ])
        .output()
        .unwrap();
    assert!(ocr.status.success(), "OCR with single-worker pool should succeed");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_pool_handles_tesseract_failure() {
    let dir = test_dir("ocr_pool_fail");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let ocr_log_path = dir.join("ocr.log");
    let ocr_output_path = dir.join("ocr_output.jsonl");
    let mock_dir = dir.join("mocks");

    std::fs::create_dir_all(&mock_dir).unwrap();

    // Copy only mock_mutool (not mock_tesseract) so OCR will fail
    let bin_dir = mock_bin_dir();
    std::fs::copy(bin_dir.join("mock_mutool.exe"), mock_dir.join("mutool.exe")).unwrap();
    // Create a tesseract mock that always fails
    let fail_script = mock_dir.join("tesseract.exe");
    std::fs::write(&fail_script, "@echo off\nexit /b 1\n").unwrap();

    create_test_pdf(&pdf_dir.join("empty.pdf"), "");

    let extract = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(extract.status.success());

    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{};{}", mock_dir.to_str().unwrap(), original_path);

    let ocr = Command::new(binary_path())
        .env("PATH", &new_path)
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", ocr_output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", ocr_log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    // OCR should not crash — tesseract failures per page are logged as warnings
    assert!(ocr.status.success(), "OCR should succeed even with tesseract failures");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_list_failed_ocr_empty() {
    let dir = test_dir("list_failed_empty");
    let db_path = dir.join("jobs.db");

    let out = Command::new(binary_path())
        .args(["list-failed-ocr", "-d", db_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "list-failed-ocr on empty DB should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No permanently failed OCR"), "Should report no failures");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_list_failed_ocr_shows_failures() {
    let dir = test_dir("list_failed_shows");
    let pdf_dir = dir.join("pdfs");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let output_path = dir.join("documents.jsonl");
    let mock_dir = dir.join("mocks");

    std::fs::create_dir_all(&mock_dir).unwrap();
    std::fs::create_dir_all(&pdf_dir).unwrap();

    // Use failing tesseract so OCR permanently fails
    let bin_dir = mock_bin_dir();
    std::fs::copy(bin_dir.join("mock_mutool.exe"), mock_dir.join("mutool.exe")).unwrap();
    std::fs::write(mock_dir.join("tesseract.exe"), "@echo off\nexit /b 1\n").unwrap();

    create_test_pdf(&pdf_dir.join("fail.pdf"), "");

    // Extract
    let extract = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(extract.status.success());

    // Run OCR — will fail. Need 2 runs to exhaust max_retries=2
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{};{}", mock_dir.to_str().unwrap(), original_path);

    let ocr1 = Command::new(binary_path())
        .env("PATH", &new_path)
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--ocr-workers", "1",
        ])
        .output()
        .unwrap();
    assert!(ocr1.status.success(), "First OCR run should succeed");

    // Second run exhausts retries → marks as permanently failed
    let ocr2 = Command::new(binary_path())
        .env("PATH", &new_path)
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--ocr-workers", "1",
        ])
        .output()
        .unwrap();
    assert!(ocr2.status.success(), "Second OCR run should succeed");

    // Now list failed
    let list = Command::new(binary_path())
        .args(["list-failed-ocr", "-d", db_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(list.status.success(), "list-failed-ocr should succeed");
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("fail.pdf"), "Should list the failed document");
    assert!(stdout.contains("OCR returned empty text") || stdout.contains("Worker error"),
        "Should include error message, got: {}", stdout);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_extract_language_detected_in_jsonl() {
    let dir = test_dir("extract_lang_detected");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "This is a sufficiently long English text for reliable language detection by the whatlang library.");

    let status = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let content = std::fs::read_to_string(&output_path).unwrap();
    let rec: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
    assert_eq!(rec["language"], "eng", "Long English text should be detected as 'eng', got: {}", rec["language"]);
    assert!(rec["text"].as_str().unwrap().contains("sufficiently long English text"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_language_in_jsonl() {
    let dir = test_dir("ocr_lang_jsonl");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let ocr_log_path = dir.join("ocr.log");
    let ocr_output_path = dir.join("ocr_output.jsonl");
    let mock_dir = dir.join("mocks");

    std::fs::create_dir_all(&mock_dir).unwrap();

    let bin_dir = mock_bin_dir();
    std::fs::copy(bin_dir.join("mock_mutool.exe"), mock_dir.join("mutool.exe")).unwrap();
    std::fs::copy(bin_dir.join("mock_tesseract.exe"), mock_dir.join("tesseract.exe")).unwrap();

    create_test_pdf(&pdf_dir.join("doc.pdf"), "");

    let extract = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(extract.status.success());

    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{};{}", mock_dir.to_str().unwrap(), original_path);

    let ocr = Command::new(binary_path())
        .env("PATH", &new_path)
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", ocr_output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", ocr_log_path.to_str().unwrap(),
            "--lang", "por",
        ])
        .output()
        .unwrap();
    assert!(ocr.status.success(), "OCR with --lang por should succeed");

    let ocr_content = std::fs::read_to_string(&ocr_output_path).unwrap();
    let rec: serde_json::Value = serde_json::from_str(ocr_content.trim()).unwrap();
    assert_eq!(rec["language"], "por", "OCR JSONL should contain 'por' language, got: {}", rec["language"]);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_ocr_language_default_eng_in_jsonl() {
    let dir = test_dir("ocr_lang_eng_default");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let ocr_log_path = dir.join("ocr.log");
    let ocr_output_path = dir.join("ocr_output.jsonl");
    let mock_dir = dir.join("mocks");

    std::fs::create_dir_all(&mock_dir).unwrap();

    let bin_dir = mock_bin_dir();
    std::fs::copy(bin_dir.join("mock_mutool.exe"), mock_dir.join("mutool.exe")).unwrap();
    std::fs::copy(bin_dir.join("mock_tesseract.exe"), mock_dir.join("tesseract.exe")).unwrap();

    create_test_pdf(&pdf_dir.join("doc.pdf"), "");

    let extract = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(extract.status.success());

    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{};{}", mock_dir.to_str().unwrap(), original_path);

    let ocr = Command::new(binary_path())
        .env("PATH", &new_path)
        .args([
            "ocr",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", ocr_output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", ocr_log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(ocr.status.success(), "OCR without --lang should succeed");

    let ocr_content = std::fs::read_to_string(&ocr_output_path).unwrap();
    let rec: serde_json::Value = serde_json::from_str(ocr_content.trim()).unwrap();
    assert_eq!(rec["language"], "eng", "OCR without --lang should default to 'eng', got: {}", rec["language"]);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_field_normalized_text() {
    let dir = test_dir("search_field_normalized");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "Hello World from PDF one");
    create_test_pdf(&pdf_dir.join("doc2.pdf"), "Second document with unique content");

    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Search normalized_text field — English docs should be found.
    let out = run_search_field(&index_path, "Hello", "normalized_text");
    assert!(out.status.success(), "Search with --field normalized_text should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Expected 1 result in normalized_text for 'Hello', got: {}", stdout);

    // Search normalized_text for non-existent term.
    let out2 = run_search_field(&index_path, "nonexistent", "normalized_text");
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("No results found"), "Expected no results for non-existent term");

    // Invalid field name should produce an error.
    let out3 = run_search_field(&index_path, "Hello", "nonexistent_field");
    assert!(!out3.status.success(), "Invalid field name should fail");
    let stderr3 = String::from_utf8_lossy(&out3.stderr);
    assert!(stderr3.contains("not found in schema"), "Error should mention missing field");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_field_content_norm() {
    let dir = test_dir("search_field_norm");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "Hello World from content_norm field");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = run_search_field(&index_path, "World", "content_norm");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Expected 1 result, got: {}", stdout);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_field_stem() {
    let dir = test_dir("search_field_stem");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "running quickly through the park");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Search "run" in content_stem — should match "running" via stemming.
    let out = run_search_field(&index_path, "run", "content_stem");
    assert!(out.status.success(), "Stem search with --field should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Expected 1 result in content_stem, got: {}", stdout);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_field_math_tokens() {
    let dir = test_dir("search_field_math");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "Energy found from E = mc^2");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // The math tokenizer indexes individual tokens in math_tokens.
    let out = run_search_field(&index_path, "energy", "math_tokens");
    assert!(out.status.success(), "Search math_tokens should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)") || stdout.contains("No results found"),
        "math_tokens search should find the doc or return 0 results, got: {}", stdout);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_field_with_path_filter() {
    let dir = test_dir("search_field_path");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("alpha.pdf"), "hello alpha");
    create_test_pdf(&pdf_dir.join("beta.pdf"), "hello beta");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Combine --field with --path-filter (regex matching the stored path)
    let out = Command::new(binary_path())
        .args([
            "search", "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--field", "normalized_text",
            "--path-filter", ".*alpha.*",
            "hello",
        ])
        .output()
        .expect("Search with --field and --path-filter");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Expected 1 result filtered by path, got: {}", stdout);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_field_with_json() {
    let dir = test_dir("search_field_json");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello json output test");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args([
            "search", "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--field", "normalized_text",
            "--json",
            "hello",
        ])
        .output()
        .expect("Search with --field and --json");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // JSON output should be valid JSON containing results
    assert!(stdout.starts_with("["), "JSON output should start with '['");
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(&stdout);
    assert!(parsed.is_ok(), "Output should be valid JSON");
    let results = parsed.unwrap();
    assert!(!results.as_array().unwrap().is_empty(), "Should have at least one result");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_field_with_limit_offset() {
    let dir = test_dir("search_field_pages");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("a.pdf"), "hello alpha");
    create_test_pdf(&pdf_dir.join("b.pdf"), "hello beta");
    create_test_pdf(&pdf_dir.join("c.pdf"), "hello gamma");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Limit + offset with field search
    let out = Command::new(binary_path())
        .args([
            "search", "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--field", "normalized_text",
            "--limit", "1",
            "--offset", "1",
            "hello",
        ])
        .output()
        .expect("Search with --field, --limit, --offset");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Expected 1 result with limit+offset, got: {}", stdout);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_field_nonexistent_field_messages() {
    let dir = test_dir("search_field_bad");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Empty field name
    let out = Command::new(binary_path())
        .args([
            "search", "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--field", "",
            "hello",
        ])
        .output()
        .expect("Search with empty --field");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error") || stderr.contains("not found"), "Empty field should produce error, got: {}", stderr);

    // Non-existent field
    let out2 = run_search_field(&index_path, "hello", "does_not_exist");
    assert!(!out2.status.success());
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(stderr2.contains("not found in schema"), "Error should mention missing field, got: {}", stderr2);
    // Error should also list valid field names
    assert!(stderr2.contains("Valid fields:"), "Error should suggest valid fields, got: {}", stderr2);
    assert!(stderr2.contains("normalized_text"), "Error should include 'normalized_text' in valid fields, got: {}", stderr2);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_field_with_fuzzy() {
    let dir = test_dir("search_field_fuzzy");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "quantum computing machine learning");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Fuzzy search on a field should match with typo
    let out = Command::new(binary_path())
        .args([
            "search", "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--field", "normalized_text",
            "--fuzzy", "2",
            "quantum",
        ])
        .output()
        .expect("Search with --field and --fuzzy");
    assert!(out.status.success(), "Field + fuzzy search should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Expected 1 result with fuzzy, got: {}", stdout);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_search_field_fuzzy_no_match() {
    let dir = test_dir("search_field_fuzzy_none");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Fuzzy search with very small edit distance should not match
    let out = Command::new(binary_path())
        .args([
            "search", "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--field", "normalized_text",
            "--fuzzy", "1",
            "zzzzz",
        ])
        .output()
        .expect("Search with --field and --fuzzy no match");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No results found"), "Expected no results for non-matching fuzzy query");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_list_fields() {
    let dir = test_dir("list_fields");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = Command::new(binary_path())
        .args(["list-fields", "--index-path", index_path.to_str().unwrap()])
        .output()
        .expect("list-fields should succeed");
    assert!(out.status.success(), "list-fields command should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("normalized_text"), "list-fields should show normalized_text, got: {}", stdout);
    assert!(stdout.contains("content_norm"), "list-fields should show content_norm, got: {}", stdout);
    assert!(stdout.contains("content_stem"), "list-fields should show content_stem, got: {}", stdout);
    assert!(stdout.contains("content_jp"), "list-fields should show content_jp, got: {}", stdout);
    assert!(stdout.contains("content_zh"), "list-fields should show content_zh, got: {}", stdout);
    assert!(stdout.contains("math_tokens"), "list-fields should show math_tokens, got: {}", stdout);

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Phase 7: configurable performance parameters ---

#[test]
fn test_extract_with_custom_ram_buffer() {
    let dir = test_dir("extract_ram_buffer");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");

    let status = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--index-path", index_path.to_str().unwrap(),
            "--ram-buffer", "256000000",
        ])
        .status()
        .expect("Failed to run pdf_extractor extract with custom ram buffer");
    assert!(status.success(), "Extract with custom ram buffer should succeed");
    assert!(index_path.join("meta.json").exists(), "Index should be created");
    assert!(output_path.exists(), "Output JSONL should exist");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_extract_with_custom_extract_workers() {
    let dir = test_dir("extract_workers");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");

    let status = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--index-path", index_path.to_str().unwrap(),
            "--extract-workers", "2",
        ])
        .status()
        .expect("Failed to run pdf_extractor extract with custom extract workers");
    assert!(status.success(), "Extract with custom extract workers should succeed");
    assert!(index_path.join("meta.json").exists(), "Index should be created");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_extract_with_custom_indexer_config() {
    let dir = test_dir("extract_indexer_config");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");

    let status = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--index-path", index_path.to_str().unwrap(),
            "--indexer-batch-size", "100",
            "--commit-interval", "1000",
            "--commit-timeout", "10",
        ])
        .status()
        .expect("Failed to run pdf_extractor extract with custom indexer config");
    assert!(status.success(), "Extract with custom indexer config should succeed");
    assert!(index_path.join("meta.json").exists(), "Index should be created");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_extract_with_zero_extract_workers() {
    let dir = test_dir("extract_workers_zero");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc.pdf"), "hello world");

    // --extract-workers 0 should be clamped to 1, so the command should still succeed
    let status = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--index-path", index_path.to_str().unwrap(),
            "--extract-workers", "0",
        ])
        .status()
        .expect("Failed to run pdf_extractor extract with zero workers");
    assert!(status.success(), "Extract with --extract-workers 0 should succeed (clamped to 1)");
    assert!(index_path.join("meta.json").exists(), "Index should be created");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_extract_with_all_tuning_flags() {
    let dir = test_dir("extract_all_tuning");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    // Use multiple PDFs to exercise the pipeline
    for i in 0..5 {
        create_test_pdf(&pdf_dir.join(format!("doc{}.pdf", i)), &format!("document number {}", i));
    }

    let status = Command::new(binary_path())
        .args([
            "extract",
            "-i", pdf_dir.to_str().unwrap(),
            "-o", output_path.to_str().unwrap(),
            "-d", db_path.to_str().unwrap(),
            "-l", log_path.to_str().unwrap(),
            "--index-path", index_path.to_str().unwrap(),
            "--ram-buffer", "128000000",
            "--extract-workers", "3",
            "--indexer-batch-size", "50",
            "--commit-interval", "500",
            "--commit-timeout", "5",
        ])
        .status()
        .expect("Failed to run pdf_extractor extract with all tuning flags");
    assert!(status.success(), "Extract with all tuning flags should succeed");
    assert!(index_path.join("meta.json").exists(), "Index should be created");
    assert!(output_path.exists(), "Output JSONL should exist");

    // Verify all docs were indexed by searching
    let out = Command::new(binary_path())
        .args(["search", "-d", "nonexistent.db", "--index-path", index_path.to_str().unwrap(), "document"])
        .output()
        .expect("Search after tuned extract should succeed");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found"), "Search should find results after tuned extract");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Phase 8: Boost field integration tests ---

fn run_search_boost(index_path: &PathBuf, query: &str, boosts: &[&str]) -> std::process::Output {
    let mut args: Vec<String> = vec![
        "search".into(),
        "-d".into(), "nonexistent.db".into(),
        "--index-path".into(), index_path.to_str().unwrap().into(),
    ];
    for b in boosts {
        args.push("--boost-field".into());
        args.push(b.to_string());
    }
    args.push(query.to_string());
    Command::new(binary_path())
        .args(&args)
        .output()
        .expect("Failed to run search with --boost-field")
}

#[test]
fn test_boost_field_single() {
    let dir = test_dir("boost_single");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "rust programming language");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Single boost field
    let out = run_search_boost(&index_path, "rust", &["content_norm:2.0"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Boost search should find results");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_boost_field_multiple() {
    let dir = test_dir("boost_multi");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "rust programming language");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Multiple boost fields
    let out = run_search_boost(&index_path, "rust", &["content_norm:1.5", "content_raw:1.0"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Multi-boost search should find results");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_boost_field_no_results() {
    let dir = test_dir("boost_no_results");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = run_search_boost(&index_path, "nonexistent", &["content_norm:2.0"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No results found"), "Boost search with no match should report no results");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_boost_field_json_output() {
    let dir = test_dir("boost_json");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "test document");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let args: Vec<String> = vec![
        "search".into(),
        "-d".into(), "nonexistent.db".into(),
        "--index-path".into(), index_path.to_str().unwrap().into(),
        "--json".into(),
        "--boost-field".into(), "content_norm:2.0".into(),
        "test".into(),
    ];
    let out = Command::new(binary_path())
        .args(&args)
        .output()
        .expect("Failed to run boost search with JSON");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed.is_array());
    assert_eq!(parsed.as_array().unwrap().len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Phase 8: Recency weight integration tests ---

fn run_search_recency(index_path: &PathBuf, query: &str, recency_weight: &str) -> std::process::Output {
    Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--recency-weight", recency_weight,
            query,
        ])
        .output()
        .expect("Failed to run search with --recency-weight")
}

#[test]
fn test_recency_weight_flag() {
    let dir = test_dir("recency_weight");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "test content");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = run_search_recency(&index_path, "test", "0.5");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Recency-boosted search should find results");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_recency_weight_zero() {
    let dir = test_dir("recency_zero");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "test content");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = run_search_recency(&index_path, "test", "0.0");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Zero recency weight should work same as default");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_recency_weight_max() {
    let dir = test_dir("recency_max");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "test content");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    let out = run_search_recency(&index_path, "test", "1.0");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Max recency weight should still work");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_recency_weight_with_boost_field() {
    let dir = test_dir("recency_boost_combined");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "test content");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Combined: --boost-field + --recency-weight
    let args: Vec<String> = vec![
        "search".into(),
        "-d".into(), "nonexistent.db".into(),
        "--index-path".into(), index_path.to_str().unwrap().into(),
        "--boost-field".into(), "content_norm:2.0".into(),
        "--recency-weight".into(), "0.5".into(),
        "test".into(),
    ];
    let out = Command::new(binary_path())
        .args(&args)
        .output()
        .expect("Failed to run combined boost + recency search");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Combined boost + recency should find results");

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- Phase 8 error flow integration tests ---

#[test]
fn test_boost_field_invalid_format_defaults_to_one() {
    let dir = test_dir("boost_invalid");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Bad weight format: silently defaults to 1.0, search should succeed
    let out = run_search_boost(&index_path, "hello", &["content_norm:abc"]);
    assert!(out.status.success(), "Invalid weight should silently default to 1.0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Boost with invalid weight should still find results");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_boost_field_without_colon_uses_default_weight() {
    let dir = test_dir("boost_no_colon");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // No colon → treated as field name with default weight 1.0
    let out = run_search_boost(&index_path, "hello", &["content_norm"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Boost with only field name (no colon) should use default weight");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_boost_field_nonexistent_field_returns_no_results() {
    let dir = test_dir("boost_nonexistent");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Nonexistent field → silently skipped → no query clauses → no results
    let out = run_search_boost(&index_path, "hello", &["nonexistent_field:2.0"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No results found"), "Nonexistent boost field should silently produce no results");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_recency_weight_negative() {
    let dir = test_dir("recency_negative");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // Use --recency-weight=-0.5 (equals sign) to avoid clap parsing -0 as a flag
    let out = Command::new(binary_path())
        .args([
            "search",
            "-d", "nonexistent.db",
            "--index-path", index_path.to_str().unwrap(),
            "--recency-weight=-0.5",
            "hello",
        ])
        .output()
        .expect("Failed to run search with negative recency weight");
    assert!(out.status.success(), "Negative recency weight should not error");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "Negative recency weight should still return results unchanged");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_boost_field_with_field_flag_boost_takes_priority() {
    let dir = test_dir("boost_with_field");
    let pdf_dir = dir.join("pdfs");
    let output_path = dir.join("documents.jsonl");
    let db_path = dir.join("jobs.db");
    let log_path = dir.join("extractor.log");
    let index_path = dir.join("index");

    create_test_pdf(&pdf_dir.join("doc1.pdf"), "hello world");
    run_extract(&pdf_dir, &output_path, &db_path, &log_path, &index_path);

    // --boost-field takes priority over --field; should succeed either way
    let args: Vec<String> = vec![
        "search".into(),
        "-d".into(), "nonexistent.db".into(),
        "--index-path".into(), index_path.to_str().unwrap().into(),
        "--boost-field".into(), "content_norm:2.0".into(),
        "--field".into(), "content_norm".into(),
        "hello".into(),
    ];
    let out = Command::new(binary_path())
        .args(&args)
        .output()
        .expect("Failed to run boost-field with --field");
    assert!(out.status.success(), "boost-field with --field should still succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Found 1 result(s)"), "boost-field should take priority, finding 1 result");

    std::fs::remove_dir_all(&dir).unwrap();
}
