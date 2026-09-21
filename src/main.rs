//! mxkeys-f4d — give the MX Keys F4 key back, without giving up F12.
//!
//! # The problem
//!
//! The universal Logitech MX Keys (`046d:b35b`) resolves Fn in firmware. macOS
//! never sees an Fn modifier, and the key in the F4 position emits plain HID
//! keyboard usage `0x45` — which is F12. Fn+F12 emits the same `0x45`. Measured
//! on the wire, the two presses are byte-identical:
//!
//! ```text
//! bare F4   ->  01 00 45 00 00 00 00 00
//! Fn + F12  ->  01 00 45 00 00 00 00 00
//! ```
//!
//! So no remapper working at the macOS layer — `hidutil`, a ByHost key mapping,
//! Karabiner — can separate them. Fixing F4 that way necessarily costs F12.
//!
//! # Why a daemon, reluctantly
//!
//! The distinction does still exist, but only on Logitech's HID++ vendor
//! channel (usage page `0xFF43`). Feature `0x1B04` can *divert* a control: the
//! key stops emitting its HID usage entirely and the device sends a
//! notification naming the control instead. Diverting only F4 leaves usage
//! `0x45` untouched, so Fn+F12 keeps typing a real F12.
//!
//! Two firmware routes that would have needed no process at all were checked
//! against the device and both are dead ends:
//!
//! - `0x1C00` (Persistent Remappable Action), which stores a remap in the
//!   keyboard's own memory, is **absent** from this firmware.
//! - `0x1B04`'s `remap` field is accepted and echoed back correctly, and then
//!   **ignored** — verified with two different targets; the key kept emitting
//!   `0x45` either way.
//!
//! Diverting is therefore the only mechanism that works, and it requires
//! something resident to receive the notifications. This is that something, and
//! it is deliberately as small as the platform allows: ~2.5 MB of which ~1.6 MB
//! is the cost of being a CoreFoundation process at all.

use std::cell::Cell;
use std::ffi::{c_void, CString};
use std::os::raw::{c_char, c_double, c_int};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Framework FFI. No crates: everything here is CoreFoundation, IOKit and
// CoreGraphics, all of which are already resident in every macOS process.
// ---------------------------------------------------------------------------

type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFNumberRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFMutableDictionaryRef = *mut c_void;
type CFRunLoopRef = *const c_void;
type IOHIDManagerRef = *const c_void;
type IOHIDDeviceRef = *const c_void;
type CGEventSourceRef = *const c_void;
type CGEventRef = *const c_void;
type IOReturn = c_int;
type CFIndex = isize;
type CFRunLoopTimerRef = *const c_void;
type CFAbsoluteTime = c_double;
type CFTimeInterval = c_double;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CF_NUMBER_INT_TYPE: CFIndex = 9;
const K_IOHID_REPORT_TYPE_OUTPUT: u32 = 1;
const K_IORETURN_SUCCESS: IOReturn = 0;
/// `kIOReturnNotPermitted`, which is what a refused Input Monitoring grant
/// actually returns. Measured, not guessed: a copy of this daemon launched from
/// a shell — where TCC attributes the request to the terminal rather than to the
/// binary — is refused with exactly this.
///
/// The test used to be against `0xE00002C1`, which is `kIOReturnNotPrivileged`,
/// a different error that an open never returns. So the branch never ran, and
/// the one message that names the fix printed as a bare hex code instead.
const K_IORETURN_NOT_PERMITTED: IOReturn = -536_870_174; // 0xE00002E2
const K_CG_EVENT_FLAG_MASK_SECONDARY_FN: u64 = 0x0080_0000;
const K_CG_HID_EVENT_TAP: u32 = 0;
const K_CG_EVENT_SOURCE_STATE_HID: u32 = 1;

type IOHIDReportCallback =
    extern "C" fn(*mut c_void, IOReturn, *mut c_void, u32, u32, *mut u8, CFIndex);
type IOHIDDeviceCallback = extern "C" fn(*mut c_void, IOReturn, *mut c_void, IOHIDDeviceRef);
type CFRunLoopTimerCallBack = extern "C" fn(CFRunLoopTimerRef, *mut c_void);

