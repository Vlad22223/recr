use crate::locale::tr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{GUID, Interface, PCWSTR, PROPVARIANT};
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

#[derive(Clone)]
pub struct Media {
    pub videos: PathBuf,
}

pub const PREVIEW_WIDTH: i32 = 160;
pub const PREVIEW_HEIGHT: i32 = 90;

const SECOND: i64 = 10_000_000;
const ANY: u32 = MF_SOURCE_READER_ANY_STREAM.0 as u32;
const ALL: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
const SOURCE: u32 = MF_SOURCE_READER_MEDIASOURCE.0 as u32;
const EOS: u32 = MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;
const STREAM_TICK: u32 = MF_SOURCE_READERF_STREAMTICK.0 as u32;
const VIDEO: usize = 0;
const AUDIO: usize = 1;

struct Foundation(bool);

impl Foundation {
    fn start() -> io::Result<Self> {
        unsafe {
            let apartment = CoInitializeEx(None, COINIT_MULTITHREADED);
            if apartment.is_err() && apartment != RPC_E_CHANGED_MODE {
                return Err(io::Error::other(windows::core::Error::from(apartment)));
            }
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                if apartment.is_ok() {
                    CoUninitialize();
                }
                return mf(Err(error));
            }
            Ok(Self(apartment.is_ok()))
        }
    }
}

impl Drop for Foundation {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            if self.0 {
                CoUninitialize();
            }
        }
    }
}

impl Media {
    pub fn duration(&self, path: &Path) -> io::Result<f64> {
        let _foundation = Foundation::start()?;
        let reader = Reader::open(path, false)?;
        Ok(reader.duration as f64 / SECOND as f64)
    }

    pub fn preview(&self, path: &Path, seconds: f64) -> io::Result<Vec<u8>> {
        let _foundation = Foundation::start()?;
        let reader = Reader::open(path, true)?;
        let index = reader.streams[VIDEO].index;
        let requested = ticks(seconds).clamp(0, (reader.duration - 1).max(0));
        reader.enable(false)?;
        let video = reader.decode(index, MFMediaType_Video, MFVideoFormat_RGB32)?;
        reader.seek(requested)?;
        let format = mf(unsafe { video.GetUINT64(&MF_MT_FRAME_SIZE) })?;
        let (width, height) = unpack(format);
        if width == 0 || height == 0 {
            return Err(io::Error::other(tr(
                "Кадр недоступен.",
                "Frame unavailable.",
            )));
        }

        let mut prior = None;
        loop {
            let packet = reader.read(index)?;
            if let Some(sample) = packet.sample {
                if packet.time >= requested {
                    return thumbnail(&sample, &video, width, height);
                }
                prior = Some(sample);
            }
            if packet.flags & EOS != 0 {
                return prior
                    .and_then(|sample| thumbnail(&sample, &video, width, height).ok())
                    .ok_or_else(|| io::Error::other(tr("Кадр недоступен.", "Frame unavailable.")));
            }
        }
    }

    pub fn edit(&self, source: &Path, keep: &[(f64, f64)]) -> io::Result<PathBuf> {
        let _foundation = Foundation::start()?;
        let duration = Reader::open(source, false)?.duration;
        let keep = checked_ranges(keep, duration)?;
        let stem = source.file_stem().unwrap_or_default().to_string_lossy();
        let stem: String = stem.chars().take(80).collect();
        let suffix = if keep.len() == 1 {
            tr("обрезка", "trim")
        } else {
            tr("монтаж", "edit")
        };
        self.write_video(&format!("{stem} · {suffix}"), |output| {
            let inputs = [Input {
                source: source.to_owned(),
                ranges: keep,
            }];
            copy_with_fallback(output, |hardware| transcode(output, &inputs, hardware))
        })
    }

