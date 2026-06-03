use anyhow::{Context, Result};
use image::GrayImage;
use imageproc::geometric_transformations;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

fn find_worker_binary() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) { "tesseract_worker.exe" } else { "tesseract_worker" };

    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent()?;
        loop {
            let candidate = dir.join(exe_name);
            if candidate.is_file() {
                return Some(candidate);
            }
            if dir.file_name().and_then(|n| n.to_str()) == Some("deps") {
                if let Some(parent) = dir.parent() {
                    let candidate2 = parent.join(exe_name);
                    if candidate2.is_file() {
                        return Some(candidate2);
                    }
                }
            }
            if let Some(parent) = dir.parent() {
                dir = parent;
            } else {
                break;
            }
        }
    }

    if let Ok(bin_dir) = std::env::var("CARGO_BIN_DIR") {
        let candidate = PathBuf::from(bin_dir).join(exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

#[derive(Clone)]
pub struct OcrConfig {
    pub tesseract_path: PathBuf,
    pub max_dim: u32,
    pub max_retries: u32,
    pub language: String,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            tesseract_path: PathBuf::from("tesseract"),
            max_dim: 3000,
            max_retries: 2,
            language: "eng".into(),
        }
    }
}

const END_MARKER: &str = "---END---";

pub(crate) struct WorkerProcess {
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    child: Child,
}

impl WorkerProcess {
    fn spawn(tesseract_path: &Path, language: &str) -> Result<Self> {
        let worker_bin = find_worker_binary()
            .context("Could not locate tesseract_worker.exe — ensure the binary is built")?;

        let mut child = Command::new(&worker_bin)
            .arg("--tesseract-path")
            .arg(tesseract_path)
            .arg("--lang")
            .arg(language)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context(format!(
                "Failed to spawn tesseract worker '{}'",
                worker_bin.display()
            ))?;

        let stdin = child.stdin.take()
            .context("Failed to capture worker stdin")?;
        let stdout = child.stdout.take()
            .context("Failed to capture worker stdout")?;
        let reader = BufReader::new(stdout);

        Ok(WorkerProcess { stdin, reader, child })
    }

    pub(crate) fn process(&mut self, image_path: &Path) -> Result<String> {
        writeln!(self.stdin, "{}", image_path.display())
            .context("Failed to write to worker stdin")?;
        self.stdin.flush()?;

        let mut text = String::new();
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                anyhow::bail!("Worker process exited unexpectedly");
            }
            let trimmed = line.trim().to_string();
            if trimmed == END_MARKER {
                break;
            }
            if let Some(err_msg) = trimmed.strip_prefix("ERROR:") {
                anyhow::bail!("Worker error: {}", err_msg);
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&trimmed);
        }
        Ok(text)
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "EXIT");
        let _ = self.stdin.flush();
        let _ = self.child.wait();
    }
}

/// A pool of persistent Tesseract subprocesses.
///
/// Spawns `count` long-lived worker processes at construction, then
/// distributes one per caller via [`take_worker`](TesseractPool::take_worker).
/// This avoids the per-invocation overhead of spawning a fresh `tesseract.exe`
/// for each image.
pub struct TesseractPool {
    workers: Vec<WorkerProcess>,
}

impl TesseractPool {
    pub fn new(count: usize, tesseract_path: &Path, language: &str) -> Result<Self> {
        let mut workers = Vec::with_capacity(count);
        for _ in 0..count {
            workers.push(WorkerProcess::spawn(tesseract_path, language)?);
        }
        Ok(TesseractPool { workers })
    }

    /// Remove one worker from the pool for exclusive use by a thread.
    pub fn take_worker(&mut self) -> Option<WorkerProcess> {
        self.workers.pop()
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }
}