/// Only `version` and `info` are ever read here: the timer outlives the process
/// and `info` points at the leaked `State`, so the retain/release hooks that
/// would manage a shorter-lived context have nothing to do.
#[repr(C)]
struct CFRunLoopTimerContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFAllocatorDefault: CFAllocatorRef;
    static kCFRunLoopDefaultMode: CFStringRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFStringCreateWithCString(a: CFAllocatorRef, s: *const c_char, e: u32) -> CFStringRef;
    fn CFNumberCreate(a: CFAllocatorRef, t: CFIndex, v: *const c_void) -> CFNumberRef;
    fn CFDictionaryCreateMutable(
        a: CFAllocatorRef,
        n: CFIndex,
        k: *const c_void,
        v: *const c_void,
    ) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(d: CFMutableDictionaryRef, k: *const c_void, v: *const c_void);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRun();
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: c_double, return_after_source: u8) -> i32;
    fn CFAbsoluteTimeGetCurrent() -> CFAbsoluteTime;
    fn CFRunLoopTimerCreate(
        a: CFAllocatorRef,
        fire_date: CFAbsoluteTime,
        interval: CFTimeInterval,
        flags: u32,
        order: CFIndex,
        callout: CFRunLoopTimerCallBack,
        context: *mut CFRunLoopTimerContext,
    ) -> CFRunLoopTimerRef;
    fn CFRunLoopAddTimer(r: CFRunLoopRef, t: CFRunLoopTimerRef, mode: CFStringRef);
    fn CFRunLoopTimerSetNextFireDate(t: CFRunLoopTimerRef, fire_date: CFAbsoluteTime);
    fn CFRelease(t: CFTypeRef);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDManagerCreate(a: CFAllocatorRef, o: u32) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatching(m: IOHIDManagerRef, d: CFDictionaryRef);
    fn IOHIDManagerRegisterDeviceMatchingCallback(
        m: IOHIDManagerRef,
        c: IOHIDDeviceCallback,
        ctx: *mut c_void,
    );
    fn IOHIDManagerRegisterDeviceRemovalCallback(
        m: IOHIDManagerRef,
        c: IOHIDDeviceCallback,
        ctx: *mut c_void,
    );
    fn IOHIDManagerScheduleWithRunLoop(m: IOHIDManagerRef, r: CFRunLoopRef, mode: CFStringRef);
    fn IOHIDManagerOpen(m: IOHIDManagerRef, o: u32) -> IOReturn;
    fn IOHIDDeviceOpen(d: IOHIDDeviceRef, o: u32) -> IOReturn;
    fn IOHIDDeviceClose(d: IOHIDDeviceRef, o: u32) -> IOReturn;
    fn IOHIDDeviceScheduleWithRunLoop(d: IOHIDDeviceRef, r: CFRunLoopRef, mode: CFStringRef);
    fn IOHIDDeviceRegisterInputReportCallback(
        d: IOHIDDeviceRef,
        buf: *mut u8,
        len: CFIndex,
        c: IOHIDReportCallback,
        ctx: *mut c_void,
    );
    fn IOHIDDeviceSetReport(
        d: IOHIDDeviceRef,
        t: u32,
        id: CFIndex,
        b: *const u8,
        l: CFIndex,
    ) -> IOReturn;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state: u32) -> CGEventSourceRef;
    fn CGEventCreateKeyboardEvent(src: CGEventSourceRef, key: u16, down: u8) -> CGEventRef;
    fn CGEventSetFlags(e: CGEventRef, flags: u64);
    fn CGEventPost(tap: u32, e: CGEventRef);
}

// ---------------------------------------------------------------------------
// HID++ 2.0
// ---------------------------------------------------------------------------

/// This keyboard declares only the long report (`0x11`, 19-byte payload) under
/// usage page `0xFF43`. There is no short `0x10` report at all, which is worth
/// knowing because most HID++ code assumes one exists — it is why at least one
/// other macOS client cannot enumerate this device.
const REPORT_LONG: u8 = 0x11;
/// `0xFF` is the device index for a directly paired (Bluetooth or BLE) device,
/// as opposed to one reached through a Bolt/Unifying receiver.
const DEVICE_INDEX: u8 = 0xFF;
/// Any non-zero value distinguishes our replies from unsolicited notifications,
/// which arrive with software id 0.
const SOFTWARE_ID: u8 = 1;
const FEATURE_ROOT: u8 = 0x00;
const FEATURE_REPROG_CONTROLS_V4: u16 = 0x1B04;

