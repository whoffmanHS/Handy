//! Call audio from a Core Audio process tap (macOS 14.4+).
//!
//! A tap copies what chosen processes play, without a virtual audio device and
//! with only the "System Audio Recording" permission. The tap is read through
//! a private aggregate device whose IO callback pushes mono samples into a
//! lock-free ring buffer; everything slower happens on the writer thread.

use std::ffi::{c_int, c_void, CStr};
use std::mem::size_of;
use std::ptr::{null, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, Message};
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyDescription,
    kAudioTapPropertyFormat, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID,
    AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareDestroyAggregateDevice, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertySelector, AudioObjectSetPropertyData, CATapDescription,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{
    NSArray, NSDictionary, NSNumber, NSOperatingSystemVersion, NSProcessInfo, NSString, NSUUID,
};

type OSStatus = i32;

/// Seconds of call audio the ring buffer holds while the writer is busy.
const RING_SECONDS: usize = 5;

#[derive(Debug)]
pub enum TapError {
    /// Process taps need macOS 14.4.
    Unsupported,
    /// The user turned System Audio Recording off for Handy.
    PermissionDenied,
    /// A Core Audio call failed; carries what failed and its status.
    CoreAudio(&'static str, OSStatus),
    /// The tap's format isn't 32-bit float PCM.
    UnexpectedFormat,
}

impl std::fmt::Display for TapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => write!(f, "call audio capture needs macOS 14.4 or later"),
            Self::PermissionDenied => write!(f, "System Audio Recording permission is denied"),
            Self::CoreAudio(what, status) => write!(f, "{what} failed (status {status})"),
            Self::UnexpectedFormat => write!(f, "the audio tap isn't 32-bit float PCM"),
        }
    }
}

impl std::error::Error for TapError {}

/// What the tap is listening to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapScope {
    /// Only the target app's processes.
    App,
    /// Everything the Mac plays; the target app wasn't running.
    System,
}

pub fn is_supported() -> bool {
    NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(NSOperatingSystemVersion {
        majorVersion: 14,
        minorVersion: 4,
        patchVersion: 0,
    })
}

/// The System Audio Recording permission, where macOS will say. There is no
/// public call for this, so it asks the TCC framework and treats any failure
/// to ask as "unknown": the tap is then created and macOS prompts as needed.
fn permission_denied() -> bool {
    const TCC: &CStr = c"/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC";
    const DENIED: c_int = 1;
    type Preflight = unsafe extern "C" fn(*const c_void, *const c_void) -> c_int;
    // SAFETY: dlopen/dlsym take valid C strings; a null result is handled.
    // The symbol has had this signature since it was introduced.
    unsafe {
        let handle = libc::dlopen(TCC.as_ptr(), libc::RTLD_LAZY);
        if handle.is_null() {
            return false;
        }
        let symbol = libc::dlsym(handle, c"TCCAccessPreflight".as_ptr());
        if symbol.is_null() {
            return false;
        }
        let preflight: Preflight = std::mem::transmute(symbol);
        let service = NSString::from_str("kTCCServiceAudioCapture");
        preflight(Retained::as_ptr(&service).cast(), null()) == DENIED
    }
}

/// The tap functions arrived in macOS 14.2. They're looked up at run time so
/// the app still launches on older systems, where meeting mode is unavailable.
type CreateTap = unsafe extern "C" fn(*const CATapDescription, *mut AudioObjectID) -> OSStatus;
type DestroyTap = unsafe extern "C" fn(AudioObjectID) -> OSStatus;

struct TapFunctions {
    create: CreateTap,
    destroy: DestroyTap,
}

impl TapFunctions {
    fn load() -> Option<Self> {
        // SAFETY: both symbols, when present, have these C signatures.
        unsafe {
            let create = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"AudioHardwareCreateProcessTap".as_ptr(),
            );
            let destroy = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"AudioHardwareDestroyProcessTap".as_ptr(),
            );
            if create.is_null() || destroy.is_null() {
                return None;
            }
            Some(Self {
                create: std::mem::transmute::<*mut c_void, CreateTap>(create),
                destroy: std::mem::transmute::<*mut c_void, DestroyTap>(destroy),
            })
        }
    }
}

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn check(what: &'static str, status: OSStatus) -> Result<(), TapError> {
    if status == 0 {
        Ok(())
    } else {
        Err(TapError::CoreAudio(what, status))
    }
}

