//! Linux output through the PulseAudio *simple* API, resolved at runtime.
//!
//! The workspace has no build-time dependency on ALSA or PulseAudio headers:
//! `libpulse-simple.so.0` (or its development name) is opened with `dlopen`,
//! the five symbols we need are resolved with `dlsym`, and the library is
//! unloaded when the writer thread exits. A PipeWire host works through its
//! PulseAudio compatibility socket, so no PipeWire header is needed either.
//!
//! # Threading
//!
//! [`start`] resolves the library and spawns one writer thread. That thread
//! opens the stream (so the raw `pa_simple` pointer never leaves it), reports
//! the result back to the constructor, then renders 512-frame stereo blocks
//! through the existing [`Mixer`] and blocks in `pa_simple_write`. On shutdown
//! [`Output`] clears the running flag and joins the thread; the stream wrapper's
//! `Drop` drains a healthy stream and frees it. After [`MAX_CONSECUTIVE_WRITE_ERRORS`]
//! consecutive write failures the writer stops and the engine reports the
//! output as unavailable instead of retrying forever.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::mixer::{CommandQueue, Control, Mixer};
use crate::synth::ClipBank;
use crate::{AudioError, CHANNELS, SAMPLE_RATE};

/// Shared libraries tried in order. The versioned soname is what every
/// PulseAudio/PipeWire runtime installs; the unversioned name is the
/// development link and is only present on build hosts.
const LIBRARY_CANDIDATES: [&str; 2] = ["libpulse-simple.so.0", "libpulse-simple.so"];

/// `RTLD_NOW | RTLD_LOCAL` from `<dlfcn.h>` on Linux.
const DLOPEN_FLAGS: c_int = 2;

/// `pa_sample_format_t::PA_SAMPLE_FLOAT32LE`.
const PA_SAMPLE_FLOAT32LE: i32 = 5;
/// `pa_stream_direction_t::PA_STREAM_PLAYBACK`.
const PA_STREAM_PLAYBACK: c_int = 1;

/// Frames rendered per blocking write. 512 frames is 10.67 ms at 48 kHz.
const WRITE_BLOCK_FRAMES: usize = 512;
/// Interleaved samples per blocking write.
const WRITE_BLOCK_SAMPLES: usize = WRITE_BLOCK_FRAMES * CHANNELS as usize;
/// Bytes per interleaved stereo frame: two f32 samples.
const BYTES_PER_FRAME: u32 = CHANNELS as u32 * 4;
/// Requested server-side playback buffer (target latency) in microseconds.
const TARGET_LATENCY_US: u32 = 25_000;
/// Server buffer expressed in frames at 48 kHz: 1200 frames = 25 ms.
const BUFFER_FRAMES: u32 = SAMPLE_RATE * TARGET_LATENCY_US / 1_000_000;
/// Server target length in bytes for 25 ms of stereo f32.
const TARGET_LENGTH_BYTES: u32 = BUFFER_FRAMES * BYTES_PER_FRAME;
/// Stop writing after this many consecutive `pa_simple_write` failures.
const MAX_CONSECUTIVE_WRITE_ERRORS: u32 = 3;
/// Human-readable device name exposed through [`crate::AudioStatus`].
const DEVICE_NAME: &str = "pulse: default";

const APPLICATION_NAME: &CStr = c"OpenBlock";
const STREAM_NAME: &CStr = c"game";

// PulseAudio error codes that mean "there is no usable sound server", as
// opposed to a programming error in our call. See `<pulse/error.h>`.
const PA_ERR_ACCESS: i32 = 1;
const PA_ERR_NOENTITY: i32 = 5;
const PA_ERR_CONNECTIONREFUSED: i32 = 6;
const PA_ERR_TIMEOUT: i32 = 8;
const PA_ERR_AUTHKEY: i32 = 9;
const PA_ERR_CONNECTIONTERMINATED: i32 = 11;
const PA_ERR_INVALIDSERVER: i32 = 13;

/// C `pa_sample_spec`. `#[repr(C)]` gives the same 12-byte layout as the
/// PulseAudio header (enum, u32, u8 + tail padding).
#[repr(C)]
#[allow(dead_code)] // The C library reads these fields; Rust only constructs them.
struct PaSampleSpec {
    format: i32,
    rate: u32,
    channels: u8,
}

/// C `pa_buffer_attr` (five `u32` fields, 20 bytes).
#[repr(C)]
#[allow(dead_code)] // The C library reads these fields; Rust only constructs them.
struct PaBufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

