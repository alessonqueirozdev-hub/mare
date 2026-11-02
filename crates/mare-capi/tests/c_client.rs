//! The library, used from C and C++ as a program built against it would.
//!
//! Each test starts a hub on a fake device in real time, compiles
//! `c/client.c` (or `c/client.cpp`, through `mare_client.hpp`) against
//! `include/` and the library this crate built, and runs it against the hub.
//! The program checks every input sample it is handed and doubles it into
//! its outputs; the test then stops the hub, and requires the program to
//! have seen the hub stop, the structures to have the sizes Rust gives them,
//! its events to have reached the hub, and its last outputs to be what the
//! device played.
//!
//! On Windows the compiler is Microsoft's, found where Visual Studio
//! installed it; elsewhere `$CC` or `cc`, and `$CXX` or `c++`. Where there
//! is none, the test says so and passes.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use mare_hub::fake::{FakeDevice, Pacing};
use mare_hub::{DeviceConfig, DeviceLatency, HubConfig, SystemClock};
use mare_sys::{HubName, current_pid};
use mareclient::{MareClientConfig, MareEvent, MareHubInfo};

fn unique_hub() -> HubName {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    HubName::new(&format!("c{:x}-{n}", current_pid() & 0xF_FFFF)).unwrap()
}

fn levels(channel: u32, _frame: u64) -> f32 {
    if channel == 0 { 0.25 } else { -0.5 }
}

/// Where cargo put the library: beside this test, in `deps` (it copies it
/// up a directory only sometimes).
fn library_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().to_owned()
}

/// Compiles `source` into `program`, or returns `None` if there is no
/// compiler.
fn compile(source: &Path, cpp: bool, program: &Path, work: &Path) -> Option<std::process::Output> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let include = root.join("../../include");
    let lib = library_dir();
    #[cfg(windows)]
    {
        let triple = if cfg!(target_arch = "aarch64") {
            "aarch64-pc-windows-msvc"
        } else {
            "x86_64-pc-windows-msvc"
        };
        let Some(tool) = cc::windows_registry::find_tool(triple, "cl.exe") else {
            eprintln!("skipped: no Microsoft C compiler found");
            return None;
        };
        let mut command = tool.to_command();
        command
            .current_dir(work)
            .args(["/nologo", "/W4", "/WX", "/utf-8"]);
        if cpp {
            command.args(["/std:c++17", "/EHsc"]);
        } else {
            command.arg("/std:c11");
        }
        command
            .arg("/I")
            .arg(&include)
            .arg(source)
            .arg(format!("/Fe:{}", program.display()))
            .arg("/link")
            .arg(format!("/LIBPATH:{}", lib.display()))
            .arg("mareclient.dll.lib");
        Some(command.output().unwrap())
    }
    #[cfg(not(windows))]
    {
        let (var, default, standard) = if cpp {
            ("CXX", "c++", "-std=c++17")
        } else {
            ("CC", "cc", "-std=c11")
        };
        let compiler = std::env::var(var).unwrap_or_else(|_| default.to_owned());
        let output = Command::new(&compiler)
            .current_dir(work)
            .args([standard, "-Wall", "-Wextra", "-Werror", "-pedantic", "-I"])
            .arg(&include)
            .arg(source)
            .arg("-o")
            .arg(program)
            .arg("-L")
            .arg(&lib)
            .arg("-lmareclient")
            .arg(format!("-Wl,-rpath,{}", lib.display()))
            .output();
        match output {
            Ok(output) => Some(output),
            Err(error) => {
                eprintln!("skipped: no compiler ({compiler}): {error}");
                None
            }
        }
    }
}

