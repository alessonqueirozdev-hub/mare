//! Lists the output and input endpoints, without opening any.

fn main() {
    #[cfg(windows)]
    for (title, list) in [
        ("Outputs", mare_wasapi::render_endpoints()),
        ("Inputs", mare_wasapi::capture_endpoints()),
    ] {
        println!("{title}:");
        match list {
            Ok(endpoints) => {
                for endpoint in endpoints {
                    let default = if endpoint.is_default {
                        " (default)"
                    } else {
                        ""
                    };
                    println!("  {}{default}\n      {}", endpoint.name, endpoint.id);
                }
            }
            Err(error) => eprintln!("  {error}"),
        }
    }
    #[cfg(not(windows))]
    eprintln!("WASAPI is Windows-only");
}
