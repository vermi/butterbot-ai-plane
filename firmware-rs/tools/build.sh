#!/usr/bin/env bash
# Build the firmware and its update image:
#   target/xtensa-esp32s3-espidf/release/butterbot-ai      (ELF)
#   target/xtensa-esp32s3-espidf/release/butterbot-ai.bin  (for `bbplane ota`, or flash at 0x20000)
#
# Uses the esp Rust toolchain (espup) and, when present, an installed ESP-IDF (~/esp/esp-idf);
# without one, esp-idf-sys downloads the version in .cargo/config.toml.
set -euo pipefail
cd "$(dirname "$0")/.."

# rustup's cargo (for the `esp` toolchain) before any other.
for d in /opt/homebrew/opt/rustup/bin "$HOME/.cargo/bin"; do
	[ -d "$d" ] && PATH="$d:$PATH"
done
export PATH
[ -f "$HOME/export-esp.sh" ] && . "$HOME/export-esp.sh"

if [ -z "${IDF_PATH:-}" ] && [ -f "$HOME/esp/esp-idf/export.sh" ]; then
	. "$HOME/esp/esp-idf/export.sh" >/dev/null
fi
if [ -n "${IDF_PATH:-}" ]; then
	export ESP_IDF_TOOLS_INSTALL_DIR=fromenv
fi

cargo build --release "$@"

out=target/xtensa-esp32s3-espidf/release
# Like ESP-IDF's own build: the ELF's SHA-256 goes into the app description (the build id).
python -m esptool --chip esp32s3 elf2image --flash_mode dio --flash_freq 80m --flash_size 16MB \
	--elf-sha256-offset 0xb0 -o "$out/butterbot-ai.bin" "$out/butterbot-ai" >/dev/null
ls -l "$out/butterbot-ai.bin" | awk '{ printf "%s: %.2f MB\n", $NF, $5 / 1048576 }'
