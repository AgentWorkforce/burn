import Foundation

/// Holds a repeating `Timer` outside of a `@MainActor` view model's isolation so
/// that the model's `deinit` (which is nonisolated and can run on whatever
/// thread drops the last reference) can tear the timer down without touching
/// actor-isolated state.
///
/// A scheduled `Timer` is retained by its run loop, and our timers capture
/// `self` weakly, so the run loop would keep firing the timer even after the
/// view model is gone. Storing the timer here means it is invalidated when the
/// box is released (i.e. when the owning view model deallocates), so no
/// orphaned timer is left ticking on the main run loop — the leak/slowness
/// field reports point straight at these never-invalidated timers.
///
/// Thread safety: the stored timer is guarded by a lock (a deinit on a
/// non-main thread may race main-thread access), and `Timer.invalidate()` is
/// only ever called on the main thread — the thread whose run loop scheduled
/// it — hopping via `DispatchQueue.main.async` when teardown originates
/// elsewhere.
final class TimerBox: @unchecked Sendable {
    private let lock = NSLock()
    private var _timer: Timer?

    /// The held timer. Access is lock-protected; schedule the timer itself on
    /// the main run loop (all our callers are `@MainActor`).
    var timer: Timer? {
        get {
            lock.lock(); defer { lock.unlock() }
            return _timer
        }
        set {
            lock.lock(); defer { lock.unlock() }
            _timer = newValue
        }
    }

    /// Whether a timer is currently held and scheduled. Reads false immediately
    /// after `invalidate()`, even while an off-main teardown's actual
    /// `Timer.invalidate()` call is still hopping to the main thread.
    var isActive: Bool {
        lock.lock(); defer { lock.unlock() }
        return _timer?.isValid ?? false
    }

    /// Takes the timer out under the lock, then invalidates it on the main
    /// thread — directly when already there, else via `DispatchQueue.main.async`
    /// (`Timer.invalidate()` must run on the thread that scheduled the timer).
    /// Safe to call repeatedly and from any thread.
    func invalidate() {
        lock.lock()
        let timer = _timer
        _timer = nil
        lock.unlock()
        guard let timer else { return }
        if Thread.isMainThread {
            timer.invalidate()
        } else {
            DispatchQueue.main.async { timer.invalidate() }
        }
    }

    deinit { invalidate() }
}
