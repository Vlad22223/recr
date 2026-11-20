use crate::locale::{tr, trf};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Graphics::{
    Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
    DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
};
use windows::Win32::Foundation::{E_POINTER, HANDLE, HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::{
    Direct3D::*,
    Direct3D11::*,
    Dxgi::{Common::*, IDXGIDevice},
    Gdi::*,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::WinRT::{
    Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
    Graphics::Capture::IGraphicsCaptureItemInterop,
    RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{GUID, Interface, PCWSTR, factory};

use crate::audio;
use crate::desktop::{Desktop, Output};
use crate::platform::wide;

const TICKS: u64 = 10_000_000;
const GPU_LIMIT: (u32, u32) = (3840, 2160);
pub const QUALITY_PRESETS: [(u32, u32); 3] = [(30, 4_000_000), (30, 8_000_000), (60, 16_000_000)];

pub struct Capture {
    stop: Arc<AtomicBool>,
    lost_mic: Arc<AtomicBool>,
    task: Option<JoinHandle<io::Result<()>>>,
    pub audio: bool,
}

impl Capture {
    pub fn start(
        quality: usize,
        mic: Option<String>,
        directory: PathBuf,
        segment_seconds: Option<u32>,
        hide_border: bool,
    ) -> io::Result<Self> {
        fs::create_dir_all(&directory)?;
        let stop = Arc::new(AtomicBool::new(false));
        let lost_mic = Arc::new(AtomicBool::new(false));
        let stop_signal = stop.clone();
        let loss_signal = lost_mic.clone();
        let (ready_tx, ready_rx) = sync_channel(1);
        let task = thread::Builder::new()
            .name("recr-capture".into())
            .spawn(move || {
                let prepare = || -> io::Result<(_, _, _, _)> {
                    let runtime = Runtime::initialize()?;
                    let microphone = match audio::Capture::open(mic.as_deref()) {
                        Ok(input) => input,
                        Err(_) if mic.is_none() => None,
                        Err(error) => {
                            return Err(io::Error::other(trf!(
                                "Микрофон недоступен. {}",
                                "Microphone unavailable. {}",
                                error
                            )));
                        }
                    };
                    let quality = Quality::from_index(quality);
                    let prepared = match Session::prepare(
                        true,
                        hide_border,
                        &directory,
                        quality,
                        microphone.is_some(),
                    ) {
                        Ok(session) => session,
                        Err(hardware) => Session::prepare(
                            false,
                            hide_border,
                            &directory,
                            quality,
                            microphone.is_some(),
                        )
                        .map_err(|software| {
                            io::Error::other(format!("GPU: {hardware}\nCPU: {software}"))
                        })?,
                    };
                    Ok((runtime, microphone, quality, prepared))
                };
                match prepare() {
                    Ok((_runtime, microphone, quality, session)) => {
                        let _ = ready_tx.send(Ok(microphone.is_some()));
                        session.run(
                            quality,
                            microphone,
                            segment_seconds,
                            &stop_signal,
                            &loss_signal,
                        )
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        Ok(())
                    }
                }
            })?;
        match ready_rx.recv() {
            Ok(Ok(audio)) => Ok(Self {
                stop,
                lost_mic,
                task: Some(task),
                audio,
            }),
            Ok(Err(error)) => {
                let _ = task.join();
                Err(error)
            }
            Err(_) => {
                let _ = task.join();
                Err(io::Error::other(tr(
                    "Поток захвата завершился при запуске.",
                    "Capture stopped during startup.",
                )))
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn audio_lost(&self) -> bool {
        self.lost_mic.load(Ordering::Acquire)
    }

    pub fn stop(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        match self.task.take() {
            Some(task) => task.join().map_err(|_| {
                io::Error::other(tr(
                    "Поток захвата аварийно завершился.",
                    "The capture thread crashed.",
                ))
            })?,
            None => Ok(()),
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct Runtime;
impl Runtime {
    fn initialize() -> io::Result<Self> {
        unsafe {
            win(RoInitialize(RO_INIT_MULTITHREADED))?;
            if let Err(error) = win(MFStartup(MF_VERSION, MFSTARTUP_FULL)) {
                RoUninitialize();
                return Err(error);
            }
        }
        Ok(Self)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            RoUninitialize();
        }
    }
}

#[derive(Clone, Copy)]
struct Quality {
    fps: u32,
    bitrate: u32,
}
impl Quality {
    fn from_index(index: usize) -> Self {
        let (fps, bitrate) = QUALITY_PRESETS
            .get(index)
            .copied()
            .unwrap_or((30, 8_000_000));
        Self { fps, bitrate }
    }

    fn tick(self, frame: u64) -> u64 {
        frame * TICKS / u64::from(self.fps)
    }

    fn slot(self, elapsed: Duration) -> u64 {
        (elapsed.as_nanos() * u128::from(self.fps) / 1_000_000_000) as u64
    }
}

struct Session {
    source: Source,
    writer: Option<Writer>,
    directory: PathBuf,
    sequence: usize,
    frame: u64,
    first_tick: u64,
    epoch: Instant,
}

impl Session {
    fn prepare(
        gpu: bool,
        hide_border: bool,
        directory: &Path,
        quality: Quality,
        audio: bool,
    ) -> io::Result<Self> {
        let mut source = if gpu {
            Source::Gpu(Box::new(Gpu::new(quality, hide_border)?))
        } else {
            Source::Gdi(Gdi::new()?)
        };
        let mut writer = Writer::new(directory, 0, &source, quality, audio)?;
        let timeout = Instant::now() + Duration::from_secs(2);
        let (first, first_tick, epoch) = loop {
            if let Some(buffer) = source.frame()? {
                break (buffer, audio::clock_100ns(), Instant::now());
            }
            if Instant::now() >= timeout {
                return Err(io::Error::other(tr(
                    "Первый кадр экрана не получен.",
                    "No initial screen frame was received.",
                )));
            }
            thread::sleep(Duration::from_millis(8));
        };
        writer.video(first, 0, quality.tick(1))?;
        Ok(Self {
            source,
            writer: Some(writer),
            directory: directory.to_owned(),
            sequence: 0,
            frame: 1,
            first_tick,
            epoch,
        })
    }

    fn run(
        mut self,
        quality: Quality,
        input: Option<audio::Capture>,
        segment_seconds: Option<u32>,
        stop: &AtomicBool,
        lost_mic: &AtomicBool,
    ) -> io::Result<()> {
        let segment_frames = segment_seconds.map(|n| u64::from(n.max(1)) * u64::from(quality.fps));
        let mut audio = AudioTimeline::new(input, self.first_tick);
        let audio_track = audio.input.is_some();
        let mut segment_frame = 0;

        let result = (|| -> io::Result<()> {
            while !stop.load(Ordering::Acquire) {
                let deadline = self.epoch + Duration::from_nanos(quality.tick(self.frame) * 100);
                let now = Instant::now();
                if deadline > now {
                    thread::sleep(deadline - now);
                }
                if stop.load(Ordering::Acquire) {
                    break;
                }
                audio.poll(lost_mic);
                // Prepare the next image before the MP4 finalization to keep its capture time close
                // to the segment boundary.
                let Some(buffer) = self.source.frame()? else {
                    continue;
                };
                // Overdue frames are dropped instead of compressing elapsed time into a shorter
                // recording. The same wall clock drives the video positions and microphone audio.
                let slot = quality.slot(self.epoch.elapsed()).max(self.frame);
                if slot - self.frame > u64::from(quality.fps) * 3 {
                    return Err(io::Error::other(tr(
                        "Захват был надолго приостановлен.",
                        "Capture was suspended for too long.",
                    )));
                }
                self.frame = slot;
                let time = quality.tick(slot);
                if segment_frames.is_some_and(|frames| self.frame - segment_frame >= frames) {
                    self.flush_audio(&mut audio, quality.tick(segment_frame), time)?;
                    self.writer.take().unwrap().complete()?;
                    self.sequence += 1;
                    self.writer = Some(Writer::new(
                        &self.directory,
                        self.sequence,
                        &self.source,
                        quality,
                        audio_track,
                    )?);
                    segment_frame = self.frame;
                }
                let writer = self.writer.as_mut().unwrap();
                writer.video(
                    buffer,
                    time - quality.tick(segment_frame),
                    quality.tick(self.frame + 1) - time,
                )?;
                self.frame += 1;
                // The microphone endpoint can deliver its most recent packet a few frames late.
                let available = time
                    .saturating_sub(600_000)
                    .max(quality.tick(segment_frame));
                self.flush_audio(&mut audio, quality.tick(segment_frame), available)?;
            }
            Ok(())
        })();
        audio.poll(lost_mic);
        let audio_result = self.flush_audio(
            &mut audio,
            quality.tick(segment_frame),
            quality.tick(self.frame),
        );
        let file_result = self.writer.take().map_or(Ok(()), Writer::complete);
        result.and(audio_result).and(file_result)
    }

    fn flush_audio(
        &mut self,
        timeline: &mut AudioTimeline,
        segment_time: u64,
        until: u64,
    ) -> io::Result<()> {
        if let Some(writer) = &self.writer {
            if writer.audio.is_some() {
                timeline.write_to(writer, segment_time, until)?;
            }
        }
        Ok(())
    }
}

struct AudioTimeline {
    input: Option<audio::Capture>,
    origin: u64,
    cursor: u64,
    blocks: VecDeque<audio::AudioBlock>,
    pcm: Vec<u8>,
}

impl AudioTimeline {
    fn new(input: Option<audio::Capture>, first_tick: u64) -> Self {
        let origin = pcm_at(first_tick);
        Self {
            input,
            origin,
            cursor: origin,
            blocks: VecDeque::new(),
            pcm: Vec::new(),
        }
    }

    fn poll(&mut self, lost: &AtomicBool) {
        let Some(input) = &mut self.input else { return };
        match input.poll() {
            Ok(blocks) => self.blocks.extend(blocks),
            Err(_) => {
                lost.store(true, Ordering::Release);
                self.input = None;
            }
        }
    }

    fn write_to(&mut self, writer: &Writer, segment_time: u64, until: u64) -> io::Result<()> {
        let align = audio::BLOCK_ALIGN as usize;
        let segment_start = self.origin + pcm_at(segment_time);
        let target = self.origin + pcm_at(until);
        while self.cursor < target {
            let end = target.min(self.cursor + 4096);
            self.pcm.clear();
            self.pcm.resize((end - self.cursor) as usize * align, 0);
            while let Some(block) = self.blocks.front() {
                let block_start = pcm_at(block.qpc_100ns);
                let block_end = block_start + u64::from(block.frames);
                if block_end <= self.cursor {
                    self.blocks.pop_front();
                    continue;
                }
                if block_start >= end {
                    break;
                }
                let from = self.cursor.max(block_start);
                let to = end.min(block_end);
                let length = (to - from) as usize * align;
                let destination = (from - self.cursor) as usize * align;
                let source = (from - block_start) as usize * align;
                if let Some(data) = block.samples.get(source..source + length) {
                    self.pcm[destination..destination + length].copy_from_slice(data);
                }
                if block_end <= end {
                    self.blocks.pop_front();
                } else {
                    break;
                }
            }
            let start = pcm_time(self.cursor.saturating_sub(segment_start));
            let finish = pcm_time(end - segment_start);
            writer.audio(&self.pcm, start, finish - start)?;
            self.cursor = end;
        }
        Ok(())
    }
}

fn pcm_at(time: u64) -> u64 {
    (u128::from(time) * u128::from(audio::SAMPLE_RATE) / u128::from(TICKS)) as u64
}
fn pcm_time(frames: u64) -> u64 {
    (u128::from(frames) * u128::from(TICKS) / u128::from(audio::SAMPLE_RATE)) as u64
}

struct Writer {
    sink: IMFSinkWriter,
    video: u32,
    audio: Option<u32>,
    file: PendingFile,
    frames: u64,
}

struct PendingFile {
    part: PathBuf,
    result: PathBuf,
}

impl Writer {
    fn new(
        directory: &Path,
        sequence: usize,
        source: &Source,
        quality: Quality,
        audio: bool,
    ) -> io::Result<Self> {
        let part = directory.join(format!("pending-{sequence:09}.mp4"));
        let result = directory.join(format!("seg-{sequence:09}.mp4"));
        let attributes = attributes(4)?;
        let (width, height) = source.size();
        let subtype = if let Source::Gpu(gpu) = source {
            unsafe {
                win(attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1))?;
                win(attributes.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &gpu.manager))?;
            }
            MFVideoFormat_NV12
        } else {
            MFVideoFormat_RGB32
        };
        unsafe {
            win(attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4))?;
        }
        let name = wide(part.as_os_str());
        let file = PendingFile { part, result };
        (|| -> io::Result<Self> {
            unsafe {
                let sink = win(MFCreateSinkWriterFromURL(
                    PCWSTR(name.as_ptr()),
                    None,
                    &attributes,
                ))?;
                let video_output = video_type(MFVideoFormat_H264, width, height, quality)?;
                win(video_output.SetUINT32(&MF_MT_AVG_BITRATE, quality.bitrate))?;
                win(video_output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32))?;
                let video = win(sink.AddStream(&video_output))?;
                let input = video_type(subtype, width, height, quality)?;
                if subtype == MFVideoFormat_RGB32 {
                    win(input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4))?;
                }
                win(sink.SetInputMediaType(video, &input, None))?;
                let audio = if audio {
                    let output = audio_type(true)?;
                    let input = audio_type(false)?;
                    let index = win(sink.AddStream(&output))?;
                    win(sink.SetInputMediaType(index, &input, None))?;
                    Some(index)
                } else {
                    None
                };
                win(sink.BeginWriting())?;
                Ok(Self {
                    sink,
                    video,
                    audio,
                    file,
                    frames: 0,
                })
            }
        })()
    }

    fn video(&mut self, buffer: IMFMediaBuffer, time: u64, duration: u64) -> io::Result<()> {
        let sample = sample(&buffer, time, duration)?;
        unsafe {
            win(self.sink.WriteSample(self.video, &sample))?;
        }
        self.frames += 1;
        Ok(())
    }

    fn audio(&self, data: &[u8], time: u64, duration: u64) -> io::Result<()> {
        if let Some(index) = self.audio {
            let sample = sample(&memory(data)?, time, duration)?;
            unsafe {
                win(self.sink.WriteSample(index, &sample))?;
            }
        }
        Ok(())
    }

    fn complete(self) -> io::Result<()> {
        let Self {
            sink, file, frames, ..
        } = self;
        let result = if frames > 0 {
            unsafe { win(sink.Finalize()) }
        } else {
            Ok(())
        };
        drop(sink);
        result?;
        if frames > 0 {
            fs::rename(&file.part, &file.result)?;
        }
        Ok(())
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.part);
    }
}

fn video_type(
    subtype: GUID,
    width: u32,
    height: u32,
    quality: Quality,
) -> io::Result<IMFMediaType> {
    unsafe {
        let ty = win(MFCreateMediaType())?;
        win(ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video))?;
        win(ty.SetGUID(&MF_MT_SUBTYPE, &subtype))?;
        win(ty.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(width) << 32) | u64::from(height),
        ))?;
        win(ty.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(quality.fps) << 32) | 1))?;
        win(ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1))?;
        win(ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))?;
        Ok(ty)
    }
}

