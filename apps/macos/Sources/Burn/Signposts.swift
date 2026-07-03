import os

/// Lightweight `os_signpost` instrumentation so the app's hot paths can be
/// profiled in Instruments (Time Profiler / os_signpost lane). Filter by
/// subsystem `com.agentworkforce.burn` and pick a category:
///
/// - `refresh`    — the view-model refresh loops and spend load.
/// - `subprocess` — each `burn` process execution (the interval message carries
///   the first CLI argument, e.g. `summary`).
///
/// Signposters are cheap and safe to leave compiled in; they emit nothing unless
/// a tool is recording. `OSSignpostIntervalState` is `Sendable`, so an interval
/// begun before an `await` can be ended after it.
enum Signposts {
    static let refresh = OSSignposter(subsystem: "com.agentworkforce.burn", category: "refresh")
    static let subprocess = OSSignposter(subsystem: "com.agentworkforce.burn", category: "subprocess")
}
