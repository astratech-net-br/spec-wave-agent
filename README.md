# spec-wave-agent

Daemon que roda na máquina do dev, pega **Features** da fila do GitHub e delega
a implementação ao Claude Code — que usa o [spec-wave](https://github.com/astratech-net-br/spec-wave-cli)
para implementar todas as user stories da Feature na ordem de dependência.

Vários devs podem rodar o agente ao mesmo tempo no mesmo repositório: a
exclusão mútua é garantida por um **lease distribuído sobre git refs**, sem
servidor central e sem banco. Implementa o RFC-001 do fluxo spec-wave.

```
issue [FEATURE] com label spec-wave:dev-agent
        │
        ▼
  spec-wave-agent (Rust)          claim atômico via git ref (CAS)
        │
        ▼
  claude -p  (orquestrador)       prompt via stdin
        │
        ├─ npx spec-wave order <feature>      → ordem de dependência das stories
        └─ npx spec-wave implement <story>    → um claude interno por story
                │
                └─ move os cards do board (Projects v2) automaticamente
```

## Como funciona

O agente é **fino de propósito**: ele não conhece stories, tasks nem grafo de
dependências. Isso tudo é responsabilidade do prompt do orquestrador + do
spec-wave. O que o agente garante é a parte que um LLM não faz bem:

- **Exclusão mútua** entre agentes de devs diferentes (lease em git ref).
- **Fencing**: se perder o lease, mata a árvore de processos na hora.
- **Retomada**: se a máquina cair, o trabalho continua de onde parou.
- **Verificação de conclusão**: exit 0 não basta — exige um marker explícito.

### Fila

Duas fontes, escolhidas por `source` na configuração:

- **`github-label`** (default): a fila de labels descrita abaixo, num único
  repositório (`repo`).
- **`api`**: os despachos da tela **Development** do spec-wave para **você**
  (o login do GitHub vinculado ao dono do token de agente), em **qualquer
  produto** do tenant — ver [Fonte `api`](#fonte-api-tela-development-do-spec-wave).

Na fonte `github-label`: issues **abertas**, com a label `spec-wave:dev-agent` **e** a label de tipo
`[FEATURE]` ou `[BUG]`. **Bugs primeiro** — trabalho corretivo tem severidade,
feature nova não; uma FIFO pura por número deixaria um bug crítico atrás de
qualquer feature antiga. Dentro de cada tipo, FIFO por número. Um item por vez:
o agente pega o primeiro
que conseguir claimar e só volta à fila (com poll fresco) quando terminar.
`[STORY]`/`[TASK]` avulsas **não** entram na fila.

### Lease distribuído (a parte crítica)

Cada feature claimada cria a ref `refs/heads/spec-wave-agent/claims/<n>`
apontando para um commit que contém só um `lease.json`
(`{issue, owner, generation, heartbeat}`), construído com git plumbing
(`hash-object`/`mktree`/`commit-tree`) — sem worktree.

| Operação | Mecanismo | Semântica |
|---|---|---|
| Adquirir | `git push` **não-forçado** | falha atômica se a ref existe (perder a corrida é normal, não é erro) |
| Renovar / roubar | `git push --force-with-lease=<ref>:<sha>` | CAS contra o sha exato observado |
| Fencing token | campo `generation` | incrementa a cada aquisição/roubo |

Um heartbeat renova o lease a cada `heartbeat_secs`. O `renew` distingue
**lease perdido** (CAS rejeitado, dono/generation divergente, ref sumiu ⇒
fencing imediato, nunca retry) de **erro transiente de rede** (retry dentro de
um orçamento de `lease_ttl_secs − 2×heartbeat_secs`). Estourou o orçamento ⇒
auto-fencing conservador, sempre **antes** de qualquer roubo legal ser
possível — um roubo exige `lease_ttl_secs` sem heartbeat gravado no remoto.

### Execução e desfechos

O trabalho acontece num clone próprio (`workdir/issue-<n>`) no branch
`agent/issue-<n>`. O executor (`feature_command`, default `claude -p …`) é
lançado como **líder de grupo de processos** e recebe o `feature_prompt` pelo
**stdin**; o output em `stream-json` é formatado para o console
(`[#9] ⏵ Bash: npx spec-wave implement 320`).

| Desfecho | O que o agente faz |
|---|---|
| **Sucesso** (marker `{"status":"ok"}`) | checkpoint de segurança, **abre o PR** do branch, remove a label da fila, libera o lease |
| **Rodada incompleta** (exit 0 sem marker) | relança o executor, que retoma pelo `git log` + `spec-wave order` |
| **Falha** (exit ≠ 0, timeout, marker `partial`) | checkpoint (commit+push do WIP), comentário na issue, **mantém a label** (segue na fila), libera o lease |
| **SIGTERM** (desligar a máquina) | checkpoint + release imediato ⇒ takeover instantâneo por outro agente |
| **Lease perdido** | mata a árvore de processos e **não faz nada** — o novo dono manda no branch |

**Um PR por issue, não por story.** O trabalho inteiro vive num branch só
(`agent/issue-<n>`), então é dele que sai o PR — aberto no sucesso, com o título
da issue. É o PR que dá conteúdo à etapa 👀 Code Review: sem ele, as Stories
chegam lá e a fila do Tech Leader mostra "sem PR" em todas. É idempotente (PR já
aberto ⇒ não abre outro) e best-effort: o trabalho já está pushado, então falha
ao abrir vira aviso, não derruba a execução. O corpo **não** usa `Closes`/`Fixes`
— o merge não encerra o item, que ainda percorre QA, Homologação e Deploy.

O **kill é sempre da árvore inteira**, porque o executor spawna `npx spec-wave`
que spawna o claude interno: em Unix, SIGKILL no grupo de processos (o filho é
líder via `process_group(0)`); em Windows, `TerminateJobObject` num job object
com `KILL_ON_JOB_CLOSE` ao qual o filho e todos os netos pertencem. Matar só o
filho direto deixaria netos órfãos trabalhando no mesmo branch — buraco de
fencing.

Por que as rodadas existem: uma chamada Bash do Claude Code tem teto de ~10
min, e um `spec-wave implement` real leva mais. O orquestrador às vezes encerra
o turno com trabalho pendente; o agente então o relança em vez de dar a feature
como concluída. Proteções: estagnação (2 rodadas sem commit novo) e teto de
`max_executor_rounds`.

## Requisitos na máquina

Checados no boot — o agente falha rápido com mensagem clara:

- `git` e `gh` autenticados (o agente usa as credenciais do dev)
- Node 18+ (`npx spec-wave`)
- Claude Code instalado e logado
- No repositório alvo: `.spec-wave.json` com `specKit.command` (o executor
  interno de cada story) — ou a env `SPEC_WAVE_IMPLEMENT_CMD`

## Instalação

### Recomendado: pelo spec-wave CLI

A partir de um repositório já inicializado (`spec-wave init`):

```bash
npx @spec-wave/cli@latest dev-agent --install            # binário + config
npx @spec-wave/cli@latest dev-agent --install --service  # + systemd/launchd
```

Baixa o binário da release desta repo (`gh release download`, com as
credenciais do próprio dev), instala em `~/.local/bin` (sem sudo), gera
`~/.config/spec-wave-agent/config.toml` já com o `owner/repo` do
`.spec-wave.json` e checa os pré-requisitos. Rodar depois:

```bash
npx @spec-wave/cli@latest dev-agent --run   # foreground, Ctrl+C = checkpoint
```

### Build do fonte

Pelo CLI (clona este repo em `~/.local/share/spec-wave-agent/src`, compila com
cargo e instala em `~/.local/bin`) — use quando não houver binário para a sua
plataforma (Mac Intel, Linux arm64) ou para rodar o código mais recente:

```bash
npx @spec-wave/cli@latest dev-agent --build             # branch main
npx @spec-wave/cli@latest dev-agent --build --tag v0.1.0  # tag específica
```

Ou direto, para quem desenvolve o agente:

```bash
make build            # cargo build --release
make install          # instala o binário em /usr/local/bin (PREFIX ajustável)
make install-config   # cria ~/.config/spec-wave-agent/config.toml se não existir
```

Releases são publicadas pelo workflow `.github/workflows/release.yml` ao
empurrar uma tag `vX.Y.Z` (assets: linux-x64, darwin-arm64 e windows-x64).
Mac Intel compila do fonte.


## Configuração

`~/.config/spec-wave-agent/config.toml` — só `repo` é obrigatório na fonte
`github-label` (a fonte `api` pede `api_url` + `agent_token`). Schema
completo comentado em [`packaging/config.example.toml`](packaging/config.example.toml).

```toml
repo = "sua-org/seu-repo"
```

| Chave | Default | Para que serve |
|---|---|---|
| `source` | `github-label` | `github-label` (fila de labels) ou `api` (tela Development do spec-wave) |
| `repo` | — | `owner/repo` (obrigatório na fonte `github-label`) |
| `api_url` | — | base da API do agente, ex.: `https://app.specwave.dev/agent-api` (fonte `api`) |
| `agent_token` | — | token de agente pessoal `swa_…` (fonte `api`); `SPEC_WAVE_AGENT_TOKEN` tem precedência |
| `live_url` | host do `api_url` | base do Session Gateway para a sessão ao vivo (fonte `api`), ex.: `wss://gateway.interno` |
| `remote_url` | GitHub | override do remoto (testes / git self-hosted). Na fonte `api` exige o placeholder `{repo}`, ex.: `git@git.interno:{repo}.git` |
| `queue_label` | `spec-wave:dev-agent` | label que marca a fila |
| `poll_interval_secs` | `60` | intervalo de consulta quando ocioso |
| `poll_backoff_max_secs` | `480` | teto do backoff quando a fila volta VAZIA (dobra a cada rodada vazia seguida; fila com item de outro agente não conta) |
| `heartbeat_secs` | `120` | renovação do lease |
| `lease_ttl_secs` | `600` | sem heartbeat por este tempo ⇒ pode ser roubado |
| `implement_timeout_secs` | `14400` | teto de **uma feature inteira** (4h) |
| `bug_timeout_secs` | `3600` | teto de **um bug** (1h) |
| `bug_command` | `claude -p …` | executor de bug; mesmo formato do `feature_command` |
| `bug_prompt` | (prompt das 4 fases) | reproduzir → causa raiz → fix mínimo → teste de regressão |
| `feature_command` | `claude -p …` | executor; argv por espaço, aspas agrupam, **sem shell**. `{issue}` |
| `feature_prompt` | (prompt de orquestração) | enviado via stdin. `{issue}` |
| `cooldown_secs` | `900` | issue processada sai da fila local por este tempo |
| `max_executor_rounds` | `8` | teto de rodadas por feature |
| `max_failures_per_issue` | `3` | falhas seguidas antes de tirar a issue da fila |
| `workdir` | `~/.spec-wave-agent` | clones por issue + `lease-repo` |
| `pr_draft` | `false` | abre o PR do trabalho como **rascunho** — CI só quando alguém marca "pronto" (exige `if: !draft` + `ready_for_review` no job do check do repo) |
| `agent_id` | `usuário@hostname` | identidade nos leases e comentários |
| `remote_url` | `https://github.com/{repo}.git` | override (SSH, git self-hosted, testes) |

Validado no boot: `lease_ttl_secs >= 4 × heartbeat_secs`, formato `owner/repo`,
intervalos > 0, `poll_backoff_max_secs >= poll_interval_secs`, `feature_command`
e `bug_command` parseáveis, `bug_prompt` não-vazio.

**Fixar o modelo** (útil quando a cota do default esgota) — lembre das **duas**
camadas:

```toml
feature_command = "claude -p --model opus --output-format stream-json --verbose --permission-mode acceptEdits --allowedTools \"Bash(npx:*),Bash(git:*),Edit,Write,Read,Glob,Grep,Task\""
```
```bash
export SPEC_WAVE_IMPLEMENT_CMD='claude -p --model opus "Implemente as tarefas do arquivo {tasksFile}. Não faça commit nem push." --permission-mode acceptEdits'
```

**Repositório em organização**: o `gh` pode ter várias contas e a ativa nem
sempre enxerga a org. Rode o agente com o token certo no ambiente:

```bash
export GH_TOKEN=$(gh auth token -u SUA-CONTA-DA-ORG)
```

## Executar o agente

### Foreground (logs ao vivo no console)

```bash
spec-wave-agent                    # nível info
RUST_LOG=debug spec-wave-agent     # verboso
spec-wave-agent --help             # opções
```

Para trabalhar em **mais de um repositório**, aponte o agente para outra config
em vez de sobrescrever a que já está em uso:

```bash
spec-wave-agent --config ~/.config/spec-wave-agent/outro-repo.toml
SPEC_WAVE_AGENT_CONFIG=~/.config/spec-wave-agent/outro-repo.toml spec-wave-agent
```

Precedência: `--config` → `SPEC_WAVE_AGENT_CONFIG` → o default
`~/.config/spec-wave-agent/config.toml`. Cada instância precisa de um `workdir`
próprio na config — dois agentes compartilhando o mesmo diretório de clones
brigariam pelos mesmos branches.

Os logs do agente saem com timestamp e nível; o que o Claude Code faz aparece
intercalado como `[#<issue>] …` (texto do orquestrador, tool calls e, no fim,
custo e número de turnos).

### Como serviço

```bash
make install-systemd    # Linux — journalctl --user -u spec-wave-agent -f
make install-launchd    # macOS  — tail -f ~/Library/Logs/spec-wave-agent.log
```

Units em [`packaging/`](packaging/). Se o Node vem de nvm/volta, ajuste o
`Environment=PATH=…`. Para o token da org, use um drop-in (fora do repo, com
permissão restrita):

```bash
systemctl --user edit spec-wave-agent
# [Service]
# Environment=GH_TOKEN=<token>
```

`SIGTERM` (parar o serviço, desligar a máquina) faz checkpoint e libera o lease
— por isso a unit usa `TimeoutStopSec=30`.

## Como fazer o agente pegar uma tarefa

1. A Feature precisa estar **decomposta** em user stories (sub-issues) — o
   fluxo spec-wave faz isso com a label `spec-wave:decompose`. Sem stories,
   o `implement` não tem o que fazer.
2. Na **issue da Feature** (não no card do Project, não numa Story), aplique a
   label **`spec-wave:dev-agent`**. A issue precisa ter também a label de tipo
   `[FEATURE]` ou `[BUG]` e estar aberta.
3. Pronto. No próximo poll (≤ 1 min por default) o agente claima e começa.

Acompanhe por três lugares:

- **Console/journal**: `claim OK: issue #N` e o streaming do orquestrador.
- **Board (Projects v2)**: Feature e Story vão para 🚧 Desenvolvimento ao
  começar; Tasks → 🎉 Done e Story → 👀 Code Review ao concluir. Quem move é o
  `spec-wave implement` (0.9+), em código — não dependa do LLM para isso.
- **spec-wave-ui**: reflete o board (a story ativa mostra "Desenvolvimento em
  andamento…" e o percentual da Feature sobe a cada story concluída).

Ao final, com sucesso, a label `spec-wave:dev-agent` **sai da issue** e o
branch `agent/issue-<n>` fica pronto para o PR (a Feature avança para
👀 Code Review quando o PR é aberto, via Action do spec-wave).

Se falhar, a label **continua** na issue (segue na fila) e um comentário
explica o motivo; o trabalho parcial está pushado no branch. Reiniciar o
agente retoma de onde parou — stories já concluídas são puladas.

## Fonte `api` (tela Development do spec-wave)

Em vez de label, quem decide o que o agente faz é a tela **Development** do
spec-wave (RFC-008): alguém arrasta a Feature (ou o Bug) do Backlog para o WIP
e escolhe **você** como executor. O agente recebe pela API, em qualquer
produto do tenant.

1. No spec-wave, em **Configurações → Minha conta**: vincule o seu login do
   GitHub e crie um **token de agente** (um por máquina). Ele aparece uma vez.
2. Na configuração do agente:

   ```toml
   source = "api"
   api_url = "https://<seu-spec-wave>/agent-api"
   agent_token = "swa_…"   # ou exporte SPEC_WAVE_AGENT_TOKEN
   ```

3. Rode o agente. No seletor de executor da tela Development você passa a
   aparecer como **online**.

O que muda em relação à fonte por label:

- **Vários repositórios.** Cada produto tem diretório de trabalho e lease
  próprios em `<workdir>/repos/<owner>__<repo>/` — a issue #12 de dois
  produtos nunca cai no mesmo clone.
- **Heartbeat para o spec-wave.** O card mostra *Rodando*, o host e a idade do
  último heartbeat. Se o despacho for **cancelado** (o card volta ao
  Backlog), refeito ou redirecionado, a resposta do heartbeat manda parar: o
  agente mata o executor, faz checkpoint no branch e libera o lease.
- **Desfechos no card.** Sucesso → *Review*; falha → continua em WIP (a falha é
  contada); depois de `max_failures_per_issue` falhas → *Blocked*, com o
  motivo. Não há label para tirar ou pôr — o card é a fila.
- **Sessão ao vivo.** A saída `stream-json` do executor é transmitida para o
  Session Gateway, e a tela Development mostra a sessão num drawer (somente
  leitura), para qualquer pessoa do tenant. É melhor esforço: sem Gateway, com
  rede ruim ou recusado, o trabalho segue igual e o drawer fica sem mensagens.
  O endereço do Gateway sai do `api_url` (o CloudFront manda `/ws/*` para ele);
  `live_url` sobrescreve quando o Gateway está em outro host. **Cada story ao
  vivo** exige o `spec-wave` CLI ≥ 1.4: o agente define `SPEC_WAVE_STREAM_DIR`
  para o executor, o `spec-wave implement` grava ali o stream do agente
  interno de cada story (`story-<n>.jsonl`), e o agente o retransmite com a
  origem — o drawer separa a sessão por story. Com CLI mais antigo, o drawer
  mostra só o orquestrador.
- **Lease igual.** A API diz o que fazer; o lease em git ref continua sendo o
  que garante um dono por vez (inclusive entre duas máquinas suas).
- **User-Agent.** O agente se identifica como `spec-wave-agent/<versão>`; o
  WAF do spec-wave recusa requisições sem User-Agent.
- **PRs no card.** No sucesso, o desfecho leva as URLs dos PRs abertos, e o
  card em *Review* mostra os links.

## Modo `--once` (Fleet Job da frota)

Na frota do spec-wave (RFC-008), o `spec-wave-sandbox` do cliente cria um Job
por execução, e dentro dele o `fleet-runner` roda:

```bash
spec-wave-agent --once [--config agent.toml]
```

O agente faz uma execução só — lease, executor, rodadas, checkpoint e PR,
exatamente como no daemon — e sai. A execução vem do ambiente que o sandbox
monta: `SPECWAVE_HUB_REPO`, `SPECWAVE_WORK_ITEM`, `SPECWAVE_KIND`, `RUN_ID` e
`FLEET_WORKDIR` (o clone fica em `$FLEET_WORKDIR/agent`, o stream das stories
em `$FLEET_WORKDIR/streams/<RUN_ID>`). Sem `--config`, valem os defaults.

Ele **não** fala com a API do spec-wave: emite eventos JSONL no stdout (os
logs continuam no stderr), que o `fleet-runner` traduz para o sandbox:

```json
{"event":"claimed","generation":1}
{"event":"line","origin":null,"line":"<stream-json do orquestrador>"}
{"event":"line","origin":"story:3","line":"<stream-json da story>"}
{"event":"outcome","state":"succeeded","reason":null,"pr_urls":["https://…/pull/7"]}
```

`state`: `succeeded`, `failed`, `blocked`, `canceled` (SIGTERM — checkpoint
pushado e lease liberado) ou `skipped` (outro agente já tem o lease, ou o
lease foi perdido). Um erro de infraestrutura também sai como `failed`.

## Desenvolvimento

Crate lib + bin: `src/lib.rs` expõe os módulos (para os testes de integração) e
`src/main.rs` só orquestra.

| Arquivo | Responsabilidade |
|---|---|
| `src/main.rs` | init do tracing, preflight, heartbeat, loop de poll, shutdown |
| `src/config.rs` | Config, validação, `split_command`, `render_template` |
| `src/lease.rs` | **o módulo dos invariantes** — CAS, renew/steal, `RenewError` |
| `src/queue.rs` | `poll_queue` + `parse_queue` (pura) |
| `src/runner.rs` | workspace, checkpoint, rodadas do executor, streaming, `kill_tree` |
| `src/shell.rs` | `run`/`run_ok` + `retry_backoff` |

```bash
cargo test                          # unit + integração (sem rede)
cargo test --test lease_integration # protocolo de lease contra bare repo local
cargo test --test feature_integration
cargo test --test bug_integration
cargo test roubo_apos_expirar       # um teste específico
```

Os testes de integração usam um **bare repo local** como origin (via
`remote_url`) e um stub no lugar do Claude Code (via `feature_command`) — não
tocam a rede nem o GitHub. Cobrem: corrida entre dois agentes (exatamente um
vence), roubo após expirar, fencing do dono antigo, rodadas com/sem progresso,
marker ausente/parcial e morte da árvore de processos no timeout.

Ao mexer no código, três invariantes não podem ser quebrados:

1. **Aquisição é push não-forçado**; renovação/roubo é `--force-with-lease` no
   sha exato. Nunca embrulhe os pushes de CAS em retry cego.
2. **`RenewError::Lost` ⇒ fencing imediato**, sem checkpoint, sem push, sem
   release. Esse caminho é livre de efeitos colaterais por definição.
3. **Kill é do grupo de processos** (`kill_tree`), porque o executor spawna
   `npx spec-wave` que spawna o claude interno — matar só o filho direto deixa
   netos órfãos trabalhando no mesmo branch.

Comentários e mensagens de log são em português; mantenha o padrão.