fn audio_type(output: bool) -> io::Result<IMFMediaType> {
    unsafe {
        let ty = win(MFCreateMediaType())?;
        win(ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio))?;
        win(ty.SetGUID(
            &MF_MT_SUBTYPE,
            if output {
                &MFAudioFormat_AAC
            } else {
                &MFAudioFormat_PCM
            },
        ))?;
        win(ty.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(audio::CHANNELS)))?;
        win(ty.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio::SAMPLE_RATE))?;
        win(ty.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16))?;
        if output {
            win(ty.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 12_000))?;
        } else {
            let align = audio::BLOCK_ALIGN as u32;
            win(ty.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, align))?;
            win(ty.SetUINT32(
                &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
                audio::SAMPLE_RATE * align,
            ))?;
        }
        Ok(ty)
    }
}

fn memory(data: &[u8]) -> io::Result<IMFMediaBuffer> {
    unsafe {
        let len = data.len() as u32;
        let buffer = win(MFCreateMemoryBuffer(len))?;
        let mut bytes = ptr::null_mut();
        win(buffer.Lock(&mut bytes, None, None))?;
        ptr::copy_nonoverlapping(data.as_ptr(), bytes, data.len());
        win(buffer.Unlock())?;
        win(buffer.SetCurrentLength(len))?;
        Ok(buffer)
    }
}

