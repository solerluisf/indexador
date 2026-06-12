use anyhow::{Context, Result};
use libloading::Library;
use std::ffi::c_void;
use std::path::Path;
use std::sync::Mutex;

macro_rules! load_fn {
    ($lib:expr, $name:ident, $ty:ty) => {
        unsafe {
            *$lib
                .get(stringify!($name).as_bytes())
                .map_err(|e| anyhow::anyhow!("Failed to find pdfium symbol '{}': {}", stringify!($name), e))?
        }
    };
}

/// Load an optional PDFium symbol — returns `None` if the DLL doesn't export it.
macro_rules! load_fn_opt {
    ($lib:expr, $name:ident, $ty:ty) => {
        unsafe {
            $lib.get::<$ty>(stringify!($name).as_bytes())
                .ok()
                .map(|sym| *sym)
        }
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FS_RECTF {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

pub struct Pdfium {
    #[allow(dead_code)]
    lib: Library,

    // Library lifecycle
    pub FPDF_InitLibrary: unsafe extern "C" fn(),

    // Document
    // On Windows, FPDF_LoadDocument takes a UTF-16 (wide) string path.
    // The DLL exports the same C name regardless of platform.
    pub FPDF_LoadDocument:
        unsafe extern "C" fn(path: *const u16, password: *const u8) -> *mut c_void,
    pub FPDF_LoadMemDocument:
        unsafe extern "C" fn(data_buf: *const u8, size: i32, password: *const u8) -> *mut c_void,
    pub FPDF_CloseDocument: unsafe extern "C" fn(doc: *mut c_void),
    pub FPDF_GetPageCount: unsafe extern "C" fn(doc: *mut c_void) -> i32,

    // Page
    pub FPDF_LoadPage: unsafe extern "C" fn(doc: *mut c_void, page_index: i32) -> *mut c_void,
    pub FPDF_ClosePage: unsafe extern "C" fn(page: *mut c_void),
    pub FPDF_GetPageWidthF: unsafe extern "C" fn(page: *mut c_void) -> f32,
    pub FPDF_GetPageHeightF: unsafe extern "C" fn(page: *mut c_void) -> f32,
    pub FPDF_GetPageRotation: Option<unsafe extern "C" fn(page: *mut c_void) -> i32>,
    pub FPDF_GetPageBoundingBox: Option<unsafe extern "C" fn(page: *mut c_void, rect: *mut FS_RECTF) -> i32>,

    // Text
    pub FPDFText_LoadPage: unsafe extern "C" fn(page: *mut c_void) -> *mut c_void,
    pub FPDFText_ClosePage: unsafe extern "C" fn(text_page: *mut c_void),
    pub FPDFText_CountChars: unsafe extern "C" fn(text_page: *mut c_void) -> i32,
    pub FPDFText_GetUnicode: unsafe extern "C" fn(text_page: *mut c_void, index: i32) -> u32,
    pub FPDFText_GetCharBox:
        unsafe extern "C" fn(text_page: *mut c_void, index: i32, left: *mut f64, right: *mut f64, bottom: *mut f64, top: *mut f64) -> i32,

    // Text search
    pub FPDFText_FindStart:
        unsafe extern "C" fn(text_page: *mut c_void, term: *const u16, flags: u32, start_index: i32) -> *mut c_void,
    pub FPDFText_FindNext: unsafe extern "C" fn(handle: *mut c_void) -> i32,
    pub FPDFText_FindClose: unsafe extern "C" fn(handle: *mut c_void),
    pub FPDFText_GetSchResultIndex: unsafe extern "C" fn(handle: *mut c_void) -> i32,
    pub FPDFText_GetSchCount: unsafe extern "C" fn(handle: *mut c_void) -> i32,

    // Bitmap
    pub FPDFBitmap_CreateEx:
        unsafe extern "C" fn(width: i32, height: i32, format: i32, first_scan: *mut c_void, stride: i32) -> *mut c_void,
    pub FPDFBitmap_Destroy: unsafe extern "C" fn(bitmap: *mut c_void),
    pub FPDFBitmap_FillRect:
        unsafe extern "C" fn(bitmap: *mut c_void, left: i32, top: i32, width: i32, height: i32, color: u32),
    pub FPDFBitmap_GetBuffer: unsafe extern "C" fn(bitmap: *mut c_void) -> *mut u8,
    pub FPDFBitmap_GetStride: unsafe extern "C" fn(bitmap: *mut c_void) -> i32,

    // Render
    pub FPDF_RenderPageBitmap:
        unsafe extern "C" fn(bitmap: *mut c_void, page: *mut c_void, start_x: i32, start_y: i32, dest_width: i32, dest_height: i32, rotate: i32, flags: i32),

    // Error
    pub FPDF_GetLastError: unsafe extern "C" fn() -> u32,
}

static PDFIUM: Mutex<Option<&'static Pdfium>> = Mutex::new(None);

impl Pdfium {
    /// Try to open pdfium.dll, first by bare name (standard search),
    /// then by absolute path relative to the current executable.
    fn open_pdfium() -> Result<Library> {
        unsafe {
            match Library::new("pdfium.dll") {
                Ok(lib) => return Ok(lib),
                Err(_) => {}
            }
            let exe = std::env::current_exe().ok().and_then(|p| {
                p.parent().map(|d| d.join("pdfium.dll"))
            });
            if let Some(path) = exe {
                if let Ok(lib) = Library::new(path.as_os_str()) {
                    return Ok(lib);
                }
            }
        }
        Err(anyhow::anyhow!("pdfium.dll not found"))
    }

    /// Get the global PDFium instance, loading pdfium.dll on first call.
    /// Returns `None` if the DLL cannot be loaded.
    /// Errors are **not** cached — if the DLL is missing on first call,
    /// subsequent calls will retry loading.
    pub fn global() -> Option<&'static Self> {
        Self::init().ok()
    }

    /// Force initialization — returns an error if pdfium.dll cannot be loaded.
    /// Errors are **not** cached — if the DLL is missing on first call,
    /// subsequent calls will retry loading.
    pub fn init() -> Result<&'static Self> {
        let mut guard = PDFIUM.lock().unwrap();
        if let Some(pdfium) = guard.as_ref() {
            return Ok(pdfium);
        }
        match unsafe { Self::load_inner() } {
            Ok(pdfium) => {
                let leaked: &'static Self = Box::leak(Box::new(pdfium));
                *guard = Some(leaked);
                Ok(leaked)
            }
            Err(e) => Err(e),
        }
    }

    /// Returns true if pdfium.dll was loaded successfully.
    pub fn is_available() -> bool {
        PDFIUM.lock().unwrap().is_some()
    }

    /// Reset the cached PDFium instance so the next call to `global()` or
    /// `init()` will reload the DLL.  The old instance remains usable until
    /// all references to it are dropped, but new callers will attempt a fresh
    /// load.
    pub fn reset() {
        *PDFIUM.lock().unwrap() = None;
    }

    unsafe fn load_inner() -> Result<Self> {
        let lib = Self::open_pdfium()
            .context("Failed to load pdfium.dll — ensure pdfium.dll is in the application directory or PATH")?;

        let init_fn: unsafe extern "C" fn() = load_fn!(lib, FPDF_InitLibrary, unsafe extern "C" fn());
        // PDFium requires InitLibrary before any other API calls
        unsafe { init_fn() };

        Ok(Self {
            FPDF_InitLibrary: init_fn,
            FPDF_LoadDocument: load_fn!(lib, FPDF_LoadDocument, unsafe extern "C" fn(*const u16, *const u8) -> *mut c_void),
            FPDF_LoadMemDocument: load_fn!(lib, FPDF_LoadMemDocument, unsafe extern "C" fn(*const u8, i32, *const u8) -> *mut c_void),
            FPDF_CloseDocument: load_fn!(lib, FPDF_CloseDocument, unsafe extern "C" fn(*mut c_void)),
            FPDF_GetPageCount: load_fn!(lib, FPDF_GetPageCount, unsafe extern "C" fn(*mut c_void) -> i32),

            FPDF_LoadPage: load_fn!(lib, FPDF_LoadPage, unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void),
            FPDF_ClosePage: load_fn!(lib, FPDF_ClosePage, unsafe extern "C" fn(*mut c_void)),
            FPDF_GetPageWidthF: load_fn!(lib, FPDF_GetPageWidthF, unsafe extern "C" fn(*mut c_void) -> f32),
            FPDF_GetPageHeightF: load_fn!(lib, FPDF_GetPageHeightF, unsafe extern "C" fn(*mut c_void) -> f32),
            FPDF_GetPageRotation: load_fn_opt!(lib, FPDF_GetPageRotation, unsafe extern "C" fn(*mut c_void) -> i32),
            FPDF_GetPageBoundingBox: load_fn_opt!(lib, FPDF_GetPageBoundingBox, unsafe extern "C" fn(*mut c_void, *mut FS_RECTF) -> i32),

            FPDFText_LoadPage: load_fn!(lib, FPDFText_LoadPage, unsafe extern "C" fn(*mut c_void) -> *mut c_void),
            FPDFText_ClosePage: load_fn!(lib, FPDFText_ClosePage, unsafe extern "C" fn(*mut c_void)),
            FPDFText_CountChars: load_fn!(lib, FPDFText_CountChars, unsafe extern "C" fn(*mut c_void) -> i32),
            FPDFText_GetUnicode: load_fn!(lib, FPDFText_GetUnicode, unsafe extern "C" fn(*mut c_void, i32) -> u32),
            FPDFText_GetCharBox: load_fn!(lib, FPDFText_GetCharBox, unsafe extern "C" fn(*mut c_void, i32, *mut f64, *mut f64, *mut f64, *mut f64) -> i32),

            FPDFText_FindStart: load_fn!(lib, FPDFText_FindStart, unsafe extern "C" fn(*mut c_void, *const u16, u32, i32) -> *mut c_void),
            FPDFText_FindNext: load_fn!(lib, FPDFText_FindNext, unsafe extern "C" fn(*mut c_void) -> i32),
            FPDFText_FindClose: load_fn!(lib, FPDFText_FindClose, unsafe extern "C" fn(*mut c_void)),
            FPDFText_GetSchResultIndex: load_fn!(lib, FPDFText_GetSchResultIndex, unsafe extern "C" fn(*mut c_void) -> i32),
            FPDFText_GetSchCount: load_fn!(lib, FPDFText_GetSchCount, unsafe extern "C" fn(*mut c_void) -> i32),

            FPDFBitmap_CreateEx: load_fn!(lib, FPDFBitmap_CreateEx, unsafe extern "C" fn(i32, i32, i32, *mut c_void, i32) -> *mut c_void),
            FPDFBitmap_Destroy: load_fn!(lib, FPDFBitmap_Destroy, unsafe extern "C" fn(*mut c_void)),
            FPDFBitmap_FillRect: load_fn!(lib, FPDFBitmap_FillRect, unsafe extern "C" fn(*mut c_void, i32, i32, i32, i32, u32)),
            FPDFBitmap_GetBuffer: load_fn!(lib, FPDFBitmap_GetBuffer, unsafe extern "C" fn(*mut c_void) -> *mut u8),
            FPDFBitmap_GetStride: load_fn!(lib, FPDFBitmap_GetStride, unsafe extern "C" fn(*mut c_void) -> i32),

            FPDF_RenderPageBitmap: load_fn!(lib, FPDF_RenderPageBitmap, unsafe extern "C" fn(*mut c_void, *mut c_void, i32, i32, i32, i32, i32, i32)),

            FPDF_GetLastError: load_fn!(lib, FPDF_GetLastError, unsafe extern "C" fn() -> u32),

            lib,
        })
    }
}

/// Error codes from FPDF_GetLastError.
pub const FPDF_ERR_SUCCESS: u32 = 0;
pub const FPDF_ERR_UNKNOWN: u32 = 1;
pub const FPDF_ERR_FILE: u32 = 2;
pub const FPDF_ERR_FORMAT: u32 = 3;
pub const FPDF_ERR_PASSWORD: u32 = 4;
pub const FPDF_ERR_SECURITY: u32 = 5;
pub const FPDF_ERR_PAGE: u32 = 6;

/// Bitmap format constants
pub const FPDFBITMAP_BGRA: i32 = 4;

/// Render flags
pub const FPDF_NONE: i32 = 0;
pub const FPDF_ANNOT: i32 = 0x02;
pub const FPDF_LCD_TEXT: i32 = 0x04;
pub const FPDF_GRAYSCALE: i32 = 0x10;
pub const FPDF_PRINTING: i32 = 0x200;

/// Coordinate transform: Y-flip PDF user-space (bottom-left) → bitmap (top-left).
/// - `pdf_y`: Y in PDF user space (origin bottom-left, points).
/// - `page_height`: effective page height (points) — caller should pass the
///   correct value considering CropBox/MediaBox and rotation swap.
/// - `rotation`: `FPDF_GetPageRotation` result (0/1/2/3 → 0°/90°/180°/270°).
pub fn flip_y(pdf_y: f64, page_height: f64, _rotation: i32) -> f64 {
    page_height - pdf_y
}

// ---------------------------------------------------------------------------
// Helper: convert a filesystem path to a null-terminated UTF-16 wide string
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub fn path_to_utf16(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Helper: PDFium error code to human-readable string
// ---------------------------------------------------------------------------

pub fn error_str(code: u32) -> &'static str {
    match code {
        FPDF_ERR_SUCCESS => "Success",
        FPDF_ERR_UNKNOWN => "Unknown error",
        FPDF_ERR_FILE => "File not found or cannot be opened",
        FPDF_ERR_FORMAT => "File is not a PDF or is corrupted",
        FPDF_ERR_PASSWORD => "Incorrect password",
        FPDF_ERR_SECURITY => "Unsupported security scheme",
        FPDF_ERR_PAGE => "Page does not exist or content error",
        _ => "Unrecognized PDFium error",
    }
}
