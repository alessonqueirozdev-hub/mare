//! The WASAPI device itself.

use std::fmt;

use mare_hub::{AudioBlock, Device, DeviceConfig, DeviceError, DeviceLatency};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_EXCLUSIVE,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, EDataFlow, IAudioCaptureClient,
    IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0, eCapture, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CO_MTA_USAGE_COOKIE, CoCreateInstance, CoDecrementMTAUsage, CoIncrementMTAUsage,
    CoTaskMemFree, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{HSTRING, PWSTR};

use crate::fifo::{CaptureFifo, CaptureMonitor, CaptureStats};
use crate::format::{SampleFormat, interleave};
use crate::{Endpoint, Latency, Mode, OpenOptions, WasapiError};

/// How long a period may take before the device counts as stopped.
const PERIOD_TIMEOUT_MS: u32 = 2000;

fn os(what: &'static str) -> impl FnOnce(windows::core::Error) -> WasapiError {
    move |e| WasapiError::Os {
        what,
        code: e.code().0,
    }
}

/// Keeps COM's multithreaded apartment alive, so that every thread that has
/// not initialised COM itself (the hub's cycle thread among them) is
/// implicitly in it and may use the device's objects.
struct Mta(CO_MTA_USAGE_COOKIE);

impl Mta {
    fn enter() -> Result<Self, WasapiError> {
        // SAFETY: plain call.
        unsafe { CoIncrementMTAUsage() }
            .map(Self)
            .map_err(os("CoIncrementMTAUsage"))
    }
}

impl Drop for Mta {
    fn drop(&mut self) {
        // SAFETY: the cookie came from `CoIncrementMTAUsage`.
        let _ = unsafe { CoDecrementMTAUsage(self.0) };
    }
}

/// Runs `f` on a new thread that has not initialised COM, and so is in the
/// multithreaded apartment `mta` keeps alive, whatever apartment the caller's
/// thread is in. Objects made there may be used from any such thread.
fn in_mta<T: Send>(
    _mta: &Mta,
    f: impl FnOnce() -> Result<T, WasapiError> + Send,
) -> Result<T, WasapiError> {
    std::thread::scope(|scope| {
        scope
            .spawn(f)
            .join()
            .unwrap_or(Err(WasapiError::Unsupported("the COM thread panicked")))
    })
}

fn enumerator() -> Result<IMMDeviceEnumerator, WasapiError> {
    // SAFETY: plain call, in the multithreaded apartment.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(os("creating the device enumerator"))
}

/// Takes a string COM allocated, and frees it.
///
/// # Safety
///
/// `text` must be a NUL-terminated string allocated with `CoTaskMemAlloc`.
unsafe fn take_string(text: PWSTR) -> String {
    // SAFETY: as the caller guarantees.
    let string = unsafe { text.to_string() }.unwrap_or_default();
    // SAFETY: as the caller guarantees; freed once.
    unsafe { CoTaskMemFree(Some(text.0.cast_const().cast())) };
    string
}

fn device_id(device: &IMMDevice) -> Result<String, WasapiError> {
    // SAFETY: `GetId` returns a string allocated with `CoTaskMemAlloc`.
    unsafe { device.GetId().map(|id| take_string(id)) }.map_err(os("reading a device id"))
}

fn friendly_name(device: &IMMDevice) -> Result<String, WasapiError> {
    // SAFETY: plain calls on valid objects; the variant is cleared once and
    // the string freed once.
    unsafe {
        let store = device
            .OpenPropertyStore(STGM_READ)
            .map_err(os("opening a device's properties"))?;
        let mut value = store
            .GetValue(&PKEY_Device_FriendlyName)
            .map_err(os("reading a device's name"))?;
        let name = PropVariantToStringAlloc(&value).map(|text| take_string(text));
        let _ = PropVariantClear(&mut value);
        name.map_err(os("reading a device's name"))
    }
}

pub(crate) fn render_endpoints() -> Result<Vec<Endpoint>, WasapiError> {
    endpoints(eRender)
}

pub(crate) fn capture_endpoints() -> Result<Vec<Endpoint>, WasapiError> {
    endpoints(eCapture)
}

