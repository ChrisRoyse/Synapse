use std::time::Instant;

use synapse_core::Rect;

#[cfg(windows)]
pub type D3d11Texture = windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

#[cfg(not(windows))]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct D3d11Texture;

#[derive(Debug)]
pub struct SendablePtr<T>(T);

#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<T> Send for SendablePtr<T> {}
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<T> Sync for SendablePtr<T> {}

impl<T> SendablePtr<T> {
    #[must_use]
    pub const fn new(inner: T) -> Self {
        Self(inner)
    }

    #[must_use]
    pub const fn get(&self) -> &T {
        &self.0
    }
}

impl<T: Clone> Clone for SendablePtr<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DxgiFormat {
    Bgra8,
    Bgra8Srgb,
    Rgba8,
    Rgba8Srgb,
    Rgba16F,
    Rgb10A2,
    Rgb10XrA2,
    Unknown(u32),
}

#[derive(Clone, Debug)]
pub struct CapturedFrame {
    pub texture: SendablePtr<D3d11Texture>,
    pub width: u32,
    pub height: u32,
    pub format: DxgiFormat,
    pub captured_at: Instant,
    pub frame_seq: u64,
    pub dirty_region: Option<Rect>,
}

#[cfg(windows)]
pub struct CapturedSoftwareBitmap {
    pub region: Rect,
    pub bitmap: windows::Graphics::Imaging::SoftwareBitmap,
}

/// Raw BGRA pixels for a screen region.
///
/// This is a plain data struct (no Windows-specific types), so it is available
/// on every platform: non-Windows `screen_region_to_bgra_bitmap` returns
/// `Err(GraphicsApiUnsupported)` rather than a value, but the `synapse-mcp`
/// callers still reference the type at compile time. Contrast
/// `CapturedSoftwareBitmap`, which wraps a `WinRT` `SoftwareBitmap` and is
/// therefore Windows-only.
#[derive(Clone, Debug)]
pub struct CapturedBgraBitmap {
    pub region: Rect,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

// Note: a `CapturedFrame::synthetic(..)` constructor used to exist here for
// non-Windows builds. It fabricated placeholder frames so the capture loop on
// Linux/macOS appeared to succeed while feeding mock pixels into perception. It
// was removed deliberately: non-Windows builds now fail loudly in
// `platform::non_windows` instead of producing fake frames. See
// `crates/synapse-capture/src/platform/non_windows.rs`.
