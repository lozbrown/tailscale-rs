//! Android connectivity monitoring for `tailscale-rs` embeddings.
//!
//! [`AndroidNetmon`] receives immutable snapshots collected by an Android
//! `ConnectivityManager.NetworkCallback`. It turns snapshot changes into the
//! portable [`ts_netmon::Netmon`] event stream used by the runtime. It does
//! not create a VPN, TUN device, or change system routes.

use std::{
    collections::HashMap,
    io,
    net::IpAddr,
    sync::{
        Arc, LazyLock, Mutex, MutexGuard, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use futures_util::StreamExt;
use ipnet::IpNet;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use ts_netmon::{BoxStream, Event, Interface, InterfaceId, MonType, Netmon, Route};

/// An Android-configured [`tailscale::Device`].
///
/// This owns the Android connectivity monitor for the device's entire lifetime
/// and dereferences to the complete portable [`tailscale::Device`] API.
pub struct AndroidDevice {
    device: tailscale::Device,
    monitor: Arc<AndroidNetmon>,
}

impl AndroidDevice {
    /// Connect a device using an already-active Android connectivity monitor.
    ///
    /// Create the monitor with [`AndroidNetmon::new`], pass its handle to
    /// `AndroidConnectivityMonitor` in Kotlin, and call `start()` on that
    /// Kotlin object before awaiting this method.
    pub async fn connect(
        mut config: tailscale::Config,
        monitor: Arc<AndroidNetmon>,
        auth_key: Option<String>,
    ) -> Result<Self, tailscale::Error> {
        config.netmon = Some(monitor.clone());
        let device = tailscale::Device::new(&config, auth_key).await?;
        Ok(Self { device, monitor })
    }

    /// Return the handle Kotlin passes to `AndroidConnectivityMonitor`.
    pub fn monitor_handle(&self) -> u64 {
        self.monitor.handle()
    }

    /// Shut down the device while retaining its monitor until shutdown ends.
    pub async fn shutdown(self, timeout: Option<std::time::Duration>) -> bool {
        self.device.shutdown(timeout).await
    }
}

impl std::ops::Deref for AndroidDevice {
    type Target = tailscale::Device;

    fn deref(&self) -> &Self::Target {
        &self.device
    }
}

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
static MONITORS: LazyLock<Mutex<HashMap<u64, Weak<Inner>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// A monitor fed from Android `ConnectivityManager` callbacks.
pub struct AndroidNetmon {
    handle: u64,
    inner: Arc<Inner>,
}

struct Inner {
    snapshots: Mutex<HashMap<u64, Snapshot>>,
    events: broadcast::Sender<Event>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    interface_name: Option<String>,
    up: bool,
    mtu: Option<usize>,
    #[serde(default)]
    addresses: Vec<IpNet>,
    #[serde(default)]
    routes: Vec<SnapshotRoute>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct SnapshotRoute {
    dst: IpNet,
    gateway: Option<IpAddr>,
}

impl AndroidNetmon {
    /// Create a monitor and return it in an [`Arc`] suitable for an
    /// application's `tailscale::Config::netmon` field.
    pub fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            snapshots: Mutex::new(HashMap::new()),
            events,
        });
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        lock(&MONITORS).insert(handle, Arc::downgrade(&inner));

        Arc::new(Self { handle, inner })
    }

    /// Opaque handle passed to `AndroidConnectivityMonitor` in Kotlin.
    pub const fn handle(&self) -> u64 {
        self.handle
    }

    /// Replace the platform snapshot for one Android `Network`.
    ///
    /// `network_handle` must be `Network.getNetworkHandle()`. The snapshot is
    /// JSON so the JNI boundary stays small and stable.
    pub fn replace_snapshot(&self, network_handle: u64, snapshot_json: &str) -> Result<(), Error> {
        self.inner.replace_snapshot(network_handle, snapshot_json)
    }

    /// Remove all state for an Android `Network` after `onLost`.
    pub fn remove_network(&self, network_handle: u64) {
        self.inner.remove_network(network_handle);
    }
}

impl Drop for AndroidNetmon {
    fn drop(&mut self) {
        lock(&MONITORS).remove(&self.handle);
    }
}

impl Netmon for AndroidNetmon {
    fn ty(&self) -> MonType {
        MonType::ANDROID_CONNECTIVITY
    }

    fn event_stream(&self) -> io::Result<BoxStream<io::Result<Event>>> {
        Ok(Box::pin(
            BroadcastStream::new(self.inner.events.subscribe()).map(|event| match event {
                Ok(event) => Ok(event),
                Err(error) => Err(io::Error::other(error)),
            }),
        ))
    }

    fn strong_delete_consistency(&self) -> bool {
        false
    }
}

impl Inner {
    fn replace_snapshot(&self, network_handle: u64, snapshot_json: &str) -> Result<(), Error> {
        let snapshot: Snapshot = serde_json::from_str(snapshot_json).map_err(Error::Snapshot)?;
        let mut snapshots = lock(&self.snapshots);
        let old = snapshots.insert(network_handle, snapshot.clone());
        self.publish_diff(network_handle, old.as_ref(), Some(&snapshot));
        Ok(())
    }

    fn remove_network(&self, network_handle: u64) {
        let mut snapshots = lock(&self.snapshots);
        if let Some(old) = snapshots.remove(&network_handle) {
            self.publish_diff(network_handle, Some(&old), None);
        }
    }

