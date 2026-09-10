CARGO := cargo +1.95.0

.PHONY: client host

client:
	$(CARGO) build --release --locked -p rustconsole-player
	$(CARGO) run --release --locked -p rustconsole-client

host:
	powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File scripts/deploy-windows-host.ps1
