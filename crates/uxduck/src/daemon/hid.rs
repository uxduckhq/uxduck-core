//! Every input element of a plugged-in joystick or gamepad and its value now,
//! read from `IOKit` on macOS when the device connects. gilrs reports an input
//! only once it changes, so without this a radio's switches and a throttle
//! resting at idle would not show, or drive, until moved. Repeated axis usages
//! on unmapped devices also stream directly by element identity: gilrs merges
//! them by usage, which loses the Tango 2's second Slider.
//!
//! The one place UXDuck calls `IOKit` itself, through the same objc2
//! bindings and calls gilrs makes; each `unsafe` block says why it holds.
#![allow(
    unsafe_code,
    reason = "IOKit's HID calls through objc2; the workspace denies unsafe elsewhere"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, c_void};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, PoisonError, mpsc};

use objc2_core_foundation::{
    CFArray, CFDictionary, CFNumber, CFRetained, CFRunLoop, CFString, kCFRunLoopDefaultMode,
};
use objc2_io_kit::{
    IOHIDDevice, IOHIDElement, IOHIDElementType, IOHIDManager, IOHIDValue, IOReturn,
    kHIDPage_Button, kHIDPage_GenericDesktop, kHIDPage_Simulation, kHIDUsage_GD_GamePad,
    kHIDUsage_GD_Joystick, kHIDUsage_GD_MultiAxisController, kIOHIDDeviceUsageKey,
    kIOHIDDeviceUsagePageKey, kIOHIDOptionsTypeNone, kIOHIDProductIDKey, kIOHIDVendorIDKey,
    kIOReturnSuccess,
};
use uxduck_engine::controller_input::{InputEvent, macos_hid_axis_code};

/// One input element: a legacy usage code or an additional axis code, and
/// its raw value with the element's logical range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Element {
    pub code: u32,
    pub axis: bool,
    pub value: i64,
    pub min: i64,
    pub max: i64,
}

impl Element {
    /// The value as gilrs reports an axis: -1 to 1, centred where the
    /// logical range has an odd number of steps, `reversed` for the axes
    /// gilrs flips on macOS (its Y sticks).
    #[must_use]
    pub fn normalized(&self, reversed: bool) -> f64 {
        #[allow(clippy::cast_precision_loss, reason = "HID ranges are small")]
        let (mut range, mut value) = ((self.max - self.min) as f64, (self.value - self.min) as f64);
        if (self.max - self.min) % 2 == 1 {
            range += 1.0;
            value += 1.0;
        }
        if range <= 0.0 {
            return 0.0;
        }
        let value = value / range * 2.0 - 1.0;
        let value = if reversed && value != 0.0 {
            -value
        } else {
            value
        };
        value.clamp(-1.0, 1.0)
    }
}

/// Every axis and button of the plugged-in device with this vendor and
/// product, in the order the device describes them, one per element. Empty
/// when `IOKit` cannot be opened or the device is not found.
#[must_use]
pub fn elements(vendor: u16, product: u16) -> Vec<Element> {
    let manager = IOHIDManager::new(None, kIOHIDOptionsTypeNone);
    let matchers = CFArray::from_retained_objects(&[
        matcher(kHIDUsage_GD_Joystick),
        matcher(kHIDUsage_GD_GamePad),
        matcher(kHIDUsage_GD_MultiAxisController),
    ]);
    // SAFETY: the matchers are dictionaries of usage page and usage.
    unsafe { manager.set_device_matching_multiple(Some(matchers.as_opaque())) };
    if manager.open(kIOHIDOptionsTypeNone) != kIOReturnSuccess {
        return Vec::new();
    }
    let Some(devices) = manager.devices() else {
        return Vec::new();
    };
    let count = usize::try_from(devices.count()).unwrap_or(0);
    let mut values: Vec<*const c_void> = vec![std::ptr::null(); count];
    // SAFETY: `values` holds `count` pointers, as many as the set has.
    unsafe { devices.values(values.as_mut_ptr()) };
    let found = values.into_iter().find_map(|device| {
        // SAFETY: IOHIDManagerCopyDevices returns a set of IOHIDDevices,
        // alive while `devices` is.
        let device = unsafe { device.cast::<IOHIDDevice>().as_ref() }?;
        (number(device, kIOHIDVendorIDKey) == Some(i64::from(vendor))
            && number(device, kIOHIDProductIDKey) == Some(i64::from(product)))
        .then(|| read(device))
    });
    manager.close(kIOHIDOptionsTypeNone);
    found.unwrap_or_default()
}

