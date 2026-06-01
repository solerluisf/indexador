use std::io::{self, BufRead, Write};
use std::process::Command;

const END_MARKER: &str = "---END---";

fn main() {
    let mut args = std::env::args().skip(1);
    let mut tesseract_path = String::from("tesseract");
    let mut lang = String::from("eng");

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tesseract-path" => {
                if let Some(v) = args.next() {
                    tesseract_path = v;
                }
            }
            "--lang" => {
                if let Some(v) = args.next() {
                    lang = v;
                }
            }
            _ => {}
        }
    }

    let stdin = io::stdin();
    let reader = stdin.lock();
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let path = line.trim().to_string();
        if path.is_empty() || path == "EXIT" {
            break;
        }

        let output = match Command::new(&tesseract_path)
            .arg(&path)
            .arg("stdout")
            .arg("-l")
            .arg(&lang)
            .arg("--psm")
            .arg("3")
            .env("OMP_THREAD_LIMIT", "1")
            .env("OMP_NUM_THREADS", "1")
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                let _ = writeln!(writer, "ERROR:{}", e);
                let _ = writeln!(writer, "{}", END_MARKER);
                let _ = writer.flush();
                continue;
            }
        };

        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let _ = writeln!(writer, "{}", text.trim());
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = writeln!(writer, "ERROR:{}", stderr.trim());
        }
        let _ = writeln!(writer, "{}", END_MARKER);
        let _ = writer.flush();
    }
}