fn endpoints(flow: EDataFlow) -> Result<Vec<Endpoint>, WasapiError> {
    let mta = Mta::enter()?;
    in_mta(&mta, || {
        let enumerator = enumerator()?;
        // SAFETY: plain calls on valid objects.
        unsafe {
            let default = enumerator
                .GetDefaultAudioEndpoint(flow, eConsole)
                .ok()
                .and_then(|d| device_id(&d).ok());
            let collection = enumerator
                .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
                .map_err(os("listing devices"))?;
            let count = collection.GetCount().map_err(os("listing devices"))?;
            let mut endpoints = Vec::with_capacity(count as usize);
            for index in 0..count {
                let device = collection.Item(index).map_err(os("listing devices"))?;
                let id = device_id(&device)?;
                endpoints.push(Endpoint {
                    name: friendly_name(&device).unwrap_or_default(),
                    is_default: default.as_deref() == Some(id.as_str()),
                    id,
                });
            }
            Ok(endpoints)
        }
    })
}

/// What a `WAVEFORMATEX` (or its extensible form) says.
#[derive(Clone, Copy, Debug)]
struct Parsed {
    sample: Option<SampleFormat>,
    channels: u16,
    rate: u32,
    mask: u32,
}

/// # Safety
///
/// `format` must point to a valid `WAVEFORMATEX`, followed by the
/// extensible fields when its tag says so.
unsafe fn parse(format: *const WAVEFORMATEX) -> Parsed {
    // SAFETY: as the caller guarantees; the structures are packed, so they
    // are read unaligned.
    let base = unsafe { format.read_unaligned() };
    let bits = base.wBitsPerSample;
    let tag = u32::from(base.wFormatTag);
    let (sample, mask) = if tag == WAVE_FORMAT_EXTENSIBLE && base.cbSize >= 22 {
        // SAFETY: as above.
        let ext = unsafe { format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
        // SAFETY: every member of the union is a u16.
        let valid = unsafe { ext.Samples.wValidBitsPerSample };
        let sub = ext.SubFormat;
        let sample = if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
            Some(SampleFormat::F32)
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
            match (bits, valid) {
                (16, 16) => Some(SampleFormat::I16),
                (24, 24) => Some(SampleFormat::I24),
                (32, 24) => Some(SampleFormat::I24In32),
                (32, 32) => Some(SampleFormat::I32),
                _ => None,
            }
        } else {
            None
        };
        (sample, ext.dwChannelMask)
    } else {
        let sample = match (tag, bits) {
            (WAVE_FORMAT_IEEE_FLOAT, 32) => Some(SampleFormat::F32),
            (WAVE_FORMAT_PCM, 16) => Some(SampleFormat::I16),
            (WAVE_FORMAT_PCM, 24) => Some(SampleFormat::I24),
            (WAVE_FORMAT_PCM, 32) => Some(SampleFormat::I32),
            _ => None,
        };
        (sample, default_mask(base.nChannels))
    };
    Parsed {
        sample,
        channels: base.nChannels,
        rate: base.nSamplesPerSec,
        mask,
    }
}

fn default_mask(channels: u16) -> u32 {
    match channels {
        1 => 0x4, // front centre
        2 => 0x3, // front left and right
        n => (1u32 << n.min(31)) - 1,
    }
}

fn extensible(sample: SampleFormat, channels: u16, rate: u32, mask: u32) -> WAVEFORMATEXTENSIBLE {
    let bytes = sample.bytes() as u16;
    let block = channels * bytes;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: bytes * 8,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: sample.valid_bits(),
        },
        dwChannelMask: mask,
        SubFormat: if sample == SampleFormat::F32 {
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            KSDATAFORMAT_SUBTYPE_PCM
        },
    }
}

/// Frames as a `REFERENCE_TIME`, in units of 100 ns, rounded.
fn frames_to_hns(frames: u32, rate: u32) -> i64 {
    ((10_000_000 * u64::from(frames) + u64::from(rate) / 2) / u64::from(rate)) as i64
}

fn hns_to_frames(hns: i64, rate: u32) -> u32 {
    ((hns.max(0) as u64 * u64::from(rate) + 5_000_000) / 10_000_000) as u32
}

/// An auto-reset event, closed on drop.
struct Event(HANDLE);