/// Every process currently known to Core Audio.
fn process_objects() -> Result<Vec<AudioObjectID>, TapError> {
    let address = address(kAudioHardwarePropertyProcessObjectList);
    let system = kAudioObjectSystemObject as AudioObjectID;
    let mut size = 0u32;
    // SAFETY: the address and size pointers are valid for the calls, and the
    // buffer is `size` bytes long.
    unsafe {
        check(
            "listing audio processes",
            AudioObjectGetPropertyDataSize(
                system,
                NonNull::from(&address),
                0,
                null(),
                NonNull::from(&mut size),
            ),
        )?;
        let mut ids = vec![0 as AudioObjectID; size as usize / size_of::<AudioObjectID>()];
        if ids.is_empty() {
            return Ok(ids);
        }
        check(
            "listing audio processes",
            AudioObjectGetPropertyData(
                system,
                NonNull::from(&address),
                0,
                null(),
                NonNull::from(&mut size),
                NonNull::new_unchecked(ids.as_mut_ptr().cast()),
            ),
        )?;
        ids.truncate(size as usize / size_of::<AudioObjectID>());
        Ok(ids)
    }
}

fn bundle_id(process: AudioObjectID) -> Option<String> {
    let address = address(kAudioProcessPropertyBundleID);
    let mut value: *mut NSString = std::ptr::null_mut();
    let mut size = size_of::<*mut NSString>() as u32;
    // SAFETY: the property is a CFString (toll-free bridged to NSString)
    // returned with a +1 retain count, which `from_raw` takes over.
    unsafe {
        let status = AudioObjectGetPropertyData(
            process,
            NonNull::from(&address),
            0,
            null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        );
        if status != 0 {
            return None;
        }
        Retained::from_raw(value).map(|id| id.to_string())
    }
}