    pub fn export(&self, snapshot: &Path, name: &str) -> io::Result<PathBuf> {
        let mut segments = fs::read_dir(snapshot)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "mp4")
                    && path
                        .file_stem()
                        .is_some_and(|name| name.to_string_lossy().starts_with("seg-"))
            })
            .collect::<Vec<_>>();
        segments.sort_unstable();
        let _foundation = Foundation::start()?;
        self.write_video(name, |output| match segments.as_slice() {
            [] => Err(io::Error::other(tr(
                "Нет фрагментов для сохранения.",
                "No segments to save.",
            ))),
            [one] => fs::hard_link(one, output).or_else(|_| fs::copy(one, output).map(|_| ())),
            _ => match remux(output, &segments) {
                Ok(()) => Ok(()),
                Err(_) => {
                    let _ = fs::remove_file(output);
                    let inputs = segments
                        .iter()
                        .map(|source| {
                            Ok(Input {
                                source: source.clone(),
                                ranges: vec![(0, Reader::open(source, false)?.duration)],
                            })
                        })
                        .collect::<io::Result<Vec<_>>>()?;
                    copy_with_fallback(output, |hardware| transcode(output, &inputs, hardware))
                }
            },
        })
    }

    fn write_video(
        &self,
        name: &str,
        write: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<PathBuf> {
        fs::create_dir_all(&self.videos)?;
        let part = self.videos.join(format!(".recr-{}.part.mp4", unique_id()));
        let result = (|| {
            write(&part)?;
            Reader::open(&part, false)?;
            for index in 0..1000 {
                let name = if index == 0 {
                    name.to_owned()
                } else {
                    format!("{name} ({index})")
                };
                let output = self.videos.join(format!("{name}.mp4"));
                let moved = unsafe {
                    windows_sys::Win32::Storage::FileSystem::MoveFileW(
                        crate::platform::wide(&part).as_ptr(),
                        crate::platform::wide(&output).as_ptr(),
                    )
                };
                if moved != 0 {
                    return Ok(output);
                }
                let error = io::Error::last_os_error();
                if !output.exists() {
                    return Err(error);
                }
            }
            Err(io::Error::other(tr(
                "Слишком много файлов с таким именем.",
                "Too many files with this name.",
            )))
        })();
        if result.is_err() {
            let _ = fs::remove_file(part);
        }
        result
    }
}

struct Stream {
    index: u32,
    native: IMFMediaType,
}

struct Reader {
    reader: IMFSourceReader,
    streams: Vec<Stream>,
    duration: i64,
}

struct Packet {
    stream: u32,
    time: i64,
    flags: u32,
    sample: Option<IMFSample>,
}

impl Reader {
    fn open(path: &Path, rgb: bool) -> io::Result<Self> {
        let wide = crate::platform::wide(path);
        let reader = unsafe {
            let attributes = attributes(1)?;
            if rgb {
                mf(attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1))?;
            }
            mf(MFCreateSourceReaderFromURL(
                PCWSTR(wide.as_ptr()),
                &attributes,
            ))?
        };
        let streams = unsafe {
            let mut video = None;
            let mut audio = None;
            for index in 0.. {
                let Ok(native) = reader.GetNativeMediaType(index, 0) else {
                    break;
                };
                let major = mf(native.GetGUID(&MF_MT_MAJOR_TYPE))?;
                if major == MFMediaType_Video && video.is_none() {
                    video = Some(Stream { index, native });
                } else if major == MFMediaType_Audio && audio.is_none() {
                    audio = Some(Stream { index, native });
                }
                if video.is_some() && audio.is_some() {
                    break;
                }
            }
            [video.ok_or_else(|| {
                io::Error::other(tr("В файле нет видео.", "The file contains no video."))
            })?]
            .into_iter()
            .chain(audio)
            .collect::<Vec<_>>()
        };
        let duration = unsafe {
            let value = mf(reader.GetPresentationAttribute(SOURCE, &MF_PD_DURATION))?;
            mf(u64::try_from(&value))?
        };
        let duration = i64::try_from(duration)
            .ok()
            .filter(|&ticks| ticks > 0)
            .ok_or_else(|| {
                io::Error::other(tr(
                    "Не удалось прочитать длительность видео.",
                    "Could not read the video duration.",
                ))
            })?;
        Ok(Self {
            reader,
            streams,
            duration,
        })
    }

    fn enable(&self, audio: bool) -> io::Result<()> {
        unsafe {
            mf(self.reader.SetStreamSelection(ALL, false))?;
            mf(self
                .reader
                .SetStreamSelection(self.streams[VIDEO].index, true))?;
            if let Some(stream) = self.streams.get(AUDIO).filter(|_| audio) {
                mf(self.reader.SetStreamSelection(stream.index, true))?;
            }
        }
        Ok(())
    }

    fn decode(&self, stream: u32, major: GUID, subtype: GUID) -> io::Result<IMFMediaType> {
        let requested = media_type(major, subtype)?;
        unsafe {
            mf(self.reader.SetCurrentMediaType(stream, None, &requested))?;
            mf(self.reader.GetCurrentMediaType(stream))
        }
    }

    fn seek(&self, time: i64) -> io::Result<()> {
        let position = PROPVARIANT::from(time);
        mf(unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &position) })
    }

    fn read(&self, index: u32) -> io::Result<Packet> {
        let mut stream = 0;
        let mut time = 0;
        let mut flags = 0;
        let mut sample = None;
        unsafe {
            mf(self.reader.ReadSample(
                index,
                0,
                Some(&mut stream),
                Some(&mut flags),
                Some(&mut time),
                Some(&mut sample),
            ))?;
        }
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err(io::Error::other(tr(
                "Не удалось прочитать кадр.",
                "Could not read the frame.",
            )));
        }
        Ok(Packet {
            stream,
            time,
            flags,
            sample,
        })
    }

    fn track(&self, stream_index: u32) -> Option<usize> {
        self.streams
            .iter()
            .position(|stream| stream.index == stream_index)
    }
}