impl Event {
    fn new() -> Result<Self, WasapiError> {
        // SAFETY: plain call. Auto-reset, initially not signalled, unnamed.
        unsafe { CreateEventW(None, false, false, None) }
            .map(Self)
            .map_err(os("creating the device's event"))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle is open and owned by this value.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A render endpoint opened for event-driven output.
pub struct WasapiDevice {
    // Drop order: the streams, then their events, then the apartment.
    input: Option<Input>,
    render: IAudioRenderClient,
    client: IAudioClient,
    event: Event,
    mode: Mode,
    name: String,
    id: String,
    sample: SampleFormat,
    channels: usize,
    sample_rate: u32,
    period_frames: u32,
    buffer_frames: u32,
    stream_latency_frames: u32,
    started: bool,
    pending: u32,
    _mta: Mta,
}

// SAFETY: every COM object here was made in the multithreaded apartment,
// which the device keeps alive, and objects of that apartment may be used
// from any of its threads; the hub's cycle thread is one (implicitly, as it
// does not initialise COM). The device is used by one thread at a time.
unsafe impl Send for WasapiDevice {}

impl fmt::Debug for WasapiDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasapiDevice")
            .field("name", &self.name)
            .field("mode", &self.mode)
            .field("sample", &self.sample)
            .field("channels", &self.channels)
            .field("sample_rate", &self.sample_rate)
            .field("latency", &self.latency_parts())
            .finish_non_exhaustive()
    }
}

/// The parts of a stream that opening returns.
struct Stream {
    client: IAudioClient,
    sample: SampleFormat,
    channels: u16,
    rate: u32,
    period_frames: u32,
}

/// Picks the shared-mode period closest to `requested` (rounding up) that the
/// engine allows: the minimum plus a whole number of fundamental periods, at
/// most the maximum. With nothing requested, the minimum.
pub(crate) fn shared_period(requested: Option<u32>, min: u32, max: u32, fundamental: u32) -> u32 {
    let Some(requested) = requested else {
        return min;
    };
    if requested <= min || fundamental == 0 {
        return min;
    }
    let steps = (requested - min).div_ceil(fundamental);
    (min + steps * fundamental).min(max)
}

fn open_shared(device: &IMMDevice, options: &OpenOptions) -> Result<Stream, WasapiError> {
    if !options.small_shared_periods {
        return open_shared_default(device);
    }
    // SAFETY: plain calls on valid objects; the mix format is freed once,
    // after its last use.
    unsafe {
        let Ok(client) = device.Activate::<IAudioClient3>(CLSCTX_ALL, None) else {
            // Before Windows 10: the engine's own period.
            return open_shared_default(device);
        };
        let mix = client
            .GetMixFormat()
            .map_err(os("reading the mix format"))?;
        let parsed = parse(mix);
        let result = (|| {
            let sample = parsed
                .sample
                .ok_or(WasapiError::Unsupported("the device's mix format"))?;
            let (mut default, mut fundamental, mut min, mut max) = (0, 0, 0, 0);
            client
                .GetSharedModeEnginePeriod(mix, &mut default, &mut fundamental, &mut min, &mut max)
                .map_err(os("reading the engine's periods"))?;
            let period = shared_period(options.period_frames, min, max, fundamental);
            client
                .InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, period, mix, None)
                .map_err(os("initialising the shared stream"))?;
            Ok((sample, period))
        })();
        CoTaskMemFree(Some(mix.cast_const().cast()));
        let (sample, period) = result?;
        Ok(Stream {
            client: client.into(),
            sample,
            channels: parsed.channels,
            rate: parsed.rate,
            period_frames: period,
        })
    }
}

/// Shared mode through `IAudioClient`, at the engine's default period.
fn open_shared_default(device: &IMMDevice) -> Result<Stream, WasapiError> {
    // SAFETY: plain calls on valid objects; the mix format is freed once,
    // after its last use.
    unsafe {
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(os("activating the device"))?;
        let mix = client
            .GetMixFormat()
            .map_err(os("reading the mix format"))?;
        let parsed = parse(mix);
        let result = (|| {
            let sample = parsed
                .sample
                .ok_or(WasapiError::Unsupported("the device's mix format"))?;
            let (mut default_hns, mut min_hns) = (0i64, 0i64);
            client
                .GetDevicePeriod(Some(&mut default_hns), Some(&mut min_hns))
                .map_err(os("reading the device's periods"))?;
            // Event-driven shared mode: a buffer of the default period, and
            // a periodicity of zero, which means the engine's own.
            client
                .Initialize(
                    windows::Win32::Media::Audio::AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    default_hns,
                    0,
                    mix,
                    None,
                )
                .map_err(os("initialising the shared stream"))?;
            Ok((sample, hns_to_frames(default_hns, parsed.rate)))
        })();
        CoTaskMemFree(Some(mix.cast_const().cast()));
        let (sample, period) = result?;
        Ok(Stream {
            client,
            sample,
            channels: parsed.channels,
            rate: parsed.rate,
            period_frames: period,
        })
    }
}

