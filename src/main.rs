fn main() {
    if let Err(error) = crux::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