/// Far enough out to mean "never". The retry timer repeats on this interval so
/// that it stays valid — a one-shot `CFRunLoopTimer` invalidates itself when it
/// fires — and every actual attempt is scheduled by moving its fire date.
const RETRY_NEVER: c_double = 1.0e9;
/// Ceiling on the backoff. A keyboard whose radio is merely slow answers within
/// a second or two; one that is switched off should not be polled hard for
/// however many hours it stays that way.
const RETRY_MAX_DELAY: c_double = 30.0;

/// `IOHIDDeviceSetReport` on this device wants the report id **both** as the
/// `reportID` argument and as byte 0 of the buffer. Passing it only as the
/// argument is silently accepted and produces no reply.
const REPORT_LEN: usize = 20;

struct Config {
    vendor_id: c_int,
    product_id: c_int,
    /// Control to divert. `0x00E1` is "Dashboard (Launchpad) / Action Center",
    /// which is the F4 key on this layout.
    cid: u16,
    /// Virtual key code to synthesise. 79 is F18, chosen because no Apple or
    /// Logitech keyboard has an F18, so binding it steals nothing.
    key_code: u16,
    verbose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { vendor_id: 0x046D, product_id: 0xB35B, cid: 0x00E1, key_code: 79, verbose: false }
    }
}

/// What the root feature table said about a feature id.
enum Probe {
    /// The device answered with the feature's index in its own table.
    Ready(u8),
    /// The device answered, and the answer is that it has no such feature.
    Absent,
    /// The device did not answer at all. Says nothing about the feature.
    Silent,
}

/// Every mutable field is a `Cell`, and every method takes `&self`.
///
/// That is not a style choice. The attach handler pumps the run loop while it
/// waits for a HID++ reply, and pumping re-enters the input-report callback —
/// so two handlers are live on the same object at once. Holding a `&mut State`
/// across that is undefined behaviour, and it does not merely *risk* breaking:
/// with optimisation on, the compiler assumes the `&mut` is unaliased, caches
/// `have_reply` in a register, and the wait loop never observes the reply that
/// the callback just wrote. The first version of this file did exactly that and
/// reported "device does not expose HID++ feature 0x1B04" against a device that
/// plainly does. Shared references plus `Cell` make the reentrancy sound.
struct State {
    cfg: Config,
    started: Instant,
    device: Cell<IOHIDDeviceRef>,
    /// Index of `0x1B04` in this device's feature table. Indices are per-device
    /// and must be looked up at runtime; they are not part of the spec.
    feature: Cell<u8>,
    pressed: Cell<bool>,
    event_source: CGEventSourceRef,
    reply: Cell<[u8; REPORT_LEN]>,
    have_reply: Cell<bool>,
    want_feature: Cell<u8>,
    /// Whether `IOHIDDeviceOpen` has succeeded for the device currently held.
    /// Separate from `device` because a bring-up can fail after the ref is
    /// known but before the open takes, and the retry must not open twice.
    opened: Cell<bool>,
    /// Fires when a failed bring-up is due another attempt. Parked at
    /// `RETRY_NEVER` whenever there is nothing owed, so an idle daemon really
    /// is idle rather than waking on a poll it almost never needs.
    retry_timer: Cell<CFRunLoopTimerRef>,
    /// Attempts made since the last success. Drives the backoff, and keeps the
    /// log to one line per failure rather than one per attempt.
    attempt: Cell<u32>,
}

impl State {
    fn log(&self, msg: &str) {
        eprintln!("[{:7.1} ms] {}", self.started.elapsed().as_secs_f64() * 1000.0, msg);
    }

    fn send(&self, feature: u8, function: u8, params: &[u8]) {
        let mut b = [0u8; REPORT_LEN];
        b[0] = REPORT_LONG;
        b[1] = DEVICE_INDEX;
        b[2] = feature;
        b[3] = (function << 4) | SOFTWARE_ID;
        let n = params.len().min(16);
        b[4..4 + n].copy_from_slice(&params[..n]);
        unsafe {
            IOHIDDeviceSetReport(
                self.device.get(),
                K_IOHID_REPORT_TYPE_OUTPUT,
                REPORT_LONG as CFIndex,
                b.as_ptr(),
                REPORT_LEN as CFIndex,
            );
        }
    }

