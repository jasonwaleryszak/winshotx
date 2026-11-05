CARGO ?= cargo

.PHONY: all build debug run run-release stop check fmt clippy clean

all: build

# A running instance locks the exe and holds the hotkey, so stop it first.
build: stop
	$(CARGO) build --release

debug: stop
	$(CARGO) build

# Debug build runs as a console app, so Ctrl+C stops it.
run: stop
	$(CARGO) run

# Release build has no console; quit from the tray icon or use `make stop`.
run-release: stop
	$(CARGO) run --release

stop:
	@taskkill /IM winshotx.exe /F >NUL 2>&1 || exit 0

check:
	$(CARGO) check

fmt:
	$(CARGO) fmt

clippy:
	$(CARGO) clippy -- -D warnings

clean:
	$(CARGO) clean
