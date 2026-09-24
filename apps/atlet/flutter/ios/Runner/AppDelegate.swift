import Flutter
import UIKit
import UserNotifications

@main
@objc class AppDelegate: FlutterAppDelegate, FlutterImplicitEngineDelegate {
  override func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    // NOTE: do NOT assign UNUserNotificationCenter.current().delegate here.
    // Taking the delegate starves FlutterFire of foreground deliveries —
    // onMessage never fires for visible pushes (order-leg smoke). Foreground
    // banners go through the atlet/notify local-notification path below;
    // backgrounded banners are OS-default.
    //
    // Action categories (ADR-0037 §2 `action` mode): category registration
    // is registry-level — no delegate takeover needed. The server's
    // `aps.category` renders these buttons on lock screen / Notification
    // Center with the app killed. Category id is the operator contract
    // (NOSTOS_PUSH_TABLES `table:action:<category>:…`).
    let track = UNNotificationAction(
      identifier: "track_order", title: "Track order", options: [.foreground]
    )
    let markReceived = UNNotificationAction(
      identifier: "mark_received", title: "Mark received", options: [.foreground]
    )
    UNUserNotificationCenter.current().setNotificationCategories([
      UNNotificationCategory(
        identifier: "order_status",
        actions: [track, markReceived],
        intentIdentifiers: []
      )
    ])
    // Authorization is a prerequisite for EVERY banner, not just the FCM ones:
    // without it iOS drops the atlet/notify local notification silently and
    // even `xcrun simctl push` refuses with "Source is not authorized"
    // (caught 2026-09-23 on the simulator). The push pilot asks for the same
    // grant via FirebaseMessaging.requestPermission(), but it is opt-in, so
    // the order banner — which needs no FCM at all — would be invisible in
    // every default build. Repeat calls are free once the user has answered.
    UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }
    return super.application(application, didFinishLaunchingWithOptions: launchOptions)
  }

  func didInitializeImplicitFlutterEngine(_ engineBridge: FlutterImplicitEngineBridge) {
    GeneratedPluginRegistrant.register(with: engineBridge.pluginRegistry)
    // iOS twin of MainActivity's atlet/notify handler: the Dart order-banner
    // posts a local notification (same UX as the FCM push) on live sync
    // changes while the app is open.
    let channel = FlutterMethodChannel(
      name: "atlet/notify", binaryMessenger: engineBridge.applicationRegistrar.messenger()
    )
    notifyChannel = channel
    channel.setMethodCallHandler { [weak self] call, result in
      // Dart asks for this once, at boot: a notification tapped from a
      // TERMINATED app is delivered by the OS before Dart has a handler, so
      // the tap is buffered here and drained by the ask. The ask is also what
      // marks Dart ready — before it, invokeMethod has nobody listening.
      if call.method == "take_pending_tap" {
        self?.dartReady = true
        let tap = self?.pendingTap
        self?.pendingTap = nil
        result(tap)
        return
      }
      guard call.method == "order_update", let body = call.arguments as? [String: Any], let text = body["body"] as? String else {
        result(FlutterMethodNotImplemented)
        return
      }
      let content = UNMutableNotificationContent()
      content.title = body["title"] as? String ?? "Atlet order update"
      content.body = text
      content.sound = .default
      // The routing keys ride in userInfo, which is what comes back on a tap
      // (didReceive below) — the same place a real APNs push carries them.
      if let data = body["data"] as? [String: Any] {
        content.userInfo = data
      }
      // Renders the order_status action buttons registered above, so the local
      // banner and the server's push look the same.
      if let category = body["category"] as? String {
        content.categoryIdentifier = category
      }
      // A fresh id per post. Reusing the body as the id made a repeat of the
      // same text UPDATE the notification already sitting in Notification
      // Center instead of presenting a new banner — silent, and indexed by
      // the one string guaranteed to repeat across a status flip-flop.
      let request = UNNotificationRequest(
        identifier: UUID().uuidString, content: content, trigger: nil)
      UNUserNotificationCenter.current().add(request) { error in
        if let error = error {
          result(FlutterError(code: "NOTIFY", message: error.localizedDescription, details: nil))
        } else {
          result(nil)
        }
      }
    }
    // A local notification posted while the app is in front is suppressed
    // unless a delegate says otherwise — which is why the order banner fired
    // and nothing appeared. Claiming it only when nobody else had it was not
    // enough: FlutterFire's messaging plugin takes the delegate during
    // GeneratedPluginRegistrant above whether or not the push pilot is on, so
    // the guard never passed and the banner stayed invisible (caught
    // 2026-09-23 — Dart logged the post, the OS showed nothing). Take it, and
    // forward, which is what the note in didFinishLaunching was actually
    // protecting: FlutterFire keeps every delivery it would have had.
    let center = UNUserNotificationCenter.current()
    if let existing = center.delegate, !(existing is ForegroundBanner) {
      foregroundBanner.next = existing
    }
    center.delegate = foregroundBanner
    foregroundBanner.onTap = { [weak self] info in self?.deliverTap(info) }
  }

  /// `atlet://history/<id>` — the external half of the deep link (see
  /// deepLinkFor in lib/push/order_push.dart). Registered in Info.plist;
  /// `xcrun simctl openurl booted "atlet://history/<id>"` is the smoke test.
  override func application(
    _ app: UIApplication, open url: URL,
    options: [UIApplication.OpenURLOptionsKey: Any] = [:]
  ) -> Bool {
    // `atlet://history/<id>` parses `history` as the host and `/<id>` as the
    // path; the one-slash form a human types puts it all in the path. Accept
    // both.
    let route = url.host == "history" ? "/history\(url.path)" : url.path
    if url.scheme == "atlet", route.hasPrefix("/history/") {
      deliverTap(["nostos_route": route])
      return true
    }
    return super.application(app, open: url, options: options)
  }

  /// Hands one tap to Dart, or holds it until Dart says it is listening.
  fileprivate func deliverTap(_ info: [String: Any]) {
    guard dartReady, let notifyChannel else {
      pendingTap = info
      return
    }
    notifyChannel.invokeMethod("notification_tap", arguments: info)
  }

  private let foregroundBanner = ForegroundBanner()
  private var notifyChannel: FlutterMethodChannel?
  private var dartReady = false
  private var pendingTap: [String: Any]?
}

