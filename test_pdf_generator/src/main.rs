use lopdf::{dictionary, Document, Object, Stream};
use std::path::Path;

fn main() {
    let out_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "test_pdfs".to_string());

    let out = Path::new(&out_dir);
    std::fs::create_dir_all(out).unwrap();

    // 1. Case sensitivity
    make_pdf(
        &out.join("test_case_sensitivity.pdf"),
        &["Pattern", "pattern", "PATTERN", ""],
    );

    // 2. Stemming
    make_pdf(
        &out.join("test_stemming.pdf"),
        &["running", "runner", "ran", ""],
    );

    // 3. LaTeX / math
    make_pdf(
        &out.join("test_latex.pdf"),
        &[
            "The sum $\\sum_{i=0}^{n} x_i$ is evaluated",
            "Integral $\\int_{a}^{b} f(x) dx$",
            "Fraction $\\frac{a}{b}$ and root $\\sqrt{x}$",
            "",
        ],
    );

    // 4. Japanese
    make_pdf(
        &out.join("test_japanese.pdf"),
        &["機械学習の基礎", "パターン認識", "自然言語処理", ""],
    );

    // 5. Chinese
    make_pdf(
        &out.join("test_chinese.pdf"),
        &["机器学习", "深度学习", "自然语言处理", ""],
    );

    // 6. Phrase search
    make_pdf(
        &out.join("test_phrase.pdf"),
        &[
            "support vector machine",
            "machine learning",
            "vector machine learning",
            "",
        ],
    );

    // 7. Mixed Chinese + English
    make_pdf(
        &out.join("test_mixed.pdf"),
        &["Machine Learning 机器学习", "deep learning 深度学习", ""],
    );

    // 8. Boolean test
    make_pdf(
        &out.join("test_boolean.pdf"),
        &["cat dog bird", "cat", "dog", ""],
    );

    // 9. Large document (stress — ~5KB of text)
    make_pdf(
        &out.join("test_large.pdf"),
        &[&"lorem ipsum dolor sit amet ".repeat(300)],
    );

    // 10. Single blank page
    make_pdf(&out.join("test_blank.pdf"), &[""]);

    // 11. Multi-page with same word on each page
    make_pdf(
        &out.join("test_repeat.pdf"),
        &["pattern", "pattern", "pattern", "pattern"],
    );

    // 12. Phrase with extra standalone word (for phrase-vs-AND regression test)
    make_pdf(
        &out.join("test_phrase_extra.pdf"),
        &["machine learning", "machine", "machine learning"],
    );

    // 13. Phrase test: machine standalone on page 1, machine learning on pages 2-3, blank page 4.
    //     "machine learning" (phrase) must return only pages 2 and 3.
    make_pdf(
        &out.join("test_custom_phrase.pdf"),
        &["machine", "machine learning", "machine learning", ""],
    );

    // 14. Rotate 90 (landscape)
    make_pdf_with_configs(
        &out.join("test_rotate90.pdf"),
        &[PageConfig {
            text: "landscape rotated page with machine learning",
            rotate: Some(90),
            cropbox: None,
            mediabox: [0.0, 0.0, 792.0, 612.0],
            font_size: 12.0,
            x_pos: 50.0,
            y_pos: 500.0,
        }],
    );

    // 15. CropBox different from MediaBox
    make_pdf_with_configs(
        &out.join("test_cropbox.pdf"),
        &[PageConfig {
            text: "cropped text content",
            rotate: None,
            cropbox: Some([50.0, 50.0, 562.0, 742.0]),
            mediabox: [0.0, 0.0, 612.0, 792.0],
            font_size: 12.0,
            x_pos: 60.0,
            y_pos: 700.0,
        }],
    );

    // 16. Two columns text
    make_pdf_with_configs(
        &out.join("test_columns.pdf"),
        &[PageConfig {
            text: "left column machine learning is a subset of artificial intelligence",
            rotate: None,
            cropbox: None,
            mediabox: [0.0, 0.0, 612.0, 792.0],
            font_size: 10.0,
            x_pos: 40.0,
            y_pos: 730.0,
        }],
    );

    // 17. Ligatures (fi/fl as regular text sequences)
    make_pdf_with_configs(
        &out.join("test_ligatures.pdf"),
        &[PageConfig {
            text: "first flight field flip flop final",
            rotate: None,
            cropbox: None,
            mediabox: [0.0, 0.0, 612.0, 792.0],
            font_size: 12.0,
            x_pos: 50.0,
            y_pos: 700.0,
        }],
    );

    // 18. Accented characters
    make_pdf_with_configs(
        &out.join("test_accented.pdf"),
        &[PageConfig {
            text: "naïve café résumé señor jalapeño über",
            rotate: None,
            cropbox: None,
            mediabox: [0.0, 0.0, 612.0, 792.0],
            font_size: 12.0,
            x_pos: 50.0,
            y_pos: 700.0,
        }],
    );

    eprintln!("✅ Generated 18 test PDFs in '{}'", out_dir);
}

