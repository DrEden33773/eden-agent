//! Publishes this build's target triple to the SDK as `EDEN_TARGET`.

fn main() {
    println!(
        "cargo:rustc-env=EDEN_TARGET={}",
        std::env::var("TARGET").expect("Cargo TARGET")
    );
}
