fn main() {
    if let Err(err) = distrun::run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
