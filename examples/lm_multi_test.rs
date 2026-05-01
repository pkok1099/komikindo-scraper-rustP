use std::fs;
use komikindo_scraper::lm_selector::LmDetector;

fn main() {
    std::env::set_var("ORT_DYLIB_PATH", "/home/z/.local/lib/python3.13/site-packages/onnxruntime/capi/libonnxruntime.so.1.25.1");
    
    let mut detector = LmDetector::new().expect("Failed to load model");
    
    let test_files = [
        "html_cache/one-piece.html",
        "html_cache/boruto-two-blue-vortex.html",
        "html_cache/20th-century-boys.html",
    ];
    
    for file in &test_files {
        let html = match fs::read_to_string(file) {
            Ok(h) => h,
            Err(_) => continue,
        };
        let slug = file.split('/').last().unwrap_or("").replace(".html", "");
        
        let results = detector.detect_all_fields(&html);
        
        println!("=== {} ===", slug);
        let title = results.title.as_ref().map(|t| t.text.clone()).unwrap_or_else(|| "NONE".to_string());
        let rating = results.rating.as_ref().map(|r| r.text.clone()).unwrap_or_else(|| "NONE".to_string());
        let genres: Vec<String> = results.genres.iter().map(|g| g.text.clone()).collect();
        let synopsis = results.synopsis.as_ref().map(|s| s.confidence).unwrap_or(0.0);
        let alt_title = results.alt_title.as_ref().map(|a| a.confidence).unwrap_or(0.0);
        let author = results.author.as_ref().map(|a| a.confidence).unwrap_or(0.0);
        let status = results.status.as_ref().map(|s| s.confidence).unwrap_or(0.0);
        
        println!("  Title:   {} | Rating: {} | Genres: {} | Synopsis: {:.3} | AltTitle: {:.3} | Author: {:.3} | Status: {:.3} | Similar: {} | Chapters: {}",
            title.chars().take(40).collect::<String>(),
            rating,
            genres.join(","),
            synopsis, alt_title, author, status,
            results.similar.len(),
            results.chapters.len()
        );
        println!();
    }
}
