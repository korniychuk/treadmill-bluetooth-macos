//! Event-driven power/sleep hooks via IOKit — replaces the old `pmset`-poll.
//!
//! ## Why event-driven, not polling (incident 2026-07-05)
//!
//! The previous implementation polled `pmset -g batt` once every 60s from
//! `daemon.rs`. A live incident showed the daemon stuck in the "on battery,
//! skip scanning" branch for 10+ hours while the machine was actually on AC
//! power the whole time — the poll cycle silently stopped advancing and
//! there was no signal from the OS to notice it. Subscribing to the real
//! IOKit notifications removes the polling delay entirely (reacts within the
//! same run-loop tick) and, just as importantly, gives every transition an
//! explicit, unmissable log line instead of relying on a periodic check that
//! can itself get stuck.
//!
//! ## Architecture
//!
//! IOKit power notifications are delivered on a `CFRunLoop`, which is not
//! async/tokio-compatible. We dedicate a plain OS thread to run the loop
//! forever (`CFRunLoopRun()` never returns by design) and bridge both
//! notification sources — `IOPSNotificationCreateRunLoopSource` (AC/battery
//! changes) and `IORegisterForSystemPower` (sleep/wake/shutdown) — into a
//! single [`PowerEvent`] channel that `daemon::run()` can `tokio::select!`
//! on.
//!
//! `io-kit-sys` 0.5 only binds the `IOPowerSources.h` (`ps::power_sources`)
//! surface; it does not bind `IORegisterForSystemPower` /
//! `IOAllowPowerChange` / `IOCancelPowerChange` (verified against its
//! source — `pwr_mgt` only carries `IOPM*` constants). Those are hand-rolled
//! `extern "C"` declarations here. No custom `build.rs` is needed: `io-kit-sys`
//! already links `IOKit.framework` itself (its build script emits
//! `cargo:rustc-link-lib=framework=IOKit`), and `core-foundation-sys` links
//! `CoreFoundation.framework` via a `#[link(...)]` attribute in its own
//! source — both propagate transitively to this binary.

use std::ffi::c_void;
use std::thread;

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::base::CFRelease;
use core_foundation_sys::runloop::{
    CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRun, kCFRunLoopDefaultMode,
};
use io_kit_sys::ps::power_sources::{
    IOPSCopyPowerSourcesInfo, IOPSGetProvidingPowerSourceType, IOPSNotificationCreateRunLoopSource,
};
use io_kit_sys::types::io_connect_t;
use io_kit_sys::{IONotificationPortGetRunLoopSource, IONotificationPortRef};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::{error, info, warn};

/// `kIOMessageCanSystemSleep` (`IOMessage.h`) — must be acknowledged via
/// `IOAllowPowerChange`/`IOCancelPowerChange` or the system blocks on us.
const K_IO_MESSAGE_CAN_SYSTEM_SLEEP: u32 = 0xE000_0270;
/// `kIOMessageSystemWillSleep` — same ack requirement as above.
const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: u32 = 0xE000_0280;
/// `kIOMessageSystemWillPowerOn` — fires before the system is fully awake;
/// we wait for `kIOMessageSystemHasPoweredOn` instead to report "did wake".
const K_IO_MESSAGE_SYSTEM_WILL_POWER_ON: u32 = 0xE000_0320;
/// `kIOMessageSystemHasPoweredOn` — system finished waking up.
const K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = 0xE000_0300;
/// `kIOMessageSystemWillPowerOff` — shutdown initiated.
const K_IO_MESSAGE_SYSTEM_WILL_POWER_OFF: u32 = 0xE000_0250;
/// `kIOMessageSystemWillRestart` — restart initiated.
const K_IO_MESSAGE_SYSTEM_WILL_RESTART: u32 = 0xE000_0310;

/// `kIOPMACPowerKey` ("AC Power") — the string `IOPSGetProvidingPowerSourceType`
/// returns while on mains power. Compared as a Rust string because the C
/// constant in `io_kit_sys` is a raw `c_char` pointer, awkward to match on
/// directly.
const AC_POWER_KEY: &str = "AC Power";

/// Power/sleep transitions the daemon reacts to. Every variant corresponds
/// to exactly one IOKit notification handled below — no periodic re-checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    /// AC power source changed. `true` = now on AC, `false` = now on battery.
    AcPowerChanged(bool),
    /// System is about to sleep (lid closed, idle sleep, Sleep menu item).
    WillSleep,
    /// System just finished waking up.
    DidWake,
    /// System is about to power off or restart.
    WillPowerOff,
}

