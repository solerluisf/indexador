mod scanner;
mod extractor;
mod normalizer;
mod output;
mod metrics;
mod pipeline;
mod indexer;
mod ocr;
mod lang;
mod tokenizers;
mod math_tokenizer;

use anyhow::Result;
use clap::{Parser, Subcommand};
use indexer::Indexer;
use std::path::PathBuf;
use std::sync::Arc;
use tantivy::schema::Value;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "pdf_extractor", about = "Extract text from PDFs into JSONL and search")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Extract {
        #[arg(short = 'i', long = "input", help = "Input directory containing PDFs")]
        input: PathBuf,

        #[arg(short = 'o', long = "output", default_value = "documents.jsonl", help = "Output JSONL file")]
        output: PathBuf,

        #[arg(short = 'd', long = "db", default_value = "jobs.db", help = "SQLite database path")]
        db: PathBuf,

        #[arg(short = 'l', long = "log", default_value = "extractor.log", help = "Log file path")]
        log: PathBuf,

        #[arg(long = "index-path", help = "Path to Tantivy search index directory")]
        index_path: Option<PathBuf>,

        #[arg(long = "ram-buffer", default_value = "500000000", help = "IndexWriter RAM buffer in bytes (default: 500MB)")]
        ram_buffer: u64,

        #[arg(long = "extract-workers", help = "Number of extractor worker threads (default: cores - 2)")]
        extract_workers: Option<usize>,

        #[arg(long = "indexer-batch-size", default_value = "500", help = "Documents per indexer batch before flush")]
        indexer_batch_size: usize,

        #[arg(long = "commit-interval", default_value = "5000", help = "Documents between index commits")]
        commit_interval: u64,

        #[arg(long = "commit-timeout", default_value = "30", help = "Max seconds before forced commit")]
        commit_timeout: u64,
    },
    Search {
        #[arg(short = 'd', long = "db", default_value = "jobs.db", help = "SQLite database path")]
        db: PathBuf,

        #[arg(long = "index-path", help = "Path to Tantivy search index directory")]
        index_path: PathBuf,

        #[arg(help = "Search query string. Supports phrase queries (\"hello world\") and term queries")]
        query: String,

        #[arg(short = 'l', long = "limit", default_value = "10", help = "Maximum number of results")]
        limit: usize,

        #[arg(long = "offset", default_value = "0", help = "Number of results to skip (for pagination)")]
        offset: usize,

        #[arg(long = "path-filter", help = "Regex pattern to filter by document path")]
        path_filter: Option<String>,

        #[arg(long = "json", help = "Output results as JSON array")]
        json: bool,

        #[arg(long = "fuzzy", num_args = 0..=1, default_missing_value = "1", default_value_t = 0, help = "Enable fuzzy search with optional edit distance (default: 1)")]
        fuzzy_distance: u8,

        #[arg(long = "stem", help = "Apply stemming and stop-word removal to the search query")]
        stem: bool,

        #[arg(long = "field", help = "Search in a specific indexed field (e.g. normalized_text, content_jp, content_zh)")]
        field: Option<String>,
    },
    IndexStats {
        #[arg(long = "index-path", help = "Path to Tantivy search index directory")]
        index_path: PathBuf,
    },
    IndexOptimize {
        #[arg(long = "index-path", help = "Path to Tantivy search index directory")]
        index_path: PathBuf,
    },
    DeleteFromIndex {
        #[arg(long = "index-path", help = "Path to Tantivy search index directory")]
        index_path: PathBuf,

        #[arg(long = "path", help = "Delete documents whose path matches this regex")]
        path: Option<String>,

        #[arg(long = "id", help = "Delete document by its numeric ID")]
        id: Option<i64>,
    },
    Ocr {
        #[arg(short = 'i', long = "input", help = "Input directory containing PDFs")]
        input: PathBuf,

        #[arg(short = 'o', long = "output", help = "Output JSONL file for OCR results")]
        output: Option<PathBuf>,

        #[arg(short = 'd', long = "db", default_value = "jobs.db", help = "SQLite database path")]
        db: PathBuf,

        #[arg(short = 'l', long = "log", default_value = "ocr.log", help = "Log file path")]
        log: PathBuf,

        #[arg(long = "index-path", help = "Path to Tantivy search index directory (optional, for re-indexing OCR text)")]
        index_path: Option<PathBuf>,

        #[arg(long = "tesseract-path", help = "Path to tesseract executable")]
        tesseract_path: Option<PathBuf>,

        #[arg(long = "ocr-workers", help = "Number of OCR worker threads")]
        ocr_workers: Option<usize>,

        #[arg(long = "max-dim", default_value = "3000", help = "Maximum image dimension in pixels for OCR preprocessing")]
        max_dim: u32,

        #[arg(long = "lang", default_value = "eng", help = "Tesseract language (e.g. eng, por, spa+por)")]
        language: String,
    },
    ListFailedOcr {
        #[arg(short = 'd', long = "db", default_value = "jobs.db", help = "SQLite database path")]
        db: PathBuf,
    },
    ListFields {
        #[arg(long = "index-path", help = "Path to Tantivy search index directory")]
        index_path: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Extract {
            input,
            output,
            db,
            log,
            index_path,
            ram_buffer,
            extract_workers,
            indexer_batch_size,
            commit_interval,
            commit_timeout,
        } => run_extract(input, output, db, log, index_path, ram_buffer, extract_workers, indexer_batch_size, commit_interval, commit_timeout),
        Commands::Search {
            db: _,
            index_path,
            query,
            limit,
            offset,
            path_filter,
            json,
            fuzzy_distance,
            stem,
            field,
        } => run_search(index_path, query, limit, offset, path_filter, json, fuzzy_distance, stem, field),
        Commands::IndexStats { index_path } => run_index_stats(index_path),
        Commands::IndexOptimize { index_path } => run_index_optimize(index_path),
        Commands::DeleteFromIndex { index_path, path, id } => run_delete_from_index(index_path, path, id),
        Commands::Ocr {
            input,
            output,
            db,
            log,
            index_path,
            tesseract_path,
            ocr_workers,
            max_dim,
            language,
        } => run_ocr(input, output, db, log, index_path, tesseract_path, ocr_workers, max_dim, language),
        Commands::ListFailedOcr { db } => run_list_failed_ocr(db),
        Commands::ListFields { index_path } => run_list_fields(index_path),
    }
}

