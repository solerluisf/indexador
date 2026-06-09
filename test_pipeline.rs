use std::path::PathBuf;

fn main() {
    // Test the pipeline by running pdf_worker directly
    let worker = PathBuf::from("C:\\Users\\Magnesium\\Documents\\Software\\Indexador\\Dev\\PdfExplorer\\bin\\Debug\\net8.0-windows10.0.17763.0\\pdf_worker.exe");
    
    if !worker.exists() {
        eprintln!("Worker not found: {}", worker.display());
        return;
    }
    
    let pdfs = vec![
        "C:\\Users\\Magnesium\\Documents\\Java\\Full Stack AngularJS for Java Developers_ Build a Full-Featured Web Application from Scratch Using AngularJS with Spring RESTful ( PDFDrive ).pdf",
        "C:\\Users\\Magnesium\\Documents\\Java\\Hunt, Andrew_Thomas, David Hurst - The Pragmatic Programmer_ Your Journey to Mastery (2019_2020, Addison-Wesley Professional) - libgen.li.pdf",
        "C:\\Users\\Magnesium\\Documents\\Java\\Intelligent Feature Selection for Machine Learning Using the -- Mark K. Hinders -- ( WeLib.org ).pdf",
    ];
    
    let mut child = std::process::Command::new(&worker)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn worker");
    
    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    
    // Write all paths
    for pdf in &pdfs {
        eprintln!("Writing: {}", pdf);
        writeln!(stdin, "{}", pdf).unwrap();
    }
    drop(stdin); // Close stdin to signal EOF
    
    // Read all frames
    let mut stdout_reader = std::io::BufReader::new(stdout);
    let mut frame_count = 0usize;
    loop {
        let mut len_buf = [0u8; 4];
        match std::io::Read::read_exact(&mut stdout_reader, &mut len_buf) {
            Ok(_) => {
                let len = u32::from_le_bytes(len_buf) as usize;
                eprintln!("Reading frame of {} bytes...", len);
                let mut data = vec![0u8; len];
                std::io::Read::read_exact(&mut stdout_reader, &mut data).unwrap();
                
                let frame: pdf_extractor::worker_ipc::WorkerFrame = bincode::deserialize(&data).unwrap();
                frame_count += 1;
                eprintln!("Frame {}: {:?}", frame_count, 
                    match &frame {
                        pdf_extractor::worker_ipc::WorkerFrame::Success(wo) => format!("Success: {}", wo.path),
                        pdf_extractor::worker_ipc::WorkerFrame::Error { path, message } => format!("Error: {} - {}", path, message),
                    });
            }
            Err(e) => {
                eprintln!("Read error: {}", e);
                break;
            }
        }
    }
    
    // Read stderr
    let mut stderr_buf = String::new();
    std::io::BufReader::new(stderr).read_to_string(&mut stderr_buf).unwrap();
    if !stderr_buf.is_empty() {
        eprintln!("Stderr: {}", stderr_buf);
    }
    
    let status = child.wait().expect("Failed to wait");
    eprintln!("Worker exited with: {:?}", status);
    eprintln!("Total frames: {}", frame_count);
}
