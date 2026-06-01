// Binary that generates benchmark PDFs (raw byte-level, no lopdf), then
// measures extraction and search performance.
//
// Usage: cargo run --release --bin bench_pdfs [--count N] [--dir DIR]

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

const ENGLISH_WORDS: &[&str] = &[
    "quantum", "computing", "machine", "learning", "neural", "network", "deep",
    "algorithm", "transformer", "attention", "gradient", "optimization", "regression",
    "classification", "clustering", "reinforcement", "supervised", "unsupervised",
    "embedding", "tokenization", "semantic", "syntax", "parsing", "inference",
    "backpropagation", "convolution", "pooling", "dropout", "normalization",
];

fn make_pdf(buf: &mut Vec<u8>, body: &str) {
    buf.clear();

    let escaped: String = body
        .chars()
        .map(|c| match c {
            '(' | ')' | '\\' => format!("\\{}", c),
            _ => c.to_string(),
        })
        .collect();
    let stream_content = format!(" BT /F1 12 Tf 100 700 Td ({}) Tj ET ", escaped);

    macro_rules! wln { ($($tt:tt)*) => { writeln!(buf, $($tt)*).unwrap() }; }
    macro_rules! off { () => { buf.len() }; }

    wln!("%PDF-1.4");

    let o1 = off!(); wln!("1 0 obj"); wln!("<< /Type /Catalog /Pages 2 0 R >>"); wln!("endobj");
    let o2 = off!(); wln!("2 0 obj"); wln!("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"); wln!("endobj");
    let o3 = off!(); wln!("3 0 obj"); wln!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]"); wln!("   /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"); wln!("endobj");
    let o4 = off!(); wln!("4 0 obj"); wln!("<< /Length {} >>", stream_content.len()); wln!("stream");
                    buf.extend_from_slice(stream_content.as_bytes()); wln!(""); wln!("endstream"); wln!("endobj");
    let o5 = off!(); wln!("5 0 obj"); wln!("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"); wln!("endobj");

    let xref_off = off!();
    wln!("xref");
    wln!("0 6");
    wln!("{:010} 65535 f ", 0);
    wln!("{:010} 00000 n ", o1);
    wln!("{:010} 00000 n ", o2);
    wln!("{:010} 00000 n ", o3);
    wln!("{:010} 00000 n ", o4);
    wln!("{:010} 00000 n ", o5);

    wln!("trailer");
    wln!("<< /Size 6 /Root 1 0 R >>");
    wln!("startxref");
    wln!("{}", xref_off);
    wln!("%%EOF");
}

