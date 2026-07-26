# spec-wave-agent — instalação na máquina do dev

## 1. Compilar e instalar

```bash
cargo build --release
cp target/release/spec-wave-agent /usr/local/bin/
```

## 2. Configuração (`~/.config/spec-wave-agent/config.toml`)

```toml
repo = "sua-org/seu-repo"
queue_label = "agent:queued"
poll_interval_secs = 60
heartbeat_secs = 120       # renovação do lease
lease_ttl_secs = 600       # sem heartbeat por 10min => outro agente rouba
implement_timeout_secs = 3600
# agent_id = "moacir@macbook"   # default: usuario@hostname
```

Invariante importante: `lease_ttl_secs` deve ser >= 4x `heartbeat_secs`,
para tolerar lentidão de rede sem roubo indevido.

## 3. Pré-requisitos na máquina

- `git` e `gh` autenticados (o agente usa as credenciais do dev)
- Node 18+ (`npx spec-wave`)
- Claude Code instalado e logado
- Repositório com `.spec-wave.json` contendo `specKit.command`
  apontando para o Claude Code (a máquina do dev É o executor)

## 4. Rodar como serviço

### macOS (launchd) — `~/Library/LaunchAgents/dev.specwave.agent.plist`

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
 "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.specwave.agent</string>
  <key>ProgramArguments</key>
    <array><string>/usr/local/bin/spec-wave-agent</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/spec-wave-agent.log</string>
  <key>StandardErrorPath</key><string>/tmp/spec-wave-agent.log</string>
</dict></plist>
```

```bash
launchctl load ~/Library/LaunchAgents/dev.specwave.agent.plist
```

### Linux (systemd user unit) — `~/.config/systemd/user/spec-wave-agent.service`

```ini
[Unit]
Description=spec-wave dev agent

[Service]
ExecStart=/usr/local/bin/spec-wave-agent
Restart=on-failure
# SIGTERM no desligamento => checkpoint + release do lease (takeover imediato)
KillSignal=SIGTERM
TimeoutStopSec=30

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now spec-wave-agent
```

## 5. Como funciona a coordenação

- Fila: issues abertas com label `agent:queued` + tipo `[STORY]`/`[TASK]`
- Lock: ref `refs/heads/spec-wave-agent/claims/<n>` com `lease.json`
  (aquisição = push não-forçado; renovação/roubo = `--force-with-lease`)
- Desligou a máquina educadamente => SIGTERM => checkpoint (commit+push
  do WIP) + release do lease => outro agente retoma no próximo poll
- Desligou na força (bateria, kernel panic) => sem heartbeat por
  `lease_ttl_secs` => outro agente ROUBA o lease e retoma do último
  commit pushado no branch `agent/issue-<n>`
