#![windows_subsystem = "windows"]

fn main() {
    if vtdrop::run().is_err() {
        std::process::exit(1);
    }
}