    fn publish_diff(&self, network_handle: u64, old: Option<&Snapshot>, new: Option<&Snapshot>) {
        let id = InterfaceId::new(MonType::ANDROID_CONNECTIVITY, network_handle);
        let had_old = old.is_some();
        let old = old.cloned().unwrap_or_else(|| Snapshot {
            interface_name: None,
            up: false,
            mtu: None,
            addresses: Vec::new(),
            routes: Vec::new(),
        });

        for route in old
            .routes
            .iter()
            .filter(|route| !new.is_some_and(|new| new.routes.contains(route)))
        {
            self.publish(Event::RouteRemoved(id.clone(), route.to_route()));
        }
        for address in old
            .addresses
            .iter()
            .filter(|address| !new.is_some_and(|new| new.addresses.contains(address)))
        {
            self.publish(Event::AddrRemoved(id.clone(), *address));
        }

        match new {
            Some(new) => {
                let interface = Interface {
                    id: id.clone(),
                    up: new.up,
                    name: new
                        .interface_name
                        .clone()
                        .unwrap_or_else(|| format!("network-{network_handle}")),
                    mtu: new.mtu,
                    hardware_addr: None,
                    metric_v4: 0,
                    metric_v6: 0,
                };
                if !had_old
                    || old.interface_name != new.interface_name
                    || old.up != new.up
                    || old.mtu != new.mtu
                {
                    self.publish(Event::InterfaceUpsert(interface));
                }
                for address in new
                    .addresses
                    .iter()
                    .filter(|address| !old.addresses.contains(address))
                {
                    self.publish(Event::AddrUpsert(id.clone(), *address));
                }
                for route in new
                    .routes
                    .iter()
                    .filter(|route| !old.routes.contains(route))
                {
                    self.publish(Event::RouteUpsert(id.clone(), route.to_route()));
                }
            }
            None => self.publish(Event::InterfaceRemoved(id)),
        }
    }

    fn publish(&self, event: Event) {
        drop(self.events.send(event));
    }
}

impl SnapshotRoute {
    fn to_route(&self) -> Route {
        Route {
            dst: self.dst,
            gateway: self.gateway.into_iter().collect(),
            metric: 0,
        }
    }
}

/// Errors from Kotlin snapshot submission.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The Kotlin snapshot did not match the documented JSON shape.
    #[error("invalid Android network snapshot")]
    Snapshot(#[source] serde_json::Error),
}

#[cfg(target_os = "android")]
fn monitor(handle: u64) -> Option<Arc<Inner>> {
    lock(&MONITORS).get(&handle).and_then(Weak::upgrade)
}

#[cfg(target_os = "android")]
mod jni {
    use jni::{
        EnvUnowned,
        errors::LogErrorAndDefault,
        objects::{JClass, JString},
        sys::{jboolean, jlong},
    };

    use super::monitor;

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tailscale_rs_android_AndroidConnectivityMonitor_nativeReplaceSnapshot(
        mut env: EnvUnowned,
        _class: JClass,
        handle: jlong,
        network_handle: jlong,
        snapshot: JString,
    ) -> jboolean {
        let Some(monitor) = monitor(handle as u64) else {
            return false;
        };
        env.with_env(|env| {
            let snapshot = snapshot.try_to_string(env)?;
            Ok::<_, jni::errors::Error>(
                monitor
                    .replace_snapshot(network_handle as u64, &snapshot)
                    .is_ok(),
            )
        })
        .resolve::<LogErrorAndDefault>()
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tailscale_rs_android_AndroidConnectivityMonitor_nativeRemoveNetwork(
        _env: EnvUnowned,
        _class: JClass,
        handle: jlong,
        network_handle: jlong,
    ) {
        if let Some(monitor) = monitor(handle as u64) {
            monitor.remove_network(network_handle as u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::StreamExt;

    use super::*;

    const SNAPSHOT: &str = r#"{
        "interfaceName":"wlan0",
        "up":true,
        "mtu":1500,
        "addresses":["192.0.2.10/24"],
        "routes":[{"dst":"0.0.0.0/0","gateway":"192.0.2.1"}]
    }"#;

    #[tokio::test]
    async fn snapshot_diff_emits_additions_and_removals() {
        let monitor = AndroidNetmon::new();
        let mut events = monitor.event_stream().unwrap();

        monitor.replace_snapshot(42, SNAPSHOT).unwrap();
        let mut additions = Vec::new();
        for _ in 0..3 {
            additions.push(
                tokio::time::timeout(Duration::from_secs(1), events.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(
            additions
                .iter()
                .any(|event| matches!(event, Event::InterfaceUpsert(_)))
        );
        assert!(
            additions
                .iter()
                .any(|event| matches!(event, Event::AddrUpsert(_, _)))
        );
        assert!(
            additions
                .iter()
                .any(|event| matches!(event, Event::RouteUpsert(_, _)))
        );

        monitor.remove_network(42);
        let mut removals = Vec::new();
        for _ in 0..3 {
            removals.push(
                tokio::time::timeout(Duration::from_secs(1), events.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(
            removals
                .iter()
                .any(|event| matches!(event, Event::InterfaceRemoved(_)))
        );
        assert!(
            removals
                .iter()
                .any(|event| matches!(event, Event::AddrRemoved(_, _)))
        );
        assert!(
            removals
                .iter()
                .any(|event| matches!(event, Event::RouteRemoved(_, _)))
        );
    }

    #[tokio::test]
    async fn first_snapshot_always_emits_an_interface() {
        let monitor = AndroidNetmon::new();
        let mut events = monitor.event_stream().unwrap();
        monitor
            .replace_snapshot(
                7,
                r#"{"interfaceName":null,"up":false,"mtu":null,"addresses":[],"routes":[]}"#,
            )
            .unwrap();

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Event::InterfaceUpsert(_)
        ));
    }
}