fn open_exclusive(device: &IMMDevice, options: &OpenOptions) -> Result<Stream, WasapiError> {
    // SAFETY: plain calls on valid objects; the mix format is freed once.
    unsafe {
        let mut client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(os("activating the device"))?;
        let mix = client
            .GetMixFormat()
            .map_err(os("reading the mix format"))?;
        let parsed = parse(mix);
        CoTaskMemFree(Some(mix.cast_const().cast()));
        let rate = options.sample_rate.unwrap_or(parsed.rate);
        let channels = parsed.channels;
        let candidates = [
            SampleFormat::F32,
            SampleFormat::I32,
            SampleFormat::I24In32,
            SampleFormat::I24,
            SampleFormat::I16,
        ];
        let (sample, format) = candidates
            .into_iter()
            .map(|s| (s, extensible(s, channels, rate, parsed.mask)))
            .find(|(_, f)| {
                client
                    .IsFormatSupported(
                        AUDCLNT_SHAREMODE_EXCLUSIVE,
                        (f as *const WAVEFORMATEXTENSIBLE).cast(),
                        None,
                    )
                    .is_ok()
            })
            .ok_or(WasapiError::Unsupported(
                "no exclusive format the device takes",
            ))?;
        let format_ptr: *const WAVEFORMATEX = (&format as *const WAVEFORMATEXTENSIBLE).cast();

        let (mut default_hns, mut min_hns) = (0i64, 0i64);
        client
            .GetDevicePeriod(Some(&mut default_hns), Some(&mut min_hns))
            .map_err(os("reading the device's periods"))?;
        let mut hns = options
            .period_frames
            .map_or(min_hns, |frames| frames_to_hns(frames, rate))
            .max(min_hns);
        let init = |client: &IAudioClient, hns: i64| {
            client.Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                hns,
                hns,
                format_ptr,
                None,
            )
        };
        if let Err(error) = init(&client, hns) {
            if error.code() != AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
                return Err(os("initialising the exclusive stream")(error));
            }
            // The driver wants a period of a whole number of its own blocks:
            // ask again with the size it chose, on a fresh client.
            let frames = client
                .GetBufferSize()
                .map_err(os("reading the buffer size"))?;
            hns = frames_to_hns(frames, rate);
            client = device
                .Activate(CLSCTX_ALL, None)
                .map_err(os("activating the device"))?;
            init(&client, hns).map_err(os("initialising the exclusive stream"))?;
        }
        let period = client
            .GetBufferSize()
            .map_err(os("reading the buffer size"))?;
        Ok(Stream {
            client,
            sample,
            channels,
            rate,
            period_frames: period,
        })
    }
}

/// A capture stream opened beside the output, in the same mode, at the same
/// sample rate, with a period as close to the output's as it allows.
struct Input {
    capture: IAudioCaptureClient,
    client: IAudioClient,
    _event: Event,
    name: String,
    id: String,
    sample: SampleFormat,
    channels: usize,
    buffer_frames: u32,
    fifo: CaptureFifo,
}