fn matcher(usage: u32) -> CFRetained<CFDictionary<CFString, CFNumber>> {
    let key = |key: &CStr| CFString::from_str(key.to_str().unwrap_or_default());
    #[allow(clippy::cast_possible_wrap, reason = "HID usages are small")]
    CFDictionary::from_slices(
        &[&*key(kIOHIDDeviceUsagePageKey), &*key(kIOHIDDeviceUsageKey)],
        &[
            &*CFNumber::new_i32(kHIDPage_GenericDesktop as i32),
            &*CFNumber::new_i32(usage as i32),
        ],
    )
}

fn number(device: &IOHIDDevice, key: &CStr) -> Option<i64> {
    let key = CFString::from_str(key.to_str().ok()?);
    device.property(&key)?.downcast::<CFNumber>().ok()?.as_i64()
}

/// An axis as gilrs takes one: X to Wheel on the generic desktop page, or a
/// simulation control.
fn is_axis(page: u32, usage: u32) -> bool {
    (page == kHIDPage_GenericDesktop && (0x30..=0x38).contains(&usage))
        || (page == kHIDPage_Simulation && matches!(usage, 0xBA..=0xC5))
}

fn read(device: &IOHIDDevice) -> Vec<Element> {
    // The legacy snapshot is also used for SDL-mapped gamepads and when a
    // native observer cannot be opened. Do not advertise inputs without events.
    identified(device)
        .into_iter()
        .filter(|(_, usage, element)| *usage == element.code)
        .map(|(_, _, element)| element)
        .collect()
}

/// Assign additional occurrences a stable descriptor-order code. The first
/// occurrence keeps its old usage code, preserving saved controller mappings.
fn identify(elements: &[(u32, u32, bool)]) -> Vec<(u32, u32, u32)> {
    let mut sorted = elements.to_vec();
    sorted.sort_by_key(|&(cookie, _, _)| cookie);
    sorted.dedup_by_key(|element| element.0);
    let mut seen = BTreeSet::new();
    let mut axis_number = 0;
    sorted
        .into_iter()
        .filter_map(|(cookie, usage, axis)| {
            if axis {
                axis_number += 1;
            }
            let first = seen.insert((usage, axis));
            if !axis && !first {
                return None;
            }
            let code = if first {
                usage
            } else {
                macos_hid_axis_code(axis_number)
            };
            Some((cookie, usage, code))
        })
        .collect()
}

fn identified(device: &IOHIDDevice) -> Vec<(u32, u32, Element)> {
    // SAFETY: no matching dictionary, so every element.
    let Some(elements) = (unsafe { device.matching_elements(None, kIOHIDOptionsTypeNone) }) else {
        return Vec::new();
    };
    // SAFETY: IOHIDDeviceCopyMatchingElements returns IOHIDElements.
    let elements = unsafe { elements.cast_unchecked::<IOHIDElement>() };
    let mut descriptions = Vec::new();
    for element in elements {
        let kind = element.r#type();
        let (page, usage) = (element.usage_page(), element.usage());
        let axis = matches!(
            kind,
            IOHIDElementType::Input_Misc | IOHIDElementType::Input_Axis
        ) && is_axis(page, usage);
        let button = kind == IOHIDElementType::Input_Button && page == kHIDPage_Button;
        let code = (page << 16) | usage;
        if axis || button {
            descriptions.push((element.cookie(), code, axis));
        }
    }
    let ids = identify(&descriptions);
    let mut found = Vec::new();
    for (cookie, usage, code) in ids {
        let Some(element) = elements.iter().find(|element| element.cookie() == cookie) else {
            continue;
        };
        let axis = is_axis(element.usage_page(), element.usage());
        let mut value: *const IOHIDValue = std::ptr::null();
        let Some(slot) = NonNull::new((&raw mut value).cast::<NonNull<IOHIDValue>>()) else {
            continue;
        };
        // SAFETY: `slot` points at `value`, written on success with a value
        // the device owns, read at once.
        let read = unsafe { device.value(&element, slot) };
        let value = if read == kIOReturnSuccess {
            // SAFETY: set on success, valid until the next read.
            unsafe { value.as_ref() }.map_or(0, |v| i64::try_from(v.integer_value()).unwrap_or(0))
        } else {
            0
        };
        found.push((
            cookie,
            usage,
            Element {
                code,
                axis,
                value,
                min: i64::try_from(element.logical_min()).unwrap_or(0),
                max: i64::try_from(element.logical_max()).unwrap_or(0),
            },
        ));
    }
    found
}

/// Keeps both values of repeated HID usages, before gilrs reduces an element
/// to its usage. Only unknown layouts with repeated axes use this observer;
/// ordinary gamepads retain their SDL mappings and existing event path.
pub struct Duplicates {
    usages: BTreeSet<u32>,
    active: Arc<Mutex<bool>>,
    start: Option<mpsc::Sender<()>>,
    pub initial: Vec<Element>,
}

impl Duplicates {
    pub fn handles(&self, code: u32) -> bool {
        self.usages.contains(&code)
    }

