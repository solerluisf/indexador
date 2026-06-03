use anyhow::{Context, Result};
use lru::LruCache;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use windows::core::HSTRING;
use windows::Data::Pdf::PdfDocument;
use windows::Storage::StorageFile;
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};

// ---------------------------------------------------------------------------
// PdfRenderer — wraps Windows.Data.Pdf to produce PNG bytes from a PDF page
// ---------------------------------------------------------------------------

pub struct PdfRenderer {
    _initialized: bool,
}

impl PdfRenderer {
    #[allow(dead_code)]
    pub fn new() -> Result<Self> {
        Ok(Self { _initialized: true })
    }

    fn open(path: &Path) -> Result<PdfDocument> {
        let hpath = HSTRING::from(path.to_str().context("Non-UTF8 path")?);
        let file = StorageFile::GetFileFromPathAsync(&hpath)
            .context("Get file")?
            .get()
            .context("Get file await")?;
        let doc = PdfDocument::LoadFromFileAsync(&file)
            .context("Open PDF")?
            .get()
            .context("Open PDF await")?;
        Ok(doc)
    }

    #[allow(dead_code)]
    pub fn page_count(path: &Path) -> Result<u32> {
        let doc = Self::open(path)?;
        Ok(doc.PageCount()?)
    }

    pub fn render_page_to_png(path: &Path, page_index: u32, _max_width: u32) -> Result<Vec<u8>> {
        let doc = Self::open(path)?;
        let page = doc.GetPage(page_index).context("Get page")?;
        let stream = InMemoryRandomAccessStream::new().context("Create stream")?;

        // Optionally configure render size
        // PdfPageRenderOptions is in a different namespace path
        // For now render at default size

        page.RenderToStreamAsync(&stream)
            .context("Render page")?
            .get()
            .context("Render page await")?;

        let size = stream.Size().context("Get stream size")? as u32;
        if size == 0 {
            anyhow::bail!("PDF page rendered zero bytes");
        }

        let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)
            .context("Create reader")?;
        reader.LoadAsync(size)
            .context("Load data")?
            .get()
            .context("Load data await")?;

        let mut buf = vec![0u8; size as usize];
        reader.ReadBytes(&mut buf).context("Read bytes")?;
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// ThumbnailCache — LRU cache for rendered PDF thumbnail bytes
// ---------------------------------------------------------------------------

struct ThumbnailEntry {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct ThumbnailCache {
    cache: Mutex<LruCache<(PathBuf, u32), ThumbnailEntry>>,
}

impl ThumbnailCache {
    pub fn new(capacity: usize) -> Self {
        let n = NonZeroUsize::new(capacity.max(1)).unwrap();
        Self {
            cache: Mutex::new(LruCache::new(n)),
        }
    }

    pub fn get(&self, path: &Path, page: u32) -> Option<(Vec<u8>, u32, u32)> {
        let mut cache = self.cache.lock().unwrap();
        cache.get(&(path.to_path_buf(), page)).map(|e| (e.data.clone(), e.width, e.height))
    }

    pub fn put(&self, path: &Path, page: u32, data: Vec<u8>, width: u32, height: u32) {
        let mut cache = self.cache.lock().unwrap();
        cache.put((path.to_path_buf(), page), ThumbnailEntry { data, width, height });
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.cache.lock().unwrap().len()
    }

    #[allow(dead_code)]
    pub fn clear(&self) {
        self.cache.lock().unwrap().clear();
    }
}

// ---------------------------------------------------------------------------
// QueryFactory — maps user-facing query type flags to Tantivy queries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    Standard,
    Fuzzy,
    Regex,
    Phrase,
}

impl QueryType {
    pub fn variants() -> &'static [QueryType] {
        &[Self::Standard, Self::Fuzzy, Self::Regex, Self::Phrase]
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Standard => "Standard",
            Self::Fuzzy => "Fuzzy",
            Self::Regex => "Regex",
            Self::Phrase => "Phrase",
        }
    }
}

pub struct QueryFactory;

