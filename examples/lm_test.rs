use std::fs;
use komikindo_scraper::lm_selector::LmDetector;

fn main() {
    std::env::set_var("ORT_DYLIB_PATH", "/home/z/.local/lib/python3.13/site-packages/onnxruntime/capi/libonnxruntime.so.1.25.1");
    
    let mut detector = LmDetector::new().expect("Failed to load model");
    println!("[OK] Model loaded");
    
    let html = fs::read_to_string("html_cache/cold.html").expect("Failed to read HTML");
    println!("[OK] HTML loaded, {} bytes", html.len());
    
    let results = detector.detect_all_fields(&html);
    println!("[OK] Detection complete");
    
    if let Some(t) = &results.title {
        println!("  Title:   {} (conf: {:.3})", t.text, t.confidence);
    } else {
        println!("  Title:   NOT DETECTED");
    }
    
    if let Some(r) = &results.rating {
        println!("  Rating:  {} (conf: {:.3})", r.text, r.confidence);
    } else {
        println!("  Rating:  NOT DETECTED");
    }
    
    println!("  Genres:  {} detected", results.genres.len());
    for g in &results.genres {
        println!("    - {} (conf: {:.3})", g.text, g.confidence);
    }
    
    if let Some(s) = &results.synopsis {
        let end = 80.min(s.text.len());
        println!("  Synopsis: {}... (conf: {:.3})", &s.text[..end], s.confidence);
    } else {
        println!("  Synopsis: NOT DETECTED");
    }
    
    if let Some(a) = &results.alt_title {
        println!("  Alt Title: {} (conf: {:.3})", a.text, a.confidence);
    } else {
        println!("  Alt Title: NOT DETECTED");
    }
    
    if let Some(a) = &results.author {
        println!("  Author:  {} (conf: {:.3})", a.text, a.confidence);
    } else {
        println!("  Author:  NOT DETECTED");
    }
    
    if let Some(s) = &results.status {
        println!("  Status:  {} (conf: {:.3})", s.text, s.confidence);
    } else {
        println!("  Status:  NOT DETECTED");
    }
    
    println!("  Similar: {} detected", results.similar.len());
    for s in results.similar.iter().take(3) {
        println!("    - {} (conf: {:.3})", s.text, s.confidence);
    }
    
    println!("  Chapters: {} detected", results.chapters.len());
    for c in results.chapters.iter().take(3) {
        println!("    - {} (conf: {:.3})", c.text, c.confidence);
    }
}