struct Writer {
    writer: IMFSinkWriter,
    indices: Vec<u32>,
}

impl Writer {
    fn new(
        output: &Path,
        types: &[(&IMFMediaType, &IMFMediaType)],
        hardware: bool,
    ) -> io::Result<Self> {
        let wide = crate::platform::wide(output);
        unsafe {
            let attributes = attributes(1)?;
            if hardware {
                mf(attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1))?;
            }
            let writer = mf(MFCreateSinkWriterFromURL(
                PCWSTR(wide.as_ptr()),
                None,
                &attributes,
            ))?;
            let mut indices = Vec::with_capacity(types.len());
            for &(input, output) in types {
                let index = mf(writer.AddStream(output))?;
                mf(writer.SetInputMediaType(index, input, None))?;
                indices.push(index);
            }
            mf(writer.BeginWriting())?;
            Ok(Self { writer, indices })
        }
    }

    fn sample(&self, track: usize, sample: &IMFSample, time: i64, duration: i64) -> io::Result<()> {
        unsafe {
            mf(sample.SetSampleTime(time))?;
            if duration > 0 {
                mf(sample.SetSampleDuration(duration))?;
            }
            mf(self.writer.WriteSample(self.indices[track], sample))
        }
    }

    fn tick(&self, track: usize, time: i64) -> io::Result<()> {
        mf(unsafe { self.writer.SendStreamTick(self.indices[track], time) })
    }

    fn finish(self) -> io::Result<()> {
        mf(unsafe { self.writer.Finalize() })
    }
}

fn remux(output: &Path, segments: &[PathBuf]) -> io::Result<()> {
    let first = Reader::open(&segments[0], false)?;
    let types = first
        .streams
        .iter()
        .map(|stream| (&stream.native, &stream.native))
        .collect::<Vec<_>>();
    let writer = Writer::new(output, &types, false)?;
    let mut offset = 0;
    for source in segments {
        let input = Reader::open(source, false)?;
        if input.streams.len() != first.streams.len()
            || input
                .streams
                .iter()
                .zip(&first.streams)
                .any(|(left, right)| !remux_compatible(&left.native, &right.native))
        {
            return Err(io::Error::other(tr(
                "Форматы фрагментов изменились.",
                "The segment formats changed.",
            )));
        }
        input.enable(true)?;
        let mut done = [false, input.streams.len() == 1];
        let mut duration = input.duration;
        while !done.iter().all(|ended| *ended) {
            let packet = input.read(ANY)?;
            let Some(track) = input.track(packet.stream) else {
                if packet.flags & EOS != 0 {
                    break;
                }
                continue;
            };
            if packet.flags & EOS != 0 {
                done[track] = true;
            }
            if let Some(sample) = packet.sample {
                unsafe {
                    if let Ok(decode_time) = sample.GetUINT64(&MFSampleExtension_DecodeTimestamp) {
                        mf(sample.SetUINT64(
                            &MFSampleExtension_DecodeTimestamp,
                            (decode_time as i64).saturating_add(offset) as u64,
                        ))?;
                    }
                    let length = sample.GetSampleDuration().unwrap_or(0).max(0);
                    duration = duration.max(packet.time.saturating_add(length));
                    writer.sample(track, &sample, offset.saturating_add(packet.time), length)?;
                }
            } else if packet.flags & STREAM_TICK != 0 {
                writer.tick(track, offset.saturating_add(packet.time))?;
            }
        }
        offset = offset.saturating_add(duration);
    }
    writer.finish()
}

