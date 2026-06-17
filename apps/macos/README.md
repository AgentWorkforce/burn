# Agent Limit — burn's macOS app

The macOS **menu bar app** for [burn](../../). It shows your **Claude Code** and
**Codex** usage limits as burndown charts — so you can see at a glance whether
you're ahead of or behind your usage pace before you hit a wall — and pulls
**spend** straight from the burn ledger.

## What it does

- Lives in the menu bar as a flame icon that warms from **orange to red** as
  your highest current usage climbs, and fills in (turns "hot") when that window
  is burning faster than its target pace.
- Click it to open a popover with a **burndown chart** for each limit window
  (e.g. 5-hour and weekly):
  - A dashed line shows the *ideal* pace (a straight burn from 100% to 0% over
    the window).
  - A solid blue line shows your *actual* remaining quota over time.
  - The gap between them is shaded **green** when you're **under pace** (you
    have headroom) or **red** when you're **over pace** (you'll hit the limit
    early).
- Switch between **OpenAI/Codex** and **Claude** with brand-icon buttons
  ([lobe-icons](https://github.com/lobehub/lobe-icons)).
- Auto-refreshes every 60 seconds, and backs off automatically if the usage API
  rate-limits (it keeps showing your last reading instead of erroring out).
- Under each window, shows **spend this period vs. last period** ($), read from
  the burn ledger via the `burn` CLI. Hidden if `burn` isn't installed.

## How it reads your usage

The app reuses the credentials the official CLIs already store on your Mac — it
never asks you to log in again:

| Provider | Credentials | Endpoint |
|----------|-------------|----------|
| Claude   | `Claude Code-credentials` login-keychain item (written by Claude Code) | `https://api.anthropic.com/api/oauth/usage` |
| Codex    | `~/.codex/auth.json` (written by the Codex CLI) | `https://chatgpt.com/backend-api/wham/usage` |

If a provider isn't authenticated, run `claude` or `codex` once to log in.

Spend figures are read from the burn ledger (`~/.agentworkforce/burn/`) by
shelling out to `burn summary --provider <p> --since <window-start> --json` —
cost isn't stored in the ledger, so we let burn price it rather than re-deriving
its pricing table. The spend line is hidden when `burn` isn't on `PATH`.

The first time it reads the Claude keychain item, macOS may prompt you to allow
access — choose **Always Allow**.

## Install

Download the latest DMG, open it, and drag **Agent Limit** to **Applications**:

> **[⬇ AgentLimit-arm64.dmg](../../releases/latest/download/AgentLimit-arm64.dmg)**
> (Apple Silicon)

The build is signed and notarized, so it launches without Gatekeeper warnings.
It runs as a menu bar item (no Dock icon).

Or build it yourself (below).

## Build & run

Requires macOS 13+ and the Swift toolchain (Xcode or the Command Line Tools).

```bash
./build.sh
open dist/AgentLimit.app
```

To install it permanently:

```bash
cp -R dist/AgentLimit.app /Applications/
```

For development you can also run straight from the package:

```bash
swift run
```

## Project layout

```
Package.swift                 Swift package manifest
App/Info.plist                Bundle metadata (LSUIElement → menu-bar-only app)
App/AppIcon.icns              App icon (regenerate with scripts/make-icon.sh)
build.sh                      Builds AgentLimit.app
release.sh                    Signs, notarizes & packages the DMG
scripts/                      Icon generator
Sources/AgentLimit/
  AgentLimitApp.swift         App entry point + menu bar label
  ContentView.swift           Popover UI + provider icon picker
  BurndownChartView.swift     Swift Charts burndown card
  BrandIcon.swift             Loads/tints the lobe-icons SVGs
  UsageViewModel.swift        Loading, refresh timer, view state
  Providers.swift             Claude + Codex usage fetchers
  Credentials.swift           Reads keychain / auth.json
  UsageHistory.swift          Persists samples for the usage curve
  Burndown.swift              Turns samples into chart data
  Models.swift                Shared types
  Resources/                  claude.svg, openai.svg (lobe-icons, MIT)
```

Brand icons are from [lobe-icons](https://github.com/lobehub/lobe-icons) (MIT).

## Releasing

Set up the same way as [Pear](../../../pear): signed + notarized DMG published to
GitHub Releases, using the **same repository secrets** so one set of Apple
credentials covers both repos.

To cut a release, go to **Actions → "Release (macOS)" → Run workflow**. The
[workflow](.github/workflows/release.yml):

1. computes a **date-based version** `YEAR.MONTH.N` (N = the next release this
   month) and tag `vYEAR.MONTH.N` — nothing is committed;
2. generates release notes from the commits since the last tag;
3. signs with a hardened runtime, notarizes via the App Store Connect API key,
   staples, and builds `AgentLimit-arm64.dmg`;
4. publishes the release as **latest**, so the stable
   `releases/latest/download/AgentLimit-arm64.dmg` link always points at it.

`release.sh` runs the same build/sign/notarize/package steps locally (set
`VERSION` and the Apple env vars listed at the top of the script).

Required repository **secrets** (Settings → Secrets and variables → Actions) —
identical to Pear's:

| Secret | What it is |
|--------|------------|
| `CSC_LINK` | base64 of your Developer ID Application cert exported as `.p12` (`base64 -i cert.p12 \| pbcopy`) |
| `CSC_KEY_PASSWORD` | password you set when exporting the `.p12` |
| `APPLE_API_KEY_BASE64` | base64 of your App Store Connect API key (`AuthKey_XXXX.p8`) |
| `APPLE_API_KEY_ID` | the API key ID |
| `APPLE_API_ISSUER` | the API key issuer UUID |

The throwaway CI keychain password is generated per-run, and the signing
identity is auto-detected from the imported certificate — no extra secrets.

> Apple Silicon only (arm64), matching the CI runner.

## License

MIT — see [LICENSE](LICENSE).
