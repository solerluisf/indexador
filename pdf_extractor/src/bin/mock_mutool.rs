use std::str::FromStr;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // `mutool info <pdf>` — print page count (configurable via env var)
    if args.get(1).map(|s| s.as_str()) == Some("info") {
        let pages = std::env::var("MOCK_MUTOOL_PAGES")
            .ok()
            .and_then(|v| u32::from_str(&v).ok())
            .unwrap_or(1);
        println!("Pages: {}", pages);
        return;
    }

    // `mutool draw -o <output> -r 300 <pdf> <page>` — render a page
    let mut out_path = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "-o" && i + 1 < args.len() {
            out_path = Some(&args[i + 1]);
            break;
        }
        i += 1;
    }
    if let Some(path) = out_path {
        let img = image::GrayImage::new(50, 50);
        img.save(path).unwrap();
    }
}
