fn main() {
    #[cfg(feature = "node-bindings")]
    napi_build::setup();
}
