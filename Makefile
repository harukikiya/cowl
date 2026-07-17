# ワーカーの完了条件は `make check` 緑。CIでも同じものを回す想定
.PHONY: build test check fmt clippy demo clean

build:
	cargo build --workspace

test:
	cargo test --workspace

fmt:
	cargo fmt --all

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace

# examples/ からレポート一式を再生成。描画を触ったら必ずこれで目視確認
demo: build
	mkdir -p out
	for f in examples/*.c; do \
	  b=$$(basename $$f .c); \
	  ./target/debug/cowl report $$f -o out/$$b.html; \
	  ./target/debug/cowl graph  $$f -o out/$$b.dot; \
	done
	@echo "out/ を確認してください（demo.html が主役）"

clean:
	cargo clean
	rm -rf out
