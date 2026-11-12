//! Windows display capture into NV12. DXGI desktop duplication delivers the
//! desktop as a GPU texture, which the D3D11 video processor converts to
//! NV12 before the CPU reads it back: 1.5 bytes per pixel instead of 4. A
//! GPU without the video processor reads BGRA back and converts on the CPU.
//! GDI capture (BitBlt) is the fallback when duplication is unavailable or
//! invalidated, and always converts on the CPU.

use super::cursor::Surface;
use crate::video::{
    display::{Com, Display, Output},
    nv12::{
        pixels::{bgra_to_nv12, Rect, BGRA_BYTES},
        Nv12Layout,
    },
};
use std::{
    io, mem, ptr, slice,
    time::{Duration, Instant},
};
use winapi::{
    shared::{
        dxgi::IDXGIResource,
        dxgi1_2::{IDXGIOutputDuplication, DXGI_OUTDUPL_FRAME_INFO},
        dxgiformat::{DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12},
        dxgitype::{
            DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
            DXGI_MODE_ROTATION_ROTATE90, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
        },
        minwindef::{FALSE, TRUE},
        windef::{HDC, RECT},
        winerror::{
            DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL, DXGI_ERROR_WAIT_TIMEOUT, HRESULT, S_OK,
        },
    },
    um::{
        d3d11::{
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
            ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
            ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
            ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
            D3D11_SDK_VERSION, D3D11_TEX2D_VPIV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
            D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
            D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
            D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
            D3D11_VIDEO_PROCESSOR_ROTATION, D3D11_VIDEO_PROCESSOR_ROTATION_180,
            D3D11_VIDEO_PROCESSOR_ROTATION_270, D3D11_VIDEO_PROCESSOR_ROTATION_90,
            D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
        },
        d3dcommon::D3D_DRIVER_TYPE_UNKNOWN,
        unknwnbase::IUnknown,
        wingdi::{BitBlt, CreateDCW, DeleteDC, GdiFlush, CAPTUREBLT, SRCCOPY},
    },
    Interface,
};

/// What a capture pass got.
pub enum Grab {
    /// A new frame is in the NV12 buffer; converting it took `convert`.
    Frame { convert: Duration },
    /// The desktop did not change.
    Unchanged,
    /// The desktop no longer has the size of the display.
    Resized,
    /// The duplication can deliver no more frames: Windows invalidated it,
    /// for example for the secure desktop or a mode change, or a rotated
    /// display lost its GPU conversion. GDI captures from here on.
    Invalidated,
}

/// The display a capturer reads, as GDI names it.
#[derive(Clone)]
pub struct Target {
    name: Vec<u16>,
    width: usize,
    height: usize,
}

pub enum Capturer {
    Dxgi(Duplication, Target),
    Gdi(Gdi),
}

impl Capturer {
    /// Duplicates the display, or captures it through GDI when DXGI cannot.
    pub fn dxgi(display: Display) -> io::Result<Self> {
        let target = Target::of(&display);
        match display.dxgi.as_ref() {
            Some(output) => match Duplication::new(output, target.width, target.height) {
                Ok(duplication) => return Ok(Self::Dxgi(duplication, target)),
                Err(err) => eprintln!("DXGI duplication failed ({err}); capturing through GDI"),
            },
            None => eprintln!(
                "DXGI does not list {}; capturing through GDI",
                display.name()
            ),
        }
        Gdi::new(target).map(Self::Gdi)
    }

    /// Duplicates the display, or None when DXGI cannot, without a log
    /// line: a retry during a lock screen fails once a second.
    pub fn try_dxgi(display: &Display) -> Option<Self> {
        let target = Target::of(display);
        let duplication =
            Duplication::new(display.dxgi.as_ref()?, target.width, target.height).ok()?;
        Some(Self::Dxgi(duplication, target))
    }

    pub fn is_dxgi(&self) -> bool {
        matches!(self, Self::Dxgi(..))
    }

    /// Captures the display through GDI. The first frame is always
    /// delivered, even when the desktop has not changed.
    pub fn gdi(display: &Display) -> io::Result<Self> {
        Gdi::new(Target::of(display)).map(Self::Gdi)
    }

