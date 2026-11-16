use zeroize::Zeroizing;

use vtdrop::vt::{Client, Lookup};

const EICAR_SHA256: &str = "275a021bbfb6489e54d471899f7db9d1663fc695ec2fe2a2c4538aabf651fd0f";

#[test]
#[ignore = "requires VT_API_KEY and network access"]
fn eicar_is_known_to_virustotal() {
    let key = std::env::var("VT_API_KEY").expect("set VT_API_KEY to a valid VirusTotal API key");
    let mut client = Client::new(Zeroizing::new(key), vtdrop::vt::RateLimiter::new(4));
    match client.lookup_file(EICAR_SHA256) {
        Ok(Lookup::Found(stats)) => {
            println!("EICAR stats: {stats:?}");
            assert!(
                stats.malicious > 0,
                "EICAR should be detected by multiple engines"
            );
        }
        Ok(Lookup::NotFound) => panic!("VirusTotal returned 404 for EICAR"),
        Err(error) => panic!("VirusTotal lookup failed: {error}"),
    }
}