fn open_input(
    enumerator: &IMMDeviceEnumerator,
    id: &str,
    options: &OpenOptions,
    rate: u32,
    period: u32,
) -> Result<Input, WasapiError> {
    // SAFETY: plain call on a valid object.
    let device =
        unsafe { enumerator.GetDevice(&HSTRING::from(id)) }.map_err(|_| WasapiError::NoDevice)?;
    let name = friendly_name(&device).unwrap_or_default();
    let id = device_id(&device)?;
    let wanted = OpenOptions {
        mode: options.mode,
        period_frames: Some(period),
        sample_rate: Some(rate),
        small_shared_periods: options.small_shared_periods,
    };
    let stream = match options.mode {
        Mode::Shared => open_shared(&device, &wanted)?,
        Mode::Exclusive => open_exclusive(&device, &wanted)?,
    };
    if stream.rate != rate {
        return Err(WasapiError::Unsupported(
            "an input at another sample rate than the output",
        ));
    }
    let event = Event::new()?;
    // SAFETY: plain calls on the initialised client.
    unsafe {
        let client = &stream.client;
        // Event-driven streams need an event, though the hub never waits on
        // this one: the output paces the cycle, and the input is drained.
        client
            .SetEventHandle(event.0)
            .map_err(os("setting the input event"))?;
        let buffer = client
            .GetBufferSize()
            .map_err(os("reading the input buffer size"))?;
        if buffer > mare_hub::MAX_FRAMES {
            return Err(WasapiError::Unsupported("a buffer longer than 8192 frames"));
        }
        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(os("getting the capture client"))?;
        let channels = stream.channels as usize;
        Ok(Input {
            capture,
            client: stream.client.clone(),
            _event: event,
            name,
            id,
            sample: stream.sample,
            channels,
            buffer_frames: buffer,
            // Sized in `open`, once the output buffer is known.
            fifo: CaptureFifo::new(channels, 1, 1),
        })
    }
}

pub(crate) fn open(
    endpoint: Option<&str>,
    input: Option<&str>,
    options: &OpenOptions,
) -> Result<WasapiDevice, WasapiError> {
    let mta = Mta::enter()?;
    let opened = in_mta(&mta, || {
        let enumerator = enumerator()?;
        // SAFETY: plain calls on valid objects.
        let device = unsafe {
            match endpoint {
                Some(id) => enumerator.GetDevice(&HSTRING::from(id)),
                None => enumerator.GetDefaultAudioEndpoint(eRender, eConsole),
            }
        }
        .map_err(|_| WasapiError::NoDevice)?;
        let name = friendly_name(&device).unwrap_or_default();
        let id = device_id(&device)?;
        let stream = match options.mode {
            Mode::Shared => open_shared(&device, options)?,
            Mode::Exclusive => open_exclusive(&device, options)?,
        };
        let input = match input {
            Some(id) => Some(open_input(
                &enumerator,
                id,
                options,
                stream.rate,
                stream.period_frames,
            )?),
            None => None,
        };
        let event = Event::new()?;
        // SAFETY: plain calls on the initialised client.
        unsafe {
            let client = &stream.client;
            client
                .SetEventHandle(event.0)
                .map_err(os("setting the device's event"))?;
            let buffer = client
                .GetBufferSize()
                .map_err(os("reading the buffer size"))?;
            if buffer > mare_hub::MAX_FRAMES {
                return Err(WasapiError::Unsupported("a buffer longer than 8192 frames"));
            }
            let latency = client
                .GetStreamLatency()
                .map_err(os("reading the latency"))?;
            let render: IAudioRenderClient = client
                .GetService()
                .map_err(os("getting the render client"))?;
            if options.mode == Mode::Exclusive {
                // The first period must be in place before the stream starts.
                render
                    .GetBuffer(buffer)
                    .map_err(os("filling the first period"))?;
                render
                    .ReleaseBuffer(buffer, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
                    .map_err(os("filling the first period"))?;
            }
            let input = input.map(|mut input| {
                // A cycle takes the output period; the backlog may hold up to
                // two of the larger of the two buffers.
                let backlog = 2 * buffer.max(input.buffer_frames) as usize;
                input.fifo =
                    CaptureFifo::new(input.channels, stream.period_frames as usize, backlog);
                // One period of cushion: see `fifo`.
                input.fifo.prime(stream.period_frames as usize);
                input
            });
            Ok(Opened {
                input,
                render,
                client: stream.client.clone(),
                event,
                name,
                id,
                sample: stream.sample,
                channels: stream.channels as usize,
                sample_rate: stream.rate,
                period_frames: stream.period_frames,
                buffer_frames: buffer,
                stream_latency_frames: hns_to_frames(latency, stream.rate),
            })
        }
    })?;
    Ok(WasapiDevice {
        input: opened.input,
        render: opened.render,
        client: opened.client,
        event: opened.event,
        mode: options.mode,
        name: opened.name,
        id: opened.id,
        sample: opened.sample,
        channels: opened.channels,
        sample_rate: opened.sample_rate,
        period_frames: opened.period_frames,
        buffer_frames: opened.buffer_frames,
        stream_latency_frames: opened.stream_latency_frames,
        started: false,
        pending: 0,
        _mta: mta,
    })
}

/// What the COM thread hands back.
struct Opened {
    input: Option<Input>,
    render: IAudioRenderClient,
    client: IAudioClient,
    event: Event,
    name: String,
    id: String,
    sample: SampleFormat,
    channels: usize,
    sample_rate: u32,
    period_frames: u32,
    buffer_frames: u32,
    stream_latency_frames: u32,
}

// SAFETY: made in the multithreaded apartment, which the caller keeps alive;
// see `WasapiDevice`.
unsafe impl Send for Opened {}

fn device_error(error: windows::core::Error) -> DeviceError {
    if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        DeviceError::Lost
    } else {
        DeviceError::Os(i64::from(error.code().0))
    }
}

