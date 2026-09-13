CARGO := cargo +1.95.0
STABLE_CARGO := cargo +stable

.PHONY: client host benchmark format check clippy test dependency-direction driver-dependencies ci install-host uninstall-host

client:
	$(CARGO) build --release --locked -p rustconsole-player
	$(CARGO) run --release --locked -p rustconsole-client

host:
	powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File scripts/package-windows.ps1

benchmark:
	$(CARGO) bench --workspace --bench '*' --locked

format:
	$(STABLE_CARGO) fmt --all --check

check:
	$(STABLE_CARGO) check --workspace --all-targets

clippy:
	$(STABLE_CARGO) clippy --workspace --all-targets -- -D warnings

test:
	$(STABLE_CARGO) test --workspace

dependency-direction:
	node scripts/check-dependency-direction.mjs

driver-dependencies:
	$(STABLE_CARGO) metadata --locked --manifest-path native/windows-input-driver/Cargo.toml --format-version 1 > /dev/null

ci:
	$(STABLE_CARGO) fmt --all --check
	$(STABLE_CARGO) check --workspace --all-targets
	$(STABLE_CARGO) clippy --workspace --all-targets -- -D warnings
	$(STABLE_CARGO) test --workspace
	node scripts/check-dependency-direction.mjs
	$(STABLE_CARGO) metadata --locked --manifest-path native/windows-input-driver/Cargo.toml --format-version 1 > /dev/null

install-host:
	./target/windows-package/rustconsole-host.exe install

uninstall-host:
	./target/windows-package/rustconsole-host.exe uninstall
