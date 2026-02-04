ci:
	cargo check
	cargo clippy
	cargo test
	./jjq-test
