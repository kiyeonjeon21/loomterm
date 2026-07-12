#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 VERSION TAP_DIRECTORY" >&2
  exit 2
fi

version=${1#v}
tap_dir=$2
release="v$version"
repo="kiyeonjeon21/loomterm"
formula="$tap_dir/Formula/loomterm.rb"

if [[ ! -d "$tap_dir/.git" ]]; then
  echo "tap directory is not a git checkout: $tap_dir" >&2
  exit 1
fi
if [[ -n $(git -C "$tap_dir" status --porcelain) ]]; then
  echo "tap checkout must be clean before updating the formula" >&2
  exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
targets=(
  aarch64-apple-darwin
  x86_64-apple-darwin
  x86_64-unknown-linux-gnu
)
for target in "${targets[@]}"; do
  asset="loomterm-v${version}-${target}.tar.gz"
  gh release download "$release" --repo "$repo" --pattern "$asset" --dir "$tmp"
done

sha_arm_mac=$(shasum -a 256 "$tmp/loomterm-v${version}-aarch64-apple-darwin.tar.gz" | awk '{print $1}')
sha_intel_mac=$(shasum -a 256 "$tmp/loomterm-v${version}-x86_64-apple-darwin.tar.gz" | awk '{print $1}')
sha_linux=$(shasum -a 256 "$tmp/loomterm-v${version}-x86_64-unknown-linux-gnu.tar.gz" | awk '{print $1}')

mkdir -p "$(dirname "$formula")"
cat > "$formula" <<EOF
class Loomterm < Formula
  desc "Durable, structured command runtime for coding agents"
  homepage "https://github.com/$repo"
  version "$version"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/$repo/releases/download/$release/loomterm-v${version}-aarch64-apple-darwin.tar.gz"
      sha256 "$sha_arm_mac"
    else
      url "https://github.com/$repo/releases/download/$release/loomterm-v${version}-x86_64-apple-darwin.tar.gz"
      sha256 "$sha_intel_mac"
    end
  end

  on_linux do
    url "https://github.com/$repo/releases/download/$release/loomterm-v${version}-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "$sha_linux"
  end

  def install
    bin.install "loom", "loomd", "loom-mcp", "loom-supervisor"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/loom --version")
    assert_match version.to_s, shell_output("#{bin}/loomd --version")
    assert_match version.to_s, shell_output("#{bin}/loom-mcp --version")
    assert_match version.to_s, shell_output("#{bin}/loom-supervisor --version")
  end
end
EOF

echo "updated $formula for $release"
git -C "$tap_dir" diff -- "$formula"
