CARGO ?= cargo

.PHONY: all build debug run run-release stop check fmt clippy clean

all: build

build:
	$(CARGO) build --release

debug:
	$(CARGO) build

# Debug build runs as a console app, so Ctrl+C stops it.
run:
	$(CARGO) run

# Release build has no console; use `make stop` to end it.
run-release:
	$(CARGO) run --release

stop:
	-taskkill /IM winshotx.exe /F

check:
	$(CARGO) check

fmt:
	$(CARGO) fmt

clippy:
	$(CARGO) clippy -- -D warnings

clean:
	$(CARGO) clean