fn remux_compatible(a: &IMFMediaType, b: &IMFMediaType) -> bool {
    unsafe {
        if a.GetGUID(&MF_MT_SUBTYPE).ok() != b.GetGUID(&MF_MT_SUBTYPE).ok() {
            return false;
        }
        let size = a.GetUINT64(&MF_MT_FRAME_SIZE).ok();
        if size.is_some() {
            let eq_header = |media: &IMFMediaType| blob(media, &MF_MT_MPEG_SEQUENCE_HEADER);
            return size == b.GetUINT64(&MF_MT_FRAME_SIZE).ok() && eq_header(a) == eq_header(b);
        }
        a.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).ok()
            == b.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).ok()
            && a.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).ok()
                == b.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).ok()
            && blob(a, &MF_MT_USER_DATA) == blob(b, &MF_MT_USER_DATA)
    }
}

fn blob(media: &IMFMediaType, guid: &GUID) -> Option<Vec<u8>> {
    unsafe {
        let size = media.GetBlobSize(guid).ok()?;
        if size > 1024 * 1024 {
            return None;
        }
        let mut bytes = vec![0; size as usize];
        media.GetBlob(guid, &mut bytes, None).ok()?;
        Some(bytes)
    }
}

struct Input {
    source: PathBuf,
    ranges: Vec<(i64, i64)>,
}

struct Decoder {
    reader: Reader,
    video: IMFMediaType,
    audio: Option<DecodedAudio>,
}

struct DecodedAudio {
    media: IMFMediaType,
    rate: i64,
    block: usize,
}

