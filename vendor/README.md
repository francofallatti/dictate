# vendor/

`whisper-rs-sys` is vendored from crates.io (0.15.0) with one change in `build.rs`: when
`WHISPER_DONT_GENERATE_BINDINGS` is set, the pregenerated (Linux) `bindings.rs` is copied
with its glibc layout asserts removed so it compiles on Windows/MSVC without libclang.
Everything else (whisper.cpp sources, CMake build) is upstream.
