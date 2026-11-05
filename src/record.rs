//! Screen recording to an H.264/AAC MP4 with the Media Foundation sink
//! writer. A worker thread grabs frames with GDI (drawing the cursor in),
//! and mixes system audio and the microphone into a single track.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{Error, Result, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, POINT, RECT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, HCURSOR, HICON,
    ICONINFO,
};

use crate::audio::{Capture, CHANNELS, SAMPLE_RATE};
use crate::capture;
use crate::gfx::Dib;

const FPS: u32 = 30;
const FRAME_100NS: i64 = 10_000_000 / FPS as i64;
const VIDEO_STREAM: u32 = 0;
const AUDIO_STREAM: u32 = 1;
/// Audio is mixed this far behind real time, so a source whose packets
/// arrive a little late isn't padded with silence.
const AUDIO_LATENCY: f64 = 0.1;
/// Samples (per source, interleaved) kept beyond what has been mixed. A
/// source running ahead of the clock is trimmed to this.
const MAX_BACKLOG: usize = (SAMPLE_RATE * CHANNELS) as usize / 4;
const MIN_SIZE: i32 = 16;

/// Trades file size for fidelity. Lower levels downscale large areas and
/// encode at lower bitrates.
#[derive(Clone, Copy, PartialEq)]
pub enum Quality {
    Low,
    Medium,
    High,
}

impl Quality {
    /// In the order shown in the menu.
    pub const ALL: [Quality; 3] = [Quality::High, Quality::Medium, Quality::Low];

    pub fn name(self) -> &'static str {
        match self {
            Quality::Low => "Low",
            Quality::Medium => "Medium",
            Quality::High => "High",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Quality::Low => "Up to 720p, smallest files",
            Quality::Medium => "Up to 1080p",
            Quality::High => "Source resolution",
        }
    }

    /// Larger areas are scaled down to about this many pixels.
    fn max_pixels(self) -> Option<f64> {
        match self {
            Quality::Low => Some(1280.0 * 720.0),
            Quality::Medium => Some(1920.0 * 1080.0),
            Quality::High => None,
        }
    }

    /// Video bitrate in bit/s, from bits per pixel per frame and limits.
    fn video_bitrate(self, w: u32, h: u32) -> u32 {
        let (bpp, min, max) = match self {
            Quality::Low => (0.03, 0.5e6, 4e6),
            Quality::Medium => (0.05, 1e6, 12e6),
            Quality::High => (0.1, 2e6, 40e6),
        };
        (w as f64 * h as f64 * FPS as f64 * bpp).clamp(min, max) as u32
    }

    /// AAC bytes per second: 96, 128 or 192 kbit/s, rates the encoder accepts.
    fn audio_bytes_per_second(self) -> u32 {
        match self {
            Quality::Low => 12_000,
            Quality::Medium => 16_000,
            Quality::High => 24_000,
        }
    }

    /// Encoded frame size for a `w` x `h` capture, with even dimensions as
    /// the H.264 encoder requires.
    fn output_size(self, w: i32, h: i32) -> (i32, i32) {
        let k = match self.max_pixels() {
            Some(max) if (w as f64 * h as f64) > max => (max / (w as f64 * h as f64)).sqrt(),
            _ => 1.0,
        };
        (((w as f64 * k) as i32) & !1, ((h as f64 * k) as i32) & !1)
    }
}

#[derive(Clone)]
pub struct Settings {
    /// Screen coordinates.
    pub rect: RECT,
    pub system_audio: bool,
    /// Endpoint ID of the microphone to record, if any.
    pub mic: Option<String>,
    pub quality: Quality,
}

enum Command {
    Pause,
    Resume,
    Stop,
    Discard,
}

/// Handle to a recording in progress. Dropping it stops and saves.
pub struct Recorder {
    tx: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Starts recording, returning once capture and encoding are set up.
    pub fn start(settings: &Settings) -> Result<Self> {
        let path = capture::output_path("WinShotX recording", "mp4")?;
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let settings = settings.clone();
        let thread = std::thread::spawn(move || run(settings, path, rx, ready_tx));
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(Error::new(
                E_FAIL,
                "The recording thread exited unexpectedly",
            )),
        }
    }

    pub fn pause(&self) {
        let _ = self.tx.send(Command::Pause);
    }

    pub fn resume(&self) {
        let _ = self.tx.send(Command::Resume);
    }

    /// Stops and saves. The file is finalised in the background; join the
    /// returned thread to wait for it.
    pub fn stop(mut self) -> Option<JoinHandle<()>> {
        let _ = self.tx.send(Command::Stop);
        self.thread.take()
    }

    /// Stops and deletes the recording, waiting until it's gone.
    pub fn discard(mut self) {
        let _ = self.tx.send(Command::Discard);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
    }
}