    /// Captures the same display through GDI from the next pass on.
    pub fn switch_to_gdi(&mut self) -> io::Result<()> {
        let target = match self {
            Self::Dxgi(_, target) => target.clone(),
            Self::Gdi(gdi) => gdi.target.clone(),
        };
        *self = Self::Gdi(Gdi::new(target)?);
        Ok(())
    }

    /// Windows drew the pointer into the frame itself, as it does when it
    /// renders the pointer in software. Never true on GDI, whose frames
    /// have no pointer.
    pub fn pointer_in_frame(&self) -> bool {
        matches!(self, Self::Dxgi(duplication, _) if duplication.pointer_in_frame)
    }

    /// Waits up to `timeout` for a new frame and writes it into `nv12` in
    /// `layout`; `nv12` is touched only for a new frame.
    pub fn frame(
        &mut self,
        timeout: Duration,
        layout: &Nv12Layout,
        nv12: &mut Vec<u8>,
    ) -> io::Result<Grab> {
        match self {
            Self::Dxgi(duplication, _) => duplication.frame(timeout, layout, nv12),
            Self::Gdi(gdi) => gdi.frame(timeout, layout, nv12),
        }
    }
}

impl Target {
    fn of(display: &Display) -> Self {
        Self {
            name: display.wide_name(),
            width: display.width(),
            height: display.height(),
        }
    }
}

fn check(result: HRESULT) -> io::Result<()> {
    match result {
        S_OK => Ok(()),
        _ => Err(io::Error::other(format!("error {result:#x}"))),
    }
}

/// The interface that a COM call creates through its out-pointer.
fn created<T: Interface>(call: impl FnOnce(*mut *mut T) -> HRESULT) -> io::Result<Com<T>> {
    let mut raw = ptr::null_mut();
    check(call(&mut raw))?;
    Com::new(raw).ok_or_else(|| io::Error::other("the call created no interface"))
}

pub struct Duplication {
    device: Com<ID3D11Device>,
    context: Com<ID3D11DeviceContext>,
    duplication: Com<IDXGIOutputDuplication>,
    rotation: DXGI_MODE_ROTATION,
    /// The size of the desktop texture, before rotation.
    source: (u32, u32),
    converter: Option<Converter>,
    /// CPU-readable copies of the NV12 output or of the BGRA desktop.
    nv12_readback: Option<Com<ID3D11Texture2D>>,
    bgra_readback: Option<Com<ID3D11Texture2D>>,
    pointer_in_frame: bool,
}

impl Duplication {
    fn new(output: &Output, width: usize, height: usize) -> io::Result<Self> {
        let (device, context) = create_device(output)?;
        let duplication = created(|out| unsafe {
            output
                .output
                .DuplicateOutput(device.as_ptr() as *mut IUnknown, out)
        })?;
        let rotated = matches!(
            output.rotation,
            DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270
        );
        let source = if rotated {
            (height as u32, width as u32)
        } else {
            (width as u32, height as u32)
        };
        let converter =
            match Converter::new(&device, &context, source, (width, height), output.rotation) {
                Ok(converter) => Some(converter),
                // The CPU conversion does not rotate; GDI captures a rotated
                // display upright.
                Err(err) if output.rotation >= DXGI_MODE_ROTATION_ROTATE90 => {
                    return Err(io::Error::other(format!(
                        "a rotated display needs the GPU NV12 conversion: {err}"
                    )))
                }
                Err(err) => {
                    eprintln!("no GPU NV12 conversion ({err}); converting on the CPU");
                    None
                }
            };
        Ok(Self {
            device,
            context,
            duplication,
            rotation: output.rotation,
            source,
            converter,
            nv12_readback: None,
            bgra_readback: None,
            pointer_in_frame: false,
        })
    }