    /// Send a request and pump the run loop until its reply lands. Used only
    /// during the short attach-time handshake; keypresses are pure callbacks.
    fn call(&self, feature: u8, function: u8, params: &[u8]) -> Option<[u8; 16]> {
        self.want_feature.set(feature);
        self.have_reply.set(false);
        self.send(feature, function, params);
        for _ in 0..100 {
            if self.have_reply.get() {
                break;
            }
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.02, 1) };
        }
        if !self.have_reply.get() {
            return None;
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(&self.reply.get()[4..REPORT_LEN]);
        Some(out)
    }

    /// The two failures here are not the same failure, and conflating them is
    /// what made a sleeping radio look like missing hardware. A reply carrying
    /// index 0 is the device saying it does not implement the feature, which no
    /// number of retries will change. *No reply at all* is a timeout, and on a
    /// BLE keyboard that is the ordinary shape of "not awake yet".
    fn feature_index(&self, id: u16) -> Probe {
        match self.call(FEATURE_ROOT, 0x00, &[(id >> 8) as u8, (id & 0xFF) as u8, 0]) {
            None => Probe::Silent,
            Some(r) if r[0] == 0 => Probe::Absent,
            Some(r) => Probe::Ready(r[0]),
        }
    }

    fn set_divert(&self, on: bool) {
        let mut p = [0u8; 16];
        p[0] = (self.cfg.cid >> 8) as u8;
        p[1] = (self.cfg.cid & 0xFF) as u8;
        // bit 1 says "the divert bit below is meaningful", bit 0 is the value.
        // The persist bits are deliberately left alone: a divert that outlived
        // this process would leave F4 dead rather than merely typing F12.
        p[2] = if on { 0x03 } else { 0x02 };
        self.send(self.feature.get(), 3, &p);
    }

    fn divert_is_set(&self) -> bool {
        let cid = [(self.cfg.cid >> 8) as u8, (self.cfg.cid & 0xFF) as u8];
        self.call(self.feature.get(), 2, &cid).map(|r| r[2] & 0x01 != 0).unwrap_or(false)
    }

    fn fire(&self) {
        for down in [1u8, 0u8] {
            let e = unsafe { CGEventCreateKeyboardEvent(self.event_source, self.cfg.key_code, down) };
            if e.is_null() {
                continue;
            }
            unsafe {
                // Real F-key events carry the function-key bit, and the window
                // server registers F-key hotkeys expecting it. Without this the
                // synthesised event matches nothing at all.
                CGEventSetFlags(e, K_CG_EVENT_FLAG_MASK_SECONDARY_FN);
                CGEventPost(K_CG_HID_EVENT_TAP, e);
                CFRelease(e);
            }
        }
        if self.cfg.verbose {
            self.log("control pressed -> key posted");
        }
    }

    fn on_report(&self, report_id: u32, report: &[u8]) {
        let mut m = [0u8; 24];
        let mut off = 0;
        if report.first() != Some(&REPORT_LONG) {
            m[0] = report_id as u8;
            off = 1;
        }
        let n = report.len().min(24 - off);
        m[off..off + n].copy_from_slice(&report[..n]);
        let total = off + n;

        if total < 7 || m[0] != REPORT_LONG || m[1] != DEVICE_INDEX {
            return;
        }
        let (feature, software_id, function) = (m[2], m[3] & 0x0F, m[3] >> 4);

        if software_id == SOFTWARE_ID && feature == self.want_feature.get() && !self.have_reply.get() {
            let mut r = [0u8; REPORT_LEN];
            let k = total.min(REPORT_LEN);
            r[..k].copy_from_slice(&m[..k]);
            self.reply.set(r);
            self.have_reply.set(true);
            return;
        }

        // divertedButtonsEvent: function 0, software id 0. The payload lists the
        // diverted controls currently held, two bytes each, so an all-zero
        // payload is the release.
        if feature == self.feature.get() && function == 0 && software_id == 0 {
            let cid = ((m[4] as u16) << 8) | m[5] as u16;
            if cid == self.cfg.cid && !self.pressed.get() {
                self.pressed.set(true);
                self.fire();
            } else if cid == 0 && self.pressed.get() {
                self.pressed.set(false);
            }
        }
    }

    fn on_attach(&self, device: IOHIDDeviceRef) {
        if !self.device.get().is_null() {
            return;
        }
        self.log("keyboard attached");
        self.device.set(device);
        self.opened.set(false);
        self.attempt.set(0);
        self.try_bring_up();
    }

    /// One complete attempt at everything the daemon needs from the keyboard:
    /// open it, locate `0x1B04`, divert the control, and confirm the divert
    /// took. Every step of that talks to a BLE radio that may still be coming
    /// up — notably right after the Mac wakes, when the attach callback beats
    /// the link by several seconds — so each transient failure schedules
    /// another attempt rather than giving up.
    ///
    /// Giving up was the old behaviour, and it left the key typing F12 until a
    /// human noticed. The external watchdog that was supposed to cover that
    /// could not: `read -t` returns 1 on timeout in the bash 3.2 that ships as
    /// `/bin/bash`, which its loop read as "the pipe died" and exited on, five
    /// seconds after every launch. Recovery belongs here, where the failure is.
    fn try_bring_up(&self) {
        let device = self.device.get();
        if device.is_null() {
            return;
        }

        if !self.opened.get() {
            let rc = unsafe { IOHIDDeviceOpen(device, 0) };
            if rc != K_IORETURN_SUCCESS {
                if rc == K_IORETURN_NOT_PERMITTED {
                    // A missing TCC grant. No number of retries produces one —
                    // it wants a human in System Settings — and retrying would
                    // only bury the line that says so. Drop the device with it:
                    // `on_attach` ignores an attach while one is held, and the
                    // reconnect after the grant is granted is the only chance
                    // this process gets to notice.
                    self.log("cannot open device: grant this binary Input Monitoring");
                    self.device.set(ptr::null());
                } else {
                    // Usually another process still holding the device. Clears.
                    self.retry(&format!("cannot open device: IOReturn {rc:#010x}"));
                }
                return;
            }
            let ctx = self as *const State as *mut c_void;
            unsafe {
                IOHIDDeviceRegisterInputReportCallback(device, report_buffer(), 64, report_cb, ctx);
                IOHIDDeviceScheduleWithRunLoop(device, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
            }
            self.opened.set(true);
        }

        match self.feature_index(FEATURE_REPROG_CONTROLS_V4) {
            Probe::Ready(f) => self.feature.set(f),
            Probe::Absent => {
                self.log("device does not expose HID++ feature 0x1B04");
                return;
            }
            Probe::Silent => {
                self.retry("keyboard did not answer the feature probe");
                return;
            }
        }

        self.set_divert(true);

        // Report success only once the keyboard confirms it, so "working" is a
        // fact about the device rather than about our intent.
        for _ in 0..40 {
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.02, 1) };
            // Pumping lets the removal callback run. If it did, there is nothing
            // left to confirm against, and the next attach starts a fresh
            // bring-up anyway.
            if self.device.get().is_null() {
                return;
            }
            if self.divert_is_set() {
                match self.attempt.get() {
                    0 => self.log("control diverted — the key is live, and F12 is untouched"),
                    n => self.log(&format!(
                        "control diverted after {} more {} — the key is live, and F12 is untouched",
                        n,
                        if n == 1 { "attempt" } else { "attempts" },
                    )),
                }
                self.attempt.set(0);
                self.disarm_retry();
                return;
            }
        }
        self.retry("divert was not confirmed by the device");
    }

    /// Note a transient failure and schedule another attempt.
    ///
    /// Only the first failure of a bring-up is logged. The rest are silent on
    /// purpose: a keyboard left switched off would otherwise write two lines a
    /// minute into a log nothing rotates, and the line that matters — the one
    /// naming why the first attempt failed — would be the hardest to find.
    fn retry(&self, why: &str) {
        let n = self.attempt.get();
        if n == 0 {
            self.log(&format!("{why}; retrying"));
        }
        self.attempt.set(n + 1);

        let delay = (1u32 << n.min(5)) as c_double;
        let timer = self.retry_timer.get();
        if !timer.is_null() {
            unsafe {
                let at = CFAbsoluteTimeGetCurrent() + delay.min(RETRY_MAX_DELAY);
                CFRunLoopTimerSetNextFireDate(timer, at);
            }
        }
    }

    fn disarm_retry(&self) {
        let timer = self.retry_timer.get();
        if !timer.is_null() {
            unsafe {
                CFRunLoopTimerSetNextFireDate(timer, CFAbsoluteTimeGetCurrent() + RETRY_NEVER);
            }
        }
    }

    fn on_detach(&self) {
        self.log("keyboard detached");
        self.device.set(ptr::null());
        self.feature.set(0);
        self.pressed.set(false);
        self.opened.set(false);
        // Nothing to retry against: the next attach starts a fresh bring-up.
        self.attempt.set(0);
        self.disarm_retry();
    }

    /// Hand the key back before exiting. Without this the control stays diverted
    /// until the keyboard next reconnects, and a diverted control with nothing
    /// listening is a dead key rather than a wrong one.
    fn shutdown(&self) {
        if self.device.get().is_null() || self.feature.get() == 0 {
            return;
        }
        self.set_divert(false);
        for _ in 0..20 {
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.02, 1) };
            if !self.divert_is_set() {
                break;
            }
        }
        unsafe { IOHIDDeviceClose(self.device.get(), 0) };
        self.log("divert cleared, exiting");
    }
}

