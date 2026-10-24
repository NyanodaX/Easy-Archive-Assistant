# Supplemental third-party texts / 补充第三方许可

See SOURCES.json for immutable upstream commits, download URLs and SHA-256. These texts supplement notices omitted from published Cargo archives. No dependency source code is modified.

- webview2-rs / callback macros: upstream MIT texts at the exact Cargo VCS commits.
- alloc-stdlib: upstream BSD license at the Cargo VCS commit.
- selectors: MPL-2.0; exact unmodified source https://crates.io/api/v1/crates/selectors/0.38.0/download and https://github.com/servo/stylo/tree/572ecba2d1600e7c3d490586692a209faf703baa/selectors . Other MPL components have exact source links in generated DEPENDENCIES.json.
- WebView2Loader.dll 1.0.3800.47: original SDK LICENSE.txt and NOTICE.txt from Microsoft's matching NuGet package. These apply to the redistributed loader; the WebView2 browser runtime is a separate installed prerequisite.

Generated package notices also retain Rust runtime and LLVM/MinGW runtime notices when built with this workspace's private fallback toolchain.