    fn frame(
        &mut self,
        timeout: Duration,
        layout: &Nv12Layout,
        nv12: &mut Vec<u8>,
    ) -> io::Result<Grab> {
        let mut info: DXGI_OUTDUPL_FRAME_INFO = unsafe { mem::zeroed() };
        let mut resource: *mut IDXGIResource = ptr::null_mut();
        let acquired = unsafe {
            self.duplication
                .AcquireNextFrame(timeout.as_millis() as u32, &mut info, &mut resource)
        };
        match acquired {
            DXGI_ERROR_WAIT_TIMEOUT => return Ok(Grab::Unchanged),
            DXGI_ERROR_ACCESS_LOST | DXGI_ERROR_INVALID_CALL => return Ok(Grab::Invalidated),
            result => check(result)?,
        }
        let resource = Com::new(resource);
        let grab = self.grab(&info, resource, layout, nv12);
        unsafe { self.duplication.ReleaseFrame() };
        grab
    }

    fn grab(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<Com<IDXGIResource>>,
        layout: &Nv12Layout,
        nv12: &mut Vec<u8>,
    ) -> io::Result<Grab> {
        // The pointer position is valid only alongside a mouse update. A
        // pointer that is not separately visible is drawn into the image.
        if unsafe { *info.LastMouseUpdateTime.QuadPart() } != 0 {
            self.pointer_in_frame = info.PointerPosition.Visible == FALSE;
        }
        if unsafe { *info.LastPresentTime.QuadPart() } == 0 {
            return Ok(Grab::Unchanged);
        }
        let texture: Com<ID3D11Texture2D> = resource
            .and_then(|resource| resource.cast())
            .ok_or_else(|| io::Error::other("the desktop frame is not a texture"))?;
        let mut desc: D3D11_TEXTURE2D_DESC = unsafe { mem::zeroed() };
        unsafe { texture.GetDesc(&mut desc) };
        if (desc.Width, desc.Height) != self.source {
            return Ok(Grab::Resized);
        }
        let started = Instant::now();
        nv12.resize(layout.buffer_len(), 0);
        let on_gpu = self.converter.is_some()
            && self
                .convert_on_gpu(&texture, layout, nv12)
                .map_err(|err| {
                    eprintln!("GPU NV12 conversion failed ({err}); converting on the CPU");
                    self.converter = None;
                })
                .is_ok();
        if !on_gpu {
            if self.rotation >= DXGI_MODE_ROTATION_ROTATE90 {
                return Ok(Grab::Invalidated);
            }
            self.convert_on_cpu(&texture, &desc, layout, nv12)?;
        }
        Ok(Grab::Frame {
            convert: started.elapsed(),
        })
    }

    fn convert_on_gpu(
        &mut self,
        texture: &Com<ID3D11Texture2D>,
        layout: &Nv12Layout,
        nv12: &mut [u8],
    ) -> io::Result<()> {
        let converter = self.converter.as_ref().expect("checked by the caller");
        converter.convert(texture)?;
        let readback = readback(
            &self.device,
            &mut self.nv12_readback,
            &converter.output_desc,
        )?;
        unsafe {
            self.context.CopyResource(
                readback.as_ptr() as *mut ID3D11Resource,
                converter.output.as_ptr() as *mut ID3D11Resource,
            )
        };
        let mapped = Mapped::new(&self.context, &readback)?;
        let (width, height) = (layout.width, layout.height);
        let pitch = mapped.pitch();
        let planes = mapped.bytes(pitch * converter.output_desc.Height as usize * 3 / 2);
        for row in 0..height {
            nv12[row * layout.y_stride..][..width].copy_from_slice(&planes[row * pitch..][..width]);
        }
        let uv = &planes[pitch * converter.output_desc.Height as usize..];
        for row in 0..height / 2 {
            nv12[layout.uv_offset + row * layout.uv_stride..][..width]
                .copy_from_slice(&uv[row * pitch..][..width]);
        }
        Ok(())
    }

    fn convert_on_cpu(
        &mut self,
        texture: &Com<ID3D11Texture2D>,
        desc: &D3D11_TEXTURE2D_DESC,
        layout: &Nv12Layout,
        nv12: &mut [u8],
    ) -> io::Result<()> {
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(io::Error::other(format!(
                "the desktop format {} is not BGRA",
                desc.Format
            )));
        }
        let readback = readback(&self.device, &mut self.bgra_readback, desc)?;
        unsafe {
            self.context.CopyResource(
                readback.as_ptr() as *mut ID3D11Resource,
                texture.as_ptr() as *mut ID3D11Resource,
            )
        };
        let mapped = Mapped::new(&self.context, &readback)?;
        let pitch = mapped.pitch();
        let bgra = mapped.bytes(pitch * layout.height);
        bgra_to_nv12(bgra, pitch, nv12, layout, Rect::whole(layout));
        Ok(())
    }
}