fn run_extract(
    input: PathBuf,
    output: PathBuf,
    db: PathBuf,
    log: PathBuf,
    index_path: Option<PathBuf>,
    ram_buffer: u64,
    extract_workers: Option<usize>,
    indexer_batch_size: usize,
    commit_interval: u64,
    commit_timeout: u64,
) -> Result<()> {
    let log_file = std::fs::File::create(&log)
        .expect("Failed to create log file");
    let (non_blocking, _guard) = tracing_appender::non_blocking(log_file);

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(non_blocking)
        .init();

    info!(
        input = %input.display(),
        output = %output.display(),
        db = %db.display(),
        ram_buffer = ram_buffer,
        extract_workers = extract_workers,
        indexer_batch_size = indexer_batch_size,
        commit_interval = commit_interval,
        commit_timeout = commit_timeout,
        "Starting pdf_extractor"
    );

    let jobs = Arc::new(scanner::JobStore::open(&db)?);
    let writer = output::JsonlWriter::new(&output)?;
    let metrics = Arc::new(metrics::Metrics::new());

    let indexer = match &index_path {
        Some(path) => {
            std::fs::create_dir_all(path).ok();
            let idx = Indexer::with_ram_buffer(path, ram_buffer)?;
            info!(index_path = %path.display(), ram_buffer = ram_buffer, "Search index initialized");
            Some(Arc::new(idx))
        }
        None => None,
    };

    let pipeline_config = pipeline::PipelineConfig {
        num_extract_workers: extract_workers,
        indexer_batch_size: Some(indexer_batch_size),
        commit_interval: Some(commit_interval),
        commit_timeout: Some(commit_timeout),
    };

    pipeline::run_pipeline(
        Arc::clone(&jobs),
        &writer,
        Arc::clone(&metrics),
        &input,
        indexer,
        &pipeline_config,
    )?;

    metrics.log_summary();
    info!(
        docs_processed = metrics.processed(),
        docs_errored = metrics.errored(),
        total_elapsed_secs = format!("{:.1}", metrics.elapsed_secs()),
        avg_throughput = format!("{:.2}", metrics.throughput()),
        "Extraction complete"
    );

    Ok(())
}

