mod scanner;
mod extractor;
mod normalizer;
mod output;
mod metrics;
mod pipeline;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
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

    let jobs = Arc::new(scanner::JobStore::open(&cli.db)?);
    let writer = output::JsonlWriter::new(&cli.output)?;
    let metrics = Arc::new(metrics::Metrics::new());

    pipeline::run_pipeline(Arc::clone(&jobs), &writer, Arc::clone(&metrics), &cli.input)?;

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