/// A device on the display's adapter, with video support when the driver
/// offers it.
fn create_device(output: &Output) -> io::Result<(Com<ID3D11Device>, Com<ID3D11DeviceContext>)> {
    let mut last = S_OK;
    for flags in [D3D11_CREATE_DEVICE_VIDEO_SUPPORT, 0] {
        let (mut device, mut context) = (ptr::null_mut(), ptr::null_mut());
        last = unsafe {
            D3D11CreateDevice(
                output.adapter.as_ptr() as *mut _,
                D3D_DRIVER_TYPE_UNKNOWN,
                ptr::null_mut(),
                flags,
                ptr::null(),
                0,
                D3D11_SDK_VERSION,
                &mut device,
                ptr::null_mut(),
                &mut context,
            )
        };
        if let (S_OK, Some(device), Some(context)) = (last, Com::new(device), Com::new(context)) {
            return Ok((device, context));
        }
    }
    check(last)?;
    Err(io::Error::other("D3D11CreateDevice returned no device"))
}

/// The staging texture in `slot` for `desc`, created or replaced as needed.
fn readback(
    device: &Com<ID3D11Device>,
    slot: &mut Option<Com<ID3D11Texture2D>>,
    desc: &D3D11_TEXTURE2D_DESC,
) -> io::Result<Com<ID3D11Texture2D>> {
    if let Some(texture) = slot {
        let mut current: D3D11_TEXTURE2D_DESC = unsafe { mem::zeroed() };
        unsafe { texture.GetDesc(&mut current) };
        if (current.Width, current.Height, current.Format) == (desc.Width, desc.Height, desc.Format)
        {
            return Ok(texture.clone());
        }
    }
    let staging = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ,
        MiscFlags: 0,
        MipLevels: 1,
        ArraySize: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        ..*desc
    };
    let texture = create_texture(device, &staging)?;
    *slot = Some(texture.clone());
    Ok(texture)
}

fn create_texture(
    device: &Com<ID3D11Device>,
    desc: &D3D11_TEXTURE2D_DESC,
) -> io::Result<Com<ID3D11Texture2D>> {
    created(|out| unsafe { device.CreateTexture2D(desc, ptr::null(), out) })
}

/// A staging texture mapped for reading until dropped.
struct Mapped<'a> {
    context: &'a Com<ID3D11DeviceContext>,
    texture: &'a Com<ID3D11Texture2D>,
    mapped: D3D11_MAPPED_SUBRESOURCE,
}

impl<'a> Mapped<'a> {
    fn new(
        context: &'a Com<ID3D11DeviceContext>,
        texture: &'a Com<ID3D11Texture2D>,
    ) -> io::Result<Self> {
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { mem::zeroed() };
        check(unsafe {
            context.Map(
                texture.as_ptr() as *mut ID3D11Resource,
                0,
                D3D11_MAP_READ,
                0,
                &mut mapped,
            )
        })?;
        Ok(Self {
            context,
            texture,
            mapped,
        })
    }

    fn pitch(&self) -> usize {
        self.mapped.RowPitch as usize
    }

    fn bytes(&self, length: usize) -> &[u8] {
        unsafe { slice::from_raw_parts(self.mapped.pData as *const u8, length) }
    }
}

impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        unsafe {
            self.context
                .Unmap(self.texture.as_ptr() as *mut ID3D11Resource, 0)
        };
    }
}

