.PHONY: run build

RUN_ARGS := $(filter-out run build,$(MAKECMDGOALS))

run:
	cargo run -- $(or $(ARGS),$(RUN_ARGS))

build:
	env TERM=xterm-256color FORCE_COLOR=1 ./script/bundle-mac -i

%:
	@:
