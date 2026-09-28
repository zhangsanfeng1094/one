.PHONY: build

# Build the UI before compiling the binary that embeds its assets.
build:
	cd crates/one-web/web && npm run build
	cargo build -p one-cli --bin one