fn run(
    settings: Settings,
    path: PathBuf,
    rx: Receiver<Command>,
    ready: mpsc::SyncSender<Result<()>>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            let _ = ready.send(Err(e));
            CoUninitialize();
            return;
        }
    }
    match Session::new(&settings, &path) {
        Err(e) => {
            let _ = ready.send(Err(e));
            let _ = std::fs::remove_file(&path);
        }
        Ok(mut session) => {
            let _ = ready.send(Ok(()));
            let outcome = session.record(&rx).and_then(|keep| {
                if keep {
                    unsafe { session.writer.Finalize()? };
                }
                Ok(keep)
            });
            drop(session);
            match outcome {
                Ok(true) => capture::deliver_file(&path),
                Ok(false) => {
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => crate::show_error(&format!("Recording failed:\n\n{e}")),
            }
        }
    }
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
}

/// Recording time, excluding pauses.
struct Clock {
    start: Instant,
    paused_at: Option<Instant>,
    paused: Duration,
}

impl Clock {
    fn elapsed(&self) -> Duration {
        self.paused_at.unwrap_or_else(Instant::now) - self.start - self.paused
    }

    fn pause(&mut self) {
        self.paused_at.get_or_insert_with(Instant::now);
    }

    fn resume(&mut self) {
        if let Some(t) = self.paused_at.take() {
            self.paused += t.elapsed();
        }
    }
}

struct Session {
    writer: IMFSinkWriter,
    screen: HDC,
    /// The captured area at full size.
    dib: Dib,
    /// The frame scaled down to the output size, if it's smaller.
    scaled: Option<Dib>,
    origin: POINT,
    sources: Vec<Capture>,
    /// Audio frames (one sample per channel) written so far.
    audio_written: u64,
    next_frame: u64,
    clock: Clock,
    cursor: Option<(HCURSOR, POINT)>,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(None, self.screen);
        }
    }
}

fn context(what: &'static str) -> impl Fn(Error) -> Error {
    move |e| Error::new(e.code(), format!("{what}: {}", e.message()))
}

impl Session {
    fn new(settings: &Settings, path: &Path) -> Result<Self> {
        let r = settings.rect;
        let (cw, ch) = (r.right - r.left, r.bottom - r.top);
        let (w, h) = settings.quality.output_size(cw, ch);
        if w < MIN_SIZE || h < MIN_SIZE {
            return Err(Error::new(
                E_INVALIDARG,
                format!("The area must be at least {MIN_SIZE}\u{00D7}{MIN_SIZE} pixels"),
            ));
        }

        let mut sources = Vec::new();
        if settings.system_audio {
            sources.push(Capture::system().map_err(context("System audio"))?);
        }
        if let Some(id) = &settings.mic {
            sources.push(Capture::microphone(id).map_err(context("Microphone"))?);
        }

        let has_audio = !sources.is_empty();
        let writer = unsafe {
            // Hardware encoders are much faster but not available everywhere.
            let (w, h, q) = (w as u32, h as u32, settings.quality);
            create_writer(path, w, h, q, has_audio, true)
                .or_else(|_| create_writer(path, w, h, q, has_audio, false))
                .map_err(context("Video encoder"))?
        };

        // Without scaling, capture just the even-sized part that's encoded.
        let scaling = w < (cw & !1) || h < (ch & !1);
        let (cw, ch) = if scaling { (cw, ch) } else { (w, h) };
        let (screen, dib, scaled) = unsafe {
            let screen = GetDC(None);
            let dibs = Dib::new(screen, cw, ch).and_then(|dib| {
                if !scaling {
                    return Ok((dib, None));
                }
                let scaled = Dib::new(screen, w, h)?;
                // Halftone averages pixels, keeping text legible when shrunk.
                let _ = SetStretchBltMode(scaled.dc, HALFTONE);
                let _ = SetBrushOrgEx(scaled.dc, 0, 0, None);
                Ok((dib, Some(scaled)))
            });
            match dibs {
                Ok((dib, scaled)) => (screen, dib, scaled),
                Err(e) => {
                    ReleaseDC(None, screen);
                    return Err(e);
                }
            }
        };

        // Drop anything captured while the encoder was being set up.
        for s in &mut sources {
            s.read()?;
            s.buffer.clear();
        }
        Ok(Self {
            writer,
            screen,
            dib,
            scaled,
            origin: POINT {
                x: r.left,
                y: r.top,
            },
            sources,
            audio_written: 0,
            next_frame: 0,
            clock: Clock {
                start: Instant::now(),
                paused_at: None,
                paused: Duration::ZERO,
            },
            cursor: None,
        })
    }