/// A video processor that takes the BGRA desktop of `source` pixels and
/// writes NV12 frames of `width` x `height`.
fn create_processor(
    video_device: &Com<ID3D11VideoDevice>,
    source: (u32, u32),
    (width, height): (usize, usize),
) -> io::Result<(
    Com<ID3D11VideoProcessorEnumerator>,
    Com<ID3D11VideoProcessor>,
)> {
    let rate = DXGI_RATIONAL {
        Numerator: 60,
        Denominator: 1,
    };
    let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: rate,
        InputWidth: source.0,
        InputHeight: source.1,
        OutputFrameRate: rate,
        OutputWidth: width as u32,
        OutputHeight: height as u32,
        Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    };
    let enumerator: Com<ID3D11VideoProcessorEnumerator> =
        created(|out| unsafe { video_device.CreateVideoProcessorEnumerator(&content, out) })?;
    supports(
        &enumerator,
        DXGI_FORMAT_B8G8R8A8_UNORM,
        D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
    )?;
    supports(
        &enumerator,
        DXGI_FORMAT_NV12,
        D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    )?;
    let processor =
        created(|out| unsafe { video_device.CreateVideoProcessor(enumerator.as_ptr(), 0, out) })?;
    Ok((enumerator, processor))
}

/// The D3D11 video processor set up to turn the BGRA desktop into BT.601
/// limited-range NV12, rotated upright.
struct Converter {
    video_device: Com<ID3D11VideoDevice>,
    video_context: Com<ID3D11VideoContext>,
    enumerator: Com<ID3D11VideoProcessorEnumerator>,
    processor: Com<ID3D11VideoProcessor>,
    output: Com<ID3D11Texture2D>,
    output_desc: D3D11_TEXTURE2D_DESC,
    output_view: Com<ID3D11VideoProcessorOutputView>,
}

impl Converter {
    fn new(
        device: &Com<ID3D11Device>,
        context: &Com<ID3D11DeviceContext>,
        source: (u32, u32),
        (width, height): (usize, usize),
        rotation: DXGI_MODE_ROTATION,
    ) -> io::Result<Self> {
        let video_device: Com<ID3D11VideoDevice> = device
            .cast()
            .ok_or_else(|| io::Error::other("no video device"))?;
        let video_context: Com<ID3D11VideoContext> = context
            .cast()
            .ok_or_else(|| io::Error::other("no video context"))?;
        let (enumerator, processor) = create_processor(&video_device, source, (width, height))?;
        let output_desc = D3D11_TEXTURE2D_DESC {
            Width: width as u32,
            Height: height as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let output = create_texture(device, &output_desc)?;
        let mut view_desc: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC = unsafe { mem::zeroed() };
        view_desc.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
        let output_view = created(|out| unsafe {
            video_device.CreateVideoProcessorOutputView(
                output.as_ptr() as *mut ID3D11Resource,
                enumerator.as_ptr(),
                &view_desc,
                out,
            )
        })?;
        let converter = Self {
            video_device,
            video_context,
            enumerator,
            processor,
            output,
            output_desc,
            output_view,
        };
        converter.configure(source, (width, height), rotation);
        Ok(converter)
    }

    fn configure(
        &self,
        source: (u32, u32),
        (width, height): (usize, usize),
        rotation: DXGI_MODE_ROTATION,
    ) {
        let (context, processor) = (&self.video_context, self.processor.as_ptr());
        // Full-range RGB in; BT.601 limited-range YCbCr out, as the
        // encoders' streams are tagged.
        let mut input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { bitfield: 0 };
        input.set_RGB_Range(0);
        let mut output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { bitfield: 0 };
        output.set_YCbCr_Matrix(0);
        output.set_Nominal_Range(D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235);
        let source_rect = RECT {
            left: 0,
            top: 0,
            right: source.0 as i32,
            bottom: source.1 as i32,
        };
        let target_rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        unsafe {
            context.VideoProcessorSetStreamFrameFormat(
                processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            context.VideoProcessorSetStreamAutoProcessingMode(processor, 0, FALSE);
            context.VideoProcessorSetStreamColorSpace(processor, 0, &input);
            let _ = context.VideoProcessorSetOutputColorSpace(processor, &output);
            context.VideoProcessorSetStreamSourceRect(processor, 0, TRUE, &source_rect);
            context.VideoProcessorSetStreamDestRect(processor, 0, TRUE, &target_rect);
            context.VideoProcessorSetOutputTargetRect(processor, TRUE, &target_rect);
            if let Some(rotation) = processor_rotation(rotation) {
                let _ = context.VideoProcessorSetStreamRotation(processor, 0, TRUE, rotation);
            }
        }
    }

    /// Converts the desktop texture into the NV12 output texture.
    fn convert(&self, texture: &Com<ID3D11Texture2D>) -> io::Result<()> {
        let view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Texture2D: D3D11_TEX2D_VPIV {
                MipSlice: 0,
                ArraySlice: 0,
            },
        };
        let input_view: Com<ID3D11VideoProcessorInputView> = created(|out| unsafe {
            self.video_device.CreateVideoProcessorInputView(
                texture.as_ptr() as *mut ID3D11Resource,
                self.enumerator.as_ptr(),
                &view_desc,
                out,
            )
        })?;
        let mut stream: D3D11_VIDEO_PROCESSOR_STREAM = unsafe { mem::zeroed() };
        stream.Enable = TRUE;
        stream.pInputSurface = input_view.as_ptr();
        check(unsafe {
            self.video_context.VideoProcessorBlt(
                self.processor.as_ptr(),
                self.output_view.as_ptr(),
                0,
                1,
                &stream,
            )
        })
    }
}

fn supports(
    enumerator: &Com<ID3D11VideoProcessorEnumerator>,
    format: DXGI_FORMAT,
    usage: u32,
) -> io::Result<()> {
    let mut flags = 0;
    check(unsafe { enumerator.CheckVideoProcessorFormat(format, &mut flags) })?;
    if flags & usage == 0 {
        return Err(io::Error::other(format!(
            "format {format} is not supported"
        )));
    }
    Ok(())
}

fn processor_rotation(rotation: DXGI_MODE_ROTATION) -> Option<D3D11_VIDEO_PROCESSOR_ROTATION> {
    match rotation {
        DXGI_MODE_ROTATION_ROTATE90 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_90),
        DXGI_MODE_ROTATION_ROTATE180 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_180),
        DXGI_MODE_ROTATION_ROTATE270 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_270),
        _ => None,
    }
}

