// triage-notify: macOS notification helper using UNUserNotificationCenter
// (the modern, supported API). Replaces terminal-notifier whose `-execute`
// click handler is silently broken on macOS 14+ (the underlying
// NSUserNotificationCenter API was deprecated and click delegates stopped
// firing reliably).
//
// Args:
//   --title <text>           required
//   --subtitle <text>        optional
//   --message <text>         optional (notification body)
//   --action <shell command> optional; runs via `/bin/sh -c <cmd>` on click
//   --timeout <seconds>      optional; default 60
//
// Bundle context: this binary MUST be invoked from inside a .app bundle
// (Contents/MacOS/triage-notify) for UNUserNotificationCenter to find a
// bundle identifier and grant permissions. See ../scripts/triage-notify/
// build.sh which assembles the .app.

import Foundation
import UserNotifications

final class Args {
    var title: String?
    var subtitle: String?
    var message: String?
    var action: String?
    var timeout: TimeInterval = 60
}

func parseArgs() -> Args {
    let result = Args()
    let argv = CommandLine.arguments
    var i = 1
    while i < argv.count {
        let flag = argv[i]
        let next: String? = (i + 1 < argv.count) ? argv[i + 1] : nil
        switch flag {
        case "--title":    result.title = next;    i += 2
        case "--subtitle": result.subtitle = next; i += 2
        case "--message":  result.message = next;  i += 2
        case "--action":   result.action = next;   i += 2
        case "--timeout":
            if let n = next, let t = TimeInterval(n) { result.timeout = t }
            i += 2
        default: i += 1
        }
    }
    return result
}

final class Delegate: NSObject, UNUserNotificationCenterDelegate {
    var actionCommand: String?
    var done = false

    // Show banners + sound even when the foreground app is the helper itself
    // (which it briefly is, before macOS hands focus back).
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        if #available(macOS 11.0, *) {
            completionHandler([.banner, .sound])
        } else {
            completionHandler([.alert, .sound])
        }
    }

    // Click / action-button callback. Default action = clicking the banner
    // body. Our explicit "open" action button is also routed here.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let id = response.actionIdentifier
        let isClick = id == UNNotificationDefaultActionIdentifier || id == "open"
        if isClick, let cmd = actionCommand {
            let task = Process()
            task.launchPath = "/bin/sh"
            task.arguments = ["-c", cmd]
            do {
                try task.run()
                task.waitUntilExit()
            } catch {
                FileHandle.standardError.write(Data("triage-notify: failed to run action: \(error)\n".utf8))
            }
        }
        done = true
        completionHandler()
    }
}

let args = parseArgs()
guard let title = args.title else {
    FileHandle.standardError.write(Data("triage-notify: --title is required\n".utf8))
    exit(2)
}

let delegate = Delegate()
delegate.actionCommand = args.action

let center = UNUserNotificationCenter.current()
center.delegate = delegate

let group = DispatchGroup()
group.enter()

center.requestAuthorization(options: [.alert, .sound]) { granted, err in
    defer { group.leave() }
    if !granted {
        FileHandle.standardError.write(Data("triage-notify: notification permission denied\n".utf8))
    }
    if let err = err {
        FileHandle.standardError.write(Data("triage-notify: auth error: \(err.localizedDescription)\n".utf8))
    }
}
group.wait()

let content = UNMutableNotificationContent()
content.title = title
if let s = args.subtitle { content.subtitle = s }
if let m = args.message { content.body = m }

if args.action != nil {
    let openAction = UNNotificationAction(
        identifier: "open",
        title: "Open",
        options: [.foreground]
    )
    let category = UNNotificationCategory(
        identifier: "triage.click",
        actions: [openAction],
        intentIdentifiers: [],
        options: []
    )
    center.setNotificationCategories([category])
    content.categoryIdentifier = "triage.click"
}

let request = UNNotificationRequest(
    identifier: UUID().uuidString,
    content: content,
    trigger: nil
)

let postGroup = DispatchGroup()
postGroup.enter()
center.add(request) { err in
    if let err = err {
        FileHandle.standardError.write(Data("triage-notify: post error: \(err.localizedDescription)\n".utf8))
    }
    postGroup.leave()
}
postGroup.wait()

// Block until the user interacts (click or dismiss) or the timeout expires,
// so the action command has a chance to fire. Without the run-loop pump the
// process would exit immediately and the delegate callback would never run.
let deadline = Date().addingTimeInterval(args.timeout)
while !delegate.done && Date() < deadline {
    RunLoop.current.run(mode: .default, before: Date(timeIntervalSinceNow: 0.5))
}
exit(0)
