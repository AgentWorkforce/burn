import Foundation

/// Holds a repeating `Timer` outside of a `@MainActor` view model's isolation so
/// that the model's `deinit` (which is nonisolated) can tear the timer down
/// without touching actor-isolated state.
///
/// A scheduled `Timer` is retained by its run loop, and our timers capture
/// `self` weakly, so the run loop keeps firing the timer even after the view
/// model is gone. Storing the timer here means it is invalidated when the box is
/// released (i.e. when the owning view model deallocates), so no orphaned timer
/// is left ticking on the main run loop — the leak/slowness field reports point
/// straight at these never-invalidated timers.
///
/// The timer is only ever scheduled and invalidated on the main run loop (both
/// happen from `@MainActor` code, and the owning model deallocates on main in
/// practice), so `@unchecked Sendable` is sound here.
final class TimerBox: @unchecked Sendable {
    var timer: Timer?

    var isActive: Bool { timer?.isValid ?? false }

    /// Invalidates and clears the timer. Safe to call repeatedly.
    func invalidate() {
        timer?.invalidate()
        timer = nil
    }

    deinit { timer?.invalidate() }
}
