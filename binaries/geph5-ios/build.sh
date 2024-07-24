#!/bin/zsh

if [ $CONFIGURATION = "Release" ]; then
    cargo build --target aarch64-apple-ios --release
else
    cargo build --target aarch64-apple-ios
fi
