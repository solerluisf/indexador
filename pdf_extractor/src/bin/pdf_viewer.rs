// Phase 9 — Native Windows PDF Viewer.
// Win32 GUI with three-pane layout. Tantivy search integrated directly.

#![windows_subsystem = "windows"]

#[path = "../viewer.rs"]
mod viewer;

use anyhow::{Context, Result};
use image::GenericImageView;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use tantivy::collector::TopDocs;

use tantivy::schema::Value;
use tantivy::TantivyDocument;
use viewer::{PdfRenderer, QueryFactory, QueryType, ThumbnailCache};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct AppResult {
    path: String,
    score: f32,
    snippet: String,
}

struct AppState {
    index: Option<tantivy::Index>,
    results: Vec<AppResult>,
    query_string: String,
    query_type: QueryType,
    current_pdf: Option<PathBuf>,
    thumbnail_cache: Arc<Mutex<ThumbnailCache>>,
}

static STATE: AtomicPtr<AppState> = AtomicPtr::new(std::ptr::null_mut());
static HWND_STORE: AtomicIsize = AtomicIsize::new(0);

const ID_EDIT: i32 = 101;
const ID_BUTTON: i32 = 102;
const ID_COMBO: i32 = 103;
const ID_LIST: i32 = 104;

// ---------------------------------------------------------------------------
// GDI helpers
// ---------------------------------------------------------------------------

fn png_to_hbitmap(png_data: &[u8]) -> Result<HBITMAP> {
    let img = image::load_from_memory(png_data)?;
    let (w, h) = img.dimensions();
    let rgba = img.to_rgba8();
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for p in rgba.chunks(4) {
        bgra.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
    }

    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: w * h * 4,
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default(); 1],
    };

    unsafe {
        let dc = GetDC(HWND::default());
        let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits_ptr, HANDLE::default(), 0)?;
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits_ptr as *mut u8, bgra.len());
        _ = ReleaseDC(HWND::default(), dc);
        Ok(hbmp)
    }
}

unsafe fn fill_rect(hdc: HDC, l: i32, t: i32, r: i32, b: i32, brush: HBRUSH) {
    let rc = RECT { left: l, top: t, right: r, bottom: b };
    FillRect(hdc, &rc, brush);
}

fn make_brush(color: u32) -> HBRUSH {
    unsafe { CreateSolidBrush(COLORREF(color)) }
}

