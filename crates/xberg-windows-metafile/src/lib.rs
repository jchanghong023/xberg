#![cfg(windows)]
#![allow(unsafe_code)]

use std::error::Error;
use std::ffi::c_void;
use std::fmt::{Display, Formatter};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
    DeleteEnhMetaFile, DeleteMetaFile, DeleteObject, HBITMAP, HDC, HENHMETAFILE, HGDIOBJ, HMETAFILE, MM_ANISOTROPIC,
    PatBlt, PlayEnhMetaFile, PlayMetaFile, RGBQUAD, SelectObject, SetEnhMetaFileBits, SetMapMode, SetMetaFileBitsEx,
    SetViewportExtEx, SetViewportOrgEx, SetWindowExtEx, SetWindowOrgEx, WHITENESS,
};

const PLACEABLE_KEY: u32 = 0x9ac6_cdd7;
const PLACEABLE_HEADER_BYTES: usize = 22;
const STANDARD_HEADER_BYTES: usize = 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetafileKind {
    Emf,
    PlaceableWmf,
    StandardWmf,
}

#[derive(Debug)]
pub struct RasterizedMetafile {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetafileError(String);

impl MetafileError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl Display for MetafileError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for MetafileError {}

fn read_u16(data: &[u8], offset: usize) -> Result<u16, MetafileError> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| MetafileError::new("header offset overflow"))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| MetafileError::new("truncated metafile header"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_i16(data: &[u8], offset: usize) -> Result<i16, MetafileError> {
    Ok(read_u16(data, offset)? as i16)
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32, MetafileError> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| MetafileError::new("header offset overflow"))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| MetafileError::new("truncated metafile header"))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_i32(data: &[u8], offset: usize) -> Result<i32, MetafileError> {
    Ok(i32::from_le_bytes(read_u32(data, offset)?.to_le_bytes()))
}

