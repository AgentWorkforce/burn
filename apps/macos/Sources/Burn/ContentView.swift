import SwiftUI

/// The popover's tabs: the headline usage burndown, and the live burn-rate
/// stream.
private enum BurnTab: Hashable {
    case usage
    case live
}

/// The popover shown when the menu bar item is clicked.
struct ContentView: View {
    @ObservedObject var viewModel: UsageViewModel
    @StateObject private var liveViewModel: LiveBurnViewModel
    @State private var tab: BurnTab = .usage

    init(viewModel: UsageViewModel) {
        self.viewModel = viewModel
        _liveViewModel = StateObject(
            wrappedValue: LiveBurnViewModel(provider: viewModel.selectedProvider))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header
            Divider()
            tabPicker
            switch tab {
            case .usage:
                content
            case .live:
                LiveBurnView(viewModel: liveViewModel)
            }
        }
        .padding(16)
        .frame(width: 380)
        .background(quitShortcut)
        // Keep the live stream tracking whichever provider the picker selects.
        .onChange(of: viewModel.selectedProvider) { provider in
            liveViewModel.select(provider)
        }
    }

    /// Segmented switch between the usage burndown and the live stream.
    private var tabPicker: some View {
        Picker("", selection: $tab) {
            Text("Usage").tag(BurnTab.usage)
            Text("Live").tag(BurnTab.live)
        }
        .pickerStyle(.segmented)
        .labelsHidden()
    }

    /// Registers ⌘Q while the popover is open. The app is menu-bar-only (no Dock
    /// icon or app menu), so there's no system Quit item — this is an invisible
    /// handler with no on-screen button.
    private var quitShortcut: some View {
        Button(action: { NSApp.terminate(nil) }) { EmptyView() }
            .keyboardShortcut("q", modifiers: .command)
            .frame(width: 0, height: 0)
            .opacity(0)
            .accessibilityHidden(true)
    }

    // MARK: Header

    private var header: some View {
        VStack(alignment: .leading, spacing: 8) {
            providerPicker

            Text(subtitle)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    /// Segmented control of brand icons, one per provider.
    private var providerPicker: some View {
        HStack(spacing: 4) {
            ForEach(ProviderName.allCases) { provider in
                let isSelected = provider == viewModel.selectedProvider
                Button {
                    viewModel.select(provider)
                } label: {
                    ProviderIcon(provider: provider, size: 17)
                        .opacity(isSelected ? 1 : 0.55)
                        .frame(width: 30, height: 26)
                        .background(
                            RoundedRectangle(cornerRadius: 7, style: .continuous)
                                .fill(isSelected ? provider.brandColor.opacity(0.16) : .clear)
                        )
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help(provider.displayName)
            }
        }
        .padding(3)
        .background(
            Capsule(style: .continuous)
                .fill(Color.primary.opacity(0.06))
        )
    }

    private var subtitle: String {
        let updated: String
        if let last = viewModel.lastUpdated {
            let seconds = Int(Date().timeIntervalSince(last))
            switch seconds {
            case ..<5: updated = "just now"
            case ..<60: updated = "\(seconds)s ago"
            default: updated = "\(seconds / 60)m ago"
            }
        } else {
            updated = viewModel.isLoading ? "loading…" : "never"
        }
        return "Updated \(updated) · Source: \(viewModel.selectedProvider.sourceLabel)"
    }

    // MARK: Content

    @ViewBuilder
    private var content: some View {
        if let status = viewModel.status {
            switch status.status {
            case .unavailable, .error:
                messageView(status.message ?? "Usage is unavailable.")
            case .rateLimited:
                noticeView(status.message ?? "Rate-limited. Retrying shortly.")
            default:
                if status.metrics.isEmpty {
                    messageView("No active limit windows reported.")
                } else {
                    chartsView(plan: status.plan)
                }
            }
        } else {
            ProgressView()
                .frame(maxWidth: .infinity)
                .padding(.vertical, 36)
        }
    }

    private func chartsView(plan: String?) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            if let notice = viewModel.notice {
                noticeView(notice)
            }
            if let plan, !plan.isEmpty {
                Text("\(plan.prefix(1).capitalized + plan.dropFirst()) plan")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 3)
                    .background(Capsule().fill(Color.primary.opacity(0.06)))
            }
            ForEach(Array(viewModel.charts.enumerated()), id: \.offset) { _, item in
                VStack(alignment: .leading, spacing: 4) {
                    if let data = item.data {
                        BurndownChartView(title: item.metric.name, data: data)
                    } else {
                        SimpleUsageRow(metric: item.metric)
                    }
                    if let spend = viewModel.spend[item.metric.name] {
                        spendRow(spend)
                    }
                }
            }
        }
    }

    /// Ledger-backed spend for one window: this period, compared to the last.
    private func spendRow(_ spend: PeriodSpend) -> some View {
        func dollars(_ amount: Double) -> String { String(format: "$%.2f", amount) }
        return HStack(spacing: 5) {
            Text("\(dollars(spend.thisPeriod)) this period")
            if let last = spend.lastPeriod {
                let delta = spend.thisPeriod - last
                Image(systemName: delta >= 0 ? "arrow.up" : "arrow.down")
                Text("\(dollars(last)) last")
            }
        }
        .font(.caption2)
        .foregroundStyle(.secondary)
        .padding(.leading, 2)
    }

    private func messageView(_ message: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            Text(message)
                .font(.callout)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 24)
    }

    /// A low-key, informational banner (e.g. transient rate-limit notice).
    private func noticeView(_ message: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "clock.arrow.circlepath")
                .foregroundStyle(.secondary)
            Text(message)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Color.primary.opacity(0.05)))
    }
}

/// Fallback row for windows that don't report a reset time (no burndown).
struct SimpleUsageRow: View {
    let metric: UsageMetric

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(metric.name)
                .font(.title3.weight(.bold))
            ProgressView(value: min(metric.percentage, 100), total: 100)
            Text("\(Int(metric.percentage.rounded()))% used")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }
}