impl Decoder {
    fn open(input: &Path) -> io::Result<Self> {
        let reader = Reader::open(input, false)?;
        reader.enable(true)?;
        let video = reader.decode(
            reader.streams[VIDEO].index,
            MFMediaType_Video,
            MFVideoFormat_NV12,
        )?;
        let audio = if let Some(stream) = reader.streams.get(AUDIO) {
            let media = reader.decode(stream.index, MFMediaType_Audio, MFAudioFormat_PCM)?;
            let rate = mf(unsafe { media.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) })? as i64;
            let block = mf(unsafe { media.GetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT) })? as usize;
            if rate == 0 || block == 0 {
                return Err(io::Error::other(tr(
                    "Не удалось прочитать формат звука.",
                    "Could not read the audio format.",
                )));
            }
            Some(DecodedAudio { media, rate, block })
        } else {
            None
        };
        Ok(Self {
            reader,
            video,
            audio,
        })
    }

    fn output_types(&self) -> io::Result<(IMFMediaType, Option<IMFMediaType>, i64)> {
        let video = media_type(MFMediaType_Video, MFVideoFormat_H264)?;
        unsafe {
            let dimensions = mf(self.video.GetUINT64(&MF_MT_FRAME_SIZE))?;
            let (width, height) = unpack(dimensions);
            let (fps, unit) = self
                .video
                .GetUINT64(&MF_MT_FRAME_RATE)
                .ok()
                .map(unpack)
                .filter(|&(n, d)| n != 0 && d != 0)
                .unwrap_or((30, 1));
            let bitrate = self.reader.streams[VIDEO]
                .native
                .GetUINT32(&MF_MT_AVG_BITRATE)
                .ok()
                .filter(|&v| v > 0)
                .unwrap_or_else(|| {
                    ((u64::from(width) * u64::from(height) * u64::from(fps))
                        / (10 * u64::from(unit)))
                    .clamp(1_000_000, 32_000_000) as u32
                });
            mf(video.SetUINT64(&MF_MT_FRAME_SIZE, dimensions))?;
            mf(video.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | u64::from(unit)))?;
            mf(video.SetUINT64(
                &MF_MT_PIXEL_ASPECT_RATIO,
                self.video
                    .GetUINT64(&MF_MT_PIXEL_ASPECT_RATIO)
                    .unwrap_or((1u64 << 32) | 1),
            ))?;
            mf(video.SetUINT32(&MF_MT_AVG_BITRATE, bitrate))?;
            mf(video.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))?;
            let audio = if let Some(format) = &self.audio {
                let output = media_type(MFMediaType_Audio, MFAudioFormat_AAC)?;
                let channels = mf(format.media.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS))?;
                mf(output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels))?;
                mf(output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, format.rate as u32))?;
                mf(output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16))?;
                mf(output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000))?;
                mf(output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0))?;
                Some(output)
            } else {
                None
            };
            Ok((video, audio, SECOND * i64::from(unit) / i64::from(fps)))
        }
    }

    fn range(
        &self,
        writer: &Writer,
        start: i64,
        end: i64,
        offset: i64,
        frame: i64,
    ) -> io::Result<bool> {
        self.reader.seek(start)?;
        let mut done = [false, self.audio.is_none()];
        let mut saw_video = false;
        while !done.iter().all(|ended| *ended) {
            let packet = self.reader.read(ANY)?;
            let Some(track) = self.reader.track(packet.stream) else {
                if packet.flags & EOS != 0 {
                    break;
                }
                continue;
            };
            if done[track] {
                continue;
            }
            if packet.flags & EOS != 0 || packet.time >= end {
                done[track] = true;
                continue;
            }
            let Some(sample) = packet.sample else {
                continue;
            };
            if track == VIDEO {
                let length = unsafe { sample.GetSampleDuration() }
                    .ok()
                    .filter(|&ticks| ticks > 0)
                    .unwrap_or(frame);
                let first = start.max(packet.time);
                let past = end.min(packet.time.saturating_add(length));
                if past > first {
                    writer.sample(track, &sample, offset + (first - start), past - first)?;
                    saw_video = true;
                }
            } else if let Some(format) = &self.audio {
                if let Some((pcm, begin, duration)) =
                    clip_pcm(sample, format, packet.time, start, end)?
                {
                    writer.sample(track, &pcm, offset + (begin - start), duration)?;
                }
            }
        }
        Ok(saw_video)
    }
}

fn transcode(output: &Path, inputs: &[Input], hardware: bool) -> io::Result<()> {
    let first = Decoder::open(&inputs[0].source)?;
    let (video, audio, frame) = first.output_types()?;
    let mut tracks = vec![(&first.video, &video)];
    if let (Some(input), Some(output)) = (&first.audio, &audio) {
        tracks.push((&input.media, output));
    }
    let writer = Writer::new(output, &tracks, hardware)?;
    let mut offset = 0;
    let mut saw_video = false;
    for (index, input) in inputs.iter().enumerate() {
        let next = (index != 0)
            .then(|| Decoder::open(&input.source))
            .transpose()?;
        let decoder = next.as_ref().unwrap_or(&first);
        if decoder.audio.is_some() != first.audio.is_some() {
            return Err(io::Error::other(tr(
                "Изменился формат звука.",
                "The audio format changed.",
            )));
        }
        for &(start, end) in &input.ranges {
            saw_video |= decoder.range(&writer, start, end, offset, frame)?;
            offset += end - start;
        }
    }
    if !saw_video {
        return Err(io::Error::other(tr(
            "Нет кадров в выбранном фрагменте.",
            "No frames in the selected segment.",
        )));
    }
    writer.finish()
}

fn copy_with_fallback(output: &Path, write: impl Fn(bool) -> io::Result<()>) -> io::Result<()> {
    if write(true).is_ok() {
        return Ok(());
    }
    let _ = fs::remove_file(output);
    write(false)
}

