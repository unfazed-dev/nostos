# Homebrew formula template for the `nostos` CLI.
#
# Tap flow (deliberately manual, not CI-pushed — matches the plan's
# "don't over-automate" call on the pub.dev/manifest side too):
#   1. Operator creates a `unfazed-dev/homebrew-tap` tap repo
#      (https://github.com/unfazed-dev/homebrew-tap), containing a
#      `Formula/nostos.rb` copied from this template.
#   2. After each `.github/workflows/release.yml` run, an operator (or,
#      later, a follow-up CI job once the tap repo exists and a push
#      credential is provisioned for it) fills in the `url`/`sha256`
#      placeholders below from that tag's release assets and commits to the
#      tap repo directly — Homebrew taps don't have a PR-review convention
#      the way this repo does, and formula updates are small/mechanical
#      enough that automating that *last* step later is low-risk. What's
#      NOT wanted is `release.yml` reaching into a *different* repo's git
#      history on every tag before a human has looked at a single release.
#   3. Users then: `brew tap unfazed-dev/tap && brew install nostos`.
#
# Archive naming/hash source: .github/workflows/release.yml's
# cli-server-macos and cli-server-linux jobs, which publish
# `nostos-<target-triple>.tar.gz` + a `.sha256` sidecar per target. Each
# archive contains both `nostos` and `nostos-server`; this formula only
# installs `nostos` (the CLI) — `nostos-server` is the fan-out server binary,
# out of scope for a developer-machine CLI install.
class Nostos < Formula
  desc "Local-first sync engine CLI — init/dev/doctor/deploy for a Postgres + Supabase sync backend"
  homepage "https://github.com/unfazed-dev/nostos"
  version "0.2.0" # bump alongside workspace.package.version in the root Cargo.toml
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/unfazed-dev/nostos/releases/download/v0.2.0/nostos-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_aarch64-apple-darwin_TAR_GZ_SHA256"
    end
    on_intel do
      url "https://github.com/unfazed-dev/nostos/releases/download/v0.2.0/nostos-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_x86_64-apple-darwin_TAR_GZ_SHA256"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/unfazed-dev/nostos/releases/download/v0.2.0/nostos-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_x86_64-unknown-linux-gnu_TAR_GZ_SHA256"
    end
  end

  def install
    # Each archive unpacks to a `nostos-<target-triple>/` directory (see
    # release.yml's Package step) containing both binaries; only the CLI
    # ships in this formula.
    bin.install Dir["nostos-*/nostos"].first => "nostos"
  end

  test do
    assert_match "nostos", shell_output("#{bin}/nostos --version")
  end
end
