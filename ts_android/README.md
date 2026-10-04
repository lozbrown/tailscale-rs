# ts_android

`ts_android` is the application-scoped Android connectivity adapter for
`tailscale-rs`. It uses `ConnectivityManager`; it does not use `VpnService`,
create a TUN device, or change Android routing. `AndroidDevice` dereferences
to the complete Rust `tailscale::Device` API.

The Rust application creates and retains the monitor, then passes its handle
to Kotlin:

```rust
let netmon = ts_android::AndroidNetmon::new();
let monitor_handle = netmon.handle();
// Pass monitor_handle to AndroidConnectivityMonitor, then call its start().
let device = ts_android::AndroidDevice::connect(config, netmon, auth_key).await?;
let stream = device.tcp_connect(remote_addr).await?;
```

The Kotlin module lives in `android/connectivity`. Build the Rust library into
an ABI-specific directory, then point Gradle 8.14.3 or later at it with
`rustJniLibsDir`:

```sh
cargo ndk -t arm64-v8a -o build/android-jni build -p ts_android --release
ANDROID_HOME=/path/to/sdk gradle -p ts_android/android \
  -PrustJniLibsDir="$PWD/build/android-jni" :connectivity:assembleRelease
```

The adapter requires Android API 24 or later and `ACCESS_NETWORK_STATE`.