extern "C" fn report_cb(
    ctx: *mut c_void,
    _r: IOReturn,
    _sender: *mut c_void,
    _t: u32,
    report_id: u32,
    report: *mut u8,
    len: CFIndex,
) {
    if ctx.is_null() || report.is_null() || len <= 0 {
        return;
    }
    let state = unsafe { &*(ctx as *const State) };
    let slice = unsafe { std::slice::from_raw_parts(report, len as usize) };
    state.on_report(report_id, slice);
}

extern "C" fn attach_cb(ctx: *mut c_void, _r: IOReturn, _s: *mut c_void, dev: IOHIDDeviceRef) {
    if ctx.is_null() {
        return;
    }
    let state = unsafe { &*(ctx as *const State) };
    state.on_attach(dev);
}

extern "C" fn retry_cb(_t: CFRunLoopTimerRef, ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    let state = unsafe { &*(ctx as *const State) };
    // Park the timer before the attempt, not after. `try_bring_up` pumps the
    // run loop while it waits on the keyboard, and a fire date that fell due
    // during that pumping would re-enter this callback on top of itself.
    state.disarm_retry();
    state.try_bring_up();
}

extern "C" fn detach_cb(ctx: *mut c_void, _r: IOReturn, _s: *mut c_void, _d: IOHIDDeviceRef) {
    if ctx.is_null() {
        return;
    }
    unsafe { &*(ctx as *const State) }.on_detach();
}