fn sample(buffer: &IMFMediaBuffer, time: u64, duration: u64) -> io::Result<IMFSample> {
    unsafe {
        let sample = win(MFCreateSample())?;
        win(sample.AddBuffer(buffer))?;
        win(sample.SetSampleTime(time as i64))?;
        win(sample.SetSampleDuration(duration as i64))?;
        Ok(sample)
    }
}

fn attributes(count: u32) -> io::Result<IMFAttributes> {
    let mut attrs = None;
    unsafe {
        win(MFCreateAttributes(&mut attrs, count))?;
    }
    attrs.ok_or_else(missing)
}

enum Source {
    Gpu(Box<Gpu>),
    Gdi(Gdi),
}
impl Source {
    fn frame(&mut self) -> io::Result<Option<IMFMediaBuffer>> {
        match self {
            Self::Gpu(source) => source.frame(),
            Self::Gdi(source) => source.frame().map(Some),
        }
    }
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Gpu(source) => source.size,
            Self::Gdi(source) => source.size,
        }
    }
}

struct Gpu {
    input: GpuInput,
    converter: VideoConverter,
    manager: IMFDXGIDeviceManager,
    size: (u32, u32),
    latest: Option<IMFMediaBuffer>,
}

enum GpuInput {
    Graphics(GraphicsFrames),
    Desktop(Desktop),
}

