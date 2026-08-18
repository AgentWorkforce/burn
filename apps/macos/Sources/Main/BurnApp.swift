import SwiftUI
import BurnCore

@main
struct BurnApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        // No SwiftUI scene drives the menu bar. The status item is created once,
        // imperatively, by AppDelegate. SwiftUI's `MenuBarExtra` can duplicate
        // its status item when the app's scene body re-evaluates — that runaway
        // ("endless menu bar flames") panicked the machine. An AppKit
        // NSStatusItem created a single time cannot be duplicated.
        Settings { EmptyView() }
    }
}