impl WasapiDevice {
    /// The device's name, for people to read.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The endpoint's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// How the device was opened.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The format of the device's buffer.
    pub fn sample_format(&self) -> SampleFormat {
        self.sample
    }

    /// The input name, if the device was opened with one.
    pub fn input_name(&self) -> Option<&str> {
        self.input.as_ref().map(|input| input.name.as_str())
    }

    /// The input endpoint id, if the device was opened with one.
    pub fn input_id(&self) -> Option<&str> {
        self.input.as_ref().map(|input| input.id.as_str())
    }

    /// The format of the input buffer, if there is an input.
    pub fn input_sample_format(&self) -> Option<SampleFormat> {
        self.input.as_ref().map(|input| input.sample)
    }

    /// What the input has delivered, and the corrections made to keep it in
    /// step with the output, if there is an input.
    pub fn input_stats(&self) -> Option<CaptureStats> {
        self.input.as_ref().map(|input| input.fifo.stats())
    }

    /// A handle to read the input counters from another thread while the
    /// hub runs the device, if there is an input. Take it before handing the
    /// device to the hub.
    pub fn input_monitor(&self) -> Option<CaptureMonitor> {
        self.input.as_ref().map(|input| input.fifo.monitor())
    }

    /// Frames the input's buffer holds, if there is an input.
    pub fn input_buffer_frames(&self) -> Option<u32> {
        self.input.as_ref().map(|input| input.buffer_frames)
    }

    /// Frames of input queued now, cushion included: how far the input lags
    /// what the hub hands on, beyond the devices' own latency.
    pub fn input_backlog(&self) -> Option<usize> {
        self.input.as_ref().map(|input| input.fifo.len())
    }

    /// What the device's latency is made of, as WASAPI reports it.
    pub fn latency_parts(&self) -> Latency {
        Latency {
            period_frames: self.period_frames,
            buffer_frames: self.buffer_frames,
            stream_latency_frames: self.stream_latency_frames,
        }
    }
}

impl Device for WasapiDevice {
    /// An estimate from WASAPI's figures, which leave out what the
    /// converters, the bus and parts of the driver add:
    ///
    /// - output, exclusive mode: the buffer the device plays while the hub
    ///   fills the next one, or the stream latency the driver reports if
    ///   that is longer;
    /// - output, shared mode: what is still queued when the hub writes (the
    ///   buffer less the period it fills), plus the stream latency the
    ///   driver reports for the system mixer;
    /// - input: the capture buffer a frame waits in before it is handed
    ///   over, plus the period of cushion the input queue keeps (see
    ///   [`fifo`](crate::fifo)).
    ///
    /// Measure the real figures with a loopback cable.
    fn latency(&self) -> DeviceLatency {
        let output_frames = match self.mode {
            Mode::Exclusive => self.buffer_frames.max(self.stream_latency_frames),
            Mode::Shared => {
                self.buffer_frames.saturating_sub(self.period_frames) + self.stream_latency_frames
            }
        };
        let input_frames = self
            .input
            .as_ref()
            .map_or(0, |input| input.buffer_frames + self.period_frames);
        DeviceLatency {
            input_frames,
            output_frames,
        }
    }