impl Gpu {
    fn new(quality: Quality, hide_border: bool) -> io::Result<Self> {
        unsafe {
            let monitor = primary_monitor();
            let desktop = hide_border
                .then(|| Output::for_monitor(monitor))
                .transpose()?;
            let adapter = desktop.as_ref().map(|screen| &screen.adapter);
            let driver = if adapter.is_some() {
                D3D_DRIVER_TYPE_UNKNOWN
            } else {
                D3D_DRIVER_TYPE_HARDWARE
            };
            let mut device = None;
            let mut context = None;
            win(D3D11CreateDevice(
                adapter,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            ))?;
            let device = device.ok_or_else(missing)?;
            let immediate = context.ok_or_else(missing)?;
            let multi: ID3D11Multithread = win(immediate.cast())?;
            let _ = multi.SetMultithreadProtected(true);
            let (input, original) = if let Some(desktop) = desktop {
                let capture = Desktop::new(desktop, &device, &immediate)?;
                let size = capture.size();
                (GpuInput::Desktop(capture), size)
            } else {
                let (capture, size) = GraphicsFrames::new(&device, monitor)?;
                (GpuInput::Graphics(capture), size)
            };
            let size = fit(original, GPU_LIMIT);
            let converter =
                VideoConverter::new(device.clone(), &immediate, original, size, quality)?;
            let mut token = 0;
            let mut manager = None;
            win(MFCreateDXGIDeviceManager(&mut token, &mut manager))?;
            let manager = manager.ok_or_else(missing)?;
            win(manager.ResetDevice(&device, token))?;
            Ok(Self {
                input,
                converter,
                manager,
                size,
                latest: None,
            })
        }
    }