fn run_search(index_path: PathBuf, query: String, limit: usize, offset: usize, path_filter: Option<String>, json: bool, fuzzy_distance: u8, stem: bool, field: Option<String>) -> Result<()> {
    let indexer = Indexer::new(&index_path)?;

    let results = if let Some(field_name) = field {
        let si = indexer.search_index();
        // Validate early: provide helpful error with valid fields.
        if si.schema.get_field(&field_name).is_err() {
            let valid: Vec<&str> = si.schema.fields().filter_map(|(_, f)| Some(f.name())).collect();
            anyhow::bail!(
                "Field '{}' not found in schema. Valid fields: {}",
                field_name, valid.join(", ")
            );
        }
        si.search_in_field_fuzzy_stem(
            &query, &field_name, limit, path_filter.as_deref(), offset, fuzzy_distance, stem,
        )?
    } else if fuzzy_distance > 0 {
        indexer.search_index().search_fuzzy_stem(&query, limit, path_filter.as_deref(), offset, fuzzy_distance, stem)?
    } else {
        indexer.search_index().search_stem(&query, limit, path_filter.as_deref(), offset, stem)?
    };

    if json {
        let json_results: Vec<serde_json::Value> = results
            .iter()
            .map(|(score, doc)| {
                let path = doc
                    .get_first(indexer.search_index().path_field)
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unknown)");
                let snippet = indexer
                    .search_index()
                    .generate_snippet(doc, &query)
                    .unwrap_or_default();
                serde_json::json!({
                    "score": score,
                    "path": path,
                    "snippet": snippet,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_results)?);
        return Ok(());
    }

    if results.is_empty() {
        println!("No results found for: {}", query);
        return Ok(());
    }

    println!("Found {} result(s) for: {}\n", results.len(), query);
    for (i, (score, doc)) in results.iter().enumerate() {
        let path = doc
            .get_first(indexer.search_index().path_field)
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)");
        let snippet = indexer
            .search_index()
            .generate_snippet(doc, &query)
            .unwrap_or_default();

        println!("{}. [score: {:.4}] {}", i + 1, score, path);
        println!("   ...{}...\n", snippet);
    }

    Ok(())
}

fn run_list_fields(index_path: PathBuf) -> Result<()> {
    let indexer = Indexer::new(&index_path)?;
    let si = indexer.search_index();
    println!("Index fields for: {}", index_path.display());
    println!("{:-<60}", "");
    for (_, field) in si.schema.fields() {
        let field_type = match field.field_type() {
            tantivy::schema::FieldType::U64(_) => "u64",
            tantivy::schema::FieldType::I64(_) => "i64",
            tantivy::schema::FieldType::F64(_) => "f64",
            tantivy::schema::FieldType::Str(_) => "string",
            tantivy::schema::FieldType::Bytes(_) => "bytes",
            _ => "other",
        };
        let is_indexed = field.is_indexed();
        let is_stored = field.is_stored();
        println!("  {:<20}  {:<8}  indexed={} stored={}", field.name(), field_type, is_indexed, is_stored);
    }
    Ok(())
}

