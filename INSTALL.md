# spec-wave-agent — instalação na máquina do dev

## 1. Compilar e instalar

```bash
make install            # cargo build --release + copia p/ /usr/local/bin
make install-config     # cria ~/.config/spec-wave-agent/config.toml se não existir
```

(ou manualmente: `cargo build --release && cp target/release/spec-wave-agent /usr/local/bin/`)

## 2. Configuração (`~/.config/spec-wave-agent/config.toml`)

Veja `packaging/config.example.toml` (schema completo comentado). Mínimo:

```toml
repo = "sua-org/seu-repo"
queue_label = "spec-wave:dev-agent"   # default
```

Invariantes validadas no boot (o agente recusa config inválida):

- `repo` no formato `owner/repo`
- `lease_ttl_secs >= 4 * heartbeat_secs` (tolera lentidão de rede sem
  roubo indevido de lease)
- intervalos > 0

## 3. Pré-requisitos na máquina (checados no boot — fail fast)

- `git` e `gh` autenticados (o agente usa as credenciais do dev)
- Node 18+ (`npx spec-wave`)
- Claude Code instalado e logado
- Repositório com `.spec-wave.json` contendo `specKit.command`
  apontando para o Claude Code (a máquina do dev É o executor)

## 4. Rodar

### Foreground (console mostra os logs ao vivo)

```bash
spec-wave-agent                    # logs info no console
RUST_LOG=debug spec-wave-agent     # mais verboso
```

Os logs do agente saem com timestamp/nível; o output do
`npx spec-wave implement` (Claude Code) aparece intercalado como
`[#<issue>][out] ...` / `[#<issue>][err] ...`.

### Linux (systemd user unit)

```bash
make install-systemd
journalctl --user -u spec-wave-agent -f    # acompanhar logs
```

Unit em `packaging/spec-wave-agent.service`. Se o Node vem de nvm/volta,
descomente/ajuste a linha `Environment=PATH=...` da unit.

### macOS (launchd)

```bash
make install-launchd
tail -f ~/Library/Logs/spec-wave-agent.log
```

Plist em `packaging/dev.specwave.agent.plist` (o Makefile substitui o
home automaticamente).

## 5. Como funciona a coordenação

- Fila: issues abertas com label `spec-wave:dev-agent` + tipo
  `[STORY]`/`[TASK]`; FIFO por número.
- **Uma tarefa por vez**: o agente pega a primeira issue que conseguir
  claimar; só depois de terminar (sucesso, falha ou interrupção) volta à
  fila — com re-poll fresco — para tentar obter a próxima.
- Lock: ref `refs/heads/spec-wave-agent/claims/<n>` com `lease.json`
  (aquisição = push não-forçado; renovação/roubo = `--force-with-lease`).
- Desligou a máquina educadamente => SIGTERM => checkpoint (commit+push
  do WIP) + release do lease => outro agente retoma no próximo poll.
- Desligou na força (bateria, kernel panic) => sem heartbeat por
  `lease_ttl_secs` => outro agente ROUBA o lease e retoma do último
  commit pushado no branch `agent/issue-<n>`.
- Rede instável: renovações com erro transiente são re-tentadas por até
  `lease_ttl_secs − 2×heartbeat_secs`; estourou => o agente se auto-cerca
  (mata o processo filho) antes de qualquer roubo ser possível.

## 6. Testes

```bash
make test    # unitários + integração do lease (origin git local, sem rede)
```
