// SPDX-License-Identifier: GPL-3.0-or-later
//! Application icons: `NSWorkspace iconForFile:` drawn into a
//! [`ICON_PIXELS`]-square bitmap and encoded as PNG, cached on disk under
//! `~/Library/Caches/cosmicMac/launcher-icons/`.
//!
//! Rendering happens off the UI thread, only for bundles that are on screen.
//! The cache key covers the bundle path and the modification time of its
//! `Info.plist`, so an updated app gets a fresh icon.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::AllocAnyThread;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSCompositingOperation, NSDeviceRGBColorSpace,
    NSGraphicsContext, NSImageInterpolation, NSWorkspace,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};

/// Bitmap size: 32 pt rows at 2x.
pub const ICON_PIXELS: u32 = 64;

/// The cache directory for a home directory.
pub fn cache_dir(home: &Path) -> PathBuf {
    home.join("Library/Caches/cosmicMac/launcher-icons")
}

/// 64-bit FNV-1a; stable across builds, unlike `DefaultHasher`.
fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Cache file for `bundle`, keyed by path, pixel size and `modified`.
pub fn cache_file(dir: &Path, bundle: &Path, modified: Option<SystemTime>) -> PathBuf {
    let mut hash = fnv1a(bundle.as_os_str().as_encoded_bytes(), 0xcbf2_9ce4_8422_2325);
    let stamp = modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    hash = fnv1a(&stamp.to_le_bytes(), hash);
    hash = fnv1a(&ICON_PIXELS.to_le_bytes(), hash);
    dir.join(format!("{hash:016x}.png"))
}

fn modified(bundle: &Path) -> Option<SystemTime> {
    fs::metadata(bundle.join("Contents/Info.plist"))
        .or_else(|_| fs::metadata(bundle))
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// PNG bytes for `bundle`'s icon, from the cache in `dir` or freshly
/// rendered (and then written to the cache, best effort).
pub fn load_or_render(bundle: &Path, dir: Option<&Path>) -> Option<Vec<u8>> {
    let cached = dir.map(|dir| cache_file(dir, bundle, modified(bundle)));
    if let Some(path) = &cached
        && let Ok(bytes) = fs::read(path)
        && is_png(&bytes)
    {
        return Some(bytes);
    }
    let png = render_png(bundle, ICON_PIXELS)?;
    if let Some(path) = &cached {
        let written = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(path, &png));
        if let Err(error) = written {
            log::debug!("cannot cache icon {}: {error}", path.display());
        }
    }
    Some(png)
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// Draws the Finder icon of `path` into a `pixels`×`pixels` RGBA bitmap and
/// encodes it as PNG. Thread-safe: it only draws into its own bitmap
/// context, which AppKit supports off the main thread.
pub fn render_png(path: &Path, pixels: u32) -> Option<Vec<u8>> {
    let side = pixels as isize;
    let image =
        NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(&path.to_string_lossy()));
    // SAFETY: a null `planes` pointer asks AppKit to allocate the pixel
    // buffer; the remaining arguments describe a valid 8-bit RGBA layout
    // (0 bytes per row / bits per pixel = let AppKit compute them).
    let bitmap = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            side,
            side,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }?;
    // Points = pixels, so the drawing fills the bitmap exactly.
    bitmap.setSize(NSSize::new(f64::from(pixels), f64::from(pixels)));
    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap)?;

    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    context.setImageInterpolation(NSImageInterpolation::High);
    let rect = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(f64::from(pixels), f64::from(pixels)),
    );
    image.drawInRect_fromRect_operation_fraction(
        rect,
        NSRect::ZERO, // whole image
        NSCompositingOperation::Copy,
        1.0,
    );
    context.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();

    // SAFETY: an empty properties dictionary is valid for every file type.
    let data = unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::time::Duration;

    #[test]
    fn cache_file_depends_on_path_and_mtime() {
        let dir = Path::new("/cache");
        let time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let a = cache_file(dir, Path::new("/Applications/A.app"), Some(time));
        assert_eq!(
            a,
            cache_file(dir, Path::new("/Applications/A.app"), Some(time))
        );
        assert_ne!(
            a,
            cache_file(dir, Path::new("/Applications/B.app"), Some(time))
        );
        assert_ne!(
            a,
            cache_file(
                dir,
                Path::new("/Applications/A.app"),
                Some(time + Duration::from_secs(1))
            )
        );
        assert_ne!(a, cache_file(dir, Path::new("/Applications/A.app"), None));
        assert_eq!(a.parent(), Some(dir));
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("png"));
        assert_eq!(a.file_stem().map(|stem| stem.len()), Some(16));
    }

    #[test]
    fn fnv1a_matches_reference_values() {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        assert_eq!(fnv1a(b"", OFFSET), OFFSET);
        assert_eq!(fnv1a(b"a", OFFSET), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar", OFFSET), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn cache_dir_is_under_library_caches() {
        assert_eq!(
            cache_dir(Path::new("/Users/tester")),
            PathBuf::from("/Users/tester/Library/Caches/cosmicMac/launcher-icons")
        );
    }

    /// Offscreen render of a system app icon (no window involved), then a
    /// cache round trip.
    #[test]
    fn renders_and_caches_a_png() {
        let calculator = Path::new("/System/Applications/Calculator.app");
        if !calculator.exists() {
            return;
        }
        let started = std::time::Instant::now();
        let png = render_png(calculator, ICON_PIXELS).expect("render");
        eprintln!("rendered {} bytes in {:?}", png.len(), started.elapsed());
        assert!(is_png(&png));
        // A fully transparent 64×64 PNG compresses to ~100 bytes; a drawn
        // icon is several kilobytes.
        assert!(png.len() > 1000, "icon looks blank: {} bytes", png.len());
        // IHDR width and height (big-endian) follow the 8-byte signature,
        // the chunk length and the chunk type.
        assert_eq!(&png[16..20], &ICON_PIXELS.to_be_bytes());
        assert_eq!(&png[20..24], &ICON_PIXELS.to_be_bytes());

        let tmp = TempDir::new("icons");
        let started = std::time::Instant::now();
        let first = load_or_render(calculator, Some(tmp.path())).unwrap();
        eprintln!("warm render + cache write in {:?}", started.elapsed());
        let file = cache_file(tmp.path(), calculator, modified(calculator));
        assert!(file.exists());
        assert_eq!(load_or_render(calculator, Some(tmp.path())).unwrap(), first);
    }
}