fn extract_benchmark(pdf_dir: &PathBuf) {
    let bin_path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("pdf_extractor.exe");

    let bench_dir = PathBuf::from(std::env::var("BENCH_DIR").unwrap_or_else(|_| {
        let dir = std::env::temp_dir().join("pdf_extractor_bench_runtime");
        std::fs::create_dir_all(&dir).ok();
        dir.display().to_string()
    }));
    let db_path = bench_dir.join("bench_jobs.db");
    let jsonl_path = bench_dir.join("bench_docs.jsonl");
    let log_path = bench_dir.join("bench_extract.log");
    let index_path = bench_dir.join("bench_index");
    let _ = std::fs::remove_dir_all(&index_path);
    let _ = std::fs::remove_file(&db_path);

    println!("Extracting PDFs from: {}", pdf_dir.display());
    let start = Instant::now();
    let output = std::process::Command::new(&bin_path)
        .args([
            "extract",
            "-i",
            pdf_dir.to_str().unwrap(),
            "-o",
            jsonl_path.to_str().unwrap(),
            "-d",
            db_path.to_str().unwrap(),
            "-l",
            log_path.to_str().unwrap(),
            "--index-path",
            index_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to run extraction");
    let elapsed = start.elapsed();

    let _stdout = String::from_utf8_lossy(&output.stdout);
    let _stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        eprintln!("Extraction failed: {}", String::from_utf8_lossy(&output.stderr));
        return;
    }

    let throughput = if let Ok(log_content) = std::fs::read_to_string(&log_path) {
        log_content
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                serde_json::from_str::<serde_json::Value>(line).ok()
            })
            .find_map(|v| {
                v.get("avg_throughput")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| {
                let count = std::fs::read_to_string(&jsonl_path)
                    .map(|c| c.lines().count())
                    .unwrap_or(0);
                format!(
                    "{:.1} (estimated from JSONL)",
                    count as f64 / elapsed.as_secs_f64()
                )
            })
    } else {
        "?".to_string()
    };

    println!("  Extraction time  : {:.2}s", elapsed.as_secs_f64());
    println!("  Throughput       : {} docs/s", throughput);

    // --- Search benchmarks ---
    let queries = &[
        "quantum",
        "machine",
        "neural",
        "gradient",
        "optimization",
        "transformer",
        "reinforcement",
    ];
    let iterations = 30;

    for &(label, extra_args) in &[
        ("basic", &[] as &[&str]),
        ("fuzzy", &["--fuzzy", "2"]),
        ("stem", &["--stem"]),
        ("field", &["--field", "normalized_text"]),
    ] {
        let mut times = Vec::with_capacity(iterations);
        for i in 0..iterations {
            let q = queries[i % queries.len()];
            let q_start = Instant::now();
            let mut cmd = std::process::Command::new(&bin_path);
            cmd.args(["search", "--index-path", index_path.to_str().unwrap()]);
            for arg in extra_args {
                cmd.arg(arg);
            }
            cmd.arg(q);
            cmd.output().ok();
            times.push(q_start.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg: f64 = times.iter().sum::<f64>() / times.len() as f64;
        let p50 = times[(times.len() as f64 * 0.50) as usize];
        let p95 = times[(times.len() as f64 * 0.95) as usize];
        let p99 = times[(times.len() as f64 * 0.99) as usize];
        println!(
            "  Search ({:6})  : avg={:6.1}ms  p50={:6.1}ms  p95={:6.1}ms  p99={:6.1}ms",
            label, avg, p50, p95, p99
        );
    }

    // --- Index stats ---
    let stats_output = std::process::Command::new(&bin_path)
        .args(["index-stats", "--index-path", index_path.to_str().unwrap()])
        .output()
        .ok();
    if let Some(out) = stats_output {
        let s = String::from_utf8_lossy(&out.stdout);
        for line in s.lines() {
            println!("  {}", line);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut count = 100usize;
    let mut pdf_dir = std::env::temp_dir().join("pdf_extractor_bench_pdfs");
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--count" => {
                count = args[i + 1].parse().unwrap();
                i += 1;
            }
            "--dir" => {
                pdf_dir = PathBuf::from(&args[i + 1]);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }

    std::fs::create_dir_all(&pdf_dir).unwrap();

    println!("=== pdf_extractor benchmark ===");
    println!("PDF count   : {}", count);
    println!("PDF dir     : {}", pdf_dir.display());
    println!();

    // Generate PDFs
    println!("Generating {} PDFs...", count);
    let gen_start = Instant::now();

    let jp_sentences = [
        "機械学習は人工知能の一分野である",
        "自然言語処理はテキストデータを解析する技術である",
        "ディープラーニングは多層ニューラルネットワークを使用する",
    ];
    let zh_sentences = [
        "机器学习是人工智能的一个分支",
        "自然语言处理是分析文本数据的技术",
        "深度学习使用多层神经网络",
    ];
    let math_exprs = [
        r"\sum_{i=0}^{n} x_i",
        r"\frac{a}{b} + \frac{c}{d}",
        r"\sqrt{x^2 + y^2}",
        r"\int_{0}^{\infty} e^{-x} dx",
        r"\lim_{n \to \infty} \frac{1}{n}",
    ];

    let mut pdf_buf = Vec::with_capacity(4096);
    for idx in 0..count {
        let words: Vec<&str> = (0..50)
            .map(|j| ENGLISH_WORDS[(idx + j) % ENGLISH_WORDS.len()])
            .collect();
        let mut body = words.join(" ");

        if idx % 10 == 0 {
            body.push(' ');
            body.push_str(math_exprs[idx % math_exprs.len()]);
        }
        if idx % 15 == 0 {
            body.push(' ');
            body.push_str(jp_sentences[idx % jp_sentences.len()]);
        }
        if idx % 15 == 5 {
            body.push(' ');
            body.push_str(zh_sentences[idx % zh_sentences.len()]);
        }

        make_pdf(&mut pdf_buf, &body);
        std::fs::write(pdf_dir.join(format!("doc_{}.pdf", idx)), &pdf_buf).unwrap();
    }

    let gen_elapsed = gen_start.elapsed();
    println!("  Generated {} PDFs in {:.2}s", count, gen_elapsed.as_secs_f64());
    println!();

    extract_benchmark(&pdf_dir);
}