// ---------------------------------------------------------------------------
// Window procedure
// ---------------------------------------------------------------------------

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SIZE => {
            let w = (lparam.0 & 0xFFFF) as i32;
            let h = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let bh = 40i32.min(h / 4);
            let rw = 250i32.min(w / 3);
            let mv = |id: i32, x: i32, y: i32, cx: i32, cy: i32| {
                if let Ok(h) = GetDlgItem(hwnd, id) {
                    let _ = SetWindowPos(h, HWND::default(), x, y, cx, cy, SWP_NOZORDER);
                }
            };
            mv(ID_EDIT, 5, h - bh + 5, (w - rw - 210).max(50), 24);
            mv(ID_BUTTON, (w - rw - 200).max(5), h - bh + 5, 80, 24);
            mv(ID_COMBO, (w - rw - 110).max(5), h - bh + 5, 100, 200);
            mv(ID_LIST, w - rw, 0, rw, h - bh);
            let _ = InvalidateRect(hwnd, None, true);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;

            if id == ID_BUTTON {
                let st = STATE.load(Ordering::Relaxed);
                if !st.is_null() {
                    let st = &mut *st;
                    if let Ok(edit) = GetDlgItem(hwnd, ID_EDIT) {
                        let len = GetWindowTextLengthW(edit);
                        if len > 0 {
                            let mut buf = vec![0u16; (len + 1) as usize];
                            GetWindowTextW(edit, &mut buf);
                            buf.truncate(len as usize);
                            st.query_string = String::from_utf16_lossy(&buf);
                        } else { st.query_string.clear(); }
                    }
                    if let Ok(combo) = GetDlgItem(hwnd, ID_COMBO) {
                        let idx = SendMessageW(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0));
                        st.query_type = match idx.0 {
                            1 => QueryType::Fuzzy,
                            2 => QueryType::Regex,
                            3 => QueryType::Phrase,
                            _ => QueryType::Standard,
                        };
                    }
                    st.results.clear();
                    if let Some(ref index) = st.index {
                        if !st.query_string.is_empty() {
                            let field = index.schema().get_field("content_norm").ok();
                            if let Some(field) = field {
                                if let Ok(q) = QueryFactory::build(st.query_type, &st.query_string, field, &index.schema(), index) {
                                    if let Ok(reader) = index.reader_builder().try_into() {
                                        let searcher = reader.searcher();
                                        if let Ok(hits) = searcher.search(&*q, &TopDocs::with_limit(50)) {
                                            let path_f = index.schema().get_field("path").ok();
                                            let raw_f = index.schema().get_field("content_raw").ok();
                                            for (score, addr) in hits {
                                                if let Ok(doc) = searcher.doc::<TantivyDocument>(addr) {
                                                    let path = path_f.and_then(|f|
                                                        doc.get_first(f).and_then(|v| v.as_str())
                                                    ).unwrap_or("").to_string();
                                                    let raw = raw_f.and_then(|f|
                                                        doc.get_first(f).and_then(|v| v.as_str())
                                                    ).unwrap_or("");
                                                    let snippet = if raw.len() > 100 {
                                                        format!("{}...", &raw[..100])
                                                    } else { raw.to_string() };
                                                    st.results.push(AppResult { path, score, snippet });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Ok(list) = GetDlgItem(hwnd, ID_LIST) {
                        let _ = SendMessageW(list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
                        for r in &st.results {
                            let text = format!("[{:.2}] {} — {}", r.score, r.path, r.snippet);
                            let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
                            let _ = SendMessageW(list, LB_ADDSTRING, WPARAM(0), LPARAM(wide.as_ptr() as isize));
                        }
                    }
                }
                let _ = InvalidateRect(hwnd, None, true);
            }

            if id == ID_LIST {
                let st = STATE.load(Ordering::Relaxed);
                if !st.is_null() {
                    let st = &mut *st;
                    if let Ok(list) = GetDlgItem(hwnd, ID_LIST) {
                        let sel = SendMessageW(list, LB_GETCURSEL, WPARAM(0), LPARAM(0));
                        let idx = sel.0 as usize;
                        if idx < st.results.len() {
                            let path = st.results[idx].path.clone();
                            st.current_pdf = Some(PathBuf::from(&path));
                            let p = st.current_pdf.clone().unwrap();
                            let cache = Arc::clone(&st.thumbnail_cache);
                            let hwnd_val = HWND_STORE.load(Ordering::Relaxed);
                            std::thread::spawn(move || {
                                if let Ok(png) = PdfRenderer::render_page_to_png(&p, 0, 1200) {
                                    if let Ok(img) = image::load_from_memory(&png) {
                                        let (iw, ih) = img.dimensions();
                                        cache.lock().unwrap().put(&p, 0, png, iw, ih);
                                        unsafe {
                                            let _ = PostMessageW(HWND(hwnd_val as *mut _), WM_PAINT, WPARAM(0), LPARAM(0));
                                        }
                                    }
                                }
                            });
                        }
                    }
                }
                let _ = InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;
            let bh = 40i32.min(h / 4);
            let rw = 250i32.min(w / 3);

            let white = make_brush(0x00FFFFFF);
            fill_rect(hdc, 0, 0, w - rw, h - bh, white);
            let _ = DeleteObject(white);

            let st = STATE.load(Ordering::Relaxed);
            if !st.is_null() {
                let st = &*st;
                if let Some(ref pdf_path) = st.current_pdf {
                    let cached = st.thumbnail_cache.lock().unwrap().get(pdf_path, 0);
                    if let Some((png_data, _, _)) = cached {
                        if let Ok(hbmp) = png_to_hbitmap(&png_data) {
                            let mem = CreateCompatibleDC(hdc);
                            let old = SelectObject(mem, hbmp);
                            let mut bmp = BITMAP::default();
                            let _ = GetObjectW(hbmp, std::mem::size_of::<BITMAP>() as i32,
                                Some(&mut bmp as *mut _ as *mut _));
                            let sx = bmp.bmWidth;
                            let sy = bmp.bmHeight;
                            let scale = ((w - rw) as f32 / sx as f32).min((h - bh) as f32 / sy as f32).min(1.0);
                            let dx = (sx as f32 * scale) as i32;
                            let dy = (sy as f32 * scale) as i32;
                            let ox = ((w - rw) - dx) / 2;
                            let oy = ((h - bh) - dy) / 2;
                            let _ = StretchBlt(hdc, ox, oy, dx, dy, mem, 0, 0, sx, sy, SRCCOPY);
                            let _ = SelectObject(mem, old);
                            let _ = DeleteDC(mem);
                        }
                    } else {
                        let txt: Vec<u16> = "Rendering…".encode_utf16().collect();
                        SetTextColor(hdc, COLORREF(0x808080));
                        SetBkMode(hdc, TRANSPARENT);
                        let _ = TextOutW(hdc, 10, 10, &txt);
                    }
                }
            }

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_DESTROY => { PostQuitMessage(0); LRESULT(0) }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    unsafe { let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); }

    let index_path = std::env::args().nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("index"));

    let index = tantivy::Index::open_in_dir(&index_path).ok();
    let mut state = AppState {
        index,
        results: Vec::new(),
        query_string: String::new(),
        query_type: QueryType::Standard,
        current_pdf: None,
        thumbnail_cache: Arc::new(Mutex::new(ThumbnailCache::new(100))),
    };
    STATE.store(&mut state, Ordering::Relaxed);

    let hmodule = unsafe { GetModuleHandleW(None) }.context("GetModuleHandle")?;
    let hinstance: HINSTANCE = hmodule.into();

    let cls = HSTRING::from("PdfViewerWindow");
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        hbrBackground: unsafe { HBRUSH(GetStockObject(WHITE_BRUSH).0) },
        lpszClassName: PCWSTR(cls.as_ptr()),
        ..Default::default()
    };

    if unsafe { RegisterClassW(&wc) } == 0 {
        anyhow::bail!("RegisterClassW failed");
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &cls,
            &HSTRING::from("PDF Viewer — Phase 9"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT, CW_USEDEFAULT,
            1200, 800,
            None, None, hinstance, None,
        )
    }.context("CreateWindowExW")?;

    HWND_STORE.store(hwnd.0 as isize, Ordering::Relaxed);

    unsafe {
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &HSTRING::from("EDIT"),
            &HSTRING::new(),
            WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(0x80),
            0, 0, 0, 0, hwnd, HMENU(ID_EDIT as _), hinstance, None,
        );

        let _ = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &HSTRING::from("BUTTON"),
            &HSTRING::from("Search"),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0),
            0, 0, 0, 0, hwnd, HMENU(ID_BUTTON as _), hinstance, None,
        );

        let combo = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &HSTRING::from("COMBOBOX"),
            &HSTRING::new(),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0x3) | WS_VSCROLL,
            0, 0, 0, 0, hwnd, HMENU(ID_COMBO as _), hinstance, None,
        ).ok().expect("Create combo");

        for qt in QueryType::variants() {
            let w: Vec<u16> = qt.label().encode_utf16().chain([0]).collect();
            let _ = SendMessageW(combo, CB_ADDSTRING, WPARAM(0), LPARAM(w.as_ptr() as isize));
        }
        let _ = SendMessageW(combo, CB_SETCURSEL, WPARAM(0), LPARAM(0));

        let _ = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &HSTRING::from("LISTBOX"),
            &HSTRING::new(),
            WS_CHILD | WS_VISIBLE | WS_VSCROLL | WINDOW_STYLE(0x80),
            0, 0, 0, 0, hwnd, HMENU(ID_LIST as _), hinstance, None,
        );
    }

    unsafe { let _ = ShowWindow(hwnd, SW_SHOW); }
    unsafe { let _ = UpdateWindow(hwnd); }

    let mut msg = MSG::default();
    unsafe { while GetMessageW(&mut msg, None, 0, 0).into() { let _ = TranslateMessage(&msg); DispatchMessageW(&msg); } }

    Ok(())
}
