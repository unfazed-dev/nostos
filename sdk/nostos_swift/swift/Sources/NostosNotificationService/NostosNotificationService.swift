// The Notification Service Extension half of nostos's push options (ADR-0047,
// docs/api/push.md). The server marks a push `mutable-content` and adds
// `nostos_image` / `nostos_sender` / `nostos_avatar`; iOS then runs the app's
// extension, and this is it:
//
//     import NostosNotificationService
//     final class NotificationService: NostosNotificationService {}
//
// Dependency-free — no Nostos core, no Firebase — because an extension gets a
// small memory budget and ~30 s, and should link nothing it does not use. A
// push without the keys passes through untouched.

import Foundation
import Intents
import UserNotifications

open class NostosNotificationService: UNNotificationServiceExtension {
    private let lock = NSLock()
    private var contentHandler: ((UNNotificationContent) -> Void)?
    private var bestAttempt: UNMutableNotificationContent?

    override open func didReceive(
        _ request: UNNotificationRequest,
        withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void
    ) {
        guard let content = request.content.mutableCopy() as? UNMutableNotificationContent else {
            return contentHandler(request.content)
        }
        lock.lock()
        self.contentHandler = contentHandler
        bestAttempt = content
        lock.unlock()
        let info = content.userInfo
        let image = (info["nostos_image"] as? String).flatMap(URL.init(string:))
        let sender = info["nostos_sender"] as? String
        let avatar = (info["nostos_avatar"] as? String).flatMap(URL.init(string:))
        Task {
            if let image, let attachment = await Self.attachment(from: image) {
                content.attachments = [attachment]
            }
            guard let sender else { return self.deliver(content) }
            self.deliver(await Self.communication(content, sender: sender, avatar: avatar))
        }
    }

    // Out of time: show what we have rather than let iOS show the bare push.
    override open func serviceExtensionTimeWillExpire() {
        lock.lock()
        let content = bestAttempt
        lock.unlock()
        if let content { deliver(content) }
    }

    /// Exactly once, whichever of the download and the deadline wins.
    private func deliver(_ content: UNNotificationContent) {
        lock.lock()
        let handler = contentHandler
        contentHandler = nil
        lock.unlock()
        handler?(content)
    }

    /// The image as an attachment. `UNNotificationAttachment` infers the type
    /// from the file extension, which the download's temp file lacks — so the
    /// file is moved to one carrying the URL's (the reason FCM asks for image
    /// URLs with an extension).
    static func attachment(from url: URL) async -> UNNotificationAttachment? {
        guard let (file, response) = try? await URLSession.shared.download(from: url),
              (response as? HTTPURLResponse)?.statusCode ?? 200 < 300
        else { return nil }
        let suggested = (response.suggestedFilename as NSString?)?.pathExtension ?? ""
        let ext = url.pathExtension.isEmpty ? suggested : url.pathExtension
        let dest = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString)
            .appendingPathExtension(ext)
        guard (try? FileManager.default.moveItem(at: file, to: dest)) != nil else { return nil }
        return try? UNNotificationAttachment(identifier: "nostos_image", url: dest)
    }

    /// A Communication Notification: the sender's name and picture over the
    /// app icon, the way messaging apps render. iOS applies it only when the
    /// host app has the Communication Notifications capability and lists
    /// `INSendMessageIntent` in `NSUserActivityTypes`; otherwise `updating`
    /// throws and the plain banner stands.
    static func communication(
        _ content: UNMutableNotificationContent,
        sender: String,
        avatar: URL?
    ) async -> UNNotificationContent {
        var image: INImage?
        if let avatar, let (data, _) = try? await URLSession.shared.data(from: avatar) {
            image = INImage(imageData: data)
        }
        let person = INPerson(
            personHandle: INPersonHandle(value: sender, type: .unknown),
            nameComponents: nil,
            displayName: sender,
            image: image,
            contactIdentifier: nil,
            customIdentifier: sender
        )
        let intent = INSendMessageIntent(
            recipients: nil,
            outgoingMessageType: .outgoingMessageText,
            content: content.body,
            speakableGroupName: nil,
            conversationIdentifier: content.threadIdentifier.isEmpty ? nil : content.threadIdentifier,
            serviceName: nil,
            sender: person,
            attachments: nil
        )
        #if !os(macOS) // macOS takes the image from the INPerson alone.
        intent.setImage(image, forParameterNamed: \.sender)
        #endif
        let interaction = INInteraction(intent: intent, response: nil)
        interaction.direction = .incoming
        try? await interaction.donate()
        return (try? content.updating(from: intent)) ?? content
    }
}
