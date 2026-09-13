CARGO := cargo +1.95.0

.PHONY: client host benchmark install-host uninstall-host

client:
	$(CARGO) build --release --locked -p rustconsole-player
	$(CARGO) run --release --locked -p rustconsole-client

host:
	powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File scripts/package-windows.ps1

benchmark:
	$(CARGO) bench --workspace --bench '*' --locked

install-host:
	./target/windows-package/rustconsole-host.exe install

uninstall-host:
	./target/windows-package/rustconsole-host.exe uninstall