fn validate_standard_wmf(data: &[u8]) -> Result<(), MetafileError> {
    if data.len() < STANDARD_HEADER_BYTES {
        return Err(MetafileError::new("truncated standard WMF header"));
    }
    if !matches!(read_u16(data, 0)?, 1 | 2) {
        return Err(MetafileError::new("invalid WMF type"));
    }
    if read_u16(data, 2)? != 9 {
        return Err(MetafileError::new("invalid WMF header size"));
    }
    if !matches!(read_u16(data, 4)?, 0x0100 | 0x0300) {
        return Err(MetafileError::new("unsupported WMF version"));
    }
    let size = usize::try_from(read_u32(data, 6)?)
        .ok()
        .and_then(|words| words.checked_mul(2))
        .ok_or_else(|| MetafileError::new("WMF size overflow"))?;
    if size < STANDARD_HEADER_BYTES || size > data.len() {
        return Err(MetafileError::new("WMF size exceeds input"));
    }
    let maximum_record = usize::try_from(read_u32(data, 12)?)
        .ok()
        .and_then(|words| words.checked_mul(2))
        .ok_or_else(|| MetafileError::new("WMF maximum record size overflow"))?;
    let records_size = size
        .checked_sub(STANDARD_HEADER_BYTES)
        .ok_or_else(|| MetafileError::new("WMF records size underflow"))?;
    if maximum_record < 6 || maximum_record > records_size {
        return Err(MetafileError::new("invalid WMF maximum record size"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct PlaceableBounds {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

fn parse_placeable(data: &[u8]) -> Result<(PlaceableBounds, &[u8]), MetafileError> {
    if data.len() < PLACEABLE_HEADER_BYTES || read_u32(data, 0)? != PLACEABLE_KEY {
        return Err(MetafileError::new("invalid placeable WMF key"));
    }
    let checksum = (0..10).try_fold(0_u16, |value, word| read_u16(data, word * 2).map(|part| value ^ part))?;
    if checksum != read_u16(data, 20)? {
        return Err(MetafileError::new("invalid placeable WMF checksum"));
    }
    let left = i32::from(read_i16(data, 6)?);
    let top = i32::from(read_i16(data, 8)?);
    let right = i32::from(read_i16(data, 10)?);
    let bottom = i32::from(read_i16(data, 12)?);
    if read_u16(data, 14)? == 0 || right <= left || bottom <= top {
        return Err(MetafileError::new("invalid placeable WMF bounds"));
    }
    let records = data
        .get(PLACEABLE_HEADER_BYTES..)
        .ok_or_else(|| MetafileError::new("missing standard WMF payload"))?;
    validate_standard_wmf(records)?;
    Ok((
        PlaceableBounds {
            left,
            top,
            width: right - left,
            height: bottom - top,
        },
        records,
    ))
}

struct DibSurface {
    dc: HDC,
    bitmap: HBITMAP,
    old_object: HGDIOBJ,
    pixels: *mut u8,
    byte_len: usize,
    width: u32,
    height: u32,
}

impl DibSurface {
    fn new(width: u32, height: u32) -> Result<Self, MetafileError> {
        let width_i32 = i32::try_from(width).map_err(|_| MetafileError::new("width exceeds GDI range"))?;
        let height_i32 = i32::try_from(height).map_err(|_| MetafileError::new("height exceeds GDI range"))?;
        let byte_len = usize::try_from(width)
            .ok()
            .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| MetafileError::new("RGBA allocation overflow"))?;

        let dc = unsafe { CreateCompatibleDC(null_mut()) };
        if dc.is_null() {
            return Err(MetafileError::new("CreateCompatibleDC failed"));
        }

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width_i32,
                biHeight: -height_i32,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default()],
        };
        let mut bits: *mut c_void = null_mut();
        let bitmap = unsafe { CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0) };
        if bitmap.is_null() {
            unsafe { DeleteDC(dc) };
            return Err(MetafileError::new("CreateDIBSection failed"));
        }
        if bits.is_null() {
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(dc);
            }
            return Err(MetafileError::new("CreateDIBSection returned no pixel buffer"));
        }
        let old_object = unsafe { SelectObject(dc, bitmap) };
        if old_object.is_null() || old_object as isize == -1 {
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(dc);
            }
            return Err(MetafileError::new("SelectObject failed"));
        }
        if unsafe { PatBlt(dc, 0, 0, width_i32, height_i32, WHITENESS) } == 0 {
            unsafe {
                SelectObject(dc, old_object);
                DeleteObject(bitmap);
                DeleteDC(dc);
            }
            return Err(MetafileError::new("white background fill failed"));
        }
        Ok(Self {
            dc,
            bitmap,
            old_object,
            pixels: bits.cast(),
            byte_len,
            width,
            height,
        })
    }

    fn into_rgba(self) -> Vec<u8> {
        let bgra = unsafe { std::slice::from_raw_parts(self.pixels, self.byte_len) };
        let mut rgba = Vec::with_capacity(self.byte_len);
        for pixel in bgra.chunks_exact(4) {
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
        rgba
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old_object);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

struct EnhancedMetafile(HENHMETAFILE);
impl Drop for EnhancedMetafile {
    fn drop(&mut self) {
        unsafe { DeleteEnhMetaFile(self.0) };
    }
}

struct Metafile(HMETAFILE);
impl Drop for Metafile {
    fn drop(&mut self) {
        unsafe { DeleteMetaFile(self.0) };
    }
}

fn fit_rect(width: u32, height: u32, source_width: i64, source_height: i64) -> RECT {
    let target_width = i64::from(width);
    let target_height = i64::from(height);
    let (draw_width, draw_height) = if source_width > 0
        && source_height > 0
        && target_width.saturating_mul(source_height) > target_height.saturating_mul(source_width)
    {
        (
            target_height.saturating_mul(source_width) / source_height,
            target_height,
        )
    } else if source_width > 0 && source_height > 0 {
        (target_width, target_width.saturating_mul(source_height) / source_width)
    } else {
        (target_width, target_height)
    };
    let left = (target_width - draw_width) / 2;
    let top = (target_height - draw_height) / 2;
    RECT {
        left: left as i32,
        top: top as i32,
        right: (left + draw_width.max(1)) as i32,
        bottom: (top + draw_height.max(1)) as i32,
    }
}

pub fn rasterize(
    data: &[u8],
    kind: MetafileKind,
    target_width: u32,
    target_height: u32,
) -> Result<RasterizedMetafile, MetafileError> {
    if target_width == 0 || target_height == 0 {
        return Err(MetafileError::new("target dimensions must be non-zero"));
    }
    let surface = DibSurface::new(target_width, target_height)?;

    match kind {
        MetafileKind::Emf => {
            let header_size =
                usize::try_from(read_u32(data, 4)?).map_err(|_| MetafileError::new("EMF header size overflow"))?;
            let total_bytes =
                usize::try_from(read_u32(data, 48)?).map_err(|_| MetafileError::new("EMF byte count overflow"))?;
            if data.len() < 88
                || read_u32(data, 0)? != 1
                || header_size < 88
                || header_size > data.len()
                || total_bytes < header_size
                || total_bytes > data.len()
                || data.get(40..44) != Some(b" EMF")
            {
                return Err(MetafileError::new("invalid EMF header"));
            }
            let byte_count = u32::try_from(total_bytes).map_err(|_| MetafileError::new("EMF input too large"))?;
            let raw_handle = unsafe { SetEnhMetaFileBits(byte_count, data[..total_bytes].as_ptr()) };
            if raw_handle.is_null() {
                return Err(MetafileError::new("SetEnhMetaFileBits failed"));
            }
            let handle = EnhancedMetafile(raw_handle);
            let frame_width = i64::from(read_i32(data, 32)?) - i64::from(read_i32(data, 24)?);
            let frame_height = i64::from(read_i32(data, 36)?) - i64::from(read_i32(data, 28)?);
            let bounds_width = i64::from(read_i32(data, 16)?) - i64::from(read_i32(data, 8)?);
            let bounds_height = i64::from(read_i32(data, 20)?) - i64::from(read_i32(data, 12)?);
            let (source_width, source_height) = if frame_width != 0 && frame_height != 0 {
                (frame_width.abs(), frame_height.abs())
            } else {
                (bounds_width.abs(), bounds_height.abs())
            };
            let rect = fit_rect(target_width, target_height, source_width, source_height);
            if unsafe { PlayEnhMetaFile(surface.dc, handle.0, &rect) } == 0 {
                return Err(MetafileError::new("PlayEnhMetaFile failed"));
            }
        }
        MetafileKind::PlaceableWmf => {
            let (bounds, records) = parse_placeable(data)?;
            play_wmf(&surface, records, Some(bounds))?;
        }
        MetafileKind::StandardWmf => {
            validate_standard_wmf(data)?;
            play_wmf(&surface, data, None)?;
        }
    }

    let width = surface.width;
    let height = surface.height;
    let rgba = surface.into_rgba();
    Ok(RasterizedMetafile { width, height, rgba })
}

fn play_wmf(surface: &DibSurface, records: &[u8], bounds: Option<PlaceableBounds>) -> Result<(), MetafileError> {
    let declared_bytes = usize::try_from(read_u32(records, 6)?)
        .ok()
        .and_then(|words| words.checked_mul(2))
        .ok_or_else(|| MetafileError::new("WMF size overflow"))?;
    let records = records
        .get(..declared_bytes)
        .ok_or_else(|| MetafileError::new("WMF size exceeds input"))?;
    let byte_count = u32::try_from(records.len()).map_err(|_| MetafileError::new("WMF input too large"))?;
    let raw_handle = unsafe { SetMetaFileBitsEx(byte_count, records.as_ptr()) };
    if raw_handle.is_null() {
        return Err(MetafileError::new("SetMetaFileBitsEx failed"));
    }
    let handle = Metafile(raw_handle);
    if unsafe { SetMapMode(surface.dc, MM_ANISOTROPIC) } == 0 {
        return Err(MetafileError::new("SetMapMode failed"));
    }
    let target_width = i32::try_from(surface.width).map_err(|_| MetafileError::new("target width too large"))?;
    let target_height = i32::try_from(surface.height).map_err(|_| MetafileError::new("target height too large"))?;
    let source = bounds.unwrap_or(PlaceableBounds {
        left: 0,
        top: 0,
        width: target_width,
        height: target_height,
    });
    let rect = fit_rect(
        surface.width,
        surface.height,
        i64::from(source.width),
        i64::from(source.height),
    );
    let ok = unsafe {
        SetWindowOrgEx(surface.dc, source.left, source.top, null_mut()) != 0
            && SetWindowExtEx(surface.dc, source.width, source.height, null_mut()) != 0
            && SetViewportOrgEx(surface.dc, rect.left, rect.top, null_mut()) != 0
            && SetViewportExtEx(surface.dc, rect.right - rect.left, rect.bottom - rect.top, null_mut()) != 0
    };
    if !ok {
        return Err(MetafileError::new("WMF mapping setup failed"));
    }
    if unsafe { PlayMetaFile(surface.dc, handle.0) } == 0 {
        return Err(MetafileError::new("PlayMetaFile failed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_u16(output: &mut Vec<u8>, value: u16) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32(output: &mut Vec<u8>, value: u32) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn wmf_record(output: &mut Vec<u8>, function: u16, parameters: &[u16]) {
        push_u32(output, (3 + parameters.len()) as u32);
        push_u16(output, function);
        for parameter in parameters {
            push_u16(output, *parameter);
        }
    }

    fn standard_wmf() -> Vec<u8> {
        let mut records = Vec::new();
        wmf_record(&mut records, 0x020b, &[0, 0]);
        wmf_record(&mut records, 0x020c, &[100, 100]);
        wmf_record(&mut records, 0x0214, &[10, 10]);
        wmf_record(&mut records, 0x0213, &[90, 90]);
        wmf_record(&mut records, 0x0000, &[]);

        let total_words = (STANDARD_HEADER_BYTES + records.len()) / 2;
        let mut output = Vec::new();
        push_u16(&mut output, 1);
        push_u16(&mut output, 9);
        push_u16(&mut output, 0x0300);
        push_u32(&mut output, total_words as u32);
        push_u16(&mut output, 0);
        push_u32(&mut output, 5);
        push_u16(&mut output, 0);
        output.extend_from_slice(&records);
        output
    }

    fn placeable_wmf() -> Vec<u8> {
        let mut output = Vec::new();
        push_u32(&mut output, PLACEABLE_KEY);
        push_u16(&mut output, 0);
        for value in [0_i16, 0, 100, 100] {
            push_u16(&mut output, value as u16);
        }
        push_u16(&mut output, 1440);
        push_u32(&mut output, 0);
        let checksum = output
            .chunks_exact(2)
            .take(10)
            .fold(0_u16, |value, bytes| value ^ u16::from_le_bytes([bytes[0], bytes[1]]));
        push_u16(&mut output, checksum);
        output.extend_from_slice(&standard_wmf());
        output
    }

    fn set_u32(output: &mut [u8], offset: usize, value: u32) {
        output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn set_i32(output: &mut [u8], offset: usize, value: i32) {
        output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn emf() -> Vec<u8> {
        let mut output = vec![0_u8; 88];
        set_u32(&mut output, 0, 1);
        set_u32(&mut output, 4, 88);
        set_i32(&mut output, 16, 100);
        set_i32(&mut output, 20, 100);
        set_i32(&mut output, 32, 2540);
        set_i32(&mut output, 36, 2540);
        output[40..44].copy_from_slice(b" EMF");
        set_u32(&mut output, 44, 0x0001_0000);
        set_u32(&mut output, 48, 140);
        set_u32(&mut output, 52, 4);
        output[56..58].copy_from_slice(&1_u16.to_le_bytes());
        set_i32(&mut output, 72, 100);
        set_i32(&mut output, 76, 100);
        set_i32(&mut output, 80, 25);
        set_i32(&mut output, 84, 25);

        for (record_type, x, y) in [(27_u32, 10_i32, 10_i32), (54, 90, 90)] {
            push_u32(&mut output, record_type);
            push_u32(&mut output, 16);
            output.extend_from_slice(&x.to_le_bytes());
            output.extend_from_slice(&y.to_le_bytes());
        }
        push_u32(&mut output, 14);
        push_u32(&mut output, 20);
        output.extend_from_slice(&[0_u8; 12]);
        output
    }

    fn assert_raster(kind: MetafileKind, bytes: &[u8]) {
        let raster = rasterize(bytes, kind, 96, 64).expect("synthetic metafile must rasterize");
        assert_eq!((raster.width, raster.height), (96, 64));
        assert_eq!(raster.rgba.len(), 96 * 64 * 4);
        assert!(
            raster.rgba.chunks_exact(4).any(|pixel| pixel != [255, 255, 255, 255]),
            "drawn line must produce at least one non-white pixel"
        );
    }
    #[test]
    fn rasterizes_all_supported_metafile_kinds_without_growing_live_handles() {
        use windows_sys::Win32::System::Threading::{GR_GDIOBJECTS, GetCurrentProcess, GetGuiResources};

        let emf = emf();
        let standard = standard_wmf();
        let placeable = placeable_wmf();
        let before = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
        for _ in 0..100 {
            assert_raster(MetafileKind::Emf, &emf);
            assert_raster(MetafileKind::StandardWmf, &standard);
            assert_raster(MetafileKind::PlaceableWmf, &placeable);
            assert!(rasterize(b"invalid", MetafileKind::Emf, 96, 64).is_err());
            assert!(rasterize(b"invalid", MetafileKind::StandardWmf, 96, 64).is_err());
        }
        let after = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
        assert!(after <= before, "GDI object count grew from {before} to {after}");
    }
}