/// Opaque C `pa_simple`; only ever handled behind a pointer.
#[repr(C)]
struct PaSimple {
    _private: [u8; 0],
}

type PaSimpleNew = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    dir: c_int,
    dev: *const c_char,
    stream_name: *const c_char,
    sample_spec: *const PaSampleSpec,
    channel_map: *const c_void,
    buffer_attr: *const PaBufferAttr,
    error: *mut c_int,
) -> *mut PaSimple;
type PaSimpleWrite = unsafe extern "C" fn(*mut PaSimple, *const c_void, usize, *mut c_int) -> c_int;
type PaSimpleDrain = unsafe extern "C" fn(*mut PaSimple, *mut c_int) -> c_int;
type PaSimpleFree = unsafe extern "C" fn(*mut PaSimple);
type PaStrerror = unsafe extern "C" fn(c_int) -> *const c_char;

#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlerror() -> *mut c_char;
}

/// One loaded shared-library handle. Abstracted so tests can exercise the
/// symbol-resolution and fallback logic without a real PulseAudio install.
trait DlHandle: Send + Sync {
    /// Resolves a NUL-terminated symbol name to its runtime address.
    fn symbol(&self, name: &str) -> Option<NonZeroUsize>;
}

/// Opens candidate shared libraries and returns handles. Abstracted for tests.
trait DlLoader {
    /// Attempts to load one library by its soname.
    fn open(&self, name: &str) -> Option<Arc<dyn DlHandle>>;
}

/// Real `dlopen` handle. A `usize` keeps the type automatically `Send + Sync`
/// without unsafe impls; the address is only cast back inside Drop/dlsym.
struct RealHandle {
    handle: usize,
}

impl DlHandle for RealHandle {
    fn symbol(&self, name: &str) -> Option<NonZeroUsize> {
        let name = CString::new(name).ok()?;
        // SAFETY: `handle` came from a successful dlopen and remains loaded
        // until this RealHandle is dropped.
        let pointer = unsafe { dlsym(self.handle as *mut c_void, name.as_ptr()) };
        NonZeroUsize::new(pointer as usize)
    }
}

impl Drop for RealHandle {
    fn drop(&mut self) {
        if self.handle != 0 {
            // SAFETY: this is the matching dlclose for DlLoader::open, and no
            // further symbol calls can happen after the last Arc is dropped.
            unsafe {
                let _ = dlclose(self.handle as *mut c_void);
            }
            self.handle = 0;
        }
    }
}

struct RealLoader;

impl DlLoader for RealLoader {
    fn open(&self, name: &str) -> Option<Arc<dyn DlHandle>> {
        let name = CString::new(name).ok()?;
        // SAFETY: the name is NUL-terminated and flags only select resolution
        // mode; dlopen is safe with any valid C string.
        let handle = unsafe { dlopen(name.as_ptr(), DLOPEN_FLAGS) };
        if handle.is_null() {
            if let Some(error) = last_dl_error() {
                log::debug!("audio: dlopen failed: {error}");
            }
            return None;
        }
        Some(Arc::new(RealHandle {
            handle: handle as usize,
        }))
    }
}

fn last_dl_error() -> Option<String> {
    // SAFETY: dlerror returns either null or a valid NUL-terminated string
    // owned by the loader until the next dlerror call.
    let pointer = unsafe { dlerror() };
    if pointer.is_null() {
        return None;
    }
    // SAFETY: non-null dlerror result is a valid C string.
    Some(unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned())
}

/// The resolved PulseAudio simple function table plus a library handle that
/// stays loaded for as long as the table is used.
struct PulseLibrary {
    _handle: Arc<dyn DlHandle>,
    new: NonZeroUsize,
    write: NonZeroUsize,
    drain: NonZeroUsize,
    free: NonZeroUsize,
    strerror: Option<NonZeroUsize>,
}

impl PulseLibrary {
    fn load() -> Result<Self, String> {
        Self::load_with(&RealLoader)
    }

