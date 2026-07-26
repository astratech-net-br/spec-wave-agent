PREFIX  ?= /usr/local
BINDIR  ?= $(PREFIX)/bin
BIN      = target/release/spec-wave-agent

.PHONY: build test install install-config install-systemd install-launchd uninstall

build:
	cargo build --release

test:
	cargo test

install: build
	install -d $(DESTDIR)$(BINDIR)
	install -m 755 $(BIN) $(DESTDIR)$(BINDIR)/

install-config:
	install -d $(HOME)/.config/spec-wave-agent
	@if [ -f $(HOME)/.config/spec-wave-agent/config.toml ]; then \
		echo "config.toml já existe; não sobrescrevendo"; \
	else \
		install -m 644 packaging/config.example.toml \
			$(HOME)/.config/spec-wave-agent/config.toml; \
		echo "edite $(HOME)/.config/spec-wave-agent/config.toml"; \
	fi

install-systemd:
	install -d $(HOME)/.config/systemd/user
	install -m 644 packaging/spec-wave-agent.service $(HOME)/.config/systemd/user/
	systemctl --user daemon-reload
	systemctl --user enable --now spec-wave-agent
	@echo "logs: journalctl --user -u spec-wave-agent -f"

install-launchd:
	install -d $(HOME)/Library/LaunchAgents $(HOME)/Library/Logs
	sed "s|/Users/CHANGE_ME|$(HOME)|g" packaging/dev.specwave.agent.plist \
		> $(HOME)/Library/LaunchAgents/dev.specwave.agent.plist
	launchctl load $(HOME)/Library/LaunchAgents/dev.specwave.agent.plist
	@echo "logs: tail -f $(HOME)/Library/Logs/spec-wave-agent.log"

uninstall:
	-systemctl --user disable --now spec-wave-agent 2>/dev/null || true
	-launchctl unload $(HOME)/Library/LaunchAgents/dev.specwave.agent.plist 2>/dev/null || true
	rm -f $(DESTDIR)$(BINDIR)/spec-wave-agent \
	      $(HOME)/.config/systemd/user/spec-wave-agent.service \
	      $(HOME)/Library/LaunchAgents/dev.specwave.agent.plist
