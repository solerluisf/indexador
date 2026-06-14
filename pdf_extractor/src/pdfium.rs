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
/// - `page_height`: effective page height (points).
pub fn flip_y(pdf_y: f64, page_height: f64) -> f64 {
    page_height - pdf_y
}

/// Describes the geometry of a single PDF page for coordinate transforms.
///
/// All dimensions are in the UNROTATED PDF user space (points, 1/72 inch).
/// Rotation transforms are applied via the `stored_to_render_*` methods.
#[derive(Debug, Clone, Copy)]
pub struct PageGeometry {
    /// MediaBox width (unrotated).
    pub media_width: f64,
    /// MediaBox height (unrotated).
    pub media_height: f64,
    /// Page rotation: 0/1/2/3 → 0°/90°/180°/270° clockwise.
    pub rotation: i32,
    /// CropBox bounding rectangle, if present.
    pub crop_rect: Option<FS_RECTF>,
}

impl PageGeometry {
    /// Construct page geometry from a loaded PDFium page handle.
    ///
    /// # Safety
    /// `page` must be a valid, non-null PDFium page handle.
    pub unsafe fn from_page(pdfium: &Pdfium, page: *mut c_void) -> Self {
        let media_width = (pdfium.FPDF_GetPageWidthF)(page) as f64;
        let media_height = (pdfium.FPDF_GetPageHeightF)(page) as f64;
        let rotation = pdfium.FPDF_GetPageRotation.map_or(0, |f| unsafe { f(page) });
        let mut crop_rect = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
        let crop_valid = pdfium.FPDF_GetPageBoundingBox
            .map_or(false, |f| unsafe { f(page, &mut crop_rect) != 0 });
        Self {
            media_width,
            media_height,
            rotation,
            crop_rect: if crop_valid { Some(crop_rect) } else { None },
        }
    }

    /// Unrotated page height — uses CropBox height if available, else MediaBox height.
    pub fn unrotated_height(&self) -> f64 {
        match self.crop_rect {
            Some(cr) => (cr.bottom - cr.top).abs() as f64,
            None => self.media_height,
        }
    }

    /// Unrotated page width — uses CropBox width if available, else MediaBox width.
    pub fn unrotated_width(&self) -> f64 {
        match self.crop_rect {
            Some(cr) => (cr.right - cr.left).abs() as f64,
            None => self.media_width,
        }
    }

    /// Rendered bitmap dimensions, accounting for rotation.
    /// For 90°/270° the rendered width is the unrotated height and vice versa.
    pub fn render_size(&self) -> (f64, f64) {
        let w = self.unrotated_width();
        let h = self.unrotated_height();
        match self.rotation {
            1 | 3 => (h, w),
            _ => (w, h),
        }
    }

    /// Convert a PDF user-space point (bottom-left, unrotated) to stored
    /// coordinates (top-left, unrotated) — this is what extractor stores.
    pub fn pdf_to_stored(&self, x: f64, y: f64) -> (f64, f64) {
        (x, self.unrotated_height() - y)
    }

    /// Convert stored coordinates (top-left, unrotated) back to PDF user space
    /// (bottom-left, unrotated).
    pub fn stored_to_pdf(&self, x: f64, y: f64) -> (f64, f64) {
        (x, self.unrotated_height() - y)
    }

    /// Convert a stored (top-left, unrotated) point to rendered bitmap pixel
    /// coordinates (top-left, rotated), given a scale factor (dpi / 72).
    pub fn stored_to_render(&self, x: f64, y: f64) -> (f64, f64) {
        // Step 1: stored (top-left, unrotated) → PDF (bottom-left, unrotated)
        let (pdf_x, pdf_y) = self.stored_to_pdf(x, y);
        // Step 2: PDF (bottom-left, unrotated) → render (top-left, rotated)
        let u_w = self.unrotated_width();
        let u_h = self.unrotated_height();
        match self.rotation {
            1 => (pdf_y, u_w - pdf_x),      //  90°
            2 => (u_w - pdf_x, u_h - pdf_y),// 180°
            3 => (u_h - pdf_y, pdf_x),      // 270°
            _ => (pdf_x, self.unrotated_height() - pdf_y), // 0°: flip to top-left
        }
    }

