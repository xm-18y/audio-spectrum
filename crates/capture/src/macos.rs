//! macOS Loopback backend: ScreenCaptureKit (ADR-0001, macOS 13+).
//!
//! SCK captures the current system audio output as float32 at the sample rate
//! and channel count we request in `SCStreamConfiguration`, so the backend's
//! normalization job is only stereo→mono mixdown (resampling is a no-op at
//! 48 kHz). The SCK callback runs on our private serial dispatch queue and
//! obeys the same invariant as every Capture Backend audio callback: write to
//! the ring buffer, never allocate or lock.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_media::{CMBlockBuffer, CMSampleBuffer};
use objc2_foundation::{NSArray, NSError, NSObject, NSOperatingSystemVersion, NSProcessInfo};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};

use crate::{
    CaptureError, CaptureStream, DeviceInfo, FeedCtx, SourceKind, StreamHolder, TARGET_SAMPLE_RATE,
};

/// The single Loopback Source SCK offers: whatever the system is playing.
pub(crate) const SYSTEM_OUTPUT_ID: &str = "macos-system-output";

pub(crate) fn macos_13_plus() -> bool {
    NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(NSOperatingSystemVersion {
        majorVersion: 13,
        minorVersion: 0,
        patchVersion: 0,
    })
}

pub(crate) fn list_loopback_devices() -> Vec<DeviceInfo> {
    if !macos_13_plus() {
        return Vec::new();
    }
    vec![DeviceInfo {
        kind: SourceKind::Loopback,
        id: SYSTEM_OUTPUT_ID.to_string(),
        name: "系统音频输出".to_string(),
    }]
}

pub(crate) fn open_loopback(source: &DeviceInfo) -> Result<CaptureStream, CaptureError> {
    if !macos_13_plus() {
        return Err(CaptureError::Unsupported(
            "Loopback 需要 macOS 13 或更高版本(ADR-0001)",
        ));
    }
    if source.id != SYSTEM_OUTPUT_ID {
        return Err(CaptureError::DeviceGone(source.name.clone()));
    }

    // Fetch displays (SCK needs one for the content filter even for audio).
    let content = fetch_shareable_content()?;
    let display = unsafe { content.displays() }
        .firstObject()
        .ok_or_else(|| CaptureError::OpenFailed("未找到可用显示器".into()))?;

    // Configure: audio-only, 48 kHz stereo (mixdown to mono in the callback).
    // Note: `capturesCurrentAudioOutput` (macOS 13–15) does not exist on
    // macOS 26 — display-wide filter + `capturesAudio` is the route there.
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setCapturesAudio(true);
        config.setExcludesCurrentProcessAudio(true);
        config.setSampleRate(TARGET_SAMPLE_RATE as isize);
        config.setChannelCount(2);
    }

    let filter: Retained<SCContentFilter> = unsafe {
        let empty: Retained<NSArray<SCWindow>> = NSArray::new();
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &empty,
        )
    };

    let (prod, cons) = crate::split_ring();
    let feed = FeedCtx::new(prod, TARGET_SAMPLE_RATE, 2);
    let handler = AudioHandler::new(feed);

    let stream: Retained<SCStream> = unsafe {
        SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None)
    };
    let queue = DispatchQueue::new("audio-spectrum.sck.audio", None);

    unsafe {
        let output: Retained<ProtocolObject<dyn SCStreamOutput>> =
            ProtocolObject::from_retained(handler.clone());
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                &output,
                SCStreamOutputType::Audio,
                Some(&queue),
            )
            .map_err(|e| CaptureError::OpenFailed(format!("addStreamOutput 失败:{}", e)))?;

        // Block until capture actually starts (or fails / times out).
        let (tx, rx) = mpsc::channel::<Option<Retained<NSError>>>();
        let start_block = RcBlock::new(move |error: *mut NSError| {
            let error = retain_error(error);
            let _ = tx.send(error);
        });
        stream.startCaptureWithCompletionHandler(Some(&start_block));
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(None) => {}
            Ok(Some(e)) => {
                return Err(CaptureError::OpenFailed(format!("启动采集失败:{e}")));
            }
            Err(_) => return Err(CaptureError::OpenFailed("启动采集超时".into())),
        }
    }

    let session = SckSession {
        _stream: stream,
        _handler: handler,
        _queue: queue,
    };
    let dead = Arc::new(AtomicBool::new(false));
    Ok(CaptureStream::new(StreamHolder::Sck(session), cons, dead))
}

/// Blocks on SCK's async shareable-content fetch.
fn fetch_shareable_content() -> Result<Retained<SCShareableContent>, CaptureError> {
    let (tx, rx) = mpsc::channel::<Option<Retained<SCShareableContent>>>();
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            if !error.is_null() {
                let _ = tx.send(None);
                return;
            }
            // The completion handler hands over a valid reference; retain it
            // so it outlives the block. (Treated as borrowed: worst case
            // this leaks one reference of a system singleton, once per open.)
            let content =
                NonNull::new(content).and_then(|nn| unsafe { Retained::retain(nn.as_ptr()) });
            let _ = tx.send(content);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, false, &block,
        );
    }
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Some(content)) => Ok(content),
        _ => Err(CaptureError::OpenFailed("无法获取屏幕捕获内容".into())),
    }
}

