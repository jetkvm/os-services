# License texts for the notices

`cargo xtask package` writes `THIRD_PARTY_NOTICES.md` into each installer. It takes the license texts of a Rust crate from the files that the crate ships. `crates.json` adds texts for the crates that ship no license file, or no file for the license that their `Cargo.toml` states. Each key is a crate name, and each value lists files in this directory.

| File | Source |
| --- | --- |
| `clipboard-win-BSL-1.0.txt` | `LICENSE` of <https://github.com/DoumanAsh/clipboard-win> |
| `objc2-MIT.txt` | `LICENSE-MIT.txt` of <https://github.com/madsmtm/objc2>, which holds `objc2`, `block2`, `dispatch2` and the framework crates |
| `openssl-Apache-2.0.txt` | `LICENSE.txt` of <https://github.com/openssl/openssl>, which `openssl-sys` builds from source and links into the service on Linux |

The AppImage notices also list the AppImage runtime with these texts:

| File | Source |
| --- | --- |
| `appimage-runtime-MIT.txt` | `LICENSE` of <https://github.com/AppImage/type2-runtime>, which names the libraries that the runtime links statically |
| `musl-MIT.txt` | `COPYRIGHT` of musl, <https://git.musl-libc.org/cgit/musl/tree/COPYRIGHT> |
| `libfuse-LGPL-2.1.txt` | `LGPL2.txt` of <https://github.com/libfuse/libfuse> |
| `squashfuse-BSD-2-Clause.txt` | `LICENSE` of <https://github.com/vasi/squashfuse> |
| `zstd-BSD-3-Clause.txt` | `LICENSE` of <https://github.com/facebook/zstd> |
| `zlib.txt` | `LICENSE` of <https://github.com/madler/zlib> |