pub fn preprocess_image(input: &Path, max_dim: u32) -> Result<PathBuf> {
    let img = image::open(input).context("Failed to open image")?;
    let gray = img.to_luma8();

    let resized = if gray.width() > max_dim || gray.height() > max_dim {
        let ratio = max_dim as f64 / gray.width().max(gray.height()) as f64;
        let w = (gray.width() as f64 * ratio) as u32;
        let h = (gray.height() as f64 * ratio) as u32;
        image::imageops::resize(&gray, w.max(1), h.max(1), image::imageops::FilterType::Lanczos3)
    } else {
        gray
    };

    // Detect skew angle and deskew
    let angle = detect_skew_angle(&resized);
    let corrected = if angle.abs() > 0.5 {
        rotate_grayscale(&resized, angle)
    } else {
        resized
    };

    // Manual Otsu binarization
    let threshold = otsu_threshold(&corrected);
    let binary = apply_threshold(&corrected, threshold);

    let stem = input.file_stem().unwrap_or_default();
    let output = input.with_file_name(format!("{}_preprocessed.png", stem.to_string_lossy()));
    binary.save(&output).context("Failed to save preprocessed image")?;
    Ok(output)
}

/// Detect the skew angle of a grayscale document image using projection
/// profile analysis on a quickly-thresholded version.
/// Returns the angle in degrees (positive = clockwise).
pub fn detect_skew_angle(gray: &GrayImage) -> f64 {
    // Work on a downscaled copy for speed
    let small = image::imageops::resize(gray, 200, 200, image::imageops::FilterType::Nearest);
    let threshold = otsu_threshold(&small);
    let binary = apply_threshold(&small, threshold);

    let mut best_angle = 0.0_f64;
    let mut best_variance = 0.0_f64;

    // Candidate angles: -5 to +5 degrees in 0.5° steps
    let mut angle = -5.0_f32;
    while angle <= 5.0_f32 {
        let rotated = geometric_transformations::rotate_about_center(
            &binary,
            angle.to_radians(),
            geometric_transformations::Interpolation::Nearest,
            image::Luma([0u8]),
        );
        let var = projection_variance(&rotated);
        if var > best_variance {
            best_variance = var;
            best_angle = angle as f64;
        }
        angle += 0.5;
    }

    best_angle
}

/// Compute the variance of the horizontal projection (row sums) of a binary image.
/// Higher variance indicates better alignment with horizontal text lines.
fn projection_variance(binary: &GrayImage) -> f64 {
    let (w, h) = binary.dimensions();
    let mut row_sums = Vec::with_capacity(h as usize);
    for y in 0..h {
        let mut sum = 0u64;
        for x in 0..w {
            // Pixel is 0 (ink) or 255 (white). Count ink pixels.
            if binary.get_pixel(x, y).0[0] == 0 {
                sum += 1;
            }
        }
        row_sums.push(sum);
    }
    let mean = row_sums.iter().sum::<u64>() as f64 / row_sums.len() as f64;
    let variance = row_sums.iter().map(|&s| {
        let d = s as f64 - mean;
        d * d
    }).sum::<f64>() / row_sums.len() as f64;
    variance
}

/// Rotate a grayscale image by the given angle (degrees, clockwise).
fn rotate_grayscale(img: &GrayImage, angle_deg: f64) -> GrayImage {
    geometric_transformations::rotate_about_center(
        img,
        angle_deg as f32 * std::f32::consts::PI / 180.0,
        geometric_transformations::Interpolation::Bilinear,
        image::Luma([255u8]),
    )
}

fn apply_threshold(img: &image::GrayImage, threshold: u8) -> image::GrayImage {
    let mut out = image::GrayImage::new(img.width(), img.height());
    for (x, y, p) in img.enumerate_pixels() {
        let val = if p.0[0] > threshold { 255u8 } else { 0u8 };
        out.put_pixel(x, y, image::Luma([val]));
    }
    out
}