    /// Records until told to stop. Returns whether to keep the file.
    fn record(&mut self, rx: &Receiver<Command>) -> Result<bool> {
        let interval = Duration::from_nanos(1_000_000_000 / FPS as u64);
        loop {
            loop {
                match rx.try_recv() {
                    Ok(Command::Pause) => self.clock.pause(),
                    Ok(Command::Resume) => {
                        self.clock.resume();
                        self.drain_audio()?;
                    }
                    Ok(Command::Stop) | Err(TryRecvError::Disconnected) => {
                        self.write_audio(self.clock.elapsed().as_secs_f64())?;
                        return Ok(true);
                    }
                    Ok(Command::Discard) => return Ok(false),
                    Err(TryRecvError::Empty) => break,
                }
            }

            if self.clock.paused_at.is_some() {
                self.drain_audio()?;
                std::thread::sleep(interval);
                continue;
            }

            let elapsed = self.clock.elapsed();
            let due = (elapsed.as_nanos() / interval.as_nanos()) as u64;
            if due >= self.next_frame {
                // Frames that couldn't be captured in time are skipped.
                self.write_frame(due as i64 * FRAME_100NS)?;
                self.next_frame = due + 1;
            }
            self.write_audio(elapsed.as_secs_f64() - AUDIO_LATENCY)?;

            let next = Duration::from_nanos(interval.as_nanos() as u64 * self.next_frame);
            if let Some(wait) = next.checked_sub(self.clock.elapsed()) {
                std::thread::sleep(wait.min(interval));
            }
        }
    }

    fn write_frame(&mut self, time: i64) -> Result<()> {
        let (cw, ch) = (self.dib.width, self.dib.height);
        unsafe {
            BitBlt(
                self.dib.dc,
                0,
                0,
                cw,
                ch,
                self.screen,
                self.origin.x,
                self.origin.y,
                SRCCOPY,
            )?;
            self.draw_cursor();
            if let Some(s) = &self.scaled {
                let _ = StretchBlt(
                    s.dc,
                    0,
                    0,
                    s.width,
                    s.height,
                    self.dib.dc,
                    0,
                    0,
                    cw,
                    ch,
                    SRCCOPY,
                );
            }
            let _ = GdiFlush();

            let frame = self.scaled.as_ref().unwrap_or(&self.dib);
            let bytes = (frame.width * frame.height * 4) as u32;
            let buffer = MFCreateMemoryBuffer(bytes)?;
            let mut dst = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            std::ptr::copy_nonoverlapping(
                frame.pixels().as_ptr() as *const u8,
                dst,
                bytes as usize,
            );
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(FRAME_100NS)?;
            self.writer.WriteSample(VIDEO_STREAM, &sample)
        }
    }

