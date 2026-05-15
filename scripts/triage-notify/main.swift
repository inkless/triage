// triage-notify: macOS notification helper using UNUserNotificationCenter.
//
// NSApplication-based. The earlier CommandLine version (no NSApp / RunLoop)
// triggered macOS's "<app> is not responding" dialog whenever 2+ helper
// instances were alive — the bundle had no Apple Event handler, so the
// `kAEOpenApplication` event the OS sends to coordinate launches timed out.
// NSApplication.run() handles those events automatically, so the OS sees
// a responsive app regardless of how many instances stack.
//
// Two modes — same binary, distinguished by whether `--title` was passed:
//
//   POST MODE  (`--title <text> ...`):
//     Called by triage to schedule a notification. Posts via
//     UNUserNotificationCenter, then RunLoops until --timeout. Clicks
//     arriving during the window are handled by THIS instance's delegate.
//
//   RESPONSE MODE  (no args):
//     macOS launches the bundle when the user clicks a queued notification
//     whose original poster has already exited. We register the delegate,
//     RunLoop for the timeout window, and run actions out of the
//     UNNotificationResponse's userInfo (`actionCommand`).
//
// In both modes, didReceive deliberately does NOT terminate the app — the
// same delegate stays alive to absorb subsequent clicks on sibling stacked
// notifications. The DispatchQueue.main.asyncAfter terminate is the only
// exit path.

import Cocoa
import UserNotifications

let debugEnabled = ProcessInfo.processInfo.environment["TRIAGE_NOTIFY_DEBUG"] == "1"

func dbg(_ msg: String) {
    guard debugEnabled else { return }
    let line = "[\(Date())] \(msg)\n"
    guard let data = line.data(using: .utf8) else { return }
    let url = URL(fileURLWithPath: "/tmp/triage-notify.log")
    if FileManager.default.fileExists(atPath: url.path) {
        if let h = try? FileHandle(forWritingTo: url) {
            h.seekToEndOfFile()
            h.write(data)
            try? h.close()
        }
    } else {
        try? data.write(to: url)
    }
}

final class Args {
    var title: String?
    var subtitle: String?
    var message: String?
    var action: String?
    var timeout: TimeInterval = 30
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

final class AppDelegate: NSObject, NSApplicationDelegate, UNUserNotificationCenterDelegate {
    let args: Args

    init(_ args: Args) {
        self.args = args
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        dbg("appDidFinishLaunching pid=\(getpid()) bundleId=\(Bundle.main.bundleIdentifier ?? "nil") hasTitle=\(args.title != nil)")
        let center = UNUserNotificationCenter.current()
        center.delegate = self

        if let title = args.title {
            postNotification(title: title)
        } else {
            dbg("response-mode: awaiting didReceive")
        }

        // Single shared terminate path: schedule on the main RunLoop and let
        // it fire once the timeout elapses. Click handlers don't terminate;
        // they just let the timer run out, which lets one helper absorb N
        // clicks on stacked notifications.
        DispatchQueue.main.asyncAfter(deadline: .now() + args.timeout) {
            dbg("timeout reached; terminating")
            NSApp.terminate(nil)
        }
    }

    // Show banner + sound when the foreground app is the helper itself.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        dbg("willPresent")
        if #available(macOS 11.0, *) {
            completionHandler([.banner, .sound])
        } else {
            completionHandler([.alert, .sound])
        }
    }

    // Click / action-button callback. Fires for body taps (default action)
    // and for the explicit "open" button. The action shell command lives in
    // notification.request.content.userInfo — works the same whether this
    // instance posted the notification or just inherited it via response-
    // mode launch.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let id = response.actionIdentifier
        dbg("didReceive actionIdentifier=\(id)")
        let isClick = id == UNNotificationDefaultActionIdentifier || id == "open"
        if isClick {
            let userInfo = response.notification.request.content.userInfo
            if let cmd = userInfo["actionCommand"] as? String {
                dbg("running action: \(cmd)")
                let task = Process()
                task.launchPath = "/bin/sh"
                task.arguments = ["-c", cmd]
                do {
                    try task.run()
                    task.waitUntilExit()
                    dbg("action exited \(task.terminationStatus)")
                } catch {
                    dbg("action failed: \(error)")
                }
            } else {
                dbg("click but no actionCommand in userInfo")
            }
        } else {
            dbg("non-click action (id=\(id)) — ignoring")
        }
        completionHandler()
        // Intentionally NOT terminating — see class comment.
    }

    private func postNotification(title: String) {
        let center = UNUserNotificationCenter.current()
        center.requestAuthorization(options: [.alert, .sound]) { granted, err in
            dbg("requestAuthorization granted=\(granted) err=\(err?.localizedDescription ?? "nil")")
            if !granted {
                FileHandle.standardError.write(Data("triage-notify: notification permission denied\n".utf8))
            }
        }

        let content = UNMutableNotificationContent()
        content.title = title
        if let s = args.subtitle { content.subtitle = s }
        if let m = args.message { content.body = m }
        if let a = args.action {
            content.userInfo = ["actionCommand": a]
        }

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
        dbg("posting notification id=\(request.identifier) hasAction=\(args.action != nil)")
        center.add(request) { err in
            dbg("center.add completion err=\(err?.localizedDescription ?? "nil")")
            if let err = err {
                FileHandle.standardError.write(Data("triage-notify: post error: \(err.localizedDescription)\n".utf8))
            }
        }
    }
}

let args = parseArgs()
let app = NSApplication.shared
let delegate = AppDelegate(args)
app.delegate = delegate
// Accessory: no Dock icon, no menu bar. LSUIElement=true in Info.plist also
// helps, but setting the activation policy explicitly is the canonical way.
app.setActivationPolicy(.accessory)
app.run()
