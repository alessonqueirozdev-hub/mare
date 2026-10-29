//! Lists the render endpoints, without opening any.

fn main() {
    #[cfg(windows)]
    match mare_wasapi::render_endpoints() {
        Ok(endpoints) => {
            for endpoint in endpoints {
                let default = if endpoint.is_default {
                    " (default)"
                } else {
                    ""
                };
                println!("{}{default}\n    {}", endpoint.name, endpoint.id);
            }
        }
        Err(error) => eprintln!("{error}"),
    }
    #[cfg(not(windows))]
    eprintln!("WASAPI is Windows-only");
}