impl QueryFactory {
    /// Build a Tantivy query from the user's query string and selected mode.
    /// Panics if the field does not exist in the schema (caller should validate).
    #[allow(unused_variables)]
    pub fn build(
        query_type: QueryType,
        query_str: &str,
        field: tantivy::schema::Field,
        schema: &tantivy::schema::Schema,
        index: &tantivy::Index,
    ) -> Result<Box<dyn tantivy::query::Query>> {
        let empty = query_str.trim().is_empty();
        match query_type {
            QueryType::Standard => {
                let qp = tantivy::query::QueryParser::for_index(index, vec![field]);
                Ok(qp.parse_query(query_str)?)
            }
            QueryType::Fuzzy => {
                let term = tantivy::Term::from_field_text(field, query_str);
                Ok(Box::new(tantivy::query::FuzzyTermQuery::new(term, 1, true)))
            }
            QueryType::Regex => {
                Ok(Box::new(tantivy::query::RegexQuery::from_pattern(query_str, field)?))
            }
            QueryType::Phrase => {
                if empty {
                    return Ok(Box::new(tantivy::query::AllQuery));
                }
                let qp = tantivy::query::QueryParser::for_index(index, vec![field]);
                // Wrap in quotes to force phrase matching
                let phrase = format!("\"{}\"", query_str);
                Ok(qp.parse_query(&phrase)?)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // -- ThumbnailCache tests --

    #[test]
    fn test_thumbnail_cache_put_get() {
        let cache = ThumbnailCache::new(10);
        cache.put(Path::new("/a.pdf"), 0, vec![1, 2, 3], 100, 200);
        let result = cache.get(Path::new("/a.pdf"), 0);
        assert!(result.is_some());
        let (data, w, h) = result.unwrap();
        assert_eq!(data, vec![1, 2, 3]);
        assert_eq!(w, 100);
        assert_eq!(h, 200);
    }

    #[test]
    fn test_thumbnail_cache_miss() {
        let cache = ThumbnailCache::new(10);
        let result = cache.get(Path::new("/nonexistent.pdf"), 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_thumbnail_cache_different_page() {
        let cache = ThumbnailCache::new(10);
        cache.put(Path::new("/a.pdf"), 0, vec![1], 10, 10);
        cache.put(Path::new("/a.pdf"), 1, vec![2], 10, 10);
        assert_eq!(cache.get(Path::new("/a.pdf"), 0).unwrap().0, vec![1]);
        assert_eq!(cache.get(Path::new("/a.pdf"), 1).unwrap().0, vec![2]);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_thumbnail_cache_eviction() {
        let cache = ThumbnailCache::new(2);
        cache.put(Path::new("/a.pdf"), 0, vec![1], 1, 1);
        cache.put(Path::new("/b.pdf"), 0, vec![2], 1, 1);
        cache.put(Path::new("/c.pdf"), 0, vec![3], 1, 1);
        // a should be evicted
        assert!(cache.get(Path::new("/a.pdf"), 0).is_none());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_thumbnail_cache_clear() {
        let cache = ThumbnailCache::new(10);
        cache.put(Path::new("/a.pdf"), 0, vec![1], 1, 1);
        cache.put(Path::new("/b.pdf"), 0, vec![2], 1, 1);
        cache.clear();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_thumbnail_cache_zero_capacity_clamped() {
        let cache = ThumbnailCache::new(0);
        assert_eq!(cache.len(), 0);
        cache.put(Path::new("/a.pdf"), 0, vec![1], 1, 1);
        assert_eq!(cache.len(), 1);
    }

    // -- QueryFactory tests --

    fn test_index() -> (tantivy::Index, tantivy::schema::Field) {
        let mut schema_builder = tantivy::schema::Schema::builder();
        let field = schema_builder.add_text_field("content", tantivy::schema::TEXT);
        let schema = schema_builder.build();
        let index = tantivy::Index::create_in_ram(schema);
        (index, field)
    }

    #[test]
    fn test_query_factory_standard() {
        let (index, field) = test_index();
        let schema = index.schema();
        let q = QueryFactory::build(QueryType::Standard, "hello", field, &schema, &index);
        assert!(q.is_ok());
    }

    #[test]
    fn test_query_factory_fuzzy() {
        let (index, field) = test_index();
        let schema = index.schema();
        let q = QueryFactory::build(QueryType::Fuzzy, "hello", field, &schema, &index);
        assert!(q.is_ok());
    }

    #[test]
    fn test_query_factory_regex() {
        let (index, field) = test_index();
        let schema = index.schema();
        let q = QueryFactory::build(QueryType::Regex, "hel.*", field, &schema, &index);
        assert!(q.is_ok());
    }

    #[test]
    fn test_query_factory_regex_invalid_pattern() {
        let (index, field) = test_index();
        let schema = index.schema();
        let q = QueryFactory::build(QueryType::Regex, "[invalid", field, &schema, &index);
        assert!(q.is_err());
    }

    #[test]
    fn test_query_factory_phrase() {
        let (index, field) = test_index();
        let schema = index.schema();
        let q = QueryFactory::build(QueryType::Phrase, "hello world", field, &schema, &index);
        assert!(q.is_ok());
    }

    #[test]
    fn test_query_factory_phrase_empty() {
        let (index, field) = test_index();
        let schema = index.schema();
        let q = QueryFactory::build(QueryType::Phrase, "", field, &schema, &index);
        assert!(q.is_ok());
    }

    #[test]
    fn test_query_factory_standard_empty() {
        let (index, field) = test_index();
        let schema = index.schema();
        let q = QueryFactory::build(QueryType::Standard, "", field, &schema, &index);
        assert!(q.is_ok());
    }

    #[test]
    fn test_query_type_labels() {
        let labels: Vec<&str> = QueryType::variants().iter().map(|q| q.label()).collect();
        assert!(labels.contains(&"Standard"));
        assert!(labels.contains(&"Fuzzy"));
        assert!(labels.contains(&"Regex"));
        assert!(labels.contains(&"Phrase"));
    }
}
