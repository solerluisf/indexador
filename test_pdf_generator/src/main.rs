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

    eprintln!("✅ Generated {} test PDFs in '{}'", 13, out_dir);
}

fn make_pdf(path: &Path, page_texts: &[&str]) {
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

    // Pre-allocate IDs: pages object + each page
    let mut next_id: ObjectId = (doc.max_id + 1, 0);
    let pages_id: ObjectId = next_id;
    next_id.0 += 1;

    let pages = if page_texts.is_empty() {
        vec![""]
    } else {
        page_texts.to_vec()
    };

    let page_ids: Vec<ObjectId> = (0..pages.len())
        .map(|_| {
            let id = next_id;
            next_id.0 += 1;
            id
        })
        .collect();

    for (i, &text) in pages.iter().enumerate() {
        let page_id = page_ids[i];
        let stream_id: ObjectId = next_id;
        next_id.0 += 1;

        let content: Vec<u8> = if text.is_empty() {
            Vec::new()
        } else {
            // Escape parens and backslashes in content stream strings
            let escaped = text
                .replace('\\', "\\\\")
                .replace('(', "\\(")
                .replace(')', "\\)");
            format!("BT /F1 12 Tf 50 700 Td ({escaped}) Tj ET").into_bytes()
        };

        doc.objects.insert(stream_id, Stream::new(dictionary! {}, content).into());
        doc.objects.insert(
            page_id,
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => Object::Array(vec![0.into(), 0.into(), 612.into(), 792.into()]),
                "Contents" => stream_id,
                "Resources" => resources_id,
            }),
        );
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