    fn frame(&mut self) -> io::Result<Option<IMFMediaBuffer>> {
        let convert = |texture: &ID3D11Texture2D| self.converter.convert(texture);
        let next = match &mut self.input {
            GpuInput::Graphics(capture) => capture.with_frame(convert)?,
            GpuInput::Desktop(capture) => capture.with_frame(convert)?,
        };
        if let Some(frame) = next {
            self.latest = Some(frame);
        }
        Ok(self.latest.clone())
    }
}

struct GraphicsFrames {
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
}

impl GraphicsFrames {
    fn new(device: &ID3D11Device, monitor: HMONITOR) -> io::Result<(Self, (u32, u32))> {
        unsafe {
            let interop = win(factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>())?;
            let item: GraphicsCaptureItem = win(interop.CreateForMonitor(monitor))?;
            let original = win(item.Size())?;
            let dxgi: IDXGIDevice = win(device.cast())?;
            let winrt: IDirect3DDevice =
                win(win(CreateDirect3D11DeviceFromDXGIDevice(&dxgi))?.cast())?;
            let pool = win(Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                original,
            ))?;
            let session = win(pool.CreateCaptureSession(&item))?;
            let _ = session.SetIsCursorCaptureEnabled(true);
            win(session.StartCapture())?;
            Ok((
                Self {
                    _item: item,
                    pool,
                    session,
                },
                (original.Width as u32, original.Height as u32),
            ))
        }
    }

    fn with_frame<T>(
        &self,
        consume: impl FnOnce(&ID3D11Texture2D) -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        let mut newest = None;
        for _ in 0..3 {
            match self.pool.TryGetNextFrame() {
                Ok(frame) => newest = Some(frame),
                // WinRT reports an empty pool using S_OK with a null pointer.
                Err(error) if error.code().is_ok() || error.code() == E_POINTER => break,
                Err(error) => return Err(io::Error::other(error)),
            }
        }
        let Some(frame) = newest else { return Ok(None) };
        let surface: IDirect3DDxgiInterfaceAccess = win(win(frame.Surface())?.cast())?;
        let texture: ID3D11Texture2D = unsafe { win(surface.GetInterface())? };
        let result = consume(&texture);
        let _ = frame.Close();
        result.map(Some)
    }
}