    /// Tries every candidate and every required symbol. A candidate that opens
    /// but lacks a required symbol is closed and the next candidate is tried.
    fn load_with(loader: &dyn DlLoader) -> Result<Self, String> {
        let mut failures = Vec::new();
        'candidates: for candidate in LIBRARY_CANDIDATES {
            let Some(handle) = loader.open(candidate) else {
                failures.push(format!("{candidate}: not found"));
                continue;
            };
            let mut required = [None; 4];
            for (slot, name) in
                required
                    .iter_mut()
                    .zip(["pa_simple_new", "pa_simple_write", "pa_simple_drain", "pa_simple_free"])
            {
                let Some(address) = handle.symbol(name) else {
                    failures.push(format!("{candidate}: missing {name}"));
                    continue 'candidates;
                };
                *slot = Some(address);
            }
            let [Some(new), Some(write), Some(drain), Some(free)] = required else {
                // Every slot was filled above; this is only here to destructure
                // Option safely without unwrap.
                continue;
            };
            log::debug!("audio: using {candidate}");
            let strerror = handle.symbol("pa_strerror");
            return Ok(Self {
                _handle: handle,
                new,
                write,
                drain,
                free,
                strerror,
            });
        }
        Err(failures.join("; "))
    }

    fn simple_new(&self) -> PaSimpleNew {
        // SAFETY: `new` was resolved by dlsym from the loaded libpulse-simple
        // handle and has the pa_simple_new ABI. The handle is kept alive by
        // self._handle for the whole call.
        unsafe { std::mem::transmute::<usize, PaSimpleNew>(self.new.get()) }
    }

    fn simple_write(&self) -> PaSimpleWrite {
        // SAFETY: symbol and ABI as above for pa_simple_write.
        unsafe { std::mem::transmute::<usize, PaSimpleWrite>(self.write.get()) }
    }

    fn simple_drain(&self) -> PaSimpleDrain {
        // SAFETY: symbol and ABI as above for pa_simple_drain.
        unsafe { std::mem::transmute::<usize, PaSimpleDrain>(self.drain.get()) }
    }

    fn simple_free(&self) -> PaSimpleFree {
        // SAFETY: symbol and ABI as above for pa_simple_free.
        unsafe { std::mem::transmute::<usize, PaSimpleFree>(self.free.get()) }
    }

    /// Best-effort PulseAudio error text; falls back to the numeric code when
    /// `pa_strerror` was not exported by the library.
    fn error_string(&self, code: i32) -> String {
        if let Some(strerror) = self.strerror {
            // SAFETY: optional symbol resolved from the same library with the
            // pa_strerror ABI.
            let strerror = unsafe { std::mem::transmute::<usize, PaStrerror>(strerror.get()) };
            // SAFETY: pa_strerror accepts any i32 and returns null or a static
            // NUL-terminated string.
            let pointer = unsafe { strerror(code) };
            if !pointer.is_null() {
                // SAFETY: non-null pa_strerror result is a valid C string.
                return unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned();
            }
        }
        format!("PulseAudio error {code}")
    }
}

/// Buffer attributes that ask the server for a ~25 ms playback buffer while
/// allowing it up to 100 ms of slack.
fn playback_buffer_attr() -> PaBufferAttr {
    PaBufferAttr {
        maxlength: TARGET_LENGTH_BYTES * 4,
        tlength: TARGET_LENGTH_BYTES,
        // -1 means "server default" for the remaining fields.
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: u32::MAX,
    }
}

struct PulseInfo {
    device_name: String,
    sample_rate: u32,
    channels: u16,
    buffer_frames: u32,
    latency_ms: f32,
}

struct PulseOpenError {
    code: i32,
    message: String,
}

impl PulseOpenError {
    /// Whether the failure means no server/device is reachable, rather than a
    /// malformed request our backend generated.
    fn is_unavailable(&self) -> bool {
        matches!(
            self.code,
            PA_ERR_ACCESS
                | PA_ERR_NOENTITY
                | PA_ERR_CONNECTIONREFUSED
                | PA_ERR_TIMEOUT
                | PA_ERR_AUTHKEY
                | PA_ERR_CONNECTIONTERMINATED
                | PA_ERR_INVALIDSERVER
        )
    }
}

/// A blocking sink the writer loop feeds. The real implementation is
/// [`PulseStream`]; tests substitute recording or failing fakes.
trait PulseSink {
    /// Writes interleaved stereo f32 samples, blocking until accepted.
    fn write(&mut self, samples: &[f32]) -> Result<(), String>;
}

/// Owning wrapper around one `pa_simple` stream. Created and used only on the
/// writer thread.
struct PulseStream {
    library: Arc<PulseLibrary>,
    /// `pa_simple *` as an integer so the struct is automatically Send. Zero
    /// means the stream was already freed.
    stream: usize,
    /// Cleared after the first write error; a broken stream is freed without a
    /// potentially blocking drain.
    healthy: bool,
}

