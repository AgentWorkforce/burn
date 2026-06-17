import AppKit

/// Crash-prevention breaker for the menu bar label.
///
/// The menu bar item is the one surface that, if it re-renders in a tight loop
/// (e.g. expensive/re-entrant work creeps back into `body`), can spin the
/// WindowServer and lock up the whole machine — there's no way for the user to
/// quit fast enough. This counts label renders in a sliding window and, if the
/// rate is wildly higher than anything legitimate, logs and terminates the app
/// so a regression degrades to "the app quit" instead of "reboot the Mac".
///
/// Main-thread only (called from `View.body`). It deliberately touches no
/// SwiftUI-observed state, so it can never itself cause a re-render.
@MainActor
enum RenderWatchdog {
    /// Renders allowed within `window` before the breaker trips. Normal operation
    /// renders a handful of times per second at most (launch, popover, refresh);
    /// a render storm is thousands per second.
    private static let limit = 240
    private static let window: TimeInterval = 1.0

    private static var stamps: [TimeInterval] = []
    private static var tripped = false

    /// Record one menu bar label render. Call once at the top of `body`.
    static func tick() {
        guard !tripped else { return }
        let now = ProcessInfo.processInfo.systemUptime
        stamps.append(now)
        stamps.removeAll { now - $0 > window }
        guard stamps.count > limit else { return }

        tripped = true
        NSLog("Burn: menu bar render storm detected (%d renders within %.0fs) — "
            + "terminating to protect the system. This is a bug; please report it.",
            stamps.count, window)
        NSApp.terminate(nil)
    }
}