/// IOKit writes incoming reports straight into this buffer, so it lives outside
/// `State` and outside Rust's aliasing rules entirely.
fn report_buffer() -> *mut u8 {
    static mut BUF: [u8; 64] = [0; 64];
    &raw mut BUF as *mut u8
}

static SHUTDOWN_STATE: AtomicPtr<State> = AtomicPtr::new(ptr::null_mut());

extern "C" fn on_signal(_sig: c_int) {
    let p = SHUTDOWN_STATE.load(Ordering::SeqCst);
    if !p.is_null() {
        unsafe { (*p).shutdown() };
    }
    std::process::exit(0);
}

unsafe extern "C" {
    fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
}

fn cfstr(s: &str) -> CFStringRef {
    let c = CString::new(s).expect("static string");
    unsafe { CFStringCreateWithCString(kCFAllocatorDefault, c.as_ptr(), K_CF_STRING_ENCODING_UTF8) }
}

fn parse_args() -> Config {
    let mut cfg = Config::default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |s: &str| -> Option<i64> {
        let t = s.trim_start_matches("0x");
        i64::from_str_radix(t, if t == s { 10 } else { 16 }).ok()
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--verbose" | "-v" => cfg.verbose = true,
            "--help" | "-h" => {
                println!(
                    "mxkeys-f4d — divert one Logitech HID++ control and post a key for it\n\n\
                     \x20 --vendor <id>    default 0x046d\n\
                     \x20 --product <id>   default 0xb35b (MX Keys)\n\
                     \x20 --cid <id>       control to divert, default 0x00e1 (F4)\n\
                     \x20 --key <code>     virtual key code to post, default 79 (F18)\n\
                     \x20 --verbose        log every keypress"
                );
                std::process::exit(0);
            }
            "--vendor" => { i += 1; if let Some(v) = args.get(i).and_then(|s| num(s)) { cfg.vendor_id = v as c_int } }
            "--product" => { i += 1; if let Some(v) = args.get(i).and_then(|s| num(s)) { cfg.product_id = v as c_int } }
            "--cid" => { i += 1; if let Some(v) = args.get(i).and_then(|s| num(s)) { cfg.cid = v as u16 } }
            "--key" => { i += 1; if let Some(v) = args.get(i).and_then(|s| num(s)) { cfg.key_code = v as u16 } }
            other => eprintln!("ignoring unknown argument: {other}"),
        }
        i += 1;
    }
    cfg
}

