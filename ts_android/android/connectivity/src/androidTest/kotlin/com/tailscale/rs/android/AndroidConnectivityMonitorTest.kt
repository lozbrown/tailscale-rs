package com.tailscale.rs.android

import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class AndroidConnectivityMonitorTest {
    @Test
    fun loadsNativeLibraryAndClosesMonitor() {
        AndroidConnectivityMonitor.loadNativeLibrary()
        AndroidConnectivityMonitor(ApplicationProvider.getApplicationContext(), 0).use { monitor ->
            monitor.start()
        }
    }
}
