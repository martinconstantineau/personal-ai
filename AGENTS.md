# Agent notes — this host's toolchain

Everything needed to build lives under `C:\Users\Marti\src\` (paths are
this machine's; nothing here is committed config — just a map).

| Tool | Location | Used for |
|---|---|---|
| Flutter 3.47.4 | `src\flutter\bin` | `apps/desktop` builds (Android + Windows) |
| Android SDK | `src\android-sdk` | platform 36, build-tools 36, platform-tools; `ANDROID_SDK_ROOT` |
| Android NDK | `src\android-sdk\ndk\29.0.14206865` | Rust FFI cross-compile (`ANDROID_NDK_HOME`) |
| cargo-ndk 4.1.2 | `~/.cargo/bin` | `scripts/build_android_ffi.sh` |
| Prebuilt OpenSSL 3.6.3 | `src\ossl-out\{arm64,armv7,x86_64}` | `OPENSSL_OUT` for Android FFI builds |
| OpenSSL src per ABI | `src\ossl-src-*` | MSYS2 build trees (regenerable) |
| MSYS2 | `src\msys64` (`usr\bin\bash.exe`) | OpenSSL Configure/make — only env where perl+make+sh agree |
| MinGW MSVCRT (winlibs) | `src\mgw-msvcrt\mingw64\bin` | host C compiles + links for `x86_64-pc-windows-gnu` |
| w64devkit | `src\w64devkit` | busybox sh/make/zstd utilities |
| Strawberry Perl | `src\strawberry` | module grafts for Git's stripped perl |

## What bites (already solved — don't re-debug)

- **Host links**: put `mgw-msvcrt\mingw64\bin` + rust's
  `lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained` on PATH. Copy
  `libgcc_eh.a` from rust's self-contained lib into the winlibs gcc dir;
  do NOT pass rust's self-contained `-L` (its `libmsvcrt.a` is a stub —
  UCRT/MSVCRT mismatch otherwise).
- **Vendored `openssl-src`** cannot cross-compile on this host — build
  OpenSSL manually in MSYS2 per `android-*` target (`-D__ANDROID_API__=26`,
  `no-shared`), then `OPENSSL_NO_VENDOR=1 OPENSSL_STATIC=1` +
  `<TRIPLE>_OPENSSL_LIB_DIR/INCLUDE_DIR`.
- **`-laaudio`** → build at `--platform 26` (minSdk 26 in the app gradle).
- **`path_provider`** is intentionally absent from `pubspec.yaml` —
  plugin resolution needs Developer Mode (symlinks), which this host
  lacks. Keep deps plugin-free or enable Developer Mode first.
- **cargo-ndk flag order**: `-p` means *package*; the API level flag is
  `--platform`.

## Build commands

```bash
export PATH="/c/Users/Marti/src/flutter/bin:$PATH"
export ANDROID_SDK_ROOT="C:/Users/Marti/src/android-sdk"
scripts/build_android_ffi.sh            # needs OPENSSL_OUT=/c/Users/Marti/src/ossl-out
cd apps/desktop && flutter build apk --release     # or appbundle
flutter build windows --release         # then copy target/release/pai_ffi.dll next to pai_app.exe
```

## Git

Commit with `-c user.name="Devin" -c user.email="devin@cognition.ai"`.
Push to `origin` (GitLab) and `github` (mirror) — both are kept in sync.