struct PageConfig<'a> {
    text: &'a str,
    rotate: Option<i32>,
    cropbox: Option<[f64; 4]>,
    mediabox: [f64; 4],
    font_size: f64,
    x_pos: f64,
    y_pos: f64,
}

fn make_pdf(path: &Path, page_texts: &[&str]) {
    let configs: Vec<PageConfig> = page_texts
        .iter()
        .map(|&text| PageConfig {
            text,
            rotate: None,
            cropbox: None,
            mediabox: [0.0, 0.0, 612.0, 792.0],
            font_size: 12.0,
            x_pos: 50.0,
            y_pos: 700.0,
        })
        .collect();
    make_pdf_with_configs(path, &configs);
}

fn make_pdf_with_configs(path: &Path, configs: &[PageConfig]) {
    use lopdf::ObjectId;
    let mut doc = Document::new();

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => font_id,
        },
    });

    let mut next_id: ObjectId = (doc.max_id + 1, 0);
    let pages_id: ObjectId = next_id;
    next_id.0 += 1;

    let pages: Vec<&PageConfig> = if configs.is_empty() {
        vec![&PageConfig {
            text: "",
            rotate: None,
            cropbox: None,
            mediabox: [0.0, 0.0, 612.0, 792.0],
            font_size: 12.0,
            x_pos: 50.0,
            y_pos: 700.0,
        }]
    } else {
        configs.iter().collect()
    };

    let page_ids: Vec<ObjectId> = (0..pages.len())
        .map(|_| {
            let id = next_id;
            next_id.0 += 1;
            id
        })
        .collect();

    for (i, cfg) in pages.iter().enumerate() {
        let page_id = page_ids[i];
        let stream_id: ObjectId = next_id;
        next_id.0 += 1;

        let content: Vec<u8> = if cfg.text.is_empty() {
            Vec::new()
        } else {
            let escaped = cfg
                .text
                .replace('\\', "\\\\")
                .replace('(', "\\(")
                .replace(')', "\\)");
            format!(
                "BT /F1 {} Tf {} {} Td ({escaped}) Tj ET",
                cfg.font_size, cfg.x_pos, cfg.y_pos
            )
            .into_bytes()
        };

        doc.objects.insert(stream_id, Stream::new(dictionary! {}, content).into());

        let mut page_dict = dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => Object::Array(
                cfg.mediabox.iter().map(|&v| v.into()).collect()
            ),
            "Contents" => stream_id,
            "Resources" => resources_id,
        };
        if let Some(rot) = cfg.rotate {
            page_dict.set("Rotate", rot);
        }
        if let Some(cb) = cfg.cropbox {
            page_dict.set(
                "CropBox",
                Object::Array(vec![cb[0].into(), cb[1].into(), cb[2].into(), cb[3].into()]),
            );
        }
        doc.objects.insert(page_id, Object::Dictionary(page_dict));
    }

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(
                page_ids.iter().map(|id| Object::Reference(*id)).collect(),
            ),
            "Count" => page_ids.len() as i32,
        }),
    );

    let catalog_id: ObjectId = next_id;
    next_id.0 += 1;
    doc.objects.insert(
        catalog_id,
        Object::Dictionary(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }),
    );
    doc.trailer.set("Root", catalog_id);

    doc.max_id = next_id.0 - 1;
    doc.save(path).unwrap();
}