fn clip_pcm(
    sample: IMFSample,
    format: &DecodedAudio,
    time: i64,
    start: i64,
    end: i64,
) -> io::Result<Option<(IMFSample, i64, i64)>> {
    let native = mf(unsafe { sample.ConvertToContiguousBuffer() })?;
    let frames = mf(unsafe { native.GetCurrentLength() })? as usize / format.block;
    if frames == 0 {
        return Ok(None);
    }
    let index = |target: i64| {
        let delta = target.saturating_sub(time).max(0) as i128;
        ((delta * i128::from(format.rate) + i128::from(SECOND - 1)) / i128::from(SECOND))
            .min(frames as i128) as usize
    };
    let first = index(start);
    let past = index(end);
    if past <= first {
        return Ok(None);
    }
    let duration = (past as i64 - first as i64) * SECOND / format.rate;
    let begin = (time + first as i64 * SECOND / format.rate).max(start);
    if first == 0 && past == frames {
        return Ok(Some((sample, begin, duration)));
    }

    let size = (past - first) * format.block;
    let trimmed = mf(unsafe { MFCreateSample() })?;
    let target = mf(unsafe { MFCreateMemoryBuffer(size as u32) })?;
    let src = Locked::new(&native)?;
    let dst = Locked::new(&target)?;
    unsafe {
        std::ptr::copy_nonoverlapping(src.data.add(first * format.block), dst.data, size);
    }
    drop(dst);
    drop(src);
    unsafe {
        mf(target.SetCurrentLength(size as u32))?;
        mf(trimmed.AddBuffer(&target))?;
    }
    Ok(Some((trimmed, begin, duration)))
}

struct Locked<'a> {
    buffer: &'a IMFMediaBuffer,
    data: *mut u8,
    len: u32,
}

impl<'a> Locked<'a> {
    fn new(buffer: &'a IMFMediaBuffer) -> io::Result<Self> {
        let mut data = null_mut();
        let mut len = 0;
        mf(unsafe { buffer.Lock(&mut data, None, Some(&mut len)) })?;
        Ok(Self { buffer, data, len })
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        let _ = unsafe { self.buffer.Unlock() };
    }
}

fn thumbnail(
    sample: &IMFSample,
    video: &IMFMediaType,
    width: u32,
    height: u32,
) -> io::Result<Vec<u8>> {
    let buffer = mf(unsafe { sample.ConvertToContiguousBuffer() })?;
    if let Ok(surface) = buffer.cast::<IMF2DBuffer>() {
        let mut row = null_mut();
        let mut stride = 0;
        if unsafe { surface.Lock2D(&mut row, &mut stride) }.is_ok() {
            let preview = scale_rgb(row, stride, width, height);
            let _ = unsafe { surface.Unlock2D() };
            return preview;
        }
    }

    let pixels = Locked::new(&buffer)?;
    let stride = unsafe { video.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|value| value as i32)
        .unwrap_or_else(|_| -(width as i32 * 4));
    let pitch = stride.unsigned_abs();
    if u64::from(pitch) * u64::from(height) > u64::from(pixels.len) {
        return Err(io::Error::other(tr("Повреждённый кадр.", "Damaged frame.")));
    }
    let row = if stride < 0 {
        unsafe { pixels.data.add(pitch as usize * (height as usize - 1)) }
    } else {
        pixels.data
    };
    scale_rgb(row, stride, width, height)
}

fn scale_rgb(row: *const u8, stride: i32, width: u32, height: u32) -> io::Result<Vec<u8>> {
    if row.is_null() || stride.unsigned_abs() < width.saturating_mul(4) {
        return Err(io::Error::other(tr("Повреждённый кадр.", "Damaged frame.")));
    }
    let width = width as usize;
    let height = height as usize;
    let canvas_w = PREVIEW_WIDTH as usize;
    let canvas_h = PREVIEW_HEIGHT as usize;
    let nw = canvas_w.min(canvas_h * width / height).max(1);
    let nh = canvas_h.min(canvas_w * height / width).max(1);
    let left = (canvas_w - nw) / 2;
    let top = (canvas_h - nh) / 2;
    let mut data = vec![0; canvas_w * canvas_h * 4];
    for y in 0..nh {
        let source = unsafe { row.offset((y * height / nh) as isize * stride as isize) };
        let destination = &mut data[((y + top) * canvas_w + left) * 4..][..nw * 4];
        for (x, pixel) in destination.chunks_exact_mut(4).enumerate() {
            unsafe {
                let source = source.add((x * width / nw) * 4);
                std::ptr::copy_nonoverlapping(source, pixel.as_mut_ptr(), 3);
            }
        }
    }
    Ok(data)
}

