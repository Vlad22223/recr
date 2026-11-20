use crate::locale::tr;
use std::io;
use std::mem::size_of;

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::{
    Direct3D11::*,
    Dxgi::{Common::*, *},
    Gdi::{HBRUSH, HMONITOR},
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, HICON,
};
use windows::core::Interface;

pub struct Output {
    pub adapter: IDXGIAdapter,
    display: IDXGIOutput1,
}

impl Output {
    pub fn for_monitor(monitor: HMONITOR) -> io::Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = win(CreateDXGIFactory1())?;
            for index in 0.. {
                let adapter = match factory.EnumAdapters(index) {
                    Ok(adapter) => adapter,
                    Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(error) => return Err(io::Error::other(error)),
                };
                for output in 0.. {
                    let display = match adapter.EnumOutputs(output) {
                        Ok(display) => display,
                        Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                        Err(error) => return Err(io::Error::other(error)),
                    };
                    let desc = win(display.GetDesc())?;
                    if desc.AttachedToDesktop.as_bool() && desc.Monitor == monitor {
                        return Ok(Self {
                            adapter,
                            display: win(display.cast())?,
                        });
                    }
                }
            }
        }
        Err(io::Error::other(tr(
            "DXGI не нашёл основной экран.",
            "DXGI could not find the primary display.",
        )))
    }
}

pub struct Desktop {
    duplication: IDXGIOutputDuplication,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    size: (u32, u32),
    acquired: bool,
    cursor: Option<POINT>,
    cursor_surface: Option<(ID3D11Texture2D, IDXGISurface1)>,
}

impl Desktop {
    pub fn new(
        output: Output,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> io::Result<Self> {
        let duplication = unsafe { win(output.display.DuplicateOutput(device))? };
        let desc = unsafe { duplication.GetDesc() };
        if !matches!(
            desc.Rotation,
            DXGI_MODE_ROTATION_IDENTITY | DXGI_MODE_ROTATION_UNSPECIFIED
        ) {
            return Err(io::Error::other(tr(
                "Для повёрнутого экрана требуется резервный захват.",
                "A rotated display requires the fallback capture method.",
            )));
        }
        let size = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        if size.0 < 2 || size.1 < 2 {
            return Err(io::Error::other(tr(
                "DXGI вернул некорректный размер экрана.",
                "DXGI reported an invalid display size.",
            )));
        }
        Ok(Self {
            duplication,
            device: device.clone(),
            context: context.clone(),
            size,
            acquired: false,
            cursor: None,
            cursor_surface: None,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn with_frame<T>(
        &mut self,
        consume: impl FnOnce(&ID3D11Texture2D) -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        unsafe {
            // Holding one frame between polls lets DXGI coalesce desktop updates.
            if self.acquired {
                self.acquired = false;
                win(self.duplication.ReleaseFrame())?;
            }
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            match self
                .duplication
                .AcquireNextFrame(0, &mut info, &mut resource)
            {
                Ok(()) => self.acquired = true,
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(error) => return Err(io::Error::other(error)),
            }
            if info.LastMouseUpdateTime != 0 {
                self.cursor = info
                    .PointerPosition
                    .Visible
                    .as_bool()
                    .then_some(info.PointerPosition.Position);
            }
            let input: ID3D11Texture2D = win(resource
                .ok_or_else(|| {
                    io::Error::other(tr("DXGI не вернул кадр.", "DXGI did not return a frame."))
                })?
                .cast())?;
            let texture = self.with_cursor(&input)?.unwrap_or(input);
            consume(&texture).map(Some)
        }
    }

    fn with_cursor(&mut self, input: &ID3D11Texture2D) -> io::Result<Option<ID3D11Texture2D>> {
        let Some(point) = self.cursor else {
            return Ok(None);
        };
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut info) }.is_err()
            || info.flags != CURSOR_SHOWING
            || info.hCursor.is_invalid()
        {
            return Ok(None);
        }
        unsafe {
            if self.cursor_surface.is_none() {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: self.size.0,
                    Height: self.size.1,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    MiscFlags: D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32,
                    ..Default::default()
                };
                let mut texture = None;
                win(self.device.CreateTexture2D(&desc, None, Some(&mut texture)))?;
                let texture = texture.ok_or_else(|| {
                    io::Error::other(tr(
                        "Не удалось создать поверхность для курсора.",
                        "Could not create the cursor surface.",
                    ))
                })?;
                let surface = win(texture.cast())?;
                self.cursor_surface = Some((texture, surface));
            }
            let (texture, surface) = self.cursor_surface.as_ref().unwrap();
            self.context.CopyResource(texture, input);
            let dc = win(surface.GetDC(false))?;
            let draw = DrawIconEx(
                dc,
                point.x,
                point.y,
                HICON(info.hCursor.0),
                0,
                0,
                0,
                HBRUSH::default(),
                DI_NORMAL,
            );
            win(surface.ReleaseDC(None))?;
            win(draw)?;
            Ok(Some(texture.clone()))
        }
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        if self.acquired {
            let _ = unsafe { self.duplication.ReleaseFrame() };
        }
    }
}

fn win<T>(result: windows::core::Result<T>) -> io::Result<T> {
    result.map_err(io::Error::other)
}