fn retain_error(error: *mut NSError) -> Option<Retained<NSError>> {
    NonNull::new(error).and_then(|nn| unsafe { Retained::retain(nn.as_ptr()) })
}

/// Keeps the SCK machinery alive for as long as the stream runs.
pub(crate) struct SckSession {
    _stream: Retained<SCStream>,
    _handler: Retained<AudioHandler>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
}

// ---------------------------------------------------------------------------
// The SCStreamOutput delegate class
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AudioSpectrumSckAudioHandler"]
    #[ivars = HandlerIvars]
    struct AudioHandler;

    unsafe impl NSObjectProtocol for AudioHandler {}

    unsafe impl SCStreamOutput for AudioHandler {
        /// Audio frames arrive here on the private serial queue.
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            if r#type != SCStreamOutputType::Audio {
                return;
            }
            if std::env::var_os("MUSIC_SPEC_DEBUG").is_some() {
                eprintln!("[sck] audio sample buffer arrived");
            }
            if let Ok(mut feed) = self.ivars().try_borrow_mut() {
                push_sample_buffer(&mut feed, sample_buffer);
            }
        }
    }
);

impl AudioHandler {
    fn new(feed: FeedCtx) -> Retained<Self> {
        let this = Self::alloc().set_ivars(RefCell::new(feed));
        unsafe { msg_send![super(this), init] }
    }
}

type HandlerIvars = RefCell<FeedCtx>;

/// Maximum planar channel buffers we handle in one AudioBufferList.
const MAX_CHANNELS: usize = 8;

/// Scratch space for the variable-length `AudioBufferList`, 16-byte aligned.
#[repr(C, align(16))]
struct AlignedSpace([u8; 256]);

/// Extracts float32 audio from the sample buffer and feeds the pipeline.
/// Runs on SCK's queue thread: no allocation, no locking.
fn push_sample_buffer(feed: &mut FeedCtx, sample_buffer: &CMSampleBuffer) {
    let mut needed: usize = 0;
    let mut block_buffer: *mut CMBlockBuffer = std::ptr::null_mut();
    let mut space = AlignedSpace([0u8; 256]);
    let list = space.0.as_mut_ptr() as *mut AudioBufferList;

    // Pass 1: query the exact AudioBufferList size CoreMedia insists on
    // (passing a larger buffer than `needed` makes the call fail with
    // kCMSampleBufferError_ArrayTooSmall — SCK quirk).
    let _ = unsafe {
        sample_buffer.audio_buffer_list_with_retained_block_buffer(
            &mut needed,
            std::ptr::null_mut(),
            0,
            None,
            None,
            0,
            &mut block_buffer,
        )
    };
    if needed == 0 || needed > space.0.len() {
        return;
    }

    let status = unsafe {
        sample_buffer.audio_buffer_list_with_retained_block_buffer(
            &mut needed,
            list,
            needed,
            None,
            None,
            objc2_core_media::kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &mut block_buffer,
        )
    };
    if status != 0 {
        if std::env::var_os("MUSIC_SPEC_DEBUG").is_some() {
            eprintln!(
                "[sck] GetAudioBufferList status={status} needed={needed} given={}",
                space.0.len()
            );
        }
        return;
    }
    // The retained block buffer owns the audio data; keep it alive for the copy.
    let _data_guard = unsafe { Retained::from_raw(block_buffer) };

    let list = unsafe { &*list };
    let n_buffers = list.mNumberBuffers as usize;
    if std::env::var_os("MUSIC_SPEC_DEBUG").is_some() {
        eprintln!(
            "[sck] buffers={n_buffers} ch={} bytes={} status={status}",
            list.mBuffers[0].mNumberChannels, list.mBuffers[0].mDataByteSize
        );
    }
    if n_buffers == 0 || n_buffers > MAX_CHANNELS {
        return;
    }
    // The C array extends past the declared single-element field.
    let buffers =
        unsafe { std::slice::from_raw_parts(&list.mBuffers[0] as *const AudioBuffer, n_buffers) };

    let data = |b: &AudioBuffer| unsafe {
        std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4)
    };

    if n_buffers == 1 {
        // Interleaved: hand the samples straight to the mixdown.
        if buffers[0].mNumberChannels > 0 && !buffers[0].mData.is_null() {
            feed.push_samples(data(&buffers[0]).iter().copied());
        }
    } else {
        // Planar: re-interleave frame-major, then mix down.
        let frames = buffers[0].mDataByteSize as usize / 4;
        if frames == 0 || buffers.iter().any(|b| b.mData.is_null()) {
            return;
        }
        for i in 0..frames {
            for b in buffers {
                let s = unsafe { *(b.mData as *const f32).add(i) };
                feed.push_sample(s);
            }
        }
    }
}