    unsafe fn draw_cursor(&mut self) {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut info).is_err() || info.flags.0 & CURSOR_SHOWING.0 == 0 {
            return;
        }
        let hot = match self.cursor {
            Some((c, hot)) if c == info.hCursor => hot,
            _ => {
                let mut icon = ICONINFO::default();
                if GetIconInfo(HICON(info.hCursor.0), &mut icon).is_err() {
                    return;
                }
                let _ = DeleteObject(icon.hbmMask);
                if !icon.hbmColor.is_invalid() {
                    let _ = DeleteObject(icon.hbmColor);
                }
                let hot = POINT {
                    x: icon.xHotspot as i32,
                    y: icon.yHotspot as i32,
                };
                self.cursor = Some((info.hCursor, hot));
                hot
            }
        };
        let x = info.ptScreenPos.x - hot.x - self.origin.x;
        let y = info.ptScreenPos.y - hot.y - self.origin.y;
        let _ = DrawIconEx(
            self.dib.dc,
            x,
            y,
            HICON(info.hCursor.0),
            0,
            0,
            0,
            None,
            DI_NORMAL,
        );
    }

    /// Discards audio captured while paused.
    fn drain_audio(&mut self) -> Result<()> {
        for s in &mut self.sources {
            s.read()?;
            s.buffer.clear();
        }
        Ok(())
    }

    /// Mixes and writes audio up to `until` seconds of recording time.
    /// Sources without enough data (loopback sends nothing during silence)
    /// are padded with silence.
    fn write_audio(&mut self, until: f64) -> Result<()> {
        if self.sources.is_empty() {
            return Ok(());
        }
        for s in &mut self.sources {
            s.read()?;
        }
        let target = (until.max(0.0) * SAMPLE_RATE as f64) as u64;
        if target <= self.audio_written {
            return Ok(());
        }
        let frames = (target - self.audio_written) as usize;
        let n = frames * CHANNELS as usize;
        let mut mix = vec![0f32; n];
        for s in &mut self.sources {
            let take = s.buffer.len().min(n);
            for (m, v) in mix.iter_mut().zip(s.buffer.drain(..take)) {
                *m += v;
            }
            let excess = s.buffer.len().saturating_sub(MAX_BACKLOG);
            s.buffer.drain(..excess);
        }

        let bytes = (n * 2) as u32;
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes)?;
            let mut dst = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            let out = std::slice::from_raw_parts_mut(dst as *mut i16, n);
            for (o, m) in out.iter_mut().zip(&mix) {
                *o = (m.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            }
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes)?;

            let rate = SAMPLE_RATE as u64;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime((self.audio_written * 10_000_000 / rate) as i64)?;
            sample.SetSampleDuration((frames as u64 * 10_000_000 / rate) as i64)?;
            self.writer.WriteSample(AUDIO_STREAM, &sample)?;
        }
        self.audio_written = target;
        Ok(())
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    (hi as u64) << 32 | lo as u64
}

unsafe fn create_writer(
    path: &Path,
    w: u32,
    h: u32,
    quality: Quality,
    audio: bool,
    hardware: bool,
) -> Result<IMFSinkWriter> {
    let mut attrs = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.ok_or_else(Error::empty)?;
    attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, hardware as u32)?;
    let url: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let writer = MFCreateSinkWriterFromURL(PCWSTR(url.as_ptr()), None, &attrs)?;

    let video = |subtype: &windows::core::GUID| -> Result<IMFMediaType> {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        Ok(t)
    };
    let out = video(&MFVideoFormat_H264)?;
    out.SetUINT32(&MF_MT_AVG_BITRATE, quality.video_bitrate(w, h))?;
    out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
    let stream = writer.AddStream(&out)?;
    debug_assert_eq!(stream, VIDEO_STREAM);
    let input = video(&MFVideoFormat_RGB32)?;
    // Positive stride: top-down rows, matching the DIB.
    input.SetUINT32(&MF_MT_DEFAULT_STRIDE, w * 4)?;
    writer.SetInputMediaType(stream, &input, None)?;

    if audio {
        let pcm = |subtype: &windows::core::GUID| -> Result<IMFMediaType> {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
            t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
            t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS)?;
            Ok(t)
        };
        let out = pcm(&MFAudioFormat_AAC)?;
        out.SetUINT32(
            &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
            quality.audio_bytes_per_second(),
        )?;
        let stream = writer.AddStream(&out)?;
        debug_assert_eq!(stream, AUDIO_STREAM);
        let input = pcm(&MFAudioFormat_PCM)?;
        input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, CHANNELS * 2)?;
        input.SetUINT32(
            &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
            SAMPLE_RATE * CHANNELS * 2,
        )?;
        writer.SetInputMediaType(stream, &input, None)?;
    }

    writer.BeginWriting()?;
    Ok(writer)
}
