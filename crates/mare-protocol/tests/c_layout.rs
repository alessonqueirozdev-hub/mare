//! The layout, checked from a second language.
//!
//! `include/mare.h` is the layout in C. This test compiles `c/layout_print.c`
//! with it (which also compiles the header's static checks of every size and
//! offset), runs it on a set of configurations, and requires the layout it
//! computes to be the one `mare-protocol` computes, invalid configurations
//! included.
//!
//! A second test includes the header from C++, which it must also compile
//! in.
//!
//! They need a compiler: `$CC` or `cc`, and `$CXX` or `c++`. Where there is none (Windows
//! without a C toolchain on the path) it says so and passes; CI's Linux and
//! macOS runners have one.

use std::path::PathBuf;
use std::process::Command;

use mare_protocol::registry::RegistryLayout;
use mare_protocol::segment::{SegmentConfig, SegmentLayout};
use mare_protocol::ump::packet_words;

fn configs() -> Vec<SegmentConfig> {
    let mut configs = Vec::new();
    for (inputs, outputs) in [(0, 0), (0, 2), (2, 2), (1, 7), (256, 256), (257, 0)] {
        for frames in [0, 1, 15, 16, 17, 64, 100, 1024, 8192, 8193] {
            for (ein, eout) in [(0, 0), (1, 0), (0, 256), (64, 16), (3, 0), (65536, 65536)] {
                configs.push(SegmentConfig {
                    input_channels: inputs,
                    output_channels: outputs,
                    max_frames: frames,
                    events_in_capacity: ein,
                    events_out_capacity: eout,
                });
            }
        }
    }
    configs
}

fn expected(config: &SegmentConfig) -> String {
    match SegmentLayout::new(*config) {
        Ok(l) => format!(
            "{} {} {} {} {} {}",
            l.control_offset(),
            l.audio_offset(),
            l.channel_stride(),
            l.events_in_offset(),
            l.events_out_offset(),
            l.size()
        ),
        Err(_) => "invalid".to_owned(),
    }
}

#[test]
fn the_c_header_computes_the_same_layout() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let include = root.join("../../include");
    let source = root.join("tests/c/layout_print.c");
    let work = std::env::temp_dir().join(format!("mare-c-layout-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let program = work.join("layout_print");
    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let compiled = Command::new(&compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-pedantic", "-I"])
        .arg(&include)
        .arg(&source)
        .arg("-o")
        .arg(&program)
        .output();
    let compiled = match compiled {
        Ok(output) => output,
        Err(error) => {
            eprintln!("skipped: no C compiler ({compiler}): {error}");
            let _ = std::fs::remove_dir_all(&work);
            return;
        }
    };
    assert!(
        compiled.status.success(),
        "mare.h does not compile cleanly:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let configs = configs();
    let args: Vec<String> = configs
        .iter()
        .map(|c| {
            format!(
                "{},{},{},{},{}",
                c.input_channels,
                c.output_channels,
                c.max_frames,
                c.events_in_capacity,
                c.events_out_capacity
            )
        })
        .collect();
    let run = Command::new(&program).args(&args).output().unwrap();
    let _ = std::fs::remove_dir_all(&work);
    assert!(run.status.success());
    let stdout = String::from_utf8(run.stdout).unwrap();
    let mut lines = stdout.lines();
    let mut valid = 0;
    for config in &configs {
        let line = lines.next().unwrap();
        assert_eq!(line, expected(config), "{config:?}");
        valid += usize::from(line != "invalid");
    }
    assert!(
        valid > 100 && valid < configs.len(),
        "both kinds were tried"
    );
    assert_eq!(
        lines.next().unwrap(),
        format!(
            "registry {} {}",
            RegistryLayout::new(1).unwrap().size(),
            RegistryLayout::new(1024).unwrap().size()
        )
    );
    assert_eq!(
        lines.next().unwrap(),
        format!(
            "ump {} {} {} {}",
            packet_words(0),
            packet_words(0x4000_0000),
            packet_words(0xB000_0000),
            packet_words(0xF000_0000)
        )
    );
}

#[test]
fn the_c_header_compiles_as_cpp_too() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("mare-cpp-layout-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let source = work.join("include.cpp");
    std::fs::write(
        &source,
        "#include \"mare.h\"
int main() { mare_segment_layout l; return mare_segment_layout_of(2, 2, 64, 0, 0, &l); }
",
    )
    .unwrap();
    let compiler = std::env::var("CXX").unwrap_or_else(|_| "c++".to_owned());
    let compiled = Command::new(&compiler)
        .args([
            "-std=c++11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsyntax-only",
            "-I",
        ])
        .arg(root.join("../../include"))
        .arg(&source)
        .output();
    let _ = std::fs::remove_dir_all(&work);
    match compiled {
        Ok(output) => assert!(
            output.status.success(),
            "mare.h does not compile as C++:
{}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => eprintln!("skipped: no C++ compiler ({compiler}): {error}"),
    }
}
