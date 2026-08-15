// Copyright (c) Nostos contributors
// SPDX-License-Identifier: Apache-2.0

package run.nostos.capacitor

import com.getcapacitor.JSObject
import com.getcapacitor.Plugin
import com.getcapacitor.PluginCall
import com.getcapacitor.PluginMethod
import com.getcapacitor.annotation.CapacitorPlugin

/**
 * Android side of the Nostos Capacitor plugin — push bridge only (beta,
 * ADR-0037).
 *
 * The sync path runs inside the webview (WASM engine + WebSocket), so this
 * class implements no sync methods. Obtaining an FCM token requires the
 * Firebase SDK, which this plugin deliberately does not vendor: the app's own
 * FirebaseMessagingService owns that dependency, and either forwards a
 * foreground message via [emitForegroundPush] or calls
 * `registerPushToken("fcm", token)` on its Nostos web instance from JS once
 * Firebase hands it a token. See the README push section.
 */
@CapacitorPlugin(name = "Nostos")
class NostosPlugin : Plugin() {

    init {
        instance = this
    }

    override fun handleOnDestroy() {
        if (instance === this) {
            instance = null
        }
    }

    companion object {
        @Volatile
        private var instance: NostosPlugin? = null

        /**
         * App-side forwarder (foreground bridge): call from your
         * FirebaseMessagingService's `onMessageReceived` to surface a
         * foreground FCM message to JS as a `foregroundPush` event
         * (payload = the message's data map). Firebase stays app-side —
         * the plugin does not depend on it.
         */
        fun emitForegroundPush(data: Map<String, String>) {
            val plugin = instance ?: return
            val payload = JSObject()
            data.forEach { (k, v) -> payload.put(k, v) }
            val event = JSObject()
            event.put("payload", payload)
            plugin.notifyListeners("foregroundPush", event)
        }
    }

    /**
     * ponytail: unimplemented on Android — obtaining an FCM token requires
     * the Firebase SDK, which stays app-side (do-not-vendor rule). Upgrade
     * path: implement via FirebaseMessaging.getToken() if this project ever
     * accepts that dependency; today the app forwards the token itself and
     * calls registerPushToken("fcm", token) on its Nostos web instance.
     */
    @PluginMethod
    fun registerForPushNotifications(call: PluginCall) {
        call.unimplemented(
            "FCM token acquisition is app-side on Android — forward the token from your " +
                "FirebaseMessagingService and call registerPushToken(\"fcm\", token)"
        )
    }
}