/// GDI capture of one display into a top-down BGRA bitmap.
pub struct Gdi {
    target: Target,
    screen: HDC,
    /// The bitmap that BitBlt copies the screen into.
    surface: Surface,
    /// The last delivered frame: GDI reports no damage, so an unchanged
    /// frame counts as no new frame.
    last: Vec<u8>,
}

impl Gdi {
    fn new(target: Target) -> io::Result<Self> {
        let mut surface = Surface::new()?;
        surface.resize(target.width, target.height)?;
        let screen =
            unsafe { CreateDCW(target.name.as_ptr(), ptr::null(), ptr::null(), ptr::null()) };
        if screen.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            target,
            screen,
            surface,
            last: Vec::new(),
        })
    }

    /// GDI reports no damage, so an unchanged frame waits out `timeout`
    /// before the next pass compares again.
    fn frame(
        &mut self,
        timeout: Duration,
        layout: &Nv12Layout,
        nv12: &mut Vec<u8>,
    ) -> io::Result<Grab> {
        let started = Instant::now();
        let (width, height) = (self.target.width as i32, self.target.height as i32);
        let copied = unsafe {
            BitBlt(
                self.surface.dc(),
                0,
                0,
                width,
                height,
                self.screen,
                0,
                0,
                SRCCOPY | CAPTUREBLT,
            )
        };
        if copied == FALSE {
            return Err(io::Error::last_os_error());
        }
        unsafe { GdiFlush() };
        let stride = self.target.width * BGRA_BYTES;
        let bgra = self.surface.pixels();
        if self.last == *bgra {
            std::thread::sleep(timeout.saturating_sub(started.elapsed()));
            return Ok(Grab::Unchanged);
        }
        self.last.clear();
        self.last.extend_from_slice(bgra);
        let converted = Instant::now();
        nv12.resize(layout.buffer_len(), 0);
        bgra_to_nv12(&self.last, stride, nv12, layout, Rect::whole(layout));
        Ok(Grab::Frame {
            convert: converted.elapsed(),
        })
    }
}

impl Drop for Gdi {
    fn drop(&mut self) {
        unsafe { DeleteDC(self.screen) };
    }
}