/// Presents banners the app posts to itself, and passes everything on to
/// whoever held the delegate first.
///
/// Exists only so the order-status update is visible without backgrounding the
/// app — the OS presents it for free once the app is not in front. `next` is
/// weak: it is a Flutter plugin the registrar already owns, or the app
/// delegate that owns this.
private final class ForegroundBanner: NSObject, UNUserNotificationCenterDelegate {
  weak var next: UNUserNotificationCenterDelegate?
  var onTap: (([String: Any]) -> Void)?

  func userNotificationCenter(
    _ center: UNUserNotificationCenter,
    willPresent notification: UNNotification,
    withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
  ) {
    guard let next, next.responds(to: #selector(UNUserNotificationCenterDelegate.userNotificationCenter(_:willPresent:withCompletionHandler:))) else {
      completionHandler([.banner, .sound])
      return
    }
    // Union, not override: FlutterFire may want the delivery recorded even
    // when it asks for no presentation, and this app always wants the banner.
    next.userNotificationCenter?(center, willPresent: notification) { options in
      completionHandler(options.union([.banner, .sound]))
    }
  }

  func userNotificationCenter(
    _ center: UNUserNotificationCenter,
    didReceive response: UNNotificationResponse,
    withCompletionHandler completionHandler: @escaping () -> Void
  ) {
    // The tap, forwarded to Dart, which owns the routing (one deep-link
    // destination for local banners, real pushes and URLs alike). Only our own
    // keys are passed on: the rest of userInfo is somebody else's payload and
    // need not survive the standard codec.
    let info = response.notification.request.content.userInfo
    var tap: [String: Any] = [:]
    for key in ["event_id", "nostos_route", "deep_link", "order_id", "status"] {
      if let value = info[key] as? String { tap[key] = value }
    }
    if !tap.isEmpty { onTap?(tap) }
    guard let next, next.responds(to: #selector(UNUserNotificationCenterDelegate.userNotificationCenter(_:didReceive:withCompletionHandler:))) else {
      completionHandler()
      return
    }
    next.userNotificationCenter?(center, didReceive: response, withCompletionHandler: completionHandler)
  }
}
