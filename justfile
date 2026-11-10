release:
	cargo install --path .

lint:
	cargo clippy --all-targets -- -D warnings

run:
	cargo run

run-release:
	cargo run --release

test:
	cargo test