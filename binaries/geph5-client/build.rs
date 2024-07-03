fn main() {
    #[cfg(all(feature = "windivert", windows))]
    {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        println!(
            "cargo:rustc-link-search=native={}",
            std::path::Path::new(&dir).join("windows-lib").display()
        );
        println!("cargo:rustc-link-lib=WinDivert");
    }
}
