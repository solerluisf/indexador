fn main() {
    let omp_thread = std::env::var("OMP_THREAD_LIMIT").unwrap_or_default();
    let omp_num = std::env::var("OMP_NUM_THREADS").unwrap_or_default();
    println!("Mock OCR text extracted from document (OMP_THREAD_LIMIT={}, OMP_NUM_THREADS={})", omp_thread, omp_num);
}