/// The app itself and its helpers: `com.example.app`, `com.example.app.helper`.
fn belongs_to(bundle_id: &str, target: &str) -> bool {
    bundle_id
        .strip_prefix(target)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

/// The audio processes of `target`, sorted so two listings compare equal.
fn target_processes(target: &str) -> Result<Vec<AudioObjectID>, TapError> {
    let mut ids: Vec<AudioObjectID> = process_objects()?
        .into_iter()
        .filter(|&id| bundle_id(id).is_some_and(|bundle| belongs_to(&bundle, target)))
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

fn process_array(ids: &[AudioObjectID]) -> Retained<NSArray<NSNumber>> {
    let numbers: Vec<Retained<NSNumber>> = ids.iter().map(|&id| NSNumber::new_u32(id)).collect();
    NSArray::from_retained_slice(&numbers)
}

fn key(name: &CStr) -> Retained<NSString> {
    NSString::from_str(name.to_str().unwrap_or_default())
}

fn object<T: Message>(value: Retained<T>) -> Retained<AnyObject> {
    // SAFETY: every Objective-C object is an `AnyObject`.
    unsafe { Retained::cast_unchecked(value) }
}

/// State the IO callback owns. Only the audio thread touches it while the
/// device runs.
struct IoContext {
    producer: rtrb::Producer<f32>,
    dropped: Arc<AtomicU64>,
}

/// Averages every channel of the tap's audio to mono and queues it.
///
/// Runs on Core Audio's real-time thread: no locks, no allocation.
unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> OSStatus {
    // SAFETY: `client` is the `IoContext` handed to `AudioDeviceCreateIOProcID`,
    // alive until the proc is destroyed; `input` is valid for this call, and
    // `mBuffers` really holds `mNumberBuffers` entries.
    let context = &mut *client.cast::<IoContext>();
    let list = input.as_ref();
    let buffers = std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize);
    let channels: usize = buffers.iter().map(|b| b.mNumberChannels as usize).sum();
    let Some(first) = buffers.first() else {
        return 0;
    };
    if channels == 0 || first.mNumberChannels == 0 {
        return 0;
    }
    let frames = first.mDataByteSize as usize / (size_of::<f32>() * first.mNumberChannels as usize);
    for frame in 0..frames {
        let mut sum = 0.0f32;
        for buffer in buffers {
            if buffer.mData.is_null() {
                continue;
            }
            let width = buffer.mNumberChannels as usize;
            let samples = std::slice::from_raw_parts(
                buffer.mData.cast::<f32>(),
                buffer.mDataByteSize as usize / size_of::<f32>(),
            );
            for channel in 0..width {
                sum += samples.get(frame * width + channel).copied().unwrap_or(0.0);
            }
        }
        if context.producer.push(sum / channels as f32).is_err() {
            context.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    0
}

/// A running tap. Dropping it stops the capture and removes the tap.
pub struct ProcessTap {
    functions: TapFunctions,
    description: Retained<CATapDescription>,
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    io_proc: AudioDeviceIOProcID,
    context: *mut IoContext,
    target: String,
    processes: Vec<AudioObjectID>,
    scope: TapScope,
    sample_rate: u32,
    dropped: Arc<AtomicU64>,
}

// SAFETY: the tap is driven from one thread at a time (it is moved into the
// writer thread), the description is only touched through `&mut self`, and
// the context is only dereferenced by the audio thread.
unsafe impl Send for ProcessTap {}

impl ProcessTap {
    /// Taps the audio processes of the app with bundle ID `target`, or the
    /// whole system when it has none. Returns the tap and its mono samples.
    pub fn start(target: &str) -> Result<(Self, rtrb::Consumer<f32>), TapError> {
        if !is_supported() {
            return Err(TapError::Unsupported);
        }
        let functions = TapFunctions::load().ok_or(TapError::Unsupported)?;
        if permission_denied() {
            return Err(TapError::PermissionDenied);
        }

        let processes = target_processes(target)?;
        let scope = if processes.is_empty() {
            TapScope::System
        } else {
            TapScope::App
        };
        // SAFETY: both initialisers take an array of process object IDs.
        let description = unsafe {
            let description = match scope {
                TapScope::App => CATapDescription::initStereoMixdownOfProcesses(
                    CATapDescription::alloc(),
                    &process_array(&processes),
                ),
                TapScope::System => CATapDescription::initStereoGlobalTapButExcludeProcesses(
                    CATapDescription::alloc(),
                    &process_array(&[]),
                ),
            };
            description.setName(&NSString::from_str("Handy Meeting"));
            description.setPrivate(true);
            description
        };

        let mut tap: AudioObjectID = 0;
        // SAFETY: the description and the out pointer are valid for the call.
        check("creating the audio tap", unsafe {
            (functions.create)(Retained::as_ptr(&description), &mut tap)
        })?;

        let mut this = Self {
            functions,
            description,
            tap,
            aggregate: 0,
            io_proc: None,
            context: std::ptr::null_mut(),
            target: target.to_string(),
            processes,
            scope,
            sample_rate: 0,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        // From here `Drop` cleans up whatever a failed step leaves behind.
        let consumer = this.open()?;
        Ok((this, consumer))
    }

    fn open(&mut self) -> Result<rtrb::Consumer<f32>, TapError> {
        let format = self.format()?;
        if format.mFormatFlags & kAudioFormatFlagIsFloat == 0 || format.mBitsPerChannel != 32 {
            return Err(TapError::UnexpectedFormat);
        }
        self.sample_rate = format.mSampleRate.round() as u32;
        if self.sample_rate == 0 {
            return Err(TapError::UnexpectedFormat);
        }

        // SAFETY: plain property reads on a live description.
        let tap_uid = unsafe { self.description.UUID() }.UUIDString();
        let sub_tap = NSDictionary::<NSString, AnyObject>::from_retained_objects(
            &[
                &*key(kAudioSubTapUIDKey),
                &*key(kAudioSubTapDriftCompensationKey),
            ],
            &[object(tap_uid), object(NSNumber::new_bool(true))],
        );
        let aggregate = NSDictionary::<NSString, AnyObject>::from_retained_objects(
            &[
                &*key(kAudioAggregateDeviceNameKey),
                &*key(kAudioAggregateDeviceUIDKey),
                &*key(kAudioAggregateDeviceIsPrivateKey),
                &*key(kAudioAggregateDeviceIsStackedKey),
                &*key(kAudioAggregateDeviceTapAutoStartKey),
                &*key(kAudioAggregateDeviceTapListKey),
            ],
            &[
                object(NSString::from_str("Handy Meeting")),
                object(NSUUID::new().UUIDString()),
                object(NSNumber::new_bool(true)),
                object(NSNumber::new_bool(false)),
                object(NSNumber::new_bool(true)),
                object(NSArray::from_retained_slice(&[sub_tap])),
            ],
        );
        // SAFETY: NSDictionary is toll-free bridged to CFDictionary, and the
        // out pointers are valid for the calls.
        unsafe {
            let dictionary = &*Retained::as_ptr(&aggregate).cast::<CFDictionary>();
            check(
                "creating the capture device",
                AudioHardwareCreateAggregateDevice(dictionary, NonNull::from(&mut self.aggregate)),
            )?;
        }

        let (producer, consumer) = rtrb::RingBuffer::new(self.sample_rate as usize * RING_SECONDS);
        self.context = Box::into_raw(Box::new(IoContext {
            producer,
            dropped: Arc::clone(&self.dropped),
        }));
        // SAFETY: `context` outlives the IO proc, which `Drop` destroys first.
        unsafe {
            check(
                "attaching to the capture device",
                AudioDeviceCreateIOProcID(
                    self.aggregate,
                    Some(io_proc),
                    self.context.cast(),
                    NonNull::from(&mut self.io_proc),
                ),
            )?;
            check(
                "starting the capture device",
                AudioDeviceStart(self.aggregate, self.io_proc),
            )?;
        }
        Ok(consumer)
    }

    fn format(&self) -> Result<AudioStreamBasicDescription, TapError> {
        let address = address(kAudioTapPropertyFormat);
        let mut format = std::mem::MaybeUninit::<AudioStreamBasicDescription>::zeroed();
        let mut size = size_of::<AudioStreamBasicDescription>() as u32;
        // SAFETY: the buffer is one zeroed `AudioStreamBasicDescription`,
        // which is plain data.
        unsafe {
            check(
                "reading the tap format",
                AudioObjectGetPropertyData(
                    self.tap,
                    NonNull::from(&address),
                    0,
                    null(),
                    NonNull::from(&mut size),
                    NonNull::from(&mut format).cast(),
                ),
            )?;
            Ok(format.assume_init())
        }
    }

    pub fn scope(&self) -> TapScope {
        self.scope
    }

    /// The rate of the samples the consumer yields.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Samples lost because the writer fell behind.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Follows the target app's audio processes as they come and go: a call
    /// can start a helper process that didn't exist when recording began.
    /// A system-wide tap already hears everything and is left alone.
    pub fn refresh(&mut self) -> Result<(), TapError> {
        if self.scope == TapScope::System {
            return Ok(());
        }
        let processes = target_processes(&self.target)?;
        if processes.is_empty() || processes == self.processes {
            return Ok(());
        }
        let address = address(kAudioTapPropertyDescription);
        let description = Retained::as_ptr(&self.description);
        // SAFETY: the property takes a pointer to the description object.
        unsafe {
            self.description.setProcesses(&process_array(&processes));
            check(
                "updating the audio tap",
                AudioObjectSetPropertyData(
                    self.tap,
                    NonNull::from(&address),
                    0,
                    null(),
                    size_of::<*const CATapDescription>() as u32,
                    NonNull::from(&description).cast(),
                ),
            )?;
        }
        self.processes = processes;
        Ok(())
    }
}

impl Drop for ProcessTap {
    fn drop(&mut self) {
        // SAFETY: each object is destroyed once, in the reverse order it was
        // created, and the context is freed only after its IO proc is gone.
        unsafe {
            if self.aggregate != 0 {
                if self.io_proc.is_some() {
                    AudioDeviceStop(self.aggregate, self.io_proc);
                    AudioDeviceDestroyIOProcID(self.aggregate, self.io_proc);
                }
                AudioHardwareDestroyAggregateDevice(self.aggregate);
            }
            if self.tap != 0 {
                (self.functions.destroy)(self.tap);
            }
            if !self.context.is_null() {
                drop(Box::from_raw(self.context));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::belongs_to;

    #[test]
    fn matches_the_app_and_its_helpers_only() {
        assert!(belongs_to("com.microsoft.teams2", "com.microsoft.teams2"));
        assert!(belongs_to(
            "com.microsoft.teams2.helper",
            "com.microsoft.teams2"
        ));
        assert!(!belongs_to("com.microsoft.teams20", "com.microsoft.teams2"));
        assert!(!belongs_to("com.microsoft.edgemac", "com.microsoft.teams2"));
    }
}