    fn config(&self) -> DeviceConfig {
        DeviceConfig {
            sample_rate: self.sample_rate,
            period_frames: self.period_frames,
            max_frames: self.buffer_frames.max(self.period_frames),
            input_channels: self.input.as_ref().map_or(0, |input| input.channels as u32),
            output_channels: self.channels as u32,
        }
    }

    fn wait_period(&mut self) -> Result<usize, DeviceError> {
        if !self.started {
            if let Some(input) = &self.input {
                // SAFETY: the client is initialised and not started.
                unsafe { input.client.Start() }.map_err(device_error)?;
            }
            // SAFETY: the client is initialised and not started.
            unsafe { self.client.Start() }.map_err(device_error)?;
            self.started = true;
        }
        loop {
            // SAFETY: the event is open.
            let woke = unsafe { WaitForSingleObject(self.event.0, PERIOD_TIMEOUT_MS) };
            if woke != WAIT_OBJECT_0 {
                return Err(DeviceError::Timeout);
            }
            let frames = match self.mode {
                Mode::Exclusive => self.buffer_frames,
                Mode::Shared => {
                    // SAFETY: the client is running.
                    let padding =
                        unsafe { self.client.GetCurrentPadding() }.map_err(device_error)?;
                    let free = self.buffer_frames.saturating_sub(padding);
                    if free == 0 {
                        continue;
                    }
                    free.min(self.period_frames)
                }
            };
            self.pending = frames;
            return Ok(frames as usize);
        }
    }

    fn read(&mut self, inputs: &mut AudioBlock<'_>) -> Result<(), DeviceError> {
        let Some(input) = self.input.as_mut() else {
            return Ok(());
        };
        let frame_bytes = input.channels * input.sample.bytes();
        // Take every packet the input has, then one period from the queue.
        loop {
            // SAFETY: the client is running.
            let next = unsafe { input.capture.GetNextPacketSize() }.map_err(device_error)?;
            if next == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut position = 0u64;
            // SAFETY: on success `data` points to `frames` frames of the
            // input format, ours until `ReleaseBuffer`.
            unsafe {
                input
                    .capture
                    .GetBuffer(
                        &mut data,
                        &mut frames,
                        &mut flags,
                        Some(&mut position),
                        None,
                    )
                    .map_err(device_error)?;
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    input.fifo.note_discontinuity();
                }
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                // Only in exclusive mode is a packet's device position in the
                // stream's own frames, and only there does the system hand a
                // packet over twice. In shared mode the position counts the
                // device's frames, which the engine may convert: the Realtek
                // microphone's advance 441 for every 480 of the stream.
                let exclusive = self.mode == Mode::Exclusive;
                match (silent, exclusive) {
                    (true, true) => input.fifo.push_silence_at(position, frames as usize),
                    (true, false) => input.fifo.push_silent_packet(frames as usize),
                    (false, exclusive) => {
                        let bytes = std::slice::from_raw_parts(data, frames as usize * frame_bytes);
                        if exclusive {
                            input
                                .fifo
                                .push_at(position, bytes, input.sample, frames as usize);
                        } else {
                            input.fifo.push(bytes, input.sample, frames as usize);
                        }
                    }
                }
                input.capture.ReleaseBuffer(frames).map_err(device_error)?;
            }
        }
        input.fifo.pop(inputs);
        Ok(())
    }

    fn write(&mut self, outputs: &AudioBlock<'_>) -> Result<(), DeviceError> {
        let frames = self.pending;
        // SAFETY: `GetBuffer` returns room for `frames` frames of the stream's
        // format, ours until `ReleaseBuffer`.
        unsafe {
            let data = self.render.GetBuffer(frames).map_err(device_error)?;
            let bytes = frames as usize * self.channels * self.sample.bytes();
            let out = std::slice::from_raw_parts_mut(data, bytes);
            if outputs.frames() == frames as usize {
                interleave(outputs, self.sample, self.channels, out);
            } else {
                out.fill(0);
            }
            self.render.ReleaseBuffer(frames, 0).map_err(device_error)
        }
    }
}

impl Drop for WasapiDevice {
    fn drop(&mut self) {
        if self.started {
            // SAFETY: the clients are running.
            unsafe {
                let _ = self.client.Stop();
                if let Some(input) = &self.input {
                    let _ = input.client.Stop();
                }
            }
        }
    }
}