/// Whether the machine is currently drawing from AC power, read on demand.
///
/// Backed directly by `IOPSCopyPowerSourcesInfo`/`IOPSGetProvidingPowerSourceType`
/// (no subprocess, unlike the old `pmset` implementation). Defaults to `true`
/// (i.e. "keep scanning") if IOKit returns nothing usable — failing open means
/// an IOKit regression degrades back to the old always-scan behavior instead
/// of silently going quiet.
pub fn is_on_ac_power() -> bool {
    // Safety: `IOPSCopyPowerSourcesInfo` follows the Copy rule (we own the
    // returned snapshot and must `CFRelease` it exactly once).
    // `IOPSGetProvidingPowerSourceType` follows the Get rule (the returned
    // `CFStringRef` is borrowed from the snapshot and must not be released,
    // and must only be read before the snapshot itself is released).
    unsafe {
        let snapshot = IOPSCopyPowerSourcesInfo();
        if snapshot.is_null() {
            warn!("IOPSCopyPowerSourcesInfo returned null — assuming AC power");
            return true;
        }
        let type_ref = IOPSGetProvidingPowerSourceType(snapshot);
        let on_ac = if type_ref.is_null() {
            warn!("IOPSGetProvidingPowerSourceType returned null — assuming AC power");
            true
        } else {
            let source_type = CFString::wrap_under_get_rule(type_ref);
            source_type == AC_POWER_KEY
        };
        CFRelease(snapshot);
        on_ac
    }
}

/// Reads the current AC/battery state without asserting it came from a
/// notification — shared by the seed value and the power-source callback.
fn read_power_source() -> bool {
    is_on_ac_power()
}

/// Context handed to the power-source-changed callback as a raw pointer.
struct PowerSourceCtx {
    tx: UnboundedSender<PowerEvent>,
    /// Last reported AC state, used to dedupe: the notification fires on
    /// *any* power-source dictionary change (battery %, time remaining,
    /// adapter wattage), not just AC <-> battery transitions.
    last_on_ac: bool,
}

extern "C" fn power_source_callback(context: *mut c_void) {
    // Safety: `context` is always a live `Box<PowerSourceCtx>` pointer we
    // allocated and leaked in `spawn_power_event_listener`, for the lifetime
    // of the dedicated run-loop thread — never freed while this fires.
    let ctx = unsafe { &mut *(context as *mut PowerSourceCtx) };
    let on_ac = read_power_source();
    if on_ac == ctx.last_on_ac {
        return; // dedupe: not an AC<->battery transition, just a metadata tick
    }
    ctx.last_on_ac = on_ac;
    info!(on_ac, "power source changed");
    if ctx.tx.send(PowerEvent::AcPowerChanged(on_ac)).is_err() {
        warn!("power-event receiver dropped; AC power change event lost");
    }
}

/// Context handed to the system-power (sleep/wake/shutdown) callback.
struct SystemPowerCtx {
    tx: UnboundedSender<PowerEvent>,
    /// The `io_connect_t` returned by `IORegisterForSystemPower`, needed to
    /// ack `kIOMessageCanSystemSleep`/`kIOMessageSystemWillSleep` via
    /// `IOAllowPowerChange`. Filled in right after registration, before the
    /// run loop starts, so it is always valid by the time a callback fires.
    root_port: io_connect_t,
}

extern "C" fn system_power_callback(
    context: *mut c_void,
    _service: u32,
    message_type: u32,
    message_argument: *mut c_void,
) {
    // Safety: same lifetime guarantee as `power_source_callback` above —
    // `context` is a leaked `Box<SystemPowerCtx>` valid for the thread's
    // entire lifetime.
    let ctx = unsafe { &*(context as *const SystemPowerCtx) };
    let notification_id = message_argument as isize;

    match message_type {
        K_IO_MESSAGE_CAN_SYSTEM_SLEEP | K_IO_MESSAGE_SYSTEM_WILL_SLEEP => {
            // We never veto sleep — always allow it immediately so macOS
            // doesn't block/timeout (~30s) waiting on this process.
            // Safety: `root_port` and `notification_id` are the exact values
            // IOKit handed us for this notification; `IOAllowPowerChange` is
            // the documented ack call for both message types.
            unsafe {
                IOAllowPowerChange(ctx.root_port, notification_id);
            }
        }
        _ => {}
    }

    let event = match message_type {
        K_IO_MESSAGE_SYSTEM_WILL_SLEEP => Some(PowerEvent::WillSleep),
        K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => Some(PowerEvent::DidWake),
        K_IO_MESSAGE_SYSTEM_WILL_POWER_OFF | K_IO_MESSAGE_SYSTEM_WILL_RESTART => {
            Some(PowerEvent::WillPowerOff)
        }
        // kIOMessageCanSystemSleep, kIOMessageSystemWillPowerOn and other
        // Root Power Domain chatter we don't act on — deliberately not
        // logged per-message to avoid ticking noise; only real transitions
        // above are.
        _ => None,
    };

    let Some(event) = event else {
        return;
    };
    info!(?event, "system power event");
    if ctx.tx.send(event).is_err() {
        warn!(?event, "power-event receiver dropped; system power event lost");
    }
}

