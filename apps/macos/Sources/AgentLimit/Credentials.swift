import Foundation

struct ClaudeCredentials {
    let accessToken: String
    let subscriptionType: String?
}

struct CodexCredentials {
    let accessToken: String
    let accountId: String
    let planType: String?
}

/// Reads locally-stored credentials written by the Claude Code and Codex CLIs.
///
/// - Claude Code stores an OAuth blob in the login keychain under the service
///   name `"Claude Code-credentials"`.
/// - Codex stores tokens in `~/.codex/auth.json`.
enum Credentials {

    // MARK: Claude

    static func claude() -> ClaudeCredentials? {
        guard let raw = keychainPassword(service: "Claude Code-credentials"),
              let data = raw.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let oauth = json["claudeAiOauth"] as? [String: Any],
              let token = oauth["accessToken"] as? String
        else {
            return nil
        }
        let subscription = oauth["subscriptionType"] as? String
        return ClaudeCredentials(accessToken: token, subscriptionType: subscription)
    }

    /// Shells out to `/usr/bin/security` to read a generic password. This mirrors
    /// what the original CLI did and avoids needing a matching keychain access
    /// group. The user may be prompted to allow access the first time.
    private static func keychainPassword(service: String) -> String? {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/security")
        process.arguments = ["find-generic-password", "-s", service, "-w"]

        let outPipe = Pipe()
        process.standardOutput = outPipe
        process.standardError = Pipe()

        do {
            try process.run()
        } catch {
            return nil
        }

        let data = outPipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else { return nil }

        let value = String(data: data, encoding: .utf8)?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return (value?.isEmpty == false) ? value : nil
    }

    // MARK: Codex

    static func codex() -> CodexCredentials? {
        let url = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".codex/auth.json")

        guard let data = try? Data(contentsOf: url),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let tokens = json["tokens"] as? [String: Any],
              let access = tokens["access_token"] as? String,
              let accountId = tokens["account_id"] as? String
        else {
            return nil
        }

        let plan = chatGPTPlan(fromJWT: access)
        return CodexCredentials(accessToken: access, accountId: accountId, planType: plan)
    }

    /// Extracts `chatgpt_plan_type` from the OpenAI auth claim of a JWT payload.
    private static func chatGPTPlan(fromJWT jwt: String) -> String? {
        let parts = jwt.split(separator: ".")
        guard parts.count == 3 else { return nil }

        var base64 = String(parts[1])
            .replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        while base64.count % 4 != 0 { base64 += "=" }

        guard let data = Data(base64Encoded: base64),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let auth = json["https://api.openai.com/auth"] as? [String: Any],
              let plan = auth["chatgpt_plan_type"] as? String
        else {
            return nil
        }
        return plan
    }
}