impl PulseStream {
    fn open(library: Arc<PulseLibrary>) -> Result<(Self, PulseInfo), PulseOpenError> {
        let spec = PaSampleSpec {
            format: PA_SAMPLE_FLOAT32LE,
            rate: SAMPLE_RATE,
            channels: CHANNELS as u8,
        };
        let attr = playback_buffer_attr();
        let mut error = 0;
        // SAFETY: all pointers either point at live local values or are null
        // (meaning server/device/channel-map defaults); the two C strings are
        // static NUL-terminated literals; `error` is a valid out-parameter.
        let pointer = unsafe {
            (library.simple_new())(
                std::ptr::null(),
                APPLICATION_NAME.as_ptr(),
                PA_STREAM_PLAYBACK,
                std::ptr::null(),
                STREAM_NAME.as_ptr(),
                &spec,
                std::ptr::null(),
                &attr,
                &mut error,
            )
        };
        if pointer.is_null() {
            return Err(PulseOpenError {
                code: error,
                message: library.error_string(error),
            });
        }
        let info = PulseInfo {
            device_name: DEVICE_NAME.to_string(),
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            buffer_frames: BUFFER_FRAMES,
            latency_ms: BUFFER_FRAMES as f32 / SAMPLE_RATE as f32 * 1000.0,
        };
        Ok((
            Self {
                library,
                stream: pointer as usize,
                healthy: true,
            },
            info,
        ))
    }
}

impl PulseSink for PulseStream {
    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        let mut error = 0;
        // SAFETY: the stream pointer is non-null and owned by this wrapper;
        // samples is a live slice of the byte length passed; error is valid.
        let result = unsafe {
            (self.library.simple_write())(
                self.stream as *mut PaSimple,
                samples.as_ptr().cast(),
                std::mem::size_of_val(samples),
                &mut error,
            )
        };
        if result < 0 {
            self.healthy = false;
            Err(self.library.error_string(error))
        } else {
            Ok(())
        }
    }
}

impl Drop for PulseStream {
    fn drop(&mut self) {
        if self.stream == 0 {
            return;
        }
        let stream = self.stream as *mut PaSimple;
        if self.healthy {
            let mut error = 0;
            // SAFETY: the stream is still valid; draining a healthy stream lets
            // the final server buffer play out during orderly shutdown.
            let result = unsafe { (self.library.simple_drain())(stream, &mut error) };
            if result < 0 {
                log::debug!("audio: PulseAudio drain failed: {}", self.library.error_string(error));
            }
        }
        // SAFETY: pa_simple_free is called exactly once per successful
        // pa_simple_new and the pointer is cleared below.
        unsafe { (self.library.simple_free())(stream) };
        self.stream = 0;
    }
}

/// Outcome of the writer loop, used to mark the backend unavailable after a
/// fatal streak of write errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriterExit {
    Stopped,
    Failed,
}

