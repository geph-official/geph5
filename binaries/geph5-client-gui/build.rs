extern crate winresource;

use swift_rs::SwiftLinker;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
         let mut res = winresource::WindowsResource::new();
        res.set_icon("icon.ico");
        res.compile().unwrap();
    }

    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "ios" {
        SwiftLinker::new("10.13")
            // Only if you are also targetting iOS
            // Ensure the same minimum supported iOS version is specified as in your `Package.swift` file
            .with_ios("11")
            .with_package("SwiftLib", "../geph5-ios/SwiftLib")
            .link();

    }
}