fn main() {
    let cfg = parse_args();

    // Leaked on purpose: the address is handed to C callbacks that outlive any
    // Rust scope, and the process owns it until exit.
    let state: &'static State = Box::leak(Box::new(State {
        cfg,
        started: Instant::now(),
        device: Cell::new(ptr::null()),
        feature: Cell::new(0),
        pressed: Cell::new(false),
        // Created up front so the first keypress does not pay for faulting
        // CoreGraphics in.
        event_source: unsafe { CGEventSourceCreate(K_CG_EVENT_SOURCE_STATE_HID) },
        reply: Cell::new([0; REPORT_LEN]),
        have_reply: Cell::new(false),
        want_feature: Cell::new(0),
        opened: Cell::new(false),
        retry_timer: Cell::new(ptr::null()),
        attempt: Cell::new(0),
    }));
    let ctx = state as *const State as *mut c_void;

    SHUTDOWN_STATE.store(state as *const State as *mut State, Ordering::SeqCst);
    unsafe {
        signal(2, on_signal);  // SIGINT
        signal(15, on_signal); // SIGTERM
    }

    state.log("starting");

    // Created before the HID manager, so it is already there for the first
    // attach callback rather than being raced by it.
    unsafe {
        let mut tctx = CFRunLoopTimerContext {
            version: 0,
            info: ctx,
            retain: ptr::null(),
            release: ptr::null(),
            copy_description: ptr::null(),
        };
        let timer = CFRunLoopTimerCreate(
            kCFAllocatorDefault,
            CFAbsoluteTimeGetCurrent() + RETRY_NEVER,
            RETRY_NEVER,
            0,
            0,
            retry_cb,
            &mut tctx,
        );
        state.retry_timer.set(timer);
        CFRunLoopAddTimer(CFRunLoopGetCurrent(), timer, kCFRunLoopDefaultMode);
    }

    unsafe {
        let mgr = IOHIDManagerCreate(kCFAllocatorDefault, 0);
        let vid = state.cfg.vendor_id;
        let pid = state.cfg.product_id;
        let v = CFNumberCreate(kCFAllocatorDefault, K_CF_NUMBER_INT_TYPE, &vid as *const _ as *const c_void);
        let p = CFNumberCreate(kCFAllocatorDefault, K_CF_NUMBER_INT_TYPE, &pid as *const _ as *const c_void);
        let d = CFDictionaryCreateMutable(
            kCFAllocatorDefault,
            2,
            &kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
        );
        CFDictionarySetValue(d, cfstr("VendorID"), v);
        CFDictionarySetValue(d, cfstr("ProductID"), p);
        IOHIDManagerSetDeviceMatching(mgr, d);

        IOHIDManagerRegisterDeviceMatchingCallback(mgr, attach_cb, ctx);
        IOHIDManagerRegisterDeviceRemovalCallback(mgr, detach_cb, ctx);
        IOHIDManagerScheduleWithRunLoop(mgr, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
        IOHIDManagerOpen(mgr, 0);

        CFRunLoopRun();
    }
}
