/// Integration tests untuk parser tl (setelah migrasi dari scraper/html5ever).
///
/// Test fungsionalitas: verifikasi output parser benar & lengkap.
/// Test benchmark: ukur kecepatan parsing tl vs baseline.

mod parser_tests {
    use std::fs;
    use std::time::Instant;

    /// Helper: load fixture file
    fn load_fixture(name: &str) -> String {
        let base = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let path = format!("{base}/tests/fixtures/{name}");
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("Failed to load fixture {name}: {e}"))
    }

    // ============================================================
    // parse_komik_list
    // ============================================================

    #[test]
    fn test_parse_komik_list_basic() {
        let html = load_fixture("list_page.html");
        let slugs = komikindo_scraper::parsers::parse_komik_list(&html);

        // Should extract all unique slugs
        assert!(slugs.contains(&"one-piece".to_string()), "Missing slug: one-piece");
        assert!(slugs.contains(&"nano-machine".to_string()), "Missing slug: nano-machine");
        assert!(slugs.contains(&"blue-lock".to_string()), "Missing slug: blue-lock");
        assert!(slugs.contains(&"jujutsu-kaisen".to_string()), "Missing slug: jujutsu-kaisen");
        assert!(slugs.contains(&"solo-leveling".to_string()), "Missing slug: solo-leveling");
        assert!(slugs.contains(&"155895-nano-machine".to_string()), "Missing slug: 155895-nano-machine");

        // Duplicate slug should be deduplicated
        let duplicate_count = slugs.iter().filter(|s| s == &"duplicate-slug").count();
        assert_eq!(duplicate_count, 1, "Duplicate slugs should be deduplicated");

        // Total should be 7 unique slugs (duplicate-slug only once)
        assert_eq!(slugs.len(), 7, "Expected 7 unique slugs, got {}", slugs.len());
    }

    #[test]
    fn test_parse_komik_list_empty_html() {
        let slugs = komikindo_scraper::parsers::parse_komik_list("");
        assert!(slugs.is_empty(), "Empty HTML should return empty slugs");
    }

    #[test]
    fn test_parse_komik_list_no_links() {
        let html = "<html><body><p>No komik links here</p></body></html>";
        let slugs = komikindo_scraper::parsers::parse_komik_list(html);
        assert!(slugs.is_empty(), "HTML without komik links should return empty");
    }

    // ============================================================
    // parse_komik_detail
    // ============================================================

    #[test]
    fn test_parse_komik_detail_full() {
        let html = load_fixture("detail_page.html");
        let detail = komikindo_scraper::parsers::parse_komik_detail("nano-machine", &html)
            .expect("Should parse detail page");

        // Slug
        assert_eq!(detail.slug, "nano-machine");

        // Title — "Komik Nano Machine" should be cleaned to "Nano Machine"
        assert_eq!(detail.judul.as_deref(), Some("Nano Machine"),
            "Title 'Komik Nano Machine' should be cleaned to 'Nano Machine'");

        // Type
        assert_eq!(detail.tipe.as_deref(), Some("Manhwa"),
            "Type should be Manhwa from typeflag class");

        // Status
        assert_eq!(detail.status_id, Some(1),
            "Status 'Berjalan' should map to id 1");

        // Author
        assert_eq!(detail.author.as_deref(), Some("Han-Joong-Wol-Ya"));

        // Artist
        assert_eq!(detail.artist.as_deref(), Some("KKOM-JAE"));

        // Alternative title
        assert_eq!(detail.alternative_title.as_deref(), Some("Nano Machine - Nanogiga"));

        // Rating
        assert_eq!(detail.rating, Some(9.2));

        // Genre IDs (Action=1, Fantasy=17, Martial Arts=36)
        assert!(detail.genre_ids.contains(&1), "Should have Action genre (id=1)");
        assert!(detail.genre_ids.contains(&17), "Should have Fantasy genre (id=17)");
        assert!(detail.genre_ids.contains(&36), "Should have Martial Arts genre (id=36)");
        assert_eq!(detail.genre_ids.len(), 3, "Should have exactly 3 genres");

        // Genre names
        assert!(detail.genre_list.contains(&"Action".to_string()));
        assert!(detail.genre_list.contains(&"Fantasy".to_string()));
        assert!(detail.genre_list.contains(&"Martial Arts".to_string()));

        // Thumbnail
        assert!(detail.thumb_domain_id.is_some(), "Should have thumb domain id");
        assert!(detail.thumb_path.is_some(), "Should have thumb path");

        // Sinopsis
        assert!(detail.sinopsis.is_some(), "Should have sinopsis");
        let sinopsis = detail.sinopsis.unwrap();
        assert!(!sinopsis.is_empty(), "Sinopsis should not be empty");
        // Should not contain the prefix "Update chapter terbaru..."
        assert!(!sinopsis.starts_with("Update chapter"), "Sinopsis prefix should be cleaned");

        // Chapters
        assert!(!detail.chapters.is_empty(), "Should have chapters");
        assert_eq!(detail.chapters.len(), 5, "Should have 5 chapters");

        // Chapters should be sorted descending
        assert_eq!(detail.chapters[0].number, 309.0, "First chapter should be 309 (descending)");
        assert_eq!(detail.chapters[1].number, 308.0, "Second chapter should be 308");
        assert_eq!(detail.chapters[2].number, 307.5, "Third chapter should be 307.5");

        // Chapter URLs should be from the detail page (not constructed)
        assert!(detail.chapters[0].url.contains("nano-machine-chapter-309"),
            "Chapter URL should come from href, not be constructed");

        // Latest chapter
        assert_eq!(detail.latest_chapter_number, Some(309.0),
            "Latest chapter should be 309.0");
    }

    #[test]
    fn test_parse_komik_detail_empty_html() {
        let result = komikindo_scraper::parsers::parse_komik_detail("test", "");
        // tl returns Err on empty string parse — should return None
        assert!(result.is_none(), "Empty HTML should return None");
    }

    #[test]
    fn test_parse_komik_detail_no_data() {
        let html = "<html><body><p>No data here</p></body></html>";
        let detail = komikindo_scraper::parsers::parse_komik_detail("test-slug", html);
        // Should still return Some with default values
        assert!(detail.is_some(), "Should return Some even with no data");
        let d = detail.unwrap();
        assert_eq!(d.slug, "test-slug");
        assert!(d.judul.is_none(), "Should have no title");
        assert!(d.chapters.is_empty(), "Should have no chapters");
    }

    // ============================================================
    // parse_homepage_updates
    // ============================================================

    #[test]
    fn test_parse_homepage_updates() {
        let html = load_fixture("homepage.html");
        let updates = komikindo_scraper::parsers::parse_homepage_updates(&html);

        assert_eq!(updates.len(), 3, "Should find 3 homepage updates");

        // First update
        let first = &updates[0];
        assert_eq!(first.slug, "one-piece");
        assert_eq!(first.judul.as_deref(), Some("One Piece"));
        assert_eq!(first.latest_chapter_number, Some(1120.0));
        assert_eq!(first.tipe.as_deref(), Some("Manga"));

        // Second
        let second = &updates[1];
        assert_eq!(second.slug, "blue-lock");
        assert_eq!(second.latest_chapter_number, Some(268.0));

        // Third
        let third = &updates[2];
        assert_eq!(third.slug, "nano-machine");
        assert_eq!(third.tipe.as_deref(), Some("Manhwa"));
    }

    #[test]
    fn test_parse_homepage_updates_empty() {
        let updates = komikindo_scraper::parsers::parse_homepage_updates("");
        assert!(updates.is_empty(), "Empty HTML should return empty updates");
    }

    // ============================================================
    // parse_komik_terbaru
    // ============================================================

    #[test]
    fn test_parse_komik_terbaru() {
        let html = load_fixture("terbaru_page.html");
        let items = komikindo_scraper::parsers::parse_komik_terbaru(&html);

        assert_eq!(items.len(), 4, "Should find 4 terbaru items");

        // First item
        let first = &items[0];
        assert_eq!(first.slug, "one-piece");
        assert_eq!(first.judul, "One Piece");
        assert_eq!(first.chapter_number, 1120.0);
        assert!(first.chapter_url.contains("one-piece-chapter-1120"));

        // Time parsing
        assert_eq!(first.time_minutes, 5, "5 menit lalu = 5 minutes");
        assert_eq!(items[1].time_minutes, 30, "30 menit lalu = 30 minutes");
        assert_eq!(items[2].time_minutes, 120, "2 jam lalu = 120 minutes");
        assert_eq!(items[3].time_minutes, 1440, "1 hari lalu = 1440 minutes");

        // URL base extraction
        assert!(items[0].url_base.contains("one-piece"), "URL base should contain slug");
    }

    #[test]
    fn test_parse_komik_terbaru_empty() {
        let items = komikindo_scraper::parsers::parse_komik_terbaru("");
        assert!(items.is_empty(), "Empty HTML should return empty items");
    }

    // ============================================================
    // Benchmark: tl parser speed (local fixtures, no network)
    // ============================================================

    #[test]
    fn bench_parse_komik_detail() {
        let html = load_fixture("detail_page.html");
        let iters = 1000;

        // Warmup
        for _ in 0..10 {
            let _ = komikindo_scraper::parsers::parse_komik_detail("bench-slug", &html);
        }

        let t0 = Instant::now();
        for _ in 0..iters {
            let result = komikindo_scraper::parsers::parse_komik_detail("bench-slug", &html);
            std::hint::black_box(result);
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let per_iter_us = elapsed / iters as f64 * 1_000_000.0;

        println!("\n[BENCH] parse_komik_detail (tl): {:.1} µs/iter | {:.0} iters/s\n",
            per_iter_us, iters as f64 / elapsed);

        // tl should be well under 1ms per detail page parse
        assert!(per_iter_us < 5000.0,
            "parse_komik_detail should be < 5ms/iter, got {:.1} µs/iter", per_iter_us);
    }

    #[test]
    fn bench_parse_komik_list() {
        let html = load_fixture("list_page.html");
        let iters = 1000;

        // Warmup
        for _ in 0..10 {
            let _ = komikindo_scraper::parsers::parse_komik_list(&html);
        }

        let t0 = Instant::now();
        for _ in 0..iters {
            let result = komikindo_scraper::parsers::parse_komik_list(&html);
            std::hint::black_box(result);
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let per_iter_us = elapsed / iters as f64 * 1_000_000.0;

        println!("\n[BENCH] parse_komik_list (tl): {:.1} µs/iter | {:.0} iters/s\n",
            per_iter_us, iters as f64 / elapsed);

        assert!(per_iter_us < 5000.0,
            "parse_komik_list should be < 5ms/iter, got {:.1} µs/iter", per_iter_us);
    }

    #[test]
    fn bench_parse_homepage_updates() {
        let html = load_fixture("homepage.html");
        let iters = 1000;

        for _ in 0..10 {
            let _ = komikindo_scraper::parsers::parse_homepage_updates(&html);
        }

        let t0 = Instant::now();
        for _ in 0..iters {
            let result = komikindo_scraper::parsers::parse_homepage_updates(&html);
            std::hint::black_box(result);
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let per_iter_us = elapsed / iters as f64 * 1_000_000.0;

        println!("\n[BENCH] parse_homepage_updates (tl): {:.1} µs/iter | {:.0} iters/s\n",
            per_iter_us, iters as f64 / elapsed);

        assert!(per_iter_us < 5000.0,
            "parse_homepage_updates should be < 5ms/iter, got {:.1} µs/iter", per_iter_us);
    }

    #[test]
    fn bench_parse_komik_terbaru() {
        let html = load_fixture("terbaru_page.html");
        let iters = 1000;

        for _ in 0..10 {
            let _ = komikindo_scraper::parsers::parse_komik_terbaru(&html);
        }

        let t0 = Instant::now();
        for _ in 0..iters {
            let result = komikindo_scraper::parsers::parse_komik_terbaru(&html);
            std::hint::black_box(result);
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let per_iter_us = elapsed / iters as f64 * 1_000_000.0;

        println!("\n[BENCH] parse_komik_terbaru (tl): {:.1} µs/iter | {:.0} iters/s\n",
            per_iter_us, iters as f64 / elapsed);

        assert!(per_iter_us < 5000.0,
            "parse_komik_terbaru should be < 5ms/iter, got {:.1} µs/iter", per_iter_us);
    }

    // ============================================================
    // Edge cases
    // ============================================================

    #[test]
    fn test_chapter_decimal_number() {
        let html = r#"<div class="bxcl">
            <span class="lchx"><a href="https://komikindo.ch/test-chapter-307-5/">Chapter 307.5</a></span>
        </div>"#;
        let detail = komikindo_scraper::parsers::parse_komik_detail("test", html);
        assert!(detail.is_some());
        let d = detail.unwrap();
        assert_eq!(d.chapters.len(), 1);
        assert_eq!(d.chapters[0].number, 307.5, "Decimal chapter number should be parsed");
    }

    #[test]
    fn test_chapter_url_from_href() {
        let html = r#"<div class="bxcl">
            <span class="lchx"><a href="https://komikindo.ch/nano-machine-chapter-309/">Chapter 309</a></span>
        </div>"#;
        let detail = komikindo_scraper::parsers::parse_komik_detail("test", html);
        assert!(detail.is_some());
        let d = detail.unwrap();
        assert_eq!(d.chapters[0].url, "https://komikindo.ch/nano-machine-chapter-309/");
    }

    #[test]
    fn test_relative_url_chapter() {
        let html = r#"<div class="bxcl">
            <span class="lchx"><a href="/test-chapter-5/">Chapter 5</a></span>
        </div>"#;
        let detail = komikindo_scraper::parsers::parse_komik_detail("test", html);
        assert!(detail.is_some());
        let d = detail.unwrap();
        assert!(d.chapters[0].url.starts_with("https://"), "Relative URL should be normalized to absolute");
    }

    #[test]
    fn test_title_without_komik_prefix() {
        let html = r#"<h1 class="titless">Solo Leveling</h1>"#;
        let detail = komikindo_scraper::parsers::parse_komik_detail("solo-leveling", html);
        assert!(detail.is_some());
        assert_eq!(detail.unwrap().judul.as_deref(), Some("Solo Leveling"),
            "Title without 'Komik' prefix should be kept as-is");
    }

    #[test]
    fn test_status_tamat() {
        let html = r#"<div class="infox"><div class="spe">
            <span><b>Status:</b> Tamat</span>
        </div></div>"#;
        let detail = komikindo_scraper::parsers::parse_komik_detail("test", html);
        assert!(detail.is_some());
        assert_eq!(detail.unwrap().status_id, Some(2), "Tamat should map to id 2");
    }

    #[test]
    fn test_genre_from_tag_links() {
        let html = r#"<div class="infox"><div class="spe">
            <a rel="tag" href="/genre/action/">Action</a>
            <a rel="tag" href="/genre/romance/">Romance</a>
        </div></div>"#;
        let detail = komikindo_scraper::parsers::parse_komik_detail("test", html);
        assert!(detail.is_some());
        let d = detail.unwrap();
        assert!(d.genre_list.contains(&"Action".to_string()));
        assert!(d.genre_list.contains(&"Romance".to_string()));
        assert!(d.genre_ids.contains(&1), "Action id=1");
        assert!(d.genre_ids.contains(&53), "Romance id=53");
    }

    #[test]
    fn test_thumbnail_normalization() {
        // Test wp-content/uploads prefix stripping and Komik- prefix stripping
        let html = r#"<div class="infoanime">
            <img src="https://imageainewgeneration.lol/data/wp-content/uploads/Komik-one-piece-300x400.jpg">
        </div>"#;
        let detail = komikindo_scraper::parsers::parse_komik_detail("test", html);
        assert!(detail.is_some());
        let d = detail.unwrap();
        assert!(d.thumb_domain_id.is_some(), "Should have thumb domain");
        assert!(d.thumb_path.is_some(), "Should have thumb path");
        let path = d.thumb_path.unwrap();
        assert!(!path.contains("wp-content/uploads/"), "WP prefix should be stripped");
        assert!(!path.contains("Komik-"), "Komik- prefix should be stripped");
        assert!(!path.contains("-300x400"), "Dimension suffix should be stripped");
    }

    #[test]
    fn test_large_html_performance() {
        // Generate a large HTML (simulating real komikindo page ~500KB)
        let mut chapters_html = String::with_capacity(200_000);
        for i in 1..=500 {
            chapters_html.push_str(&format!(
                r#"<span class="lchx"><a href="https://komikindo.ch/big-komik-chapter-{}/">Chapter {}</a></span>"#,
                i, i
            ));
        }
        let html = format!(r#"
        <html><body>
        <div class="infoanime"><img src="https://imageainewgeneration.lol/data/wp-content/uploads/Komik-test.jpg"></div>
        <div class="infox">
            <h1 class="titless">Big Komik</h1>
            <span class="typeflag Manga">Manga</span>
            <div class="spe">
                <span><b>Status:</b> Berjalan</span>
                <span><b>Genre:</b> Action, Adventure</span>
                <a rel="tag" href="/genre/action/">Action</a>
                <a rel="tag" href="/genre/adventure/">Adventure</a>
            </div>
        </div>
        <div class="bxcl">{}</div>
        </body></html>"#, chapters_html);

        let iters = 100;

        // Warmup
        for _ in 0..5 {
            let _ = komikindo_scraper::parsers::parse_komik_detail("big-komik", &html);
        }

        let t0 = Instant::now();
        for _ in 0..iters {
            let result = komikindo_scraper::parsers::parse_komik_detail("big-komik", &html);
            std::hint::black_box(result);
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let per_iter_ms = elapsed / iters as f64 * 1000.0;

        println!("\n[BENCH] parse_komik_detail (large HTML, 500 chapters): {:.2} ms/iter | {:.1} iters/s\n",
            per_iter_ms, iters as f64 / elapsed);

        // Even with 500 chapters, should be under 10ms
        assert!(per_iter_ms < 50.0,
            "Large detail page should parse in < 50ms, got {:.2} ms", per_iter_ms);
    }
}
