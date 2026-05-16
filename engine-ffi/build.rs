// Set the macOS install_name to @rpath/libengine_ffi.dylib so that any
// consumer linking against this dylib captures an @rpath-relative reference.
// Without this, cargo bakes in the absolute path to target/<profile>/, which
// makes the dylib non-relocatable when bundled into a macOS .app.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libengine_ffi.dylib");
    }
}
