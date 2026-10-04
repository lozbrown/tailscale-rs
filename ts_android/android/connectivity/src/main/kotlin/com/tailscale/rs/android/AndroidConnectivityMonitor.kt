package com.tailscale.rs.android

import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.Handler
import android.os.HandlerThread
import org.json.JSONArray
import org.json.JSONObject
import java.io.Closeable
import java.util.concurrent.ConcurrentHashMap

/**
 * Feeds Android connectivity snapshots into a Rust [ts_android::AndroidNetmon].
 *
 * This class only observes the app's available networks. It neither requests
 * [android.net.VpnService] permission nor creates a TUN device or alters routes.
 * The caller must retain the Rust monitor for this object's whole lifetime.
 */
class AndroidConnectivityMonitor(
    context: Context,
    private val nativeHandle: Long,
) : Closeable {
    private val connectivity = context.applicationContext.getSystemService(ConnectivityManager::class.java)
    private val callbackThread = HandlerThread("tailscale-rs-netmon").apply { start() }
    private val callbackHandler = Handler(callbackThread.looper)
    private val states = ConcurrentHashMap<Long, State>()

    private data class State(
        var capabilities: NetworkCapabilities? = null,
        var linkProperties: LinkProperties? = null,
        var blocked: Boolean = false,
    )

    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) {
            states.getOrPut(network.networkHandle) { State() }.capabilities = capabilities
            publish(network)
        }

        override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) {
            states.getOrPut(network.networkHandle) { State() }.linkProperties = linkProperties
            publish(network)
        }

        override fun onBlockedStatusChanged(network: Network, blocked: Boolean) {
            states.getOrPut(network.networkHandle) { State() }.blocked = blocked
            publish(network)
        }

        override fun onLost(network: Network) {
            states.remove(network.networkHandle)
            nativeRemoveNetwork(nativeHandle, network.networkHandle)
        }
    }

    /** Start receiving callbacks. Calling this more than once is an error. */
    fun start() {
        val request = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .build()
        connectivity.registerNetworkCallback(request, callback, callbackHandler)
    }

    private fun publish(network: Network) {
        val state = states[network.networkHandle] ?: return
        val properties = state.linkProperties ?: return
        val capabilities = state.capabilities ?: return
        val snapshot = JSONObject().apply {
            put("interfaceName", properties.interfaceName ?: JSONObject.NULL)
            put("up", !state.blocked && capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET))
            put("mtu", properties.mtu.takeIf { it > 0 } ?: JSONObject.NULL)
            put("addresses", JSONArray().apply {
                properties.linkAddresses.forEach { address -> put(address.toString()) }
            })
            put("routes", JSONArray().apply {
                properties.routes.forEach { route ->
                    put(JSONObject().apply {
                        put("dst", route.destination.toString())
                        put("gateway", route.gateway?.hostAddress ?: JSONObject.NULL)
                    })
                }
            })
        }
        nativeReplaceSnapshot(nativeHandle, network.networkHandle, snapshot.toString())
    }

    override fun close() {
        try {
            connectivity.unregisterNetworkCallback(callback)
        } catch (_: IllegalArgumentException) {
            // The adapter was never started or was already closed.
        }
        states.keys.forEach { networkHandle -> nativeRemoveNetwork(nativeHandle, networkHandle) }
        states.clear()
        callbackThread.quitSafely()
    }

    companion object {
        /** Load the `cdylib` built from the `ts_android` Rust crate. */
        @JvmStatic
        fun loadNativeLibrary() = System.loadLibrary("ts_android")

        @JvmStatic
        private external fun nativeReplaceSnapshot(handle: Long, networkHandle: Long, snapshot: String): Boolean

        @JvmStatic
        private external fun nativeRemoveNetwork(handle: Long, networkHandle: Long)
    }
}
