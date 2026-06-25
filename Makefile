.PHONY: run build build-clean

RUN_ARGS := $(filter-out run build build-clean,$(MAKECMDGOALS))

run:
	cargo run -- $(or $(ARGS),$(RUN_ARGS))

build:
	env TERM=xterm-256color FORCE_COLOR=1 ./script/bundle-mac -i
	@echo "Successfully built and installed /Applications/Zed Dev.app"

build-clean:
	@target_dir="$$(mktemp -d "$${TMPDIR:-/tmp}/zed-build.XXXXXX")"; \
	trap 'rm -rf "$$target_dir"' EXIT; \
	env CARGO_TARGET_DIR="$$target_dir" TERM=xterm-256color FORCE_COLOR=1 ./script/bundle-mac -i
	@echo "Successfully built and installed /Applications/Zed Dev.app"

%:
	@:
