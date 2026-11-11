#![allow(
    unsafe_code,
    reason = "the isolated regression child enumerates and signals only its own Mach threads"
)]

use super::*;
use mach2::mach_port::mach_port_deallocate;
use mach2::traps::mach_task_self;

extern "C" fn caught_signal(_: libc::c_int) {}

fn interrupt_threads() {
    unsafe {
        let task = mach_task_self();
        let mut threads = std::ptr::null_mut();
        let mut count = 0;
        assert_eq!(
            libc::task_threads(task, &raw mut threads, &raw mut count),
            0
        );
        assert!(count > 0 && !threads.is_null());
        let ports = std::slice::from_raw_parts(threads, count as usize);
        for &port in ports {
            let thread = libc::pthread_from_mach_thread_np(port);
            if thread != 0 {
                let result = libc::pthread_kill(thread, libc::SIGUSR1);
                assert!(result == 0 || result == libc::ESRCH);
            }
            assert_eq!(mach_port_deallocate(task, port), 0);
        }
        assert_eq!(
            libc::vm_deallocate(task, threads.addr(), std::mem::size_of_val(ports)),
            0
        );
    }
}

#[test]
fn caught_signals_do_not_abort_plugins() {
    if std::env::var_os("UXDUCK_TEST_PLUGIN_SIGNALS").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "daemon::plugins::signal_tests::caught_signals_do_not_abort_plugins",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("UXDUCK_TEST_PLUGIN_SIGNALS", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "plugin child {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = (caught_signal as *const ()).addr();
        action.sa_flags = libc::SA_RESTART;
        assert_eq!(libc::sigemptyset(&raw mut action.sa_mask), 0);
        assert_eq!(
            libc::sigaction(libc::SIGUSR1, &raw const action, std::ptr::null_mut()),
            0
        );
    }
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("signal.ts");
    std::fs::write(
        &source,
        "export function onTick() { return { store: { alive: true } }; }",
    )
    .unwrap();
    let (mut plugin, _) = load(source.to_str().unwrap(), true).unwrap();
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(20));
        interrupt_threads();
        let result: Effects = call(&mut plugin, "on_tick", &TickInput::default()).unwrap();
        assert_eq!(result.store["alive"], true);
    }
    let trap = directory.path().join("out-of-bounds.wasm");
    std::fs::write(
        &trap,
        b"\0asm\x01\0\0\0\x01\x05\x01\x60\0\x01\x7f\x03\x02\x01\0\x05\x03\x01\0\x01\x07\x0c\x01\x08describe\0\0\x0a\x0b\x01\x09\0\x41\x80\x80\x04\x28\x02\0\x0b",
    )
    .unwrap();
    let error = load(trap.to_str().unwrap(), false).err().unwrap();
    assert!(
        error.starts_with("describe: error while executing at wasm backtrace:"),
        "{error}"
    );
    let result: Effects = call(&mut plugin, "on_tick", &TickInput::default()).unwrap();
    assert_eq!(result.store["alive"], true);
}