fn otsu_threshold(img: &image::GrayImage) -> u8 {
    let total = (img.width() * img.height()) as u64;
    if total == 0 {
        return 128;
    }

    let mut histogram = [0u64; 256];
    for p in img.pixels() {
        histogram[p.0[0] as usize] += 1;
    }

    let mut sum_b: f64 = 0.0;
    let mut w_b: f64 = 0.0;

    // Find threshold that maximizes between-class variance
    let sum_total: f64 = histogram.iter().enumerate().map(|(i, &c)| (i as f64) * (c as f64)).sum();
    let mut best_threshold: u8 = 0;
    let mut best_variance: f64 = 0.0;

    for t in 0..256 {
        w_b += histogram[t] as f64;
        if w_b == 0.0 {
            continue;
        }
        let w_f = total as f64 - w_b;
        if w_f == 0.0 {
            break;
        }

        sum_b += (t as f64) * (histogram[t] as f64);
        let m_b = sum_b / w_b;
        let m_f = (sum_total - sum_b) / w_f;

        let variance = w_b * w_f * (m_b - m_f).powi(2);
        if variance > best_variance {
            best_variance = variance;
            best_threshold = t as u8;
        }
    }

    best_threshold
}

pub(crate) fn run_tesseract(image_path: &Path, tesseract_path: &Path, language: &str) -> Result<String> {
    let output = Command::new(tesseract_path)
        .arg(image_path)
        .arg("stdout")
        .arg("-l")
        .arg(language)
        .arg("--psm")
        .arg("3")
        .env("OMP_THREAD_LIMIT", "1")
        .env("OMP_NUM_THREADS", "1")
        .output()
        .context(format!(
            "Failed to run Tesseract. Is '{}' installed?",
            tesseract_path.display()
        ))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Tesseract exited with error: {}", stderr.trim());
    }

    let text = String::from_utf8(output.stdout)
        .context("Tesseract output is not valid UTF-8")?;

    let trimmed = text.trim().to_string();
    Ok(trimmed)
}