/// Spawns a dedicated OS thread running a `CFRunLoop` with both the
/// power-source (AC/battery) and system-power (sleep/wake/shutdown)
/// notification sources attached, and returns a channel of [`PowerEvent`]s.
///
/// Emits the current AC/battery state as the first event so the async side
/// has a seed value without an extra round-trip.
pub fn spawn_power_event_listener() -> UnboundedReceiver<PowerEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    let initial_on_ac = read_power_source();
    if tx.send(PowerEvent::AcPowerChanged(initial_on_ac)).is_err() {
        warn!("power-event receiver dropped before listener thread started");
    }

    thread::Builder::new()
        .name("power-event-listener".into())
        .spawn(move || {
            // Safety: this closure runs on its own dedicated OS thread for
            // the lifetime of the process. All raw pointers created below
            // (`Box::into_raw`, notification ports, run-loop sources) are
            // intentionally leaked — there is no shutdown path for this
            // thread (matches `CFRunLoopRun()` never returning), so there is
            // no use-after-free or double-free risk in practice.
            unsafe {
                let power_source_ctx = Box::into_raw(Box::new(PowerSourceCtx {
                    tx: tx.clone(),
                    last_on_ac: initial_on_ac,
                }));
                let ps_source = IOPSNotificationCreateRunLoopSource(
                    power_source_callback,
                    power_source_ctx as *mut c_void,
                );
                if ps_source.is_null() {
                    error!("IOPSNotificationCreateRunLoopSource failed — no AC/battery events");
                } else {
                    CFRunLoopAddSource(CFRunLoopGetCurrent(), ps_source, kCFRunLoopDefaultMode);
                }

                let system_power_ctx = Box::into_raw(Box::new(SystemPowerCtx {
                    tx,
                    root_port: 0, // patched below once registration returns
                }));
                let mut notify_port: IONotificationPortRef = std::ptr::null_mut();
                let mut notifier: u32 = 0;
                let root_port = IORegisterForSystemPower(
                    system_power_ctx as *mut c_void,
                    &mut notify_port,
                    system_power_callback,
                    &mut notifier,
                );
                if root_port == 0 {
                    error!("IORegisterForSystemPower failed — no sleep/wake/shutdown events");
                } else {
                    // Safety: no callback can have fired yet (the run loop
                    // hasn't started), so writing root_port here is race-free.
                    (*system_power_ctx).root_port = root_port;
                    let sp_source = IONotificationPortGetRunLoopSource(notify_port);
                    CFRunLoopAddSource(CFRunLoopGetCurrent(), sp_source, kCFRunLoopDefaultMode);
                }

                info!("power-event-listener thread started, entering CFRunLoopRun");
                CFRunLoopRun(); // blocks forever — this thread's sole job
            }
        })
        .expect("failed to spawn power-event-listener thread");

    rx
}

// -- Hand-rolled IOKit bindings not exposed by `io-kit-sys` 0.5 --
//
// `io-kit-sys`'s `pwr_mgt` module only carries `IOPM*` constants (verified
// against its source), not the `IORegisterForSystemPower` notification-port
// API declared in `<IOKit/pwr_mgt/IOPMLib.h>`. Declared here instead.

/// Callback signature for `IOServiceAddInterestNotification`-style system
/// power notifications, matching `IOServiceInterestCallback` in `IOKitLib.h`.
type IOServiceInterestCallback =
    extern "C" fn(refcon: *mut c_void, service: u32, message_type: u32, message_argument: *mut c_void);

unsafe extern "C" {
    /// `<IOKit/pwr_mgt/IOPMLib.h>`. Registers for system-wide power
    /// notifications (sleep/wake/shutdown), returning a connection port used
    /// to ack sleep and, together with `notifier`, to deregister later.
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        the_port_ref: *mut IONotificationPortRef,
        callback: IOServiceInterestCallback,
        notifier: *mut u32,
    ) -> io_connect_t;

    /// Acks `kIOMessageCanSystemSleep`/`kIOMessageSystemWillSleep` so the
    /// system proceeds with sleep instead of waiting on a ~30s timeout.
    fn IOAllowPowerChange(kernel_port: io_connect_t, notification_id: isize) -> i32;
}