impl Drop for GraphicsFrames {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

struct VideoConverter {
    device: ID3D11Device,
    context: ID3D11VideoContext,
    video: ID3D11VideoDevice,
    video_enum: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    size: (u32, u32),
}

impl VideoConverter {
    fn new(
        device: ID3D11Device,
        immediate: &ID3D11DeviceContext,
        original: (u32, u32),
        size: (u32, u32),
        quality: Quality,
    ) -> io::Result<Self> {
        unsafe {
            let video: ID3D11VideoDevice = win(device.cast())?;
            let context: ID3D11VideoContext = win(immediate.cast())?;
            let rate = DXGI_RATIONAL {
                Numerator: quality.fps,
                Denominator: 1,
            };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: original.0,
                InputHeight: original.1,
                OutputFrameRate: rate,
                OutputWidth: size.0,
                OutputHeight: size.1,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let video_enum = win(video.CreateVideoProcessorEnumerator(&content))?;
            let processor = win(video.CreateVideoProcessor(&video_enum, 0))?;
            context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            if let Ok(v1) = context.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                v1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
            }
            Ok(Self {
                device,
                context,
                video,
                video_enum,
                processor,
                size,
            })
        }
    }

    fn convert(&self, input: &ID3D11Texture2D) -> io::Result<IMFMediaBuffer> {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.size.0,
                Height: self.size.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            let mut nv12 = None;
            win(self.device.CreateTexture2D(&desc, None, Some(&mut nv12)))?;
            let nv12 = nv12.ok_or_else(missing)?;
            let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut source = None;
            let mut target = None;
            win(self.video.CreateVideoProcessorInputView(
                input,
                &self.video_enum,
                &input_desc,
                Some(&mut source),
            ))?;
            win(self.video.CreateVideoProcessorOutputView(
                &nv12,
                &self.video_enum,
                &output_desc,
                Some(&mut target),
            ))?;
            let target = target.ok_or_else(missing)?;
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(source),
                ..Default::default()
            }];
            let converted = self
                .context
                .VideoProcessorBlt(&self.processor, &target, 0, &streams);
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            win(converted)?;
            let buffer = win(MFCreateDXGISurfaceBuffer(
                &ID3D11Texture2D::IID,
                &nv12,
                0,
                false,
            ))?;
            win(buffer.SetCurrentLength(self.size.0 * self.size.1 * 3 / 2))?;
            Ok(buffer)
        }
    }
}

struct Gdi {
    display: HDC,
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pixels: *mut u8,
    screen: RECT,
    size: (u32, u32),
    pointer: Option<(HCURSOR, i32, i32)>,
}

