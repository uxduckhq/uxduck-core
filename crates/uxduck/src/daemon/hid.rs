//! Every input element of a plugged-in joystick or gamepad and its value now,
//! read from `IOKit` on macOS when the device connects. gilrs reports an input
//! only once it changes, so without this a radio's switches and a throttle
//! resting at idle would not show, or drive, until moved.
//!
//! The one place UXDuck calls `IOKit` itself, through the same objc2
//! bindings and calls gilrs makes; each `unsafe` block says why it holds.
#![allow(
    unsafe_code,
    reason = "IOKit's HID calls through objc2; the workspace denies unsafe elsewhere"
)]

use std::ffi::{CStr, c_void};
use std::ptr::NonNull;

use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString};
use objc2_io_kit::{
    IOHIDDevice, IOHIDElement, IOHIDElementType, IOHIDManager, IOHIDValue, kHIDPage_Button,
    kHIDPage_GenericDesktop, kHIDPage_Simulation, kHIDUsage_GD_GamePad, kHIDUsage_GD_Joystick,
    kHIDUsage_GD_MultiAxisController, kIOHIDDeviceUsageKey, kIOHIDDeviceUsagePageKey,
    kIOHIDOptionsTypeNone, kIOHIDProductIDKey, kIOHIDVendorIDKey, kIOReturnSuccess,
};

/// One input element: the code gilrs gives it (`page << 16 | usage`), and
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
/// product, in the order the device describes them, one per code. Empty
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
    // SAFETY: no matching dictionary, so every element.
    let Some(elements) = (unsafe { device.matching_elements(None, kIOHIDOptionsTypeNone) }) else {
        return Vec::new();
    };
    // SAFETY: IOHIDDeviceCopyMatchingElements returns IOHIDElements.
    let elements = unsafe { elements.cast_unchecked::<IOHIDElement>() };
    let mut found: Vec<Element> = Vec::new();
    for element in elements {
        let kind = element.r#type();
        let (page, usage) = (element.usage_page(), element.usage());
        let axis = matches!(
            kind,
            IOHIDElementType::Input_Misc | IOHIDElementType::Input_Axis
        ) && is_axis(page, usage);
        let button = kind == IOHIDElementType::Input_Button && page == kHIDPage_Button;
        let code = (page << 16) | usage;
        if !(axis || button) || found.iter().any(|e| e.code == code) {
            continue;
        }
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
        found.push(Element {
            code,
            axis,
            value,
            min: i64::try_from(element.logical_min()).unwrap_or(0),
            max: i64::try_from(element.logical_max()).unwrap_or(0),
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

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
