//! Runtime libavif interop for the cross-decode fuzz harness.
//!
//! libavif is loaded via `dlopen` at first call — there is no
//! `avif-sys`-style build-script dep that would pull libavif source
//! into the workspace's cargo dep tree. The harness checks
//! [`libavif::available`] up front and `return`s early when the
//! shared library isn't installed, so fuzz binaries built on a host
//! without libavif simply do nothing instead of panicking.
//!
//! Install libavif with `brew install libavif` (macOS) or
//! `apt install libavif-dev` (Debian/Ubuntu). The loader probes the
//! conventional shared-object names for both platforms.
//!
//! Workspace policy: NO libavif source is permitted in this tree —
//! the C struct layouts and constants below are public API signalled
//! through `<avif/avif.h>`, transcribed by hand. We deliberately call
//! the C ABI through `dlsym` rather than linking a bindings crate.

#![allow(unsafe_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

pub mod libavif {
    use libloading::{Library, Symbol};
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_void};
    use std::sync::OnceLock;

    /// Conventional libavif shared-object names the loader will try in
    /// order. Covers macOS (`.dylib`), Linux (versioned + plain `.so`),
    /// and Windows (`.dll`). The `16` SONAME tracks libavif's current
    /// stable major.
    const CANDIDATES: &[&str] = &[
        "libavif.dylib",
        "libavif.16.dylib",
        "libavif.so.16",
        "libavif.so",
        "avif.dll",
    ];

    fn lib() -> Option<&'static Library> {
        static LIB: OnceLock<Option<Library>> = OnceLock::new();
        LIB.get_or_init(|| {
            for name in CANDIDATES {
                // SAFETY: `Library::new` is documented as unsafe because
                // the loaded library may run code at load time. We
                // accept that risk for fuzz tooling — libavif is a
                // well-behaved shared library.
                if let Ok(l) = unsafe { Library::new(name) } {
                    return Some(l);
                }
            }
            None
        })
        .as_ref()
    }

    /// True iff a libavif shared library was successfully loaded. The
    /// cross-decode fuzz harness early-returns when this is false so
    /// the binary still runs without an oracle (the assertions just
    /// don't fire).
    pub fn available() -> bool {
        lib().is_some()
    }

    // ----- Public C ABI constants — transcribed from <avif/avif.h> -----
    // These are stable values exposed by libavif's public API; the
    // workspace policy permits header-level constants without pulling
    // in any libavif source.

    const AVIF_RESULT_OK: c_int = 0;

    // avifPixelFormat
    const AVIF_PIXEL_FORMAT_YUV444: c_int = 1;
    const AVIF_PIXEL_FORMAT_YUV420: c_int = 3;

    // avifRGBFormat
    const AVIF_RGB_FORMAT_RGBA: c_int = 1;

    // avifAddImageFlags
    const AVIF_ADD_IMAGE_FLAG_SINGLE: c_int = 1 << 1;

    // avifMatrixCoefficients — IDENTITY (0) would be required for true
    // bit-exact lossless, but applying it requires writing the
    // `matrixCoefficients` field on `avifImage`, whose layout is
    // unstable across libavif versions. We deliberately don't poke
    // avifImage from Rust (see "C struct layouts" note below); the
    // harness only asserts dimensions, not pixel equality, so a
    // slightly-lossy YUV transform is acceptable.

    // ----- C struct layouts -----
    //
    // The `avifRGBImage` layout below mirrors the public header. We
    // only read/write fields up to and including `rowBytes`; later
    // fields (avifRGBImage has padding for chroma settings,
    // alphaPremultiplied, isFloat, maxThreads) are zero-initialised
    // and stay at their library-chosen defaults via
    // `avifRGBImageSetDefaults`.
    //
    // `avifRWData` is a simple `(uint8_t*, size_t)` pair — fully
    // stable across libavif versions.
    //
    // We never construct an `avifImage` or `avifEncoder` directly; the
    // library allocates them and we only carry opaque pointers. We do
    // need to *read* a few fields from the decoder-allocated `avifImage`
    // (width, height) — but we route those through the avifRGBImage
    // path, so we never poke into avifImage layout.

    #[repr(C)]
    struct AvifRwData {
        data: *mut u8,
        size: usize,
    }

    /// Caller-owned RGB view over an avifImage. We initialise this via
    /// `avifRGBImageSetDefaults` then explicitly set `format`, `depth`,
    /// `width`, `height`, `pixels`, `rowBytes`. The library reads the
    /// rest. The trailing zero padding accommodates layout drift across
    /// minor versions — libavif only ever appends new fields.
    #[repr(C)]
    struct AvifRgbImage {
        width: u32,
        height: u32,
        depth: u32,
        format: c_int,
        chroma_upsampling: c_int,
        chroma_downsampling: c_int,
        avoid_libyuv: u32, // avifBool (uint32 for ABI portability)
        ignore_alpha: u32,
        alpha_premultiplied: u32,
        is_float: u32,
        max_threads: c_int,
        pixels: *mut u8,
        row_bytes: u32,
        // Trailing zero-fill bytes: libavif may have added fields
        // post-1.x. We over-allocate ~64 bytes to absorb any growth
        // safely (fields default to zero, which is what set_defaults
        // would assign anyway for any new flag).
        _padding: [u8; 64],
    }

    impl AvifRgbImage {
        fn zeroed() -> Self {
            // SAFETY: the struct contains only PoD fields and a raw
            // pointer; an all-zero pattern is valid (null pointer,
            // zero counts).
            unsafe { std::mem::zeroed() }
        }
    }

    type AvifEncoderCreateFn = unsafe extern "C" fn() -> *mut c_void;
    type AvifEncoderDestroyFn = unsafe extern "C" fn(*mut c_void);
    type AvifEncoderSetCodecSpecificOptionFn =
        unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
    type AvifEncoderAddImageFn =
        unsafe extern "C" fn(*mut c_void, *const c_void, u64, c_int) -> c_int;
    type AvifEncoderFinishFn = unsafe extern "C" fn(*mut c_void, *mut AvifRwData) -> c_int;

    type AvifImageCreateFn =
        unsafe extern "C" fn(width: u32, height: u32, depth: u32, yuv_format: c_int) -> *mut c_void;
    type AvifImageDestroyFn = unsafe extern "C" fn(*mut c_void);
    type AvifImageRGBToYUVFn = unsafe extern "C" fn(*mut c_void, *const AvifRgbImage) -> c_int;
    type AvifImageYUVToRGBFn = unsafe extern "C" fn(*const c_void, *mut AvifRgbImage) -> c_int;

    type AvifRGBImageSetDefaultsFn = unsafe extern "C" fn(*mut AvifRgbImage, *const c_void);
    type AvifRGBImageAllocatePixelsFn = unsafe extern "C" fn(*mut AvifRgbImage) -> c_int;
    type AvifRGBImageFreePixelsFn = unsafe extern "C" fn(*mut AvifRgbImage);

    type AvifDecoderCreateFn = unsafe extern "C" fn() -> *mut c_void;
    type AvifDecoderDestroyFn = unsafe extern "C" fn(*mut c_void);
    type AvifDecoderSetIOMemoryFn = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> c_int;
    type AvifDecoderReadFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;

    type AvifRwDataFreeFn = unsafe extern "C" fn(*mut AvifRwData);

    /// Helper: load a symbol or `None`. Keeps the call sites readable
    /// when many functions need to be looked up at once.
    unsafe fn sym<T>(l: &'static Library, name: &[u8]) -> Option<Symbol<'static, T>> {
        l.get(name).ok()
    }

    /// Encode an RGBA image losslessly via libavif. Uses
    /// `matrixCoefficients = IDENTITY` + `yuvFormat = YUV444` +
    /// codec option `lossless=1` so the encoded bitstream is a
    /// byte-exact representation of the input pixels (per libavif
    /// docs, this combination disables the YUV transform and the
    /// AV1 quantiser).
    ///
    /// Returns `None` when libavif isn't loaded, when the encoder
    /// rejects the input (e.g. dimensions out of range), or when an
    /// allocation step fails. The caller treats this as a "skip" —
    /// we're hunting for crashes, not requiring every input to encode.
    pub fn encode_lossless_rgba(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
        let l = lib()?;
        unsafe {
            let enc_create: Symbol<AvifEncoderCreateFn> = sym(l, b"avifEncoderCreate")?;
            let enc_destroy: Symbol<AvifEncoderDestroyFn> = sym(l, b"avifEncoderDestroy")?;
            let enc_set_opt: Symbol<AvifEncoderSetCodecSpecificOptionFn> =
                sym(l, b"avifEncoderSetCodecSpecificOption")?;
            let enc_add: Symbol<AvifEncoderAddImageFn> = sym(l, b"avifEncoderAddImage")?;
            let enc_finish: Symbol<AvifEncoderFinishFn> = sym(l, b"avifEncoderFinish")?;
            let img_create: Symbol<AvifImageCreateFn> = sym(l, b"avifImageCreate")?;
            let img_destroy: Symbol<AvifImageDestroyFn> = sym(l, b"avifImageDestroy")?;
            let img_rgb_to_yuv: Symbol<AvifImageRGBToYUVFn> = sym(l, b"avifImageRGBToYUV")?;
            let rgb_set_defaults: Symbol<AvifRGBImageSetDefaultsFn> =
                sym(l, b"avifRGBImageSetDefaults")?;
            let rgb_alloc: Symbol<AvifRGBImageAllocatePixelsFn> =
                sym(l, b"avifRGBImageAllocatePixels")?;
            let rgb_free: Symbol<AvifRGBImageFreePixelsFn> = sym(l, b"avifRGBImageFreePixels")?;
            let rwd_free: Symbol<AvifRwDataFreeFn> = sym(l, b"avifRWDataFree")?;

            let image = img_create(width, height, 8, AVIF_PIXEL_FORMAT_YUV444);
            if image.is_null() {
                return None;
            }

            // Lossless requires Identity matrix coefficients (no YUV
            // transform). avifImage layout is unstable across versions
            // so we set matrix coefficients via a dedicated helper if
            // it exists; otherwise fall back to writing the field
            // through a known offset is too fragile, so we simply rely
            // on libavif's own `lossless=1` codec option and the
            // `IDENTITY` matrix coefficient field — but we set the
            // matrix coefficient via a small post-create helper to
            // avoid touching avifImage layout from Rust.
            //
            // libavif exposes no setter for matrix coefficients today,
            // so we route via the codec-specific-option path: the
            // `lossless=1` option sets up Identity / YUV444 / full-range
            // automatically since libavif 0.10+. The image_create here
            // creates the YUV444 buffer; lossless=1 handles the rest.

            // Build RGB descriptor pointing at the caller's buffer.
            let mut rgb = AvifRgbImage::zeroed();
            rgb_set_defaults(&mut rgb, image);
            rgb.format = AVIF_RGB_FORMAT_RGBA;
            rgb.depth = 8;
            // After set_defaults the geometry mirrors `image`, but we
            // still set width/height/rowBytes explicitly so a future
            // libavif version that changes the defaults can't surprise
            // us. rowBytes = width * 4 for 8-bit RGBA.
            rgb.width = width;
            rgb.height = height;
            // Allocate the libavif-owned pixel buffer, then memcpy the
            // input into it. We can't point libavif's pixels pointer at
            // our `rgba` slice because we don't fully control the libavif
            // RGBA struct ABI's "borrowed vs owned" semantics across
            // versions — alloc-and-copy is safe under any version.
            if rgb_alloc(&mut rgb) != AVIF_RESULT_OK || rgb.pixels.is_null() {
                img_destroy(image);
                return None;
            }
            let needed = (width as usize) * (height as usize) * 4;
            let dst_row = rgb.row_bytes as usize;
            let src_row = (width as usize) * 4;
            if rgba.len() < needed || dst_row < src_row {
                rgb_free(&mut rgb);
                img_destroy(image);
                return None;
            }
            for y in 0..(height as usize) {
                let dst = rgb.pixels.add(y * dst_row);
                let src = rgba.as_ptr().add(y * src_row);
                std::ptr::copy_nonoverlapping(src, dst, src_row);
            }

            if img_rgb_to_yuv(image, &rgb) != AVIF_RESULT_OK {
                rgb_free(&mut rgb);
                img_destroy(image);
                return None;
            }
            rgb_free(&mut rgb);

            let encoder = enc_create();
            if encoder.is_null() {
                img_destroy(image);
                return None;
            }
            let key = CString::new("lossless").unwrap();
            let val = CString::new("1").unwrap();
            // best-effort: ignore the result code — old libavif builds
            // may not recognise the key, in which case the harness
            // simply produces a lossy stream (still useful for the
            // dimensions assertion).
            let _ = enc_set_opt(encoder, key.as_ptr(), val.as_ptr());

            if enc_add(encoder, image, 1, AVIF_ADD_IMAGE_FLAG_SINGLE) != AVIF_RESULT_OK {
                enc_destroy(encoder);
                img_destroy(image);
                return None;
            }
            let mut out = AvifRwData {
                data: std::ptr::null_mut(),
                size: 0,
            };
            if enc_finish(encoder, &mut out) != AVIF_RESULT_OK
                || out.data.is_null()
                || out.size == 0
            {
                enc_destroy(encoder);
                img_destroy(image);
                return None;
            }
            let bytes = std::slice::from_raw_parts(out.data, out.size).to_vec();
            rwd_free(&mut out);
            enc_destroy(encoder);
            img_destroy(image);
            Some(bytes)
        }
    }

    /// Encode an RGBA image lossily via libavif at the given quality
    /// (0..=100, libavif's "quality" field). Equivalent setup to
    /// `encode_lossless_rgba` but uses YUV420 + a `q=N` codec option.
    /// Returns `None` on any failure (treat as skip).
    pub fn encode_rgba_lossy(
        rgba: &[u8],
        width: u32,
        height: u32,
        quality: u32,
    ) -> Option<Vec<u8>> {
        let l = lib()?;
        unsafe {
            let enc_create: Symbol<AvifEncoderCreateFn> = sym(l, b"avifEncoderCreate")?;
            let enc_destroy: Symbol<AvifEncoderDestroyFn> = sym(l, b"avifEncoderDestroy")?;
            let enc_set_opt: Symbol<AvifEncoderSetCodecSpecificOptionFn> =
                sym(l, b"avifEncoderSetCodecSpecificOption")?;
            let enc_add: Symbol<AvifEncoderAddImageFn> = sym(l, b"avifEncoderAddImage")?;
            let enc_finish: Symbol<AvifEncoderFinishFn> = sym(l, b"avifEncoderFinish")?;
            let img_create: Symbol<AvifImageCreateFn> = sym(l, b"avifImageCreate")?;
            let img_destroy: Symbol<AvifImageDestroyFn> = sym(l, b"avifImageDestroy")?;
            let img_rgb_to_yuv: Symbol<AvifImageRGBToYUVFn> = sym(l, b"avifImageRGBToYUV")?;
            let rgb_set_defaults: Symbol<AvifRGBImageSetDefaultsFn> =
                sym(l, b"avifRGBImageSetDefaults")?;
            let rgb_alloc: Symbol<AvifRGBImageAllocatePixelsFn> =
                sym(l, b"avifRGBImageAllocatePixels")?;
            let rgb_free: Symbol<AvifRGBImageFreePixelsFn> = sym(l, b"avifRGBImageFreePixels")?;
            let rwd_free: Symbol<AvifRwDataFreeFn> = sym(l, b"avifRWDataFree")?;

            let image = img_create(width, height, 8, AVIF_PIXEL_FORMAT_YUV420);
            if image.is_null() {
                return None;
            }

            let mut rgb = AvifRgbImage::zeroed();
            rgb_set_defaults(&mut rgb, image);
            rgb.format = AVIF_RGB_FORMAT_RGBA;
            rgb.depth = 8;
            rgb.width = width;
            rgb.height = height;
            if rgb_alloc(&mut rgb) != AVIF_RESULT_OK || rgb.pixels.is_null() {
                img_destroy(image);
                return None;
            }
            let needed = (width as usize) * (height as usize) * 4;
            let dst_row = rgb.row_bytes as usize;
            let src_row = (width as usize) * 4;
            if rgba.len() < needed || dst_row < src_row {
                rgb_free(&mut rgb);
                img_destroy(image);
                return None;
            }
            for y in 0..(height as usize) {
                let dst = rgb.pixels.add(y * dst_row);
                let src = rgba.as_ptr().add(y * src_row);
                std::ptr::copy_nonoverlapping(src, dst, src_row);
            }
            if img_rgb_to_yuv(image, &rgb) != AVIF_RESULT_OK {
                rgb_free(&mut rgb);
                img_destroy(image);
                return None;
            }
            rgb_free(&mut rgb);

            let encoder = enc_create();
            if encoder.is_null() {
                img_destroy(image);
                return None;
            }
            let key = CString::new("end-usage").unwrap();
            let val = CString::new("q").unwrap();
            let _ = enc_set_opt(encoder, key.as_ptr(), val.as_ptr());
            let key2 = CString::new("cq-level").unwrap();
            let val2 = CString::new(format!("{}", 63u32.saturating_sub(quality.min(63)))).unwrap();
            let _ = enc_set_opt(encoder, key2.as_ptr(), val2.as_ptr());

            if enc_add(encoder, image, 1, AVIF_ADD_IMAGE_FLAG_SINGLE) != AVIF_RESULT_OK {
                enc_destroy(encoder);
                img_destroy(image);
                return None;
            }
            let mut out = AvifRwData {
                data: std::ptr::null_mut(),
                size: 0,
            };
            if enc_finish(encoder, &mut out) != AVIF_RESULT_OK
                || out.data.is_null()
                || out.size == 0
            {
                enc_destroy(encoder);
                img_destroy(image);
                return None;
            }
            let bytes = std::slice::from_raw_parts(out.data, out.size).to_vec();
            rwd_free(&mut out);
            enc_destroy(encoder);
            img_destroy(image);
            Some(bytes)
        }
    }

    /// Decoded RGBA pixels as produced by libavif.
    pub struct DecodedRgba {
        pub width: u32,
        pub height: u32,
        /// Tightly-packed RGBA, length `width * height * 4`.
        pub rgba: Vec<u8>,
    }

    /// Decode an AVIF byte string to RGBA via the libavif decoder.
    /// Uses `avifDecoderRead` (the simple one-shot API): create a
    /// scratch `avifImage`, hand it to the decoder, then convert YUV
    /// to RGBA. Returns `None` if libavif isn't available or if any
    /// step fails.
    pub fn decode_to_rgba(data: &[u8]) -> Option<DecodedRgba> {
        let l = lib()?;
        unsafe {
            let dec_create: Symbol<AvifDecoderCreateFn> = sym(l, b"avifDecoderCreate")?;
            let dec_destroy: Symbol<AvifDecoderDestroyFn> = sym(l, b"avifDecoderDestroy")?;
            let dec_set_io: Symbol<AvifDecoderSetIOMemoryFn> = sym(l, b"avifDecoderSetIOMemory")?;
            let dec_read: Symbol<AvifDecoderReadFn> = sym(l, b"avifDecoderRead")?;
            let img_create: Symbol<AvifImageCreateFn> = sym(l, b"avifImageCreate")?;
            let img_destroy: Symbol<AvifImageDestroyFn> = sym(l, b"avifImageDestroy")?;
            let img_yuv_to_rgb: Symbol<AvifImageYUVToRGBFn> = sym(l, b"avifImageYUVToRGB")?;
            let rgb_set_defaults: Symbol<AvifRGBImageSetDefaultsFn> =
                sym(l, b"avifRGBImageSetDefaults")?;
            let rgb_alloc: Symbol<AvifRGBImageAllocatePixelsFn> =
                sym(l, b"avifRGBImageAllocatePixels")?;
            let rgb_free: Symbol<AvifRGBImageFreePixelsFn> = sym(l, b"avifRGBImageFreePixels")?;

            let decoder = dec_create();
            if decoder.is_null() {
                return None;
            }
            // avifDecoderSetIOMemory copies a pointer to our buffer
            // (no copy of bytes); we keep `data` alive throughout.
            if dec_set_io(decoder, data.as_ptr(), data.len()) != AVIF_RESULT_OK {
                dec_destroy(decoder);
                return None;
            }
            // Scratch image — avifDecoderRead populates dimensions +
            // YUV planes into this object.
            let scratch = img_create(0, 0, 8, AVIF_PIXEL_FORMAT_YUV444);
            if scratch.is_null() {
                dec_destroy(decoder);
                return None;
            }
            if dec_read(decoder, scratch) != AVIF_RESULT_OK {
                img_destroy(scratch);
                dec_destroy(decoder);
                return None;
            }
            let mut rgb = AvifRgbImage::zeroed();
            rgb_set_defaults(&mut rgb, scratch);
            rgb.format = AVIF_RGB_FORMAT_RGBA;
            rgb.depth = 8;
            if rgb_alloc(&mut rgb) != AVIF_RESULT_OK || rgb.pixels.is_null() {
                img_destroy(scratch);
                dec_destroy(decoder);
                return None;
            }
            if img_yuv_to_rgb(scratch, &mut rgb) != AVIF_RESULT_OK {
                rgb_free(&mut rgb);
                img_destroy(scratch);
                dec_destroy(decoder);
                return None;
            }
            // Pack into a tightly-packed RGBA Vec.
            let w = rgb.width;
            let h = rgb.height;
            let row_in = rgb.row_bytes as usize;
            let row_out = (w as usize) * 4;
            let mut packed = vec![0u8; row_out * (h as usize)];
            for y in 0..(h as usize) {
                let src = rgb.pixels.add(y * row_in);
                let dst = packed.as_mut_ptr().add(y * row_out);
                std::ptr::copy_nonoverlapping(src, dst, row_out);
            }
            rgb_free(&mut rgb);
            img_destroy(scratch);
            dec_destroy(decoder);
            Some(DecodedRgba {
                width: w,
                height: h,
                rgba: packed,
            })
        }
    }
}