fn run_index_stats(index_path: PathBuf) -> Result<()> {
    let indexer = Indexer::new(&index_path)?;
    let stats = indexer.search_index().compute_stats(&index_path)?;

    let size_mb = stats.size_bytes as f64 / (1024.0 * 1024.0);
    println!("Index stats for: {}", index_path.display());
    println!("  Documents:    {}", stats.num_docs);
    println!("  Segments:     {}", stats.num_segments);
    println!("  Size on disk: {:.2} MB", size_mb);

    Ok(())
}

fn run_index_optimize(index_path: PathBuf) -> Result<()> {
    let indexer = Indexer::new(&index_path)?;
    let (before, after) = indexer.search_index().optimize()?;
    println!("Index optimized: {} segments -> {} segments", before, after);
    Ok(())
}

fn run_delete_from_index(index_path: PathBuf, path: Option<String>, id: Option<i64>) -> Result<()> {
    let indexer = Indexer::new(&index_path)?;
    match (path, id) {
        (Some(p), None) => {
            let count = indexer.search_index().delete_by_path(&p)?;
            println!("Deleted {} document(s) matching path pattern: {}", count, p);
        }
        (None, Some(i)) => {
            let found = indexer.search_index().delete_by_id(i)?;
            println!("Deleted {} document(s) with id: {}", if found { 1 } else { 0 }, i);
        }
        (None, None) => {
            println!("Error: specify --path <regex> or --id <id> to delete documents");
        }
        (Some(_), Some(_)) => {
            println!("Error: specify only one of --path or --id");
        }
    }
    Ok(())
}

fn run_ocr(input: PathBuf, output: Option<PathBuf>, db: PathBuf, log: PathBuf, index_path: Option<PathBuf>, tesseract_path: Option<PathBuf>, ocr_workers: Option<usize>, max_dim: u32, language: String) -> Result<()> {
    let log_file = std::fs::File::create(&log)
        .expect("Failed to create log file");
    let (non_blocking, _guard) = tracing_appender::non_blocking(log_file);

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(non_blocking)
        .init();

    info!(
        input = %input.display(),
        db = %db.display(),
        "Starting OCR post-processing"
    );

    let jobs = Arc::new(scanner::JobStore::open(&db)?);

    // Scan input directory to register any new PDFs in SQLite
    let scanned = scanner::scan_directory(&jobs, &input)?;
    info!(scanned = scanned, "Directory scan complete");

    // Find tesseract
    let tesseract_bin = tesseract_path
        .or_else(|| ocr::find_tesseract())
        .unwrap_or_else(|| PathBuf::from("tesseract"));

    let config = ocr::OcrConfig {
        tesseract_path: tesseract_bin,
        max_dim,
        language,
        ..Default::default()
    };

    let processed = pipeline::run_ocr_post_processing(
        Arc::clone(&jobs),
        &config,
        output.clone(),
        ocr_workers,
    )?;

    info!(
        ocr_processed = processed,
        output = output.as_ref().map(|p| p.display().to_string()),
        "OCR processing complete"
    );

    // If index_path provided, show a note about re-indexing
    if let Some(idx_path) = index_path {
        info!(
            index_path = %idx_path.display(),
            "OCR text extracted. Re-run extraction with --index-path to update the search index."
        );
    }

    if let Some(ref out_path) = output {
        println!("OCR processed {} document(s), output written to {}", processed, out_path.display());
    } else {
        println!("OCR processed {} document(s)", processed);
    }
    Ok(())
}

fn run_list_failed_ocr(db: PathBuf) -> Result<()> {
    let jobs = Arc::new(scanner::JobStore::open(&db)?);
    let failed = jobs.fetch_failed_ocr()?;

    if failed.is_empty() {
        println!("No permanently failed OCR items found.");
        return Ok(());
    }

    println!("ID\tPath\tError");
    for (id, path, _checksum, error) in &failed {
        let err_display = error.as_deref().unwrap_or("unknown");
        println!("{}\t{}\t{}", id, path, err_display);
    }
    println!("--- {} permanently failed OCR item(s) ---", failed.len());
    Ok(())
}
