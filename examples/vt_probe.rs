//! Developer probe: looks up a SHA-256 (EICAR by default) with the API key
//! stored by the app (DPAPI) or with `VT_API_KEY` if present.
//!
//! Run with: cargo run --example vt_probe [sha256]

use zeroize::Zeroizing;

use vtdrop::vt::{Client, Lookup, RateLimiter};

const EICAR_SHA256: &str = "275a021bbfb6489e54d471899f7db9d1663fc695ec2fe2a2c4538aabf651fd0f";

fn main() {
    let key = match std::env::var("VT_API_KEY") {
        Ok(key) => Zeroizing::new(key),
        Err(_) => match vtdrop::config::load_api_key() {
            Ok(Some(key)) => key,
            Ok(None) => {
                eprintln!("No API key found. Enter it in VTDrop or set VT_API_KEY.");
                std::process::exit(2);
            }
            Err(error) => {
                eprintln!("Could not decrypt the API key: {error}");
                std::process::exit(2);
            }
        },
    };

    let custom = std::env::args().nth(1);
    let sha256 = custom.as_deref().unwrap_or(EICAR_SHA256);
    let mut client = Client::new(key, RateLimiter::new(4));
    match client.lookup_file(sha256) {
        Ok(Lookup::Found(stats)) if custom.is_some() => {
            println!("stats: {stats:?}");
            println!("ratio: {}/{}", stats.ratio().0, stats.ratio().1);
        }
        Ok(Lookup::NotFound) if custom.is_some() => println!("404: VirusTotal does not know {sha256}"),
        Ok(Lookup::Found(stats)) => {
            println!("EICAR SHA-256: {EICAR_SHA256}");
            println!("stats: {stats:?}");
            println!("ratio: {}/{}", stats.ratio().0, stats.ratio().1);
            if stats.malicious == 0 {
                eprintln!("Unexpected: EICAR has zero malicious detections.");
                std::process::exit(1);
            }
        }
        Ok(Lookup::NotFound) => {
            eprintln!("VirusTotal returned 404 for EICAR.");
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("Lookup failed: {error}");
            std::process::exit(1);
        }
    }
}