    /// Convert a stored bounding box (top-left, unrotated) to rendered bitmap
    /// pixel coordinates. Returns (render_x_min, render_y_min, render_x_max, render_y_max).
    pub fn bbox_stored_to_render(
        &self,
        x_min: f64, y_min: f64,
        x_max: f64, y_max: f64,
    ) -> (f64, f64, f64, f64) {
        let (r_x1, r_y1) = self.stored_to_render(x_min, y_min);
        let (r_x2, r_y2) = self.stored_to_render(x_max, y_max);
        (
            r_x1.min(r_x2),
            r_y1.min(r_y2),
            r_x1.max(r_x2),
            r_y1.max(r_y2),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_geometry_no_rotation_no_crop() {
        let g = PageGeometry {
            media_width: 612.0,
            media_height: 792.0,
            rotation: 0,
            crop_rect: None,
        };
        assert_eq!(g.unrotated_width(), 612.0);
        assert_eq!(g.unrotated_height(), 792.0);
        let (rw, rh) = g.render_size();
        assert_eq!(rw, 612.0);
        assert_eq!(rh, 792.0);

        // Stored → render for rotation 0: identity (both are top-left, unrotated)
        let (rx, ry) = g.stored_to_render(100.0, 100.0);
        assert!((rx - 100.0).abs() < 0.001);
        assert!((ry - 100.0).abs() < 0.001);

        // bbox round-trip
        let (x1, y1, x2, y2) = g.bbox_stored_to_render(100.0, 80.0, 200.0, 92.0);
        assert!((x1 - 100.0).abs() < 0.001);
        assert!((y1 - 80.0).abs() < 0.001);
        assert!((x2 - 200.0).abs() < 0.001);
        assert!((y2 - 92.0).abs() < 0.001);
    }

    #[test]
    fn test_page_geometry_rotation_90() {
        // Page with MediaBox [0 0 612 792], /Rotate 90
        let g = PageGeometry {
            media_width: 612.0,
            media_height: 792.0,
            rotation: 1,
            crop_rect: None,
        };
        assert_eq!(g.unrotated_width(), 612.0);
        assert_eq!(g.unrotated_height(), 792.0);
        let (rw, rh) = g.render_size();
        assert_eq!(rw, 792.0); // swapped
        assert_eq!(rh, 612.0); // swapped

        // PDF coord (100, 700) → stored (top-left, unrotated): flip Y
        // stored_y = 792 - 700 = 92
        // → render: (pdf_y, u_w - pdf_x) = (700, 612 - 100) = (700, 512)
        let (rx, ry) = g.stored_to_render(100.0, 92.0);
        assert!((rx - 700.0).abs() < 0.001);
        assert!((ry - 512.0).abs() < 0.001);
    }

    #[test]
    fn test_page_geometry_rotation_270() {
        let g = PageGeometry {
            media_width: 612.0,
            media_height: 792.0,
            rotation: 3,
            crop_rect: None,
        };
        let (rw, rh) = g.render_size();
        assert_eq!(rw, 792.0);
        assert_eq!(rh, 612.0);

        // PDF coord (100, 700) → stored: (100, 792-700=92)
        // → render: (u_h - pdf_y, pdf_x) = (792 - 700, 100) = (92, 100)
        let (rx, ry) = g.stored_to_render(100.0, 92.0);
        assert!((rx - 92.0).abs() < 0.001);
        assert!((ry - 100.0).abs() < 0.001);
    }

    #[test]
    fn test_page_geometry_rotation_180() {
        let g = PageGeometry {
            media_width: 612.0,
            media_height: 792.0,
            rotation: 2,
            crop_rect: None,
        };
        // PDF coord (100, 700) → stored: (100, 792-700=92)
        // → render: (u_w - pdf_x, u_h - pdf_y) = (612-100, 792-700) = (512, 92)
        let (rx, ry) = g.stored_to_render(100.0, 92.0);
        assert!((rx - 512.0).abs() < 0.001);
        assert!((ry - 92.0).abs() < 0.001);
    }

    #[test]
    fn test_page_geometry_with_crop() {
        let g = PageGeometry {
            media_width: 612.0,
            media_height: 792.0,
            rotation: 0,
            crop_rect: Some(FS_RECTF { left: 50.0, top: 50.0, right: 562.0, bottom: 742.0 }),
        };
        assert!((g.unrotated_height() - 692.0).abs() < 0.001); // 742 - 50
        assert!((g.unrotated_width() - 512.0).abs() < 0.001);  // 562 - 50
    }

    #[test]
    fn test_bbox_rotation_90() {
        let g = PageGeometry {
            media_width: 612.0,
            media_height: 792.0,
            rotation: 1,
            crop_rect: None,
        };
        // PDF bbox: x=[100,200], y=[700,712]
        // stored (top-left, unrotated): x=[100,200], y=[792-712, 792-700] = [80, 92]
        // render 90°: x' = pdf_y, y' = u_w - pdf_x
        //   (stored_x=100, stored_y=80) → pdf: (100, 792-80=712) → render: (712, 612-100=512)
        //   (stored_x=200, stored_y=92) → pdf: (200, 792-92=700) → render: (700, 612-200=412)
        let (x1, y1, x2, y2) = g.bbox_stored_to_render(100.0, 80.0, 200.0, 92.0);
        assert!((x1 - 700.0).abs() < 0.001); // min of 712, 700
        assert!((y1 - 412.0).abs() < 0.001); // min of 412, 512
        assert!((x2 - 712.0).abs() < 0.001); // max of 712, 700
        assert!((y2 - 512.0).abs() < 0.001); // max of 412, 512
    }
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