fn checked_ranges(keep: &[(f64, f64)], duration: i64) -> io::Result<Vec<(i64, i64)>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            tr(
                "Неверные границы фрагментов.",
                "Invalid segment boundaries.",
            ),
        )
    };
    if keep.is_empty() {
        return Err(invalid());
    }
    let mut previous = 0;
    keep.iter()
        .map(|&(from, to)| {
            if !from.is_finite() || !to.is_finite() || from < 0.0 {
                return Err(invalid());
            }
            let start = ticks(from);
            let end = ticks(to).min(duration);
            if start < previous || end <= start {
                return Err(invalid());
            }
            previous = end;
            Ok((start, end))
        })
        .collect()
}

fn ticks(seconds: f64) -> i64 {
    (seconds * SECOND as f64).round() as i64
}
fn unpack(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

fn media_type(major: GUID, subtype: GUID) -> io::Result<IMFMediaType> {
    unsafe {
        let media = mf(MFCreateMediaType())?;
        mf(media.SetGUID(&MF_MT_MAJOR_TYPE, &major))?;
        mf(media.SetGUID(&MF_MT_SUBTYPE, &subtype))?;
        Ok(media)
    }
}

fn attributes(capacity: u32) -> io::Result<IMFAttributes> {
    let mut attrs = None;
    unsafe {
        mf(MFCreateAttributes(&mut attrs, capacity))?;
    }
    attrs.ok_or_else(|| {
        io::Error::other(tr(
            "Media Foundation не вернул настройки.",
            "Media Foundation did not return its settings.",
        ))
    })
}

fn mf<T>(result: windows::core::Result<T>) -> io::Result<T> {
    result.map_err(io::Error::other)
}

pub fn unique_id() -> String {
    static SEQUENCE: AtomicU32 = AtomicU32::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let n = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{time}-{}-{n}", std::process::id())
}

pub fn recording_name(prefix: &str) -> String {
    let mut time: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut time) };
    format!(
        "{prefix} {:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_sample(bytes: &[u8]) -> io::Result<IMFSample> {
        unsafe {
            let buffer = mf(MFCreateMemoryBuffer(bytes.len() as u32))?;
            let locked = Locked::new(&buffer)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), locked.data, bytes.len());
            drop(locked);
            mf(buffer.SetCurrentLength(bytes.len() as u32))?;
            let sample = mf(MFCreateSample())?;
            mf(sample.AddBuffer(&buffer))?;
            Ok(sample)
        }
    }

    fn fixture(path: &Path, audio: bool) -> io::Result<()> {
        let picture = media_type(MFMediaType_Video, MFVideoFormat_NV12)?;
        let h264 = media_type(MFMediaType_Video, MFVideoFormat_H264)?;
        let pcm = media_type(MFMediaType_Audio, MFAudioFormat_PCM)?;
        let aac = media_type(MFMediaType_Audio, MFAudioFormat_AAC)?;
        let width = 160usize;
        let height = 90usize;
        let fps = 20usize;
        let rate = 48_000usize;
        let frame = SECOND / fps as i64;
        unsafe {
            for video in [&picture, &h264] {
                mf(video.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64))?;
                mf(video.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1))?;
                mf(video.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1))?;
                mf(video.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))?;
            }
            mf(h264.SetUINT32(&MF_MT_AVG_BITRATE, 200_000))?;
            for audio in [&pcm, &aac] {
                mf(audio.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 1))?;
                mf(audio.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate as u32))?;
                mf(audio.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16))?;
            }
            mf(pcm.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 2 * rate as u32))?;
            mf(pcm.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 2))?;
            mf(aac.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000))?;
            mf(aac.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0))?;
        }
        let types = [(&picture, &h264), (&pcm, &aac)];
        let writer = Writer::new(path, &types[..1 + usize::from(audio)], false)?;
        for index in 0..(3 * fps) {
            let (y, u, v) = match index / fps {
                0 => (81, 90, 240),
                1 => (145, 54, 34),
                _ => (41, 240, 110),
            };
            let mut pixels = vec![y; width * height * 3 / 2];
            for uv in pixels[width * height..].chunks_exact_mut(2) {
                uv.copy_from_slice(&[u, v]);
            }
            let timestamp = frame * index as i64;
            writer.sample(VIDEO, &buffer_sample(&pixels)?, timestamp, frame)?;
            if audio {
                let frames = rate / fps;
                let wave: Vec<_> = (index * frames..(index + 1) * frames)
                    .flat_map(|n| {
                        let phase = std::f64::consts::TAU * 440.0 * n as f64 / rate as f64;
                        let amplitude = if index / fps == 1 { 0.0 } else { 9_000.0 };
                        ((amplitude * phase.sin()) as i16).to_le_bytes()
                    })
                    .collect();
                writer.sample(AUDIO, &buffer_sample(&wave)?, timestamp, frame)?;
            }
        }
        writer.finish()
    }

    #[test]
    fn media_foundation_joins_and_cuts_recordings() -> io::Result<()> {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _foundation = Foundation::start()?;
        let folder = Scratch(std::env::temp_dir().join(format!("recr-media-{}", unique_id())));
        fs::create_dir_all(&folder.0)?;
        let media = Media {
            videos: folder.0.clone(),
        };
        for audio in [false, true] {
            let source = folder.0.join(format!("Исходник-{audio}.mp4"));
            fixture(&source, audio)?;
            let edit = media.edit(&source, &[(0.25, 0.95), (2.05, 2.85)])?;
            let red = media.preview(&edit, 0.25)?;
            let blue = media.preview(&edit, 1.05)?;
            let center = ((PREVIEW_HEIGHT / 2 * PREVIEW_WIDTH + PREVIEW_WIDTH / 2) * 4) as usize;
            assert!(
                red[center + 2] > 180 && red[center] < 45,
                "{:?}",
                &red[center..center + 4]
            );
            assert!(
                blue[center] > 180 && blue[center + 2] < 45,
                "{:?}",
                &blue[center..center + 4]
            );
            let edit_info = Reader::open(&edit, false)?;
            assert!((1.45..1.60).contains(&(edit_info.duration as f64 / SECOND as f64)));
            assert_eq!(edit_info.streams.len(), 1 + usize::from(audio));
            if audio {
                let decoder = Decoder::open(&edit)?;
                decoder.reader.seek(ticks(1.0))?;
                let index = decoder.reader.streams[AUDIO].index;
                let mut amplitude = 0;
                loop {
                    let packet = decoder.reader.read(index)?;
                    if let Some(sample) = packet.sample.filter(|_| packet.time >= ticks(1.0)) {
                        let buffer = mf(unsafe { sample.ConvertToContiguousBuffer() })?;
                        let bytes = Locked::new(&buffer)?;
                        let pcm =
                            unsafe { std::slice::from_raw_parts(bytes.data, bytes.len as usize) };
                        amplitude = pcm
                            .chunks_exact(2)
                            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]).unsigned_abs())
                            .max()
                            .unwrap_or(0);
                        break;
                    }
                    if packet.flags & EOS != 0 {
                        break;
                    }
                }
                assert!(amplitude > 2_000, "Cut discarded the sound: {amplitude}");
            }
            let output = folder.0.join(format!("Joined-{audio}.mp4"));
            remux(&output, &[source.clone(), source.clone()])?;
            let joined = Reader::open(&output, false)?;
            assert!((5.9..6.2).contains(&(joined.duration as f64 / SECOND as f64)));
            assert_eq!(joined.streams.len(), 1 + usize::from(audio));
            assert!((2.9..3.1).contains(&media.duration(&source)?));
        }
        Ok(())
    }
}
