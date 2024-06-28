fn main() {
    #[cfg(all(feature = "windivert", windows))]
    {
        let dir = env::var("CARGO_MANIFEST_DIR").unwrap();
        println!(
            "cargo:rustc-link-search=native={}",
            Path::new(&dir).join("windows-lib").display()
        );
        println!("cargo:rustc-link-lib=WinDivert");
    }
}
