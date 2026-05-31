mod scanner;
mod extractor;
mod normalizer;
mod output;
mod metrics;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tracing::{info, error};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "pdf_extractor", about = "Extract text from PDFs into JSONL")]
struct Cli {
    #[arg(short = 'i', long = "input", help = "Input directory containing PDFs")]
    input: PathBuf,

    #[arg(short = 'o', long = "output", default_value = "documents.jsonl", help = "Output JSONL file")]
    output: PathBuf,

    #[arg(short = 'd', long = "db", default_value = "jobs.db", help = "SQLite database path")]
    db: PathBuf,

    #[arg(short = 'l', long = "log", default_value = "extractor.log", help = "Log file path")]
    log: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_file = std::fs::File::create(&cli.log)
        .expect("Failed to create log file");
    let (non_blocking, _guard) = tracing_appender::non_blocking(log_file);

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(non_blocking)
        .init();

    info!(
        input = %cli.input.display(),
        output = %cli.output.display(),
        db = %cli.db.display(),
        "Starting pdf_extractor"
    );

    let jobs = scanner::JobStore::open(&cli.db)?;
    let writer = output::JsonlWriter::new(&cli.output)?;
    let metrics = metrics::Metrics::new();

    let scanned = scanner::scan_directory(&jobs, &cli.input)?;
    info!(scanned = scanned, "Directory scan complete");

    let pending = jobs.count_pending()?;
    info!(pending = pending, "Jobs pending extraction");

    if pending == 0 {
        info!("No pending jobs to process");
        return Ok(());
    }

    process_pending(&jobs, &writer, &metrics)?;

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

fn process_pending(
    jobs: &scanner::JobStore,
    writer: &output::JsonlWriter,
    metrics: &metrics::Metrics,
) -> Result<()> {
    loop {
        let batch = jobs.fetch_pending(100)?;
        if batch.is_empty() {
            break;
        }

        for (id, path, checksum) in &batch {
            let path_obj = PathBuf::from(path);
            match extractor::extract_pdf(&path_obj) {
                Ok(result) => {
                    let normalized = if !result.ocr_flag {
                        normalizer::normalize_text(&result.text)
                    } else {
                        String::new()
                    };

                    let record = output::DocumentRecord {
                        id: *id,
                        path: path.clone(),
                        checksum: checksum.clone(),
                        ocr_flag: result.ocr_flag,
                        language: None,
                        text: normalized,
                    };

                    if let Err(e) = writer.write_record(&record) {
                        error!(id = id, path = %path, error = %e, "Failed to write record");
                        metrics.increment_errored();
                        continue;
                    }

                    if let Err(e) = jobs.mark_done(*id, result.ocr_flag) {
                        error!(id = id, error = %e, "Failed to mark job done");
                    }

                    metrics.increment_processed();
                    info!(
                        id = id,
                        path = %path,
                        ocr = result.ocr_flag,
                        "Document extracted"
                    );
                }
                Err(e) => {
                    error!(id = id, path = %path, error = %e, "Extraction failed");
                    if let Err(me) = jobs.mark_error(*id, &format!("{}", e)) {
                        error!(id = id, error = %me, "Failed to record job error");
                    }
                    metrics.increment_errored();
                }
            }

            metrics.log_summary();
        }
    }

    Ok(())
}