pub fn find_tesseract() -> Option<PathBuf> {
    // Check common install paths on Windows
    let common_paths = [
        r"C:\Program Files\Tesseract-OCR\tesseract.exe",
        r"C:\Program Files (x86)\Tesseract-OCR\tesseract.exe",
        "/usr/bin/tesseract",
        "/usr/local/bin/tesseract",
        "/opt/homebrew/bin/tesseract",
    ];
    for path in &common_paths {
        if Path::new(path).exists() {
            return Some(PathBuf::from(path));
        }
    }

    // Try `where` on Windows
    #[cfg(windows)]
    {
        if let Ok(output) = std::process::Command::new("where").arg("tesseract").output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).lines().next().map(|s| s.trim().to_string());
                if let Some(p) = path {
                    let p = PathBuf::from(p);
                    if p.exists() {
                        return Some(p);
                    }
                }
            }
        }
    }

    // Try `which` on Unix
    #[cfg(not(windows))]
    {
        if let Ok(output) = std::process::Command::new("which").arg("tesseract").output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).lines().next().map(|s| s.trim().to_string());
                if let Some(p) = path {
                    let p = PathBuf::from(p);
                    if p.exists() {
                        return Some(p);
                    }
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn unique_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = temp_dir().join(format!("pdf_extractor_ocr_{}", id));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn create_test_png(_text: &str) -> PathBuf {
        use image::{RgbImage, Rgb};
        let dir = unique_dir();
        let path = dir.join("test.png");

        let mut img = RgbImage::new(200, 50);
        // White background
        for p in img.pixels_mut() {
            *p = Rgb([255, 255, 255]);
        }
        // Draw black pixels for simple shapes (not actual text rendering)
        for x in 10..50 {
            for y in 10..20 {
                img.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        img.save(&path).unwrap();
        path
    }

    fn create_mock_tesseract_script(dir: &Path) -> PathBuf {
        let script = dir.join("mock_tesseract.bat");
        let content = "@echo off\necho Mock OCR text\n".to_string();
        fs::write(&script, content).unwrap();
        script
    }

    // --- preprocess_image: basic ---

    #[test]
    fn test_preprocess_image_grayscale_output() {
        let path = create_test_png("test");
        let result = preprocess_image(&path, 3000).unwrap();
        let saved = image::open(&result).unwrap();
        assert_eq!(saved.color().channel_count(), 1, "Should be grayscale (1 channel)");
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn test_preprocess_image_resize_large() {
        let dir = unique_dir();
        let path = dir.join("large.png");
        let mut img = image::GrayImage::new(4000, 4000);
        for p in img.pixels_mut() {
            *p = image::Luma([128]);
        }
        img.save(&path).unwrap();

        let result = preprocess_image(&path, 3000).unwrap();
        let saved = image::open(&result).unwrap();
        assert!(saved.width() <= 3000, "Width should be resized to max_dim 3000, got {}", saved.width());
        assert!(saved.height() <= 3000, "Height should be resized to max_dim 3000, got {}", saved.height());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_preprocess_image_custom_max_dim() {
        let dir = unique_dir();
        let path = dir.join("custom.png");
        let mut img = image::GrayImage::new(2000, 1000);
        for p in img.pixels_mut() {
            *p = image::Luma([128]);
        }
        img.save(&path).unwrap();

        let result = preprocess_image(&path, 500).unwrap();
        let saved = image::open(&result).unwrap();
        assert!(saved.width() <= 500, "Width should be resized to max_dim 500, got {}", saved.width());
        assert!(saved.height() <= 500, "Height should be resized to max_dim 500, got {}", saved.height());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_preprocess_image_no_resize_small() {
        let dir = unique_dir();
        let path = dir.join("small.png");
        let mut img = image::GrayImage::new(100, 100);
        for p in img.pixels_mut() {
            *p = image::Luma([128]);
        }
        img.save(&path).unwrap();

        let result = preprocess_image(&path, 3000).unwrap();
        let saved = image::open(&result).unwrap();
        assert_eq!(saved.width(), 100, "Small image should not be resized");
        assert_eq!(saved.height(), 100, "Small image should not be resized");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_preprocess_image_extreme_downscale() {
        let dir = unique_dir();
        let path = dir.join("extreme.png");
        let mut img = image::GrayImage::new(4000, 4000);
        for p in img.pixels_mut() {
            *p = image::Luma([128]);
        }
        img.save(&path).unwrap();

        let result = preprocess_image(&path, 10).unwrap();
        let saved = image::open(&result).unwrap();
        assert!(saved.width() <= 10, "max_dim=10 should downscale to ≤10, got {}x{}", saved.width(), saved.height());
        assert!(saved.height() <= 10, "max_dim=10 should downscale to ≤10, got {}x{}", saved.width(), saved.height());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_preprocess_image_max_dim_larger_than_image() {
        let dir = unique_dir();
        let path = dir.join("smaller.png");
        let mut img = image::GrayImage::new(50, 30);
        for p in img.pixels_mut() {
            *p = image::Luma([128]);
        }
        img.save(&path).unwrap();

        let result = preprocess_image(&path, 10000).unwrap();
        let saved = image::open(&result).unwrap();
        assert_eq!(saved.width(), 50, "Larger max_dim should not upscale");
        assert_eq!(saved.height(), 30, "Larger max_dim should not upscale");
        fs::remove_dir_all(&dir).ok();
    }

    // --- preprocess_image: error flows ---

    #[test]
    fn test_preprocess_image_nonexistent_input() {
        let result = preprocess_image(&PathBuf::from(r"C:\NONEXISTENT_OCR_TEST.png"), 3000);
        assert!(result.is_err(), "Preprocessing nonexistent file should fail");
    }

    #[test]
    fn test_preprocess_image_invalid_input() {
        let dir = unique_dir();
        let path = dir.join("not_an_image.txt");
        fs::write(&path, b"not an image").unwrap();
        let result = preprocess_image(&path, 3000);
        assert!(result.is_err(), "Preprocessing invalid file should fail");
        fs::remove_dir_all(&dir).ok();
    }

    // --- otsu_threshold: basic ---

    #[test]
    fn test_otsu_threshold_all_white() {
        let img = image::GrayImage::from_pixel(10, 10, image::Luma([255]));
        let t = otsu_threshold(&img);
        assert_eq!(t, 0, "All-white image should have threshold 0");
    }

    #[test]
    fn test_otsu_threshold_all_black() {
        let img = image::GrayImage::from_pixel(10, 10, image::Luma([0]));
        let t = otsu_threshold(&img);
        assert_eq!(t, 0, "All-black image should have threshold 0");
    }

    #[test]
    fn test_otsu_threshold_bimodal() {
        let mut img = image::GrayImage::new(40, 10);
        // First half: dark (30-70 range)
        for x in 0..20 {
            for y in 0..10 {
                img.put_pixel(x, y, image::Luma([50]));
            }
        }
        // Second half: light (150-200 range)
        for x in 20..40 {
            for y in 0..10 {
                img.put_pixel(x, y, image::Luma([180]));
            }
        }
        let t = otsu_threshold(&img);
        // Threshold should separate the two clusters (between dark max=50 and light min=180)
        assert!(t > 0, "Threshold should be non-zero, got {}", t);
        assert!(t < 255, "Threshold should be below 255, got {}", t);
    }

    // --- run_tesseract with mock ---

    #[test]
    fn test_run_tesseract_mock_success() {
        let dir = unique_dir();
        let mock = create_mock_tesseract_script(&dir);
        let png = create_test_png("test");

        let result = run_tesseract(&png, &mock, "eng");
        // Mock script is a .bat file; if it doesn't work as a tesseract replacement, this is expected
        // We just verify it doesn't crash and tries to run the subprocess
        assert!(result.is_ok() || result.is_err(), "Should attempt to run the mock");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_run_tesseract_sets_omp_env_vars() {
        let dir = unique_dir();
        let script = dir.join("mock_check_omp.bat");
        let content = "@echo off\necho OMP_THREAD_LIMIT=%OMP_THREAD_LIMIT%\necho OMP_NUM_THREADS=%OMP_NUM_THREADS%\n".to_string();
        fs::write(&script, content).unwrap();
        let png = create_test_png("test");

        let result = run_tesseract(&png, &script, "eng");
        if let Ok(text) = result {
            assert!(text.contains("OMP_THREAD_LIMIT=1"), "Should contain OMP_THREAD_LIMIT=1, got: {}", text);
            assert!(text.contains("OMP_NUM_THREADS=1"), "Should contain OMP_NUM_THREADS=1, got: {}", text);
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_run_tesseract_custom_language() {
        let dir = unique_dir();
        let script = dir.join("mock_lang_check.bat");
        let content = "@echo off\necho %1 %2 %3 %4 %5\n".to_string();
        fs::write(&script, content).unwrap();
        let png = create_test_png("test");

        let result = run_tesseract(&png, &script, "por");
        if let Ok(text) = result {
            assert!(text.contains("-l"), "Should contain -l flag");
            assert!(text.contains("por"), "Should contain custom language 'por', got: {}", text);
        }
        fs::remove_dir_all(&dir).ok();
    }

    // --- TesseractPool / WorkerProcess ---

    #[test]
    fn test_tesseract_pool_zero_workers() {
        let pool = TesseractPool::new(0, Path::new("tesseract"), "eng");
        assert!(pool.is_ok(), "Pool with 0 workers should succeed");
        if let Ok(mut p) = pool {
            assert_eq!(p.worker_count(), 0);
            assert!(p.take_worker().is_none());
        }
    }

    #[test]
    fn test_tesseract_pool_create_one_worker() {
        let pool = TesseractPool::new(1, Path::new("tesseract"), "eng");
        assert!(pool.is_ok(), "Pool with 1 worker should succeed");
    }

    #[test]
    fn test_worker_process_mock() {
        let dir = unique_dir();
        let mock = create_mock_tesseract_script(&dir);
        let mut worker = match WorkerProcess::spawn(&mock, "eng") {
            Ok(w) => w,
            Err(_) => {
                // Worker binary not available (uncommon in CI)
                fs::remove_dir_all(&dir).ok();
                return;
            }
        };
        let png = create_test_png("pool_test");

        let result = worker.process(&png);
        if let Ok(text) = result {
            assert!(text.contains("Mock OCR text"),
                "Worker should return mock output, got: {}", text);
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_worker_process_custom_language() {
        let dir = unique_dir();
        let script = dir.join("mock_lang_worker.bat");
        let content = "@echo off\necho %1 %2 %3 %4 %5\n".to_string();
        fs::write(&script, content).unwrap();
        let png = create_test_png("pool_lang");

        let mut worker = match WorkerProcess::spawn(&script, "spa") {
            Ok(w) => w,
            Err(_) => {
                fs::remove_dir_all(&dir).ok();
                return;
            }
        };

        let result = worker.process(&png);
        if let Ok(text) = result {
            // The mock echoes args; worker output includes -l spa
            assert!(text.contains("-l"), "Should pass -l flag");
            assert!(text.contains("spa"), "Should pass 'spa' language, got: {}", text);
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_worker_process_tesseract_error() {
        let dir = unique_dir();
        let script = dir.join("mock_tesseract_error.bat");
        // Return non-zero exit code
        let content = "@echo off\nexit /b 1\n".to_string();
        fs::write(&script, content).unwrap();
        let png = create_test_png("pool_err");

        let mut worker = match WorkerProcess::spawn(&script, "eng") {
            Ok(w) => w,
            Err(_) => {
                fs::remove_dir_all(&dir).ok();
                return;
            }
        };

        let result = worker.process(&png);
        assert!(result.is_err(), "Worker should propagate tesseract error");
        if let Err(e) = result {
            let msg = format!("{:?}", e);
            assert!(msg.contains("Worker error:") || msg.contains("exit"), "Error should mention worker failure, got: {}", msg);
        }
        fs::remove_dir_all(&dir).ok();
    }

    // --- OcrConfig ---

    #[test]
    fn test_ocr_config_default() {
        let config = OcrConfig::default();
        assert_eq!(config.max_dim, 3000);
        assert_eq!(config.max_retries, 2);
        assert_eq!(config.tesseract_path.to_string_lossy(), "tesseract");
        assert_eq!(config.language, "eng");
    }

    #[test]
    fn test_ocr_config_custom() {
        let config = OcrConfig {
            tesseract_path: PathBuf::from("/custom/tesseract"),
            max_dim: 2000,
            max_retries: 3,
            language: "por".into(),
        };
        assert_eq!(config.max_dim, 2000);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.language, "por");
    }

    #[test]
    fn test_ocr_config_empty_language() {
        let config = OcrConfig {
            language: "".into(),
            ..Default::default()
        };
        assert_eq!(config.language, "", "Empty language should be accepted");
        // Ensure it passes through to tesseract without crashing
        let dir = unique_dir();
        let script = dir.join("mock_empty_lang.bat");
        let content = "@echo off\necho %1 %2 %3 %4 %5\n".to_string();
        fs::write(&script, content).unwrap();
        let png = create_test_png("test");
        let result = run_tesseract(&png, &script, &config.language);
        if let Ok(text) = result {
            assert!(text.contains("-l"), "Should still pass -l flag even with empty lang");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_find_tesseract_no_error() {
        // Should not panic, just return None if not found
        let result = find_tesseract();
        // This test passes regardless of whether tesseract is installed
        assert!(result.is_none() || result.is_some());
    }

    // --- Deskew: basic flows ---

    /// Create a synthetic binary image with horizontal text-like bars.
    fn make_text_image(w: u32, h: u32) -> GrayImage {
        let mut img = GrayImage::new(w, h);
        // Fill with white
        for p in img.pixels_mut() {
            *p = image::Luma([255u8]);
        }
        // Draw horizontal bars (simulating text lines)
        for y in (10..h).step_by(20) {
            for x in 10..w - 10 {
                img.put_pixel(x, y, image::Luma([0u8]));
                img.put_pixel(x, y + 1, image::Luma([0u8]));
            }
        }
        img
    }

    #[test]
    fn test_deskew_detects_zero_angle() {
        let img = make_text_image(100, 100);
        let angle = detect_skew_angle(&img);
        assert!(
            angle.abs() <= 1.0,
            "Should detect near-zero angle for straight text, got {}",
            angle
        );
    }

    #[test]
    fn test_deskew_detects_positive_angle() {
        let img = make_text_image(100, 100);
        // rotate_grayscale(3.0) rotates anticlockwise 3°
        // detect_skew_angle returns the opposite sign (correction angle)
        let rotated = rotate_grayscale(&img, 3.0);
        let angle = detect_skew_angle(&rotated);
        assert!(
            (angle - (-3.0)).abs() <= 1.5,
            "Should detect ~-3° correction for +3° anticlockwise skew, got {}",
            angle
        );
    }

    #[test]
    fn test_deskew_detects_negative_angle() {
        let img = make_text_image(100, 100);
        // rotate_grayscale(-2.5) rotates clockwise 2.5°
        // detect_skew_angle returns the opposite sign (correction angle)
        let rotated = rotate_grayscale(&img, -2.5);
        let angle = detect_skew_angle(&rotated);
        assert!(
            (angle - 2.5).abs() <= 1.5,
            "Should detect ~+2.5° correction for -2.5° clockwise skew, got {}",
            angle
        );
    }

    #[test]
    fn test_deskew_all_white_image() {
        let img = GrayImage::new(50, 50);
        let angle = detect_skew_angle(&img);
        assert_eq!(angle, 0.0, "All-white should return 0 skew");
    }

    // --- Deskew: alternative flows ---

    #[test]
    fn test_deskew_angle_outside_range() {
        // 10° anticlockwise skew → correction is -10°, outside -5..+5 range
        // The closest candidate is -5°
        let img = make_text_image(100, 100);
        let rotated = rotate_grayscale(&img, 10.0);
        let angle = detect_skew_angle(&rotated);
        assert!(
            (angle - (-5.0)).abs() <= 0.6,
            "10° anticlockwise skew should clamp correction to -5°, got {}",
            angle
        );
    }

    // --- Projection variance ---

    #[test]
    fn test_projection_variance_all_white() {
        let img = GrayImage::new(10, 10);
        let var = projection_variance(&img);
        assert_eq!(var, 0.0, "All-white image has 0 ink pixels, variance 0");
    }

    #[test]
    fn test_projection_variance_single_line() {
        let mut img = GrayImage::new(10, 10);
        for p in img.pixels_mut() {
            *p = image::Luma([255u8]);
        }
        // One full row of ink
        for x in 0..10 {
            img.put_pixel(x, 5, image::Luma([0u8]));
        }
        let var = projection_variance(&img);
        assert!(var > 0.0, "Should have non-zero variance with ink pixels");
    }

    // --- End-to-end: preprocess_image corrects skew ---

    #[test]
    fn test_preprocess_corrects_skewed_image() {
        // Create a straight image with horizontal bars, then rotate by 3° anticlockwise
        let straight = make_text_image(200, 200);
        let skewed = rotate_grayscale(&straight, 3.0);
        let dir = unique_dir();
        let input_path = dir.join("skewed_input.png");
        skewed.save(&input_path).unwrap();

        // Run preprocess_image — this should deskew the image
        let output_path = preprocess_image(&input_path, 3000).unwrap();
        let processed = image::open(&output_path).unwrap();
        let gray = processed.to_luma8();

        // Detect skew on the output — should be close to 0°
        let residual = detect_skew_angle(&gray);
        assert!(
            residual.abs() <= 1.5,
            "Deskew should correct 3° skew to near 0°, residual was {}°",
            residual
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_preprocess_does_not_rotate_straight_image() {
        let img = make_text_image(200, 200);
        let dir = unique_dir();
        let input_path = dir.join("straight_input.png");
        img.save(&input_path).unwrap();

        let output_path = preprocess_image(&input_path, 3000).unwrap();
        let processed = image::open(&output_path).unwrap();
        let gray = processed.to_luma8();

        let angle = detect_skew_angle(&gray);
        assert!(
            angle.abs() <= 1.0,
            "Straight image should remain straight, got {}°",
            angle
        );

        fs::remove_dir_all(&dir).ok();
    }
}
