fn main() {
    println!(
        "cargo:rustc-env=EDEN_TARGET={}",
        std::env::var("TARGET").expect("Cargo TARGET")
    );
}