/// Kills the program if the test fails before it ends.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_client(source: &str, cpp: bool) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let kind = if cpp { "cpp" } else { "c" };
    let work = std::env::temp_dir().join(format!("mare-capi-{kind}-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let program = work.join(format!("client{}", std::env::consts::EXE_SUFFIX));
    let Some(compiled) = compile(&root.join(source), cpp, &program, &work) else {
        let _ = std::fs::remove_dir_all(&work);
        return;
    };
    assert!(
        compiled.status.success(),
        "{source} does not compile cleanly:\n{}{}",
        String::from_utf8_lossy(&compiled.stdout),
        String::from_utf8_lossy(&compiled.stderr)
    );

    let name = unique_hub();
    let device = FakeDevice::new(
        DeviceConfig {
            sample_rate: 48_000,
            period_frames: 64,
            max_frames: 128,
            input_channels: 2,
            output_channels: 2,
        },
        Pacing::RealTime,
    )
    .with_input(levels)
    .with_latency(DeviceLatency {
        input_frames: 96,
        output_frames: 160,
    });
    let hub = mare_hub::start(
        &HubConfig::new(name.clone()),
        device,
        Arc::new(SystemClock::new()),
    )
    .unwrap();

    // The program finds the library through its search path, which starts
    // at the directory cargo built it in.
    let lib = library_dir();
    let mut command = Command::new(&program);
    command
        .args([name.as_str(), unique_hub().as_str()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for var in ["PATH", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"] {
        let mut paths = vec![lib.clone()];
        if let Some(old) = std::env::var_os(var) {
            paths.extend(std::env::split_paths(&old));
        }
        command.env(var, std::env::join_paths(paths).unwrap());
    }
    let mut client = Running(command.spawn().unwrap());

    // Let it run 300 cycles (0.4 s), then stop the hub under it.
    let start = Instant::now();
    loop {
        let stats = hub.stats();
        if stats.client_cycles >= 300 {
            break;
        }
        if let Some(status) = client.0.try_wait().unwrap() {
            let output = std::mem::replace(&mut client.0, Command::new(&program).spawn().unwrap())
                .wait_with_output()
                .unwrap();
            panic!(
                "the client ended early ({status}):\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the client never ran: {stats:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let stats = hub.stats();
    let (engine, error) = hub.stop();
    assert!(error.is_none());
    let child = std::mem::replace(&mut client.0, Command::new(&program).spawn().unwrap());
    let output = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&work);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    eprintln!("{kind}: {}", stdout.trim().replace('\n', "; "));
    assert!(
        output.status.success(),
        "the client failed:\n{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut lines = stdout.lines();
    assert_eq!(
        lines.next().unwrap(),
        format!(
            "sizes {} {} {}",
            size_of::<MareClientConfig>(),
            size_of::<MareHubInfo>(),
            size_of::<MareEvent>()
        )
    );
    assert_eq!(lines.next().unwrap(), "missing 1 null");
    assert_eq!(lines.next().unwrap(), "bad name 12");
    assert_eq!(lines.next().unwrap(), "hub 48000 64 128 2 2");
    assert_eq!(lines.next().unwrap(), "latency 96 160 1");
    assert!(lines.next().unwrap().starts_with("stopped 9 cycles "));
    assert!(stats.events_received >= 16, "{stats:?}");
    // The last cycle's outputs reached the device, unless the client was
    // late for it, which a loaded machine allows.
    if stats.late == 0 {
        let device = engine.device();
        assert!(device.last_output(0).iter().all(|&x| x == 0.5));
        assert!(device.last_output(1).iter().all(|&x| x == -1.0));
    }
}

#[test]
fn a_client_in_c_runs_against_a_hub() {
    run_client("tests/c/client.c", false);
}

#[test]
fn a_client_in_cpp_runs_against_a_hub() {
    run_client("tests/c/client.cpp", true);
}

#[test]
fn the_rust_side_answers_without_a_hub() {
    use mareclient::*;
    assert_eq!(mare_abi_version(), MARE_ABI_VERSION);
    // SAFETY: static nul-terminated strings.
    let text = unsafe { std::ffi::CStr::from_ptr(mare_status_text(MARE_FULL)) };
    assert_eq!(text.to_str().unwrap(), "the hub has no free slot");
    // SAFETY: as above.
    let unknown = unsafe { std::ffi::CStr::from_ptr(mare_status_text(-1)) };
    assert_eq!(unknown.to_str().unwrap(), "unknown status");

    let mut config = std::mem::MaybeUninit::<MareClientConfig>::uninit();
    // SAFETY: a writable config.
    unsafe { mare_client_config_init(config.as_mut_ptr()) };
    // SAFETY: just filled.
    let mut config = unsafe { config.assume_init() };
    assert_eq!(config.size as usize, size_of::<MareClientConfig>());
    assert_eq!(
        (config.events_in_capacity, config.connect_timeout_ms),
        (256, 2000)
    );

    let mut client = std::ptr::dangling_mut::<MareClient>();
    // SAFETY: valid arguments; there is no hub of this name.
    let status = unsafe { mare_connect(c"nohub".as_ptr(), &config, &mut client) };
    assert_eq!((status, client.is_null()), (MARE_NO_HUB, true));
    // A config from an older, smaller header is refused, not misread.
    config.size = 8;
    // SAFETY: as above.
    let status = unsafe { mare_connect(std::ptr::null(), &config, &mut client) };
    assert_eq!(status, MARE_BAD_ARGUMENT);
    // SAFETY: null arguments are refused, not followed.
    unsafe {
        assert_eq!(
            mare_connect(std::ptr::null(), std::ptr::null(), &mut client),
            MARE_BAD_ARGUMENT
        );
        assert_eq!(
            mare_wait_cycle(std::ptr::null_mut(), 0, None, std::ptr::null_mut()),
            MARE_BAD_ARGUMENT
        );
        mare_leave(std::ptr::null_mut());
    }
}