    /// Called after Connected and the initial values have been delivered.
    pub fn activate(&mut self) {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = true;
        if let Some(start) = self.start.take() {
            let _ = start.send(());
        }
    }
}

impl Drop for Duplicates {
    fn drop(&mut self) {
        // Serialize with delivery: no stale native event can follow Disconnect.
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = false;
    }
}

struct Observer {
    id: String,
    elements: BTreeMap<u32, Element>,
    active: Arc<Mutex<bool>>,
    events: Box<dyn Fn(InputEvent)>,
    steady: super::controllers::Steady,
}

impl Observer {
    fn update(&mut self, cookie: u32, raw: i64) {
        let active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if !*active {
            return;
        }
        let Some(mut element) = self.elements.get(&cookie).copied() else {
            return;
        };
        element.value = raw;
        let normalized = element.normalized(false);
        if self.steady.changed(&self.id, element.code, normalized) {
            (self.events)(InputEvent::Axis {
                id: self.id.clone(),
                axis: None,
                code: Some(element.code),
                value: normalized,
            });
        }
    }
}

unsafe extern "C-unwind" fn input_value(
    context: *mut c_void,
    result: IOReturn,
    _sender: *mut c_void,
    value: NonNull<IOHIDValue>,
) {
    if result != kIOReturnSuccess {
        return;
    }
    // SAFETY: the context is boxed for the lifetime of this thread's scheduled
    // callback. IOKit supplies a live value; callbacks run serially on this loop.
    let observer = unsafe { &mut *context.cast::<Observer>() };
    let value = unsafe { value.as_ref() };
    observer.update(
        value.element().cookie(),
        i64::try_from(value.integer_value()).unwrap_or(0),
    );
}

