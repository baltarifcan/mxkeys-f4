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

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CF_NUMBER_INT_TYPE: CFIndex = 9;
const K_IOHID_REPORT_TYPE_OUTPUT: u32 = 1;
const K_IORETURN_SUCCESS: IOReturn = 0;
const K_IORETURN_NOT_PERMITTED: IOReturn = -536_870_207; // 0xE00002C1
const K_CG_EVENT_FLAG_MASK_SECONDARY_FN: u64 = 0x0080_0000;
const K_CG_HID_EVENT_TAP: u32 = 0;
const K_CG_EVENT_SOURCE_STATE_HID: u32 = 1;

type IOHIDReportCallback =
    extern "C" fn(*mut c_void, IOReturn, *mut c_void, u32, u32, *mut u8, CFIndex);
type IOHIDDeviceCallback = extern "C" fn(*mut c_void, IOReturn, *mut c_void, IOHIDDeviceRef);

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

    fn feature_index(&self, id: u16) -> Option<u8> {
        let r = self.call(FEATURE_ROOT, 0x00, &[(id >> 8) as u8, (id & 0xFF) as u8, 0])?;
        (r[0] != 0).then_some(r[0])
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

    fn on_attach(&self, device: IOHIDDeviceRef, buf: *mut u8) {
        if !self.device.get().is_null() {
            return;
        }
        self.log("keyboard attached");
        self.device.set(device);

        let rc = unsafe { IOHIDDeviceOpen(device, 0) };
        if rc != K_IORETURN_SUCCESS {
            self.device.set(ptr::null());
            if rc == K_IORETURN_NOT_PERMITTED {
                self.log("cannot open device: grant this binary Input Monitoring");
            } else {
                self.log(&format!("cannot open device: IOReturn {rc:#010x}"));
            }
            return;
        }

        let ctx = self as *const State as *mut c_void;
        unsafe {
            IOHIDDeviceRegisterInputReportCallback(device, buf, 64, report_cb, ctx);
            IOHIDDeviceScheduleWithRunLoop(device, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
        }

        match self.feature_index(FEATURE_REPROG_CONTROLS_V4) {
            Some(f) => self.feature.set(f),
            None => {
                self.log("device does not expose HID++ feature 0x1B04");
                return;
            }
        }

        self.set_divert(true);

        // Report success only once the keyboard confirms it, so "working" is a
        // fact about the device rather than about our intent.
        for _ in 0..40 {
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.02, 1) };
            if self.divert_is_set() {
                self.log("control diverted — the key is live, and F12 is untouched");
                return;
            }
        }
        self.log("divert was not confirmed by the device");
    }

    fn on_detach(&self) {
        self.log("keyboard detached");
        self.device.set(ptr::null());
        self.feature.set(0);
        self.pressed.set(false);
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
    state.on_attach(dev, report_buffer());
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
    }));
    let ctx = state as *const State as *mut c_void;

    SHUTDOWN_STATE.store(state as *const State as *mut State, Ordering::SeqCst);
    unsafe {
        signal(2, on_signal);  // SIGINT
        signal(15, on_signal); // SIGTERM
    }

    state.log("starting");

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
