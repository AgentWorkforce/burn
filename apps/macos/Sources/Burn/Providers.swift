import Foundation

protocol UsageProvider {
    var name: ProviderName { get }
    func fetch() async -> ProviderStatus
}

// MARK: - Claude

struct ClaudeProvider: UsageProvider {
    let name: ProviderName = .claude

    func fetch() async -> ProviderStatus {
        // Credential loading shells out to `security` and reads files — keep it
        // off the MainActor caller so it can't stall the menu bar UI.
        let credentials = await Task.detached(priority: .userInitiated) {
            Credentials.claude()
        }.value
        guard let credentials else {
            return .unavailable(.claude, "Not logged in. Run 'claude' to authenticate.")
        }

        var request = URLRequest(url: URL(string: "https://api.anthropic.com/api/oauth/usage")!)
        request.httpMethod = "GET"
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("monitor/1.0.0", forHTTPHeaderField: "User-Agent")
        request.setValue("Bearer \(credentials.accessToken)", forHTTPHeaderField: "Authorization")
        request.setValue("oauth-2025-04-20", forHTTPHeaderField: "anthropic-beta")

        do {
            let (data, response) = try await URLSession.shared.data(for: request)
            guard let http = response as? HTTPURLResponse else {
                return .failure(.claude, "No response from server.")
            }
            if http.statusCode == 401 {
                return .failure(.claude, "Token expired. Run 'claude' to re-authenticate.")
            }
            if http.statusCode == 429 {
                return .rateLimited(.claude)
            }
            guard http.statusCode == 200 else {
                return .failure(.claude, "API error: \(http.statusCode)")
            }

            let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
            var metrics: [UsageMetric] = []

            func window(_ key: String, name: String, period: Double) -> UsageMetric? {
                guard let w = json[key] as? [String: Any],
                      let utilization = (w["utilization"] as? NSNumber)?.doubleValue
                else { return nil }
                let resetsAt = (w["resets_at"] as? String).flatMap(DateParsing.date(from:))
                return UsageMetric(name: name, percentage: utilization, resetsAt: resetsAt, periodSeconds: period)
            }

            if let m = window("five_hour", name: "5-hour", period: 5 * 3600) { metrics.append(m) }
            if let m = window("seven_day", name: "Weekly", period: 7 * 24 * 3600) { metrics.append(m) }
            if let m = window("seven_day_opus", name: "Opus", period: 7 * 24 * 3600), m.percentage > 0 {
                metrics.append(m)
            }

            let maxUsage = metrics.map(\.percentage).max() ?? 0
            return ProviderStatus(
                provider: .claude,
                status: maxUsage >= 80 ? .warning : .ok,
                plan: credentials.subscriptionType ?? "Pro",
                metrics: metrics,
                message: nil
            )
        } catch {
            return .failure(.claude, error.localizedDescription)
        }
    }
}

// MARK: - Codex

struct CodexProvider: UsageProvider {
    let name: ProviderName = .codex

    func fetch() async -> ProviderStatus {
        // Credential loading reads files off the MainActor caller (see Claude).
        let credentials = await Task.detached(priority: .userInitiated) {
            Credentials.codex()
        }.value
        guard let credentials else {
            return .unavailable(.codex, "Not logged in. Run 'codex' to authenticate.")
        }

        var request = URLRequest(url: URL(string: "https://chatgpt.com/backend-api/wham/usage")!)
        request.setValue("Bearer \(credentials.accessToken)", forHTTPHeaderField: "Authorization")
        request.setValue(credentials.accountId, forHTTPHeaderField: "ChatGPT-Account-Id")
        request.setValue("codex_cli_rs", forHTTPHeaderField: "originator")
        request.setValue("codex_cli_rs/0.77.0", forHTTPHeaderField: "User-Agent")

        do {
            let (data, response) = try await URLSession.shared.data(for: request)
            guard let http = response as? HTTPURLResponse else {
                return .failure(.codex, "No response from server.")
            }
            if http.statusCode == 401 {
                return .failure(.codex, "Token expired. Run 'codex' to re-authenticate.")
            }
            if http.statusCode == 429 {
                return .rateLimited(.codex)
            }
            guard http.statusCode == 200 else {
                return .failure(.codex, "API error: \(http.statusCode)")
            }

            let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
            let rateLimit = json["rate_limit"] as? [String: Any]

            var metrics: [UsageMetric] = []

            func parse(_ w: [String: Any], primary: Bool) -> UsageMetric? {
                guard let used = (w["used_percent"] as? NSNumber)?.doubleValue,
                      let windowSeconds = (w["limit_window_seconds"] as? NSNumber)?.doubleValue
                else { return nil }

                let resetsAt = (w["reset_at"] as? NSNumber)
                    .map { Date(timeIntervalSince1970: $0.doubleValue) }

                let name: String
                if primary {
                    let hours = Int((windowSeconds / 3600).rounded())
                    name = "\(hours)-hour"
                } else {
                    let days = Int((windowSeconds / 86400).rounded())
                    name = days >= 7 ? "Weekly" : "\(days)-day"
                }
                return UsageMetric(name: name, percentage: used, resetsAt: resetsAt, periodSeconds: windowSeconds)
            }

            if let primary = rateLimit?["primary_window"] as? [String: Any],
               let m = parse(primary, primary: true) {
                metrics.append(m)
            }
            if let secondary = rateLimit?["secondary_window"] as? [String: Any],
               let m = parse(secondary, primary: false) {
                metrics.append(m)
            }

            let limitReached = (rateLimit?["limit_reached"] as? Bool) ?? false
            let maxUsage = metrics.map(\.percentage).max() ?? 0
            let planType = (json["plan_type"] as? String).map {
                $0.prefix(1).uppercased() + $0.dropFirst()
            } ?? "Unknown"

            return ProviderStatus(
                provider: .codex,
                status: (limitReached || maxUsage >= 80) ? .warning : .ok,
                plan: planType,
                metrics: metrics,
                message: nil
            )
        } catch {
            return .failure(.codex, error.localizedDescription)
        }
    }
}