impl Gdi {
    fn new() -> io::Result<Self> {
        unsafe {
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(primary_monitor(), &mut info).as_bool() {
                return Err(io::Error::last_os_error());
            }
            let screen = info.rcMonitor;
            let size = fit(
                (
                    (screen.right - screen.left) as u32,
                    (screen.bottom - screen.top) as u32,
                ),
                (1920, 1080),
            );
            let mut capture = Self {
                display: GetDC(HWND::default()),
                dc: HDC::default(),
                bitmap: HBITMAP::default(),
                previous: HGDIOBJ::default(),
                pixels: ptr::null_mut(),
                screen,
                size,
                pointer: None,
            };
            if capture.display.is_invalid() {
                return Err(io::Error::last_os_error());
            }
            capture.dc = CreateCompatibleDC(capture.display);
            if capture.dc.is_invalid() {
                return Err(io::Error::last_os_error());
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size.0 as i32,
                    biHeight: -(size.1 as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut data: *mut c_void = ptr::null_mut();
            capture.bitmap = win(CreateDIBSection(
                capture.dc,
                &info,
                DIB_RGB_COLORS,
                &mut data,
                HANDLE::default(),
                0,
            ))?;
            capture.previous = SelectObject(capture.dc, capture.bitmap);
            capture.pixels = data.cast();
            SetStretchBltMode(capture.dc, COLORONCOLOR);
            Ok(capture)
        }
    }

    fn frame(&mut self) -> io::Result<IMFMediaBuffer> {
        unsafe {
            let width = self.screen.right - self.screen.left;
            let height = self.screen.bottom - self.screen.top;
            let options = SRCCOPY | CAPTUREBLT;
            if (width as u32, height as u32) == self.size {
                win(BitBlt(
                    self.dc,
                    0,
                    0,
                    width,
                    height,
                    self.display,
                    self.screen.left,
                    self.screen.top,
                    options,
                ))?;
            } else if !StretchBlt(
                self.dc,
                0,
                0,
                self.size.0 as i32,
                self.size.1 as i32,
                self.display,
                self.screen.left,
                self.screen.top,
                width,
                height,
                options,
            )
            .as_bool()
            {
                return Err(io::Error::last_os_error());
            }
            self.cursor(width, height);
            GdiFlush().ok()?;
            memory(std::slice::from_raw_parts(
                self.pixels,
                (self.size.0 * self.size.1 * 4) as usize,
            ))
        }
    }

    fn cursor(&mut self, width: i32, height: i32) {
        unsafe {
            let mut cursor = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            if GetCursorInfo(&mut cursor).is_err() || cursor.flags != CURSOR_SHOWING {
                return;
            }
            if self.pointer.is_none_or(|shape| shape.0 != cursor.hCursor) {
                let mut icon = ICONINFO::default();
                if GetIconInfo(cursor.hCursor, &mut icon).is_err() {
                    return;
                }
                self.pointer = Some((cursor.hCursor, icon.xHotspot as i32, icon.yHotspot as i32));
                if !icon.hbmMask.is_invalid() {
                    let _ = DeleteObject(icon.hbmMask);
                }
                if !icon.hbmColor.is_invalid() {
                    let _ = DeleteObject(icon.hbmColor);
                }
            }
            if let Some((handle, hot_x, hot_y)) = self.pointer {
                let x =
                    (cursor.ptScreenPos.x - self.screen.left - hot_x) * self.size.0 as i32 / width;
                let y =
                    (cursor.ptScreenPos.y - self.screen.top - hot_y) * self.size.1 as i32 / height;
                let _ = DrawIconEx(
                    self.dc,
                    x,
                    y,
                    handle,
                    0,
                    0,
                    0,
                    None,
                    DI_NORMAL | DI_DEFAULTSIZE,
                );
            }
        }
    }
}
impl Drop for Gdi {
    fn drop(&mut self) {
        unsafe {
            if !self.dc.is_invalid() {
                if !self.previous.is_invalid() {
                    SelectObject(self.dc, self.previous);
                }
                let _ = DeleteDC(self.dc);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap);
            }
            if !self.display.is_invalid() {
                ReleaseDC(HWND::default(), self.display);
            }
        }
    }
}

fn primary_monitor() -> HMONITOR {
    unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }
}

pub fn planned_size() -> (u32, u32) {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(primary_monitor(), &mut info) }.as_bool() {
        return GPU_LIMIT;
    }
    let screen = info.rcMonitor;
    fit(
        (
            (screen.right - screen.left) as u32,
            (screen.bottom - screen.top) as u32,
        ),
        GPU_LIMIT,
    )
}

fn fit(size: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let scale = (max.0 as f64 / size.0 as f64)
        .min(max.1 as f64 / size.1 as f64)
        .min(1.0);
    (
        (size.0 as f64 * scale).round() as u32 & !1,
        (size.1 as f64 * scale).round() as u32 & !1,
    )
}

fn missing() -> io::Error {
    io::Error::other(tr(
        "Windows не выдала объект захвата.",
        "Windows did not provide a capture object.",
    ))
}
fn win<T>(result: windows::core::Result<T>) -> io::Result<T> {
    result.map_err(io::Error::other)
}
