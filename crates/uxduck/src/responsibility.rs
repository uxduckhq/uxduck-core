//! Whose permissions the daemon runs with on macOS.
//!
//! macOS decides privacy permissions, Local Network among them, for the
//! process it holds responsible for another: by default the app that started
//! it, inherited by every child. UXDuck's launcher exits once the daemon
//! answers. macOS then refuses the daemon the local network whatever the
//! person allowed, because the process it would ask about is gone: a vehicle
//! on a radio or an Ethernet cable is heard for a few seconds and never again.
//! A daemon started in the background therefore starts once more in its own
//! place, responsible for itself, as Chromium's helpers do.

use std::ffi::{CString, c_char, c_int};
use std::os::unix::ffi::OsStringExt;

/// Set by a launcher that exits before the daemon it starts.
pub const DISCLAIM: &str = "UXDUCK_DISCLAIM_RESPONSIBILITY";

/// Replaces this process with itself, same pid, responsible for itself, when
/// its launcher asked. Returns why it could not, and `None` when nobody asked.
pub fn disclaim() -> Option<std::io::Error> {
    std::env::var_os(DISCLAIM)?;
    replace().err()
}

fn checked(code: c_int) -> std::io::Result<()> {
    // These calls return the error number instead of setting errno.
    if code == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(code))
    }
}

#[allow(
    unsafe_code,
    reason = "posix_spawn and macOS's responsibility call have no safe wrapper"
)]
fn replace() -> std::io::Result<()> {
    let string = |bytes: Vec<u8>| CString::new(bytes).map_err(std::io::Error::other);
    let program = string(std::env::current_exe()?.into_os_string().into_vec())?;
    let arguments = std::env::args_os()
        .map(|argument| string(argument.into_vec()))
        .collect::<Result<Vec<_>, _>>()?;
    // Without the request, so nothing the daemon starts asks again.
    let environment = std::env::vars_os()
        .filter(|(name, _)| name != DISCLAIM)
        .map(|(name, value)| {
            let mut pair = name.into_vec();
            pair.push(b'=');
            pair.extend(value.into_vec());
            string(pair)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let pointers = |strings: &[CString]| {
        strings
            .iter()
            .map(|text| text.as_ptr().cast_mut())
            .chain([std::ptr::null_mut()])
            .collect::<Vec<*mut c_char>>()
    };
    let (argv, envp) = (pointers(&arguments), pointers(&environment));
    // SAFETY: every pointer outlives the calls, both arrays end in null, and
    // the attributes are initialised before use and destroyed after. The
    // symbol is looked up by name because Apple does not publish it; it has
    // had this signature since macOS 10.14, and a system without it is
    // reported instead of failing to load.
    unsafe {
        let symbol = libc::dlsym(
            libc::RTLD_DEFAULT,
            c"responsibility_spawnattrs_setdisclaim".as_ptr(),
        );
        if symbol.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "this macOS has no responsibility_spawnattrs_setdisclaim",
            ));
        }
        let set_disclaim = std::mem::transmute::<
            *mut libc::c_void,
            unsafe extern "C" fn(*mut libc::posix_spawnattr_t, c_int) -> c_int,
        >(symbol);
        let mut attributes = std::ptr::null_mut();
        checked(libc::posix_spawnattr_init(&raw mut attributes))?;
        // POSIX_SPAWN_SETEXEC replaces this process as exec does, so the
        // launcher's child handle, the process group and the open standard
        // streams stay as they were.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the flag is 0x40 and the call takes a short"
        )]
        let replaced = checked(libc::posix_spawnattr_setflags(
            &raw mut attributes,
            libc::POSIX_SPAWN_SETEXEC as libc::c_short,
        ))
        .and_then(|()| checked(set_disclaim(&raw mut attributes, 1)))
        .and_then(|()| {
            checked(libc::posix_spawn(
                std::ptr::null_mut(),
                program.as_ptr(),
                std::ptr::null(),
                &raw const attributes,
                argv.as_ptr(),
                envp.as_ptr(),
            ))
        });
        libc::posix_spawnattr_destroy(&raw mut attributes);
        replaced
    }
}