/// Renders and writes fixed-size blocks until `running` is cleared or the
/// server fails repeatedly. Dropping the sink drains and frees a healthy
/// stream; no allocation, logging or locking happens in the steady-state path.
fn run_writer<S: PulseSink>(mut sink: S, mut mixer: Mixer, control: &Control, running: &AtomicBool) -> WriterExit {
    // Pause is handled inside the mixer: a disabled control fades to silence
    // while the writer keeps feeding blocks, so un-pausing resumes seamlessly.
    // A server-side underrun needs no special action; the next write resumes
    // the stream after the server has inserted silence.
    let mut block = [0.0f32; WRITE_BLOCK_SAMPLES];
    let mut consecutive_errors = 0u32;
    let exit = loop {
        if !running.load(Ordering::Acquire) {
            break WriterExit::Stopped;
        }
        mixer.render(&mut block, usize::from(CHANNELS));
        match sink.write(&block) {
            Ok(()) => consecutive_errors = 0,
            Err(error) => {
                control.record_device_error();
                consecutive_errors += 1;
                if consecutive_errors == 1 || consecutive_errors >= MAX_CONSECUTIVE_WRITE_ERRORS {
                    log::warn!(
                        "audio: PulseAudio write failed ({consecutive_errors}/{MAX_CONSECUTIVE_WRITE_ERRORS}): {error}"
                    );
                }
                if consecutive_errors >= MAX_CONSECUTIVE_WRITE_ERRORS {
                    break WriterExit::Failed;
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
    };
    // Explicit for clarity: a healthy PulseStream drains and frees here.
    drop(sink);
    exit
}

/// Engine-side owner of the writer thread. Dropping it disarms and joins the
/// thread, which drains and frees the PulseAudio stream.
pub(crate) struct Output {
    running: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Output {
    /// Whether the writer stopped because PulseAudio kept rejecting writes.
    pub(crate) fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Disarms the writer and waits for it to drain and exit. Idempotent.
    pub(crate) fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            self.failed.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Metadata reported back once the writer thread has opened the stream.
pub(crate) struct Started {
    pub(crate) output: Output,
    pub(crate) device_name: Option<String>,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    pub(crate) buffer_frames: u32,
    pub(crate) latency_ms: f32,
}

/// Loads libpulse-simple, opens the default playback stream and starts the
/// writer thread. Returns [`AudioError::NoOutputDevice`] when no server or
/// library is available; the caller then runs silently.
pub(crate) fn start(
    bank: Arc<ClipBank>,
    control: Arc<Control>,
    queue: Arc<CommandQueue>,
) -> Result<Started, AudioError> {
    let library = PulseLibrary::load().map_err(|details| {
        log::debug!("audio: libpulse-simple unavailable: {details}");
        AudioError::NoOutputDevice
    })?;
    let library = Arc::new(library);
    let running = Arc::new(AtomicBool::new(true));
    let failed = Arc::new(AtomicBool::new(false));
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let thread_running = Arc::clone(&running);
    let thread_failed = Arc::clone(&failed);
    let thread_control = Arc::clone(&control);
    let thread = thread::Builder::new()
        .name("ob-audio-pulse".to_string())
        .spawn(move || match PulseStream::open(library) {
            Ok((stream, info)) => {
                if ready_sender.send(Ok(info)).is_err() {
                    return;
                }
                let mixer = Mixer::new(bank, Arc::clone(&thread_control), queue, f64::from(SAMPLE_RATE));
                if run_writer(stream, mixer, &thread_control, &thread_running) == WriterExit::Failed {
                    thread_failed.store(true, Ordering::Relaxed);
                }
            }
            Err(error) => {
                let _ = ready_sender.send(Err(error));
            }
        })
        .map_err(|error| AudioError::Backend(format!("could not start audio thread: {error}")))?;

    match ready_receiver.recv() {
        Ok(Ok(info)) => Ok(Started {
            output: Output {
                running,
                failed,
                thread: Some(thread),
            },
            device_name: Some(info.device_name),
            sample_rate: info.sample_rate,
            channels: info.channels,
            buffer_frames: info.buffer_frames,
            latency_ms: info.latency_ms,
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            let message = format!("PulseAudio: {}", error.message);
            log::debug!("audio: {message} (error {})", error.code);
            if error.is_unavailable() {
                Err(AudioError::NoOutputDevice)
            } else {
                Err(AudioError::Backend(message))
            }
        }
        Err(_) => {
            let _ = thread.join();
            Err(AudioError::Backend(
                "audio thread exited before opening PulseAudio".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::mixer::PlayCommand;
    use crate::synth::{Clip, GROUP_COUNT, GroupRange};

    fn test_bank() -> Arc<ClipBank> {
        let clip = Clip {
            samples: vec![0.25; 4_800].into_boxed_slice(),
        };
        let groups = (0..GROUP_COUNT).map(|_| GroupRange { start: 0, count: 1 }).collect();
        Arc::new(ClipBank {
            groups,
            clips: vec![clip.clone()],
            ambient: clip.clone(),
            music: clip,
        })
    }

    fn test_mixer(control: &Arc<Control>, queue: &Arc<CommandQueue>) -> Mixer {
        Mixer::new(
            test_bank(),
            Arc::clone(control),
            Arc::clone(queue),
            f64::from(SAMPLE_RATE),
        )
    }

    fn fake_symbols() -> Vec<(&'static str, NonZeroUsize)> {
        [
            "pa_simple_new",
            "pa_simple_write",
            "pa_simple_drain",
            "pa_simple_free",
            "pa_strerror",
        ]
        .into_iter()
        .enumerate()
        .map(|(index, name)| (name, NonZeroUsize::new(0x1000 + index).expect("nonzero fake address")))
        .collect()
    }

    struct FakeHandle {
        symbols: Vec<(&'static str, NonZeroUsize)>,
        dropped: Arc<AtomicUsize>,
    }

    impl DlHandle for FakeHandle {
        fn symbol(&self, name: &str) -> Option<NonZeroUsize> {
            self.symbols
                .iter()
                .find(|(symbol, _)| *symbol == name)
                .map(|(_, address)| *address)
        }
    }

    impl Drop for FakeHandle {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    type FakeEntries = Vec<(&'static str, Option<Arc<FakeHandle>>)>;

    struct FakeLoader {
        entries: Mutex<FakeEntries>,
        opened: Mutex<Vec<String>>,
    }

    impl FakeLoader {
        fn new(entries: FakeEntries) -> Self {
            Self {
                entries: Mutex::new(entries),
                opened: Mutex::new(Vec::new()),
            }
        }

        fn opened(&self) -> Vec<String> {
            self.opened.lock().expect("opened lock").clone()
        }
    }

    impl DlLoader for FakeLoader {
        fn open(&self, name: &str) -> Option<Arc<dyn DlHandle>> {
            self.opened.lock().expect("opened lock").push(name.to_string());
            let mut entries = self.entries.lock().expect("entries lock");
            let entry = entries.iter_mut().find(|(candidate, _)| *candidate == name)?;
            entry.1.take().map(|handle| handle as Arc<dyn DlHandle>)
        }
    }

    fn fake_handle(dropped: &Arc<AtomicUsize>, symbols: Vec<(&'static str, NonZeroUsize)>) -> Arc<FakeHandle> {
        Arc::new(FakeHandle {
            symbols,
            dropped: Arc::clone(dropped),
        })
    }

    #[test]
    fn library_loading_falls_back_and_reports_missing_symbols() {
        let first_dropped = Arc::new(AtomicUsize::new(0));
        let second_dropped = Arc::new(AtomicUsize::new(0));
        let mut incomplete = fake_symbols();
        incomplete.retain(|(name, _)| *name != "pa_simple_drain");
        let first = fake_handle(&first_dropped, incomplete);
        let second = fake_handle(&second_dropped, fake_symbols());
        let loader = FakeLoader::new(vec![
            ("libpulse-simple.so.0", Some(Arc::clone(&first))),
            ("libpulse-simple.so", Some(Arc::clone(&second))),
        ]);

        let library = PulseLibrary::load_with(&loader).expect("second candidate must resolve");
        assert_eq!(
            loader.opened(),
            vec!["libpulse-simple.so.0".to_string(), "libpulse-simple.so".to_string()]
        );
        // The incomplete first handle was closed after the required-symbol
        // check; the test's last Arc keeps it observable.
        drop(first);
        assert_eq!(first_dropped.load(Ordering::Relaxed), 1);
        assert_eq!(library.strerror, Some(fake_symbols().last().expect("strerror").1));

        // The successful candidate remains loaded until the table is dropped.
        drop(library);
        drop(second);
        assert_eq!(second_dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn library_loading_reports_every_candidate_when_all_fail() {
        let loader = FakeLoader::new(vec![("libpulse-simple.so.0", None), ("libpulse-simple.so", None)]);
        let error = match PulseLibrary::load_with(&loader) {
            Ok(_) => panic!("no candidate can resolve"),
            Err(error) => error,
        };
        assert!(error.contains("libpulse-simple.so.0"), "{error}");
        assert!(error.contains("libpulse-simple.so"), "{error}");
        assert_eq!(
            loader.opened(),
            vec!["libpulse-simple.so.0".to_string(), "libpulse-simple.so".to_string()]
        );
    }

    #[test]
    fn every_required_symbol_is_checked_and_closes_the_handle() {
        for missing in ["pa_simple_new", "pa_simple_write", "pa_simple_drain", "pa_simple_free"] {
            let dropped = Arc::new(AtomicUsize::new(0));
            let mut symbols = fake_symbols();
            symbols.retain(|(name, _)| *name != missing);
            let handle = fake_handle(&dropped, symbols);
            let loader = FakeLoader::new(vec![("libpulse-simple.so.0", Some(Arc::clone(&handle)))]);
            let error = match PulseLibrary::load_with(&loader) {
                Ok(_) => panic!("missing required symbol must fail"),
                Err(error) => error,
            };
            assert!(error.contains(missing), "{error}");
            // The opened-but-unusable handle is dropped (dlclose) immediately.
            drop(handle);
            assert_eq!(
                dropped.load(Ordering::Relaxed),
                1,
                "{missing} leaked the library handle"
            );
        }
    }

    #[test]
    fn library_handle_lives_until_the_last_arc_and_nothing_else() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let handle = fake_handle(&dropped, fake_symbols());
        let loader = FakeLoader::new(vec![("libpulse-simple.so.0", Some(handle))]);
        let library = Arc::new(PulseLibrary::load_with(&loader).expect("library loads"));
        let clone = Arc::clone(&library);
        drop(library);
        assert_eq!(
            dropped.load(Ordering::Relaxed),
            0,
            "a live clone must keep the library loaded"
        );
        drop(clone);
        assert_eq!(
            dropped.load(Ordering::Relaxed),
            1,
            "the library is closed after the last user"
        );
    }

    #[test]
    fn resolved_types_are_send_and_sync_and_drop_is_idempotent() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PulseLibrary>();
        assert_send_sync::<PulseStream>();
        assert_send_sync::<Output>();
        assert_send_sync::<RealHandle>();

        let running = Arc::new(AtomicBool::new(false));
        let mut output = Output {
            running,
            failed: Arc::new(AtomicBool::new(false)),
            thread: None,
        };
        // No thread to join; repeated stop/drop must be a no-op rather than a
        // double free or panic.
        output.stop();
        output.stop();
        assert!(!output.is_failed());
        drop(output);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_dlopen_rejects_a_missing_library_and_resolves_libc_symbols() {
        let loader = RealLoader;
        assert!(loader.open("libopenblock-no-such-library-for-tests.so").is_none());
        let handle = loader.open("libc.so.6").expect("libc is present on Linux");
        assert!(handle.symbol("malloc").is_some());
        assert!(handle.symbol("openblock_symbol_that_does_not_exist").is_none());
    }

    #[test]
    fn open_error_classification_matches_pulse_codes() {
        for code in [
            PA_ERR_ACCESS,
            PA_ERR_NOENTITY,
            PA_ERR_CONNECTIONREFUSED,
            PA_ERR_TIMEOUT,
            PA_ERR_AUTHKEY,
            PA_ERR_CONNECTIONTERMINATED,
            PA_ERR_INVALIDSERVER,
        ] {
            assert!(
                PulseOpenError {
                    code,
                    message: String::new()
                }
                .is_unavailable(),
                "code {code} should mean no usable server"
            );
        }
        // A malformed request generated by this backend must remain an error,
        // not be silently downgraded to "no output device".
        assert!(
            !PulseOpenError {
                code: 2,
                message: String::new()
            }
            .is_unavailable()
        );
    }

    #[test]
    fn pa_strerror_is_optional() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut required = fake_symbols();
        required.retain(|(name, _)| *name != "pa_strerror");
        let handle = fake_handle(&dropped, required);
        let loader = FakeLoader::new(vec![("libpulse-simple.so.0", Some(handle))]);
        let library = PulseLibrary::load_with(&loader).expect("strerror is optional");
        assert!(library.strerror.is_none());
        assert_eq!(library.error_string(6), "PulseAudio error 6");
    }

    #[test]
    fn pulse_abi_and_buffer_target_match_the_contract() {
        assert_eq!(std::mem::size_of::<PaSampleSpec>(), 12);
        assert_eq!(std::mem::align_of::<PaSampleSpec>(), 4);
        assert_eq!(std::mem::size_of::<PaBufferAttr>(), 20);
        assert_eq!(PA_SAMPLE_FLOAT32LE, 5);
        assert_eq!(PA_STREAM_PLAYBACK, 1);
        assert_eq!(WRITE_BLOCK_FRAMES, 512);
        assert_eq!(WRITE_BLOCK_SAMPLES, 1_024);
        assert_eq!(BUFFER_FRAMES, 1_200);
        assert_eq!(TARGET_LENGTH_BYTES, 9_600);
        let spec = PaSampleSpec {
            format: PA_SAMPLE_FLOAT32LE,
            rate: SAMPLE_RATE,
            channels: CHANNELS as u8,
        };
        assert_eq!(spec.format, 5);
        assert_eq!(spec.rate, 48_000);
        assert_eq!(spec.channels, 2);
        let attr = playback_buffer_attr();
        assert_eq!(attr.tlength, 9_600);
        assert_eq!(attr.maxlength, 38_400);
        assert_eq!(attr.prebuf, u32::MAX);
        assert_eq!(attr.minreq, u32::MAX);
        assert_eq!(attr.fragsize, u32::MAX);
        let latency_ms = f32::from(BUFFER_FRAMES as u16) / SAMPLE_RATE as f32 * 1000.0;
        assert!((latency_ms - 25.0).abs() < 1e-6, "{latency_ms}");
    }

    struct RecordingSink {
        running: Arc<AtomicBool>,
        stop_after: usize,
        writes: usize,
        lengths: Arc<Mutex<Vec<usize>>>,
        peak: Arc<Mutex<f32>>,
        drops: Arc<AtomicUsize>,
    }

    impl PulseSink for RecordingSink {
        fn write(&mut self, samples: &[f32]) -> Result<(), String> {
            self.writes += 1;
            self.lengths.lock().expect("lengths lock").push(samples.len());
            let block_peak = samples.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()));
            let mut peak = self.peak.lock().expect("peak lock");
            *peak = peak.max(block_peak);
            if self.writes >= self.stop_after {
                self.running.store(false, Ordering::Release);
            }
            Ok(())
        }
    }

    impl Drop for RecordingSink {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn writer_renders_fixed_blocks_from_the_mixer_and_disarms() {
        let control = Arc::new(Control::new(1.0, 1.0, 0.0));
        let queue = Arc::new(CommandQueue::new());
        queue.push(
            PlayCommand {
                group: 0,
                seed: 7,
                gain: 1.0,
                pan: 0.0,
                rate: 1.0,
            },
            &control,
        );
        let mixer = test_mixer(&control, &queue);
        let running = Arc::new(AtomicBool::new(true));
        let lengths = Arc::new(Mutex::new(Vec::new()));
        let peak = Arc::new(Mutex::new(0.0));
        let drops = Arc::new(AtomicUsize::new(0));
        let sink = RecordingSink {
            running: Arc::clone(&running),
            stop_after: 4,
            writes: 0,
            lengths: Arc::clone(&lengths),
            peak: Arc::clone(&peak),
            drops: Arc::clone(&drops),
        };

        let exit = run_writer(sink, mixer, &control, &running);

        assert_eq!(exit, WriterExit::Stopped);
        assert_eq!(*lengths.lock().expect("lengths lock"), vec![WRITE_BLOCK_SAMPLES; 4]);
        assert!(
            *peak.lock().expect("peak lock") > 1.0e-4,
            "the queued pickup must become audible inside the first blocks"
        );
        assert_eq!(drops.load(Ordering::Relaxed), 1, "the sink is dropped exactly once");
        assert_eq!(control.device_errors(), 0);
    }

    struct FailingSink {
        failures_left: u32,
        fail_forever: bool,
        writes: usize,
        running: Arc<AtomicBool>,
        stop_after: usize,
        drops: Arc<AtomicUsize>,
    }

    impl PulseSink for FailingSink {
        fn write(&mut self, _samples: &[f32]) -> Result<(), String> {
            if self.fail_forever || self.failures_left > 0 {
                self.failures_left = self.failures_left.saturating_sub(1);
                return Err("server went away".to_string());
            }
            self.writes += 1;
            if self.writes >= self.stop_after {
                self.running.store(false, Ordering::Release);
            }
            Ok(())
        }
    }

    impl Drop for FailingSink {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn writer_gives_up_after_repeated_failures() {
        let control = Arc::new(Control::new(1.0, 1.0, 0.0));
        let queue = Arc::new(CommandQueue::new());
        let mixer = test_mixer(&control, &queue);
        let running = Arc::new(AtomicBool::new(true));
        let drops = Arc::new(AtomicUsize::new(0));
        let sink = FailingSink {
            failures_left: 0,
            fail_forever: true,
            writes: 0,
            running: Arc::clone(&running),
            stop_after: usize::MAX,
            drops: Arc::clone(&drops),
        };

        let exit = run_writer(sink, mixer, &control, &running);

        assert_eq!(exit, WriterExit::Failed);
        assert_eq!(control.device_errors(), MAX_CONSECUTIVE_WRITE_ERRORS as usize);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn transient_write_errors_are_retried() {
        let control = Arc::new(Control::new(1.0, 1.0, 0.0));
        let queue = Arc::new(CommandQueue::new());
        let mixer = test_mixer(&control, &queue);
        let running = Arc::new(AtomicBool::new(true));
        let drops = Arc::new(AtomicUsize::new(0));
        let sink = FailingSink {
            failures_left: 2,
            fail_forever: false,
            writes: 0,
            running: Arc::clone(&running),
            stop_after: 3,
            drops: Arc::clone(&drops),
        };

        let exit = run_writer(sink, mixer, &control, &running);

        assert_eq!(exit, WriterExit::Stopped);
        assert_eq!(control.device_errors(), 2);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn output_drop_disarms_and_joins_the_writer_thread() {
        let running = Arc::new(AtomicBool::new(true));
        let failed = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let thread = {
            let running = Arc::clone(&running);
            let finished = Arc::clone(&finished);
            thread::spawn(move || {
                while running.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                finished.store(true, Ordering::Release);
            })
        };
        let mut output = Output {
            running: Arc::clone(&running),
            failed,
            thread: Some(thread),
        };

        assert!(!output.is_failed());
        output.stop();
        assert!(!running.load(Ordering::Acquire));
        assert!(finished.load(Ordering::Acquire), "stop must join the writer");
        output.stop();
        assert!(!output.is_failed());
        drop(output);
    }
}
