release:
	cargo install --path .

lint:
	cargo clippy --all-targets -- -D warnings
