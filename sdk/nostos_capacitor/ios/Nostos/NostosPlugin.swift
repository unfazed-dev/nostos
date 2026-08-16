import Foundation
import Capacitor
import UIKit
import UserNotifications

/**
 * iOS side of the Nostos Capacitor plugin — push bridge only (beta, ADR-0037).
 *
 * The sync path runs inside the webview (the WASM engine + WebSocket), so this
 * class implements exactly one bridge method: `registerForPushNotifications`.
 * It registers the app with APNs (no permission prompt — silent pushes need
 * only registration; the visible-notification authorization, the push
 * entitlement, and the APNs key/team config are app-side — see the README)
 * and hands the resulting device token to JS as a `pushToken` event. The app
 * then calls `registerPushToken("apns", token)` on its Nostos web instance,
 * which POSTs it to the server with the same JWT the sync session uses.
 *
 * Foreground bridge: user-visible notifications that arrive while the app is
 * foregrounded are surfaced to JS as a `foregroundPush` event and NOT shown
 * by the OS (the app decides — typically it reconnects/re-reads instead,
 * because a foregrounded live socket has already applied the data).
 *
 * Capacitor 8 wiring: plugins self-describe via CAPBridgedPlugin (no .m macro
 * registration file), the APNs callbacks arrive as
 * .capacitorDidRegisterForRemoteNotifications notifications posted by the
 * app's AppDelegate (same wiring as @capacitor/push-notifications), and
 * foreground delivery routes through the bridge's notificationRouter.
 */
@objc(NostosPlugin)
public class NostosPlugin: CAPPlugin, CAPBridgedPlugin {
    public let identifier = "NostosPlugin"
    public let jsName = "Nostos"
    public let pluginMethods: [CAPPluginMethod] = [
        CAPPluginMethod(name: "registerForPushNotifications", returnType: CAPPluginReturnPromise)
    ]

    private let foregroundHandler = NostosForegroundPushHandler()

    override public func load() {
        // Register with Capacitor's notification router — the router owns the
        // UNUserNotificationCenter delegate, so plugins must not claim it
        // directly (same wiring as @capacitor/push-notifications).
        bridge?.notificationRouter.pushNotificationHandler = foregroundHandler
        foregroundHandler.plugin = self

        // The app's AppDelegate forwards the APNs registration results to
        // plugins via NotificationCenter (see README, "What stays app-side").
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(self.didRegisterForRemoteNotificationsWithDeviceToken(notification:)),
            name: .capacitorDidRegisterForRemoteNotifications,
            object: nil
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(self.didFailToRegisterForRemoteNotificationsWithError(notification:)),
            name: .capacitorDidFailToRegisterForRemoteNotifications,
            object: nil
        )
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
    }

    /// Register with APNs. Resolves immediately; the token arrives as a
    /// `pushToken` event (or `pushTokenError` on failure). Requires the
    /// app-side `aps-environment` entitlement.
    @objc func registerForPushNotifications(_ call: CAPPluginCall) {
        DispatchQueue.main.async {
            UIApplication.shared.registerForRemoteNotifications()
        }
        call.resolve()
    }

    @objc public func didRegisterForRemoteNotificationsWithDeviceToken(notification: NSNotification) {
        guard let deviceToken = notification.object as? Data else {
            notifyListeners("pushTokenError", data: ["message": "APNs registration returned no device token"])
            return
        }
        let token = deviceToken.map { String(format: "%02x", $0) }.joined()
        notifyListeners("pushToken", data: ["platform": "apns", "token": token])
    }

    @objc public func didFailToRegisterForRemoteNotificationsWithError(notification: NSNotification) {
        let message = (notification.object as? Error)?.localizedDescription
            ?? "APNs registration failed"
        notifyListeners("pushTokenError", data: ["message": message])
    }
}

/**
 * Foreground bridge (beta): a user-visible notification arrived while the app
 * was foregrounded. Hand the payload to JS and suppress the OS presentation —
 * the doorbell contract (ADR-0037 §2) says the app syncs in this state anyway.
 */
public class NostosForegroundPushHandler: NSObject, NotificationHandlerProtocol {
    weak var plugin: NostosPlugin?

    public func willPresent(notification: UNNotification) -> UNNotificationPresentationOptions {
        let payload = JSTypes.coerceDictionaryToJSObject(notification.request.content.userInfo) ?? [:]
        plugin?.notifyListeners("foregroundPush", data: ["payload": payload])
        return []
    }

    public func didReceive(response: UNNotificationResponse) {
        // ponytail: notification taps are not bridged — the beta push surface
        // has no tap event. Upgrade path: emit a `notificationTap` event if an
        // app ever needs one.
    }
}
