ci:
	cargo check
	cargo clippy
	cargo test
	./jjq-test

release:
	nix build .#tarball
	@echo "Built: $$(tar tzf result | head -1 | cut -d/ -f1).tar.gz -> result"