/// Establish the reader synchronously so gilrs events are suppressed only once
/// the native reader is ready. Opening uses shared, read-only HID access.
pub fn observe_duplicates(
    vendor: u16,
    product: u16,
    id: String,
    events: impl Fn(InputEvent) + Send + 'static,
) -> Option<Duplicates> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (start_tx, start_rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("hid-duplicate-axes".into())
        .spawn(move || {
            let manager = IOHIDManager::new(None, kIOHIDOptionsTypeNone);
            let keys = [
                CFString::from_str("VendorID"),
                CFString::from_str("ProductID"),
            ];
            let matching = CFDictionary::from_slices(
                &[&*keys[0], &*keys[1]],
                &[
                    &*CFNumber::new_i32(i32::from(vendor)),
                    &*CFNumber::new_i32(i32::from(product)),
                ],
            );
            // SAFETY: a valid property dictionary; no callback context is registered yet.
            unsafe { manager.set_device_matching(Some(matching.as_opaque())) };
            if manager.open(kIOHIDOptionsTypeNone) != kIOReturnSuccess {
                return;
            }
            let Some(devices) = manager.devices() else {
                manager.close(kIOHIDOptionsTypeNone);
                return;
            };
            // Do not route a different physical device's controls when VID/PID is ambiguous.
            if devices.count() != 1 {
                tracing::warn!(
                    vendor,
                    product,
                    count = devices.count(),
                    "cannot distinguish duplicate-axis HID devices"
                );
                manager.close(kIOHIDOptionsTypeNone);
                return;
            }
            let mut device = std::ptr::null();
            // SAFETY: the set contains exactly one retained IOHIDDevice.
            unsafe { devices.values(&raw mut device) };
            let Some(device) = (unsafe { device.cast::<IOHIDDevice>().as_ref() }) else {
                manager.close(kIOHIDOptionsTypeNone);
                return;
            };
            let all = identified(device);
            let usages: BTreeSet<u32> = all
                .iter()
                .filter(|(_, usage, element)| element.axis && *usage != element.code)
                .map(|(_, usage, _)| *usage)
                .collect();
            if usages.is_empty() {
                manager.close(kIOHIDOptionsTypeNone);
                return;
            }
            let active = Arc::new(Mutex::new(false));
            let mut observer = Box::new(Observer {
                id,
                elements: all
                    .iter()
                    .filter(|(_, usage, _)| usages.contains(usage))
                    .map(|(cookie, _, element)| (*cookie, *element))
                    .collect(),
                active: active.clone(),
                events: Box::new(events),
                steady: super::controllers::Steady::default(),
            });
            let Some(run_loop) = CFRunLoop::current() else {
                manager.close(kIOHIDOptionsTypeNone);
                return;
            };
            // SAFETY: default mode is a process-lifetime CoreFoundation constant.
            let Some(mode) = (unsafe { kCFRunLoopDefaultMode }) else {
                manager.close(kIOHIDOptionsTypeNone);
                return;
            };
            // SAFETY: the boxed context and device stay alive until unscheduling and
            // unregistering below, on this same thread. No other thread accesses them.
            unsafe {
                device
                    .register_input_value_callback(Some(input_value), (&raw mut *observer).cast());
                device.schedule_with_run_loop(&run_loop, mode);
            }
            let ready = Duplicates {
                usages,
                active: active.clone(),
                start: Some(start_tx),
                initial: all.into_iter().map(|(_, _, element)| element).collect(),
            };
            if ready_tx.send(ready).is_ok() && start_rx.recv().is_ok() {
                // Event-driven reads; the bounded wait only lets a detached observer
                // clean up promptly after Disconnect without blocking other gamepads.
                while *active.lock().unwrap_or_else(PoisonError::into_inner) {
                    CFRunLoop::run_in_mode(Some(mode), 1.0, false);
                }
            }
            // SAFETY: callbacks run on this thread; the box remains alive until
            // both registrations have been removed, before closing the manager.
            unsafe {
                device.unschedule_from_run_loop(&run_loop, mode);
                device.register_input_value_callback(None, std::ptr::null_mut());
            }
            manager.close(kIOHIDOptionsTypeNone);
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not start duplicate-axis HID observer");
        return None;
    }
    ready_rx.recv().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tango_elements() -> Vec<(u32, u32, bool)> {
        // Captured TBS Tango 2/FreedomTX 1.4.0: the last two usages are Slider.
        [0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x36]
            .into_iter()
            .enumerate()
            .map(|(i, usage)| (29 + u32::try_from(i).unwrap(), 0x10000 | usage, true))
            .collect()
    }

    #[test]
    fn repeated_sliders_keep_descriptor_identity_and_legacy_mappings() {
        let elements = tango_elements();
        let codes = identify(&elements);
        assert_eq!(codes.len(), 8);
        assert_eq!(codes[6], (35, 0x10036, 0x10036));
        assert_eq!(codes[7], (36, 0x10036, macos_hid_axis_code(8)));
        assert_eq!(codes.iter().map(|x| x.2).collect::<BTreeSet<_>>().len(), 8);
        let mut reordered = elements;
        reordered.reverse();
        reordered.push(reordered[0]);
        assert_eq!(
            identify(&reordered),
            codes,
            "enumeration order and repeated cookies cannot change saved codes"
        );
    }

    #[test]
    fn xbox_style_unique_axes_and_buttons_keep_their_codes() {
        let elements = [
            (1, 0x10030, true),
            (2, 0x10031, true),
            (3, 0x10032, true),
            (4, 0x90001, false),
            (5, 0x90002, false),
        ];
        for (_, usage, code) in identify(&elements) {
            assert_eq!(code, usage);
        }
    }

    #[test]
    fn both_sliders_deliver_identical_values_independently_and_stop_on_disconnect() {
        let values = Arc::new(Mutex::new(Vec::new()));
        let output = values.clone();
        let active = Arc::new(Mutex::new(true));
        let mut observer = Observer {
            id: "tango".into(),
            active: active.clone(),
            elements: [(35, 0x10036), (36, macos_hid_axis_code(8))]
                .into_iter()
                .map(|(cookie, code)| {
                    (
                        cookie,
                        Element {
                            code,
                            axis: true,
                            value: 0,
                            min: 0,
                            max: 2047,
                        },
                    )
                })
                .collect(),
            events: Box::new(move |event| output.lock().unwrap().push(event)),
            steady: super::super::controllers::Steady::default(),
        };
        observer.update(35, 2047);
        observer.update(36, 2047);
        observer.update(36, 0);
        observer.update(35, 1023);
        let delivered: Vec<_> = values
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                InputEvent::Axis { code, value, .. } => (code.unwrap(), *value),
                _ => panic!("axis expected"),
            })
            .collect();
        assert_eq!(delivered.len(), 4);
        assert_eq!(delivered[0], (0x10036, 1.0));
        assert_eq!(delivered[1], (macos_hid_axis_code(8), 1.0));
        assert!(delivered[2].1 < -0.99);
        assert_eq!(delivered[3], (0x10036, 0.0));
        *active.lock().unwrap() = false;
        observer.update(35, 2047);
        assert_eq!(values.lock().unwrap().len(), 4);
    }

    #[test]
    fn values_normalize_as_gilrs_does() {
        let at = |value| Element {
            code: 0x1_0030,
            axis: true,
            value,
            min: 0,
            max: 2047,
        };
        assert!(
            (at(0).normalized(false) + 1.0).abs() < 1e-3,
            "the bottom of an odd range"
        );
        assert!(at(1023).normalized(false).abs() < 1e-9, "centred");
        assert!((at(2047).normalized(false) - 1.0).abs() < 1e-9);
        assert!((at(2047).normalized(true) + 1.0).abs() < 1e-9, "flipped");
    }
}
