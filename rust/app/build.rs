fn main() {
    println!("cargo:rustc-check-cfg=cfg(chatgpt_web_replay)");
    println!(
        "cargo:rustc-env=NINEROUTER_BUILD_TARGET={}",
        std::env::var("TARGET").expect("Cargo target triple")
    );
}
