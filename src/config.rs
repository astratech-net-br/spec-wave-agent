//! Configuração do agente (~/.config/spec-wave-agent/config.toml).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// De onde o agente recebe trabalho.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Issues abertas com a label da fila no repositório `repo` (RFC-001).
    #[default]
    GithubLabel,
    /// Despachos da tela Development do spec-wave para o login do dono do
    /// token (RFC-008 fase 2) — em qualquer produto do tenant.
    Api,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Fonte de trabalho. Default: label no GitHub (comportamento original).
    #[serde(default)]
    pub source: Source,
    /// "owner/repo" — obrigatório em `source = "github-label"`. Em
    /// `source = "api"` o repositório vem de cada item despachado.
    #[serde(default)]
    pub repo: String,
    /// Base da API do agente no spec-wave, ex.:
    /// "https://app.specwave.dev/agent-api" (só `source = "api"`).
    pub api_url: Option<String>,
    /// Token de agente pessoal (Configurações → Minha conta → Tokens de
    /// agente). A variável SPEC_WAVE_AGENT_TOKEN tem precedência — útil para
    /// não deixar o segredo no arquivo.
    pub agent_token: Option<String>,
    /// Base do Session Gateway para a transmissão ao vivo (fonte api), ex.:
    /// "wss://gateway.interno". Default: o host de `api_url` (em produção o
    /// CloudFront manda `/ws/*` do domínio do app para o Gateway).
    pub live_url: Option<String>,
    /// Label que marca itens na fila (aplicada por humano ou automação do board)
    #[serde(default = "d_queue_label")]
    pub queue_label: String,
    #[serde(default = "d_poll")]
    pub poll_interval_secs: u64,
    /// Teto do backoff quando o poll volta com a fila VAZIA (dobra a cada
    /// rodada vazia seguida, a partir de `poll_interval_secs`, até este
    /// teto — zera assim que a fila tiver algo de novo). Fila cheia sem
    /// claim (outro agente já pegou tudo) e erro de poll NÃO entram no
    /// backoff — só fila genuinamente vazia é ociosidade.
    #[serde(default = "d_poll_backoff_max")]
    pub poll_backoff_max_secs: u64,
    #[serde(default = "d_heartbeat")]
    pub heartbeat_secs: u64,
    /// Sem heartbeat por este tempo => lease considerado morto (pode roubar)
    #[serde(default = "d_ttl")]
    pub lease_ttl_secs: i64,
    /// Tempo máximo de UMA FEATURE inteira (todas as stories)
    #[serde(default = "d_impl_timeout")]
    pub implement_timeout_secs: u64,
    /// Comando do executor da feature — argv separado por espaço (aspas
    /// simples/duplas suportadas; SEM shell). Placeholder: {issue}
    #[serde(default = "d_feature_command")]
    pub feature_command: String,
    /// Prompt de orquestração enviado ao executor via stdin. Placeholder: {issue}
    #[serde(default = "d_feature_prompt")]
    pub feature_prompt: String,
    /// Comando do executor de BUG. Mesmo formato do feature_command; separado
    /// para permitir modelo/ferramentas diferentes numa correção.
    #[serde(default = "d_bug_command")]
    pub bug_command: String,
    /// Prompt de correção enviado ao executor via stdin. Placeholder: {issue}
    #[serde(default = "d_bug_prompt")]
    pub bug_prompt: String,
    /// Tempo máximo de UM BUG. Menor que o de feature por natureza: um defeito
    /// que passa de uma hora quase sempre está mal delimitado, e o teto serve
    /// para o agente devolver a issue à fila em vez de queimar o dia nela.
    #[serde(default = "d_bug_timeout")]
    pub bug_timeout_secs: u64,
    /// Comando do executor de REVISÃO de PR (modo frota, `SPECWAVE_KIND=review`).
    /// Mesmo formato do feature_command; o default libera também o `gh`, que a
    /// revisão usa para ler o PR e publicar o review.
    #[serde(default = "d_review_command")]
    pub review_command: String,
    /// Prompt de revisão enviado ao executor via stdin. Placeholder: {issue}
    #[serde(default = "d_review_prompt")]
    pub review_prompt: String,
    /// Tempo máximo de UMA revisão de PR (uma rodada de review + correções).
    #[serde(default = "d_review_timeout")]
    pub review_timeout_secs: u64,
    /// Comando da execução de QA (modo frota, `SPECWAVE_KIND=qa`): a CLI roda
    /// o plano de QA e grava o desfecho em JSON. Placeholders: {cli} {issue}.
    #[serde(default = "d_qa_command")]
    pub qa_command: String,
    /// Executor que a CLI chama para rodar os cenários (vai em
    /// `SPEC_WAVE_QA_CMD`) quando o `.spec-wave.json` do hub não define
    /// `qa.command`. Os placeholders ({contextFile} etc.) são da CLI.
    #[serde(default = "d_qa_executor")]
    pub qa_executor: String,
    /// Tempo máximo de UMA execução de QA (todos os cenários).
    #[serde(default = "d_qa_timeout")]
    pub qa_timeout_secs: u64,
    /// Comando de uma etapa do fluxo na frota (`SPECWAVE_KIND=flow:<etapa>`).
    /// Placeholders: {cli} {issue} {step}. Exit 0 = concluída; 2 = recusa do
    /// fluxo (já comentada na issue pela CLI); outro = falha.
    #[serde(default = "d_flow_command")]
    pub flow_command: String,
    #[serde(default = "d_flow_timeout")]
    pub flow_timeout_secs: u64,
    /// A etapa desta execução (`flow:<etapa>` do ambiente) — só no `--once`.
    #[serde(skip)]
    pub flow_step: Option<String>,
    /// Depois de processar uma issue (qualquer desfecho), ela fica fora da
    /// fila deste agente por este tempo — evita re-claim imediato por atraso
    /// do índice de busca do GitHub e loop de retry em falha.
    #[serde(default = "d_cooldown")]
    pub cooldown_secs: u64,
    /// Máximo de rodadas do executor por feature: exit 0 sem marker relança
    /// o executor para continuar (ele retoma pelo estado do git/spec-wave).
    #[serde(default = "d_max_rounds")]
    pub max_executor_rounds: u32,
    /// Quantas falhas SEGUIDAS na mesma issue antes de tirá-la da fila.
    ///
    /// Falha mantém a label de propósito — a maioria é transitória e outro
    /// agente (ou o mesmo, depois) retoma do checkpoint. Mas há falha
    /// DETERMINÍSTICA: o executor concluir que não há como reproduzir o
    /// defeito, por exemplo. Essa nunca vai passar, e sem um teto a issue é
    /// re-claimada a cada cooldown para sempre, pagando um executor inteiro
    /// por tentativa.
    #[serde(default = "d_max_failures")]
    pub max_failures_per_issue: u32,
    /// Diretório de trabalho do agente
    #[serde(default = "d_workdir")]
    pub workdir: String,
    /// Abre o PR do trabalho concluído como RASCUNHO (`gh pr create --draft`).
    /// Rascunho não mergeia até alguém marcar "pronto para revisão", e um
    /// repositório que pule draft no job do required check (types com
    /// ready_for_review + `if: !draft` no job) só paga CI quando a revisão
    /// começa — economia real quando o trabalho chega em pilha. Default false:
    /// quem não configurou nada mantém o comportamento de sempre.
    #[serde(default = "d_pr_draft")]
    pub pr_draft: bool,
    /// Comando que abre o PR da branch de trabalho pela porta de SCM da CLI
    /// (RFC-008 P17) — o agente acrescenta `--repo`, `--head`, `--work-item`,
    /// `--hub`, `--body`, `--json` e `--draft`. Mesmo formato do
    /// feature_command (sem shell). Se a CLI não tiver o comando (anterior à
    /// 1.7.0) ou falhar, o agente cai no `gh pr create` de antes.
    #[serde(default = "d_pr_command")]
    pub pr_command: String,
    /// Comando do retrato de progresso da Feature (`{issue}` vira o número):
    /// quantas Tasks ela tem e quais já estão concluídas — a base da barra de
    /// progresso da tela Development (progress.rs). Mesmo formato do
    /// feature_command (sem shell). Vazio desliga o progresso por Task; CLI
    /// sem o comando (anterior à 1.9.0) deixa o card sem a barra por Task.
    #[serde(default = "d_progress_command")]
    pub progress_command: String,
    /// Como chamar a CLI do spec-wave — o que entra no lugar de `{cli}` nos
    /// prompts e comandos (feature/bug, pr_command, progress_command) e vai
    /// para o executor em SPEC_WAVE_CLI. Ausente: o `spec-wave` do PATH pelo
    /// caminho absoluto e, sem ele, `npx spec-wave` (ver `detect_cli`).
    pub cli_command: Option<String>,
    /// Identidade do agente (default: usuario@hostname)
    pub agent_id: Option<String>,
    /// Override da URL do remoto (testes / git self-hosted). Default: GitHub.
    pub remote_url: Option<String>,
    /// Namespace das refs de lease. Default `refs/heads/spec-wave-agent/claims`
    /// (aceito por todo host git). `refs/spec-wave/claims` tira as refs da
    /// lista de branches e não dispara CI — passou na suíte de conformidade
    /// do GitHub (tests/lease_conformance.rs); em outro host, rode a suíte
    /// antes de trocar. Todos os agentes de um repositório precisam usar o
    /// MESMO prefixo, senão o lease deixa de ser exclusivo.
    #[serde(default = "d_lease_prefix")]
    pub lease_ref_prefix: String,
    /// Gate de CI: depois do marker "ok" o agente abre o PR, ESPERA os checks
    /// do commit de cabeça e só então declara sucesso. Check vermelho relança
    /// o executor com o log da falha (até `max_ci_fix_rounds` vezes).
    ///
    /// Existe porque o marker é a palavra do executor, e ela não basta: sem o
    /// gate, uma feature saía como concluída com o CI do PR vermelho — o CI só
    /// roda depois que o PR abre, quando o agente já tinha ido embora.
    #[serde(default = "d_ci_gate")]
    pub ci_gate: bool,
    /// Comando que consulta os checks de um PR — o agente acrescenta
    /// `<url-do-pr> --json headRefOid,statusCheckRollup`. Mesmo formato do
    /// feature_command (sem shell). Outro SCM: aponte para um comando que
    /// devolva o mesmo JSON. Comando ausente ou falhando NÃO reprova a
    /// execução: o gate é pulado com aviso, como o PR é best-effort.
    #[serde(default = "d_checks_command")]
    pub checks_command: String,
    /// Intervalo entre consultas aos checks.
    #[serde(default = "d_ci_poll")]
    pub ci_poll_secs: u64,
    /// Teto de espera por checks EM ANDAMENTO. Estourou: o agente segue como
    /// sucesso, com aviso — CI lento ou fila de runner parada não é defeito do
    /// trabalho, e o PR fica aberto mostrando o estado real.
    #[serde(default = "d_ci_timeout")]
    pub ci_timeout_secs: u64,
    /// Quanto esperar o PRIMEIRO check aparecer no commit de cabeça. Passou
    /// disso sem nenhum, o repositório não tem CI para este PR (ou pula
    /// rascunho) e o gate libera.
    #[serde(default = "d_ci_grace")]
    pub ci_no_checks_grace_secs: u64,
    /// Quantas vezes o executor é relançado para corrigir CI vermelho antes
    /// de a execução falhar.
    #[serde(default = "d_max_ci_fix_rounds")]
    pub max_ci_fix_rounds: u32,
}

fn d_lease_prefix() -> String { crate::lease::DEFAULT_LEASE_PREFIX.into() }

fn d_queue_label() -> String { "spec-wave:dev-agent".into() }
fn d_pr_command() -> String { "{cli} pr open".into() }
fn d_progress_command() -> String { "{cli} progress {issue} --json".into() }
fn d_poll() -> u64 { 60 }
fn d_poll_backoff_max() -> u64 { 480 } // 8x o d_poll() default — ver next_poll_delay em queue.rs
fn d_heartbeat() -> u64 { 120 }
fn d_ttl() -> i64 { 600 }
fn d_impl_timeout() -> u64 { 14400 }
fn d_executor_command() -> String {
    // stream-json: cada evento (texto, tool calls, resultado) vira uma linha
    // JSON no stdout, que o runner formata para o console (ver runner.rs).
    //
    // `Bash` inteiro, sem lista de prefixos: o prompt manda rodar a suíte do
    // repositório ("o comando que o repositório documenta" — npm, cargo, go,
    // make…), e em `-p` um comando fora da lista é NEGADO, não perguntado. A
    // lista antiga (npx/CLI/git) deixava o executor sem como validar o
    // próprio trabalho e não protegia nada: `npx` já roda qualquer coisa, e o
    // agente interno do `spec-wave implement` sempre teve Bash liberado.
    "claude -p --output-format stream-json --verbose \
     --permission-mode acceptEdits \
     --allowedTools \"Bash,Edit,Write,Read,Glob,Grep,Task\""
        .into()
}
fn d_feature_command() -> String { d_executor_command() }
fn d_bug_command() -> String { d_executor_command() }
fn d_bug_timeout() -> u64 { 3600 }
fn d_review_timeout() -> u64 { 3600 }
fn d_qa_timeout() -> u64 { 5400 }
fn d_flow_command() -> String { "{cli} step {step} --issue {issue}".into() }
fn d_flow_timeout() -> u64 { 3600 }

/// Nome de etapa aceito em `flow:<etapa>` (função PURA): começa por letra,
/// depois minúsculas e hífen. A letra inicial não é estética: `--help` passaria
/// no charset, viraria `spec-wave step --help`, sairia 0 e o run seria
/// "succeeded" sem rodar nada.
pub fn valid_flow_step(step: &str) -> bool {
    step.len() <= 40
        && step.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && step.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}
/// Arquivo do desfecho do `spec-wave qa --outcome-file` (raiz do clone do hub).
pub const QA_OUTCOME_FILE: &str = ".spec-wave-agent-qa.json";
fn d_qa_command() -> String { format!("{{cli}} qa {{issue}} --outcome-file {QA_OUTCOME_FILE}") }
fn d_qa_executor() -> String {
    // A CLI renderiza {contextFile} e roda pelo shell: o prompt vai como
    // argumento. Bash inteiro — o QA sobe a aplicação (qa.setup) e roda
    // Playwright; Edit/Write para os scripts descartáveis e o resultado.
    //
    // O prompt vem LOGO depois do `-p`: `--allowedTools` aceita vários
    // valores, e um prompt depois dele seria lido como mais uma ferramenta
    // ("was read as a value of --allowedTools, not as a prompt") — o executor
    // saía com erro sem gravar o resultado e todo QA da frota morria assim.
    "claude -p \"Execute o QA descrito em {contextFile}: siga as instruções do arquivo e \
     grave o resultado exatamente onde e como ele pede.\" \
     --permission-mode acceptEdits --allowedTools \"Bash,Edit,Write,Read,Glob,Grep\""
        .into()
}
// Mesmo comando do executor: com `Bash` inteiro, o `gh` de que a revisão
// precisa (ler o PR, publicar o review) já está liberado.
fn d_review_command() -> String { d_executor_command() }
fn d_review_prompt() -> String {
    "Você está no clone do repositório hub da Feature #{issue}. Os repositórios \
     de código que ela toca, quando existem, estão em `.spec-wave/repos/<slug>/`.\n\
     Sua tarefa é REVISAR os Pull Requests abertos desta Feature e CORRIGIR o \
     que a revisão encontrar — UMA rodada, sem aprovar e sem mergear.\n\
     \n\
     1. ACHAR OS PRs — em cada repositório (o hub e cada um em \
     `.spec-wave/repos/`), liste os PRs abertos com \
     `gh pr list --state open --json number,url,headRefName,isDraft,body,isCrossRepository`. \
     Os itens da Feature são ela mesma, as Stories dela (`{cli} order {issue}`) \
     e os filhos de cada Story — Bug aberto pelo QA, Spike — \
     (`gh api repos/{owner}/{repo}/issues/<story>/sub_issues --jq '.[].number'`, \
     no clone do hub). É elegível o PR cuja branch é `agent/issue-N` ou cujo \
     corpo fecha o item #N (`Closes <hub>#N`), para N um desses itens. \
     NÃO são elegíveis: rascunho (draft), \
     branch `spec-wave/...` (PR de documento) e PR de fork. Se não houver \
     nenhum PR elegível, escreva o marker com `\"prs\": []` e PARE.\n\
     2. ENTENDER — leia a spec e o plan da Feature (`docs/features/<slug>/`) e \
     o diff de cada PR (`gh pr diff <n>`). A régua da revisão é: o código faz \
     o que a spec e o plan pedem, sem defeito e sem quebrar o que existia.\n\
     3. REVISAR — procure defeitos reais: erro de lógica, requisito da spec não \
     atendido ou atendido errado, caso de borda sem tratamento, falha de \
     segurança, teste que não testa o que diz. Não aponte gosto pessoal nem \
     estilo que o projeto já aceita. Publique UM review por PR com os achados: \
     `gh pr review <n> --comment --body \"...\"` — cada achado com arquivo, \
     linha e o porquê. Se não houver achado, o review diz isso em uma linha.\n\
     4. CORRIGIR — para cada achado que você mesmo apontou, faça a correção na \
     branch do PR (`gh pr checkout <n>` no clone daquele repositório). UM \
     commit por correção: \"fix: <o que estava errado> (#{issue}) \
     [spec-wave-agent]\". Rode a suíte de testes do repositório antes de dar \
     push. Achado que você não conseguir corrigir com segurança fica no review \
     como pendente — não force uma correção duvidosa.\n\
     5. FECHAR — comente em cada PR o resumo final, começando EXATAMENTE por \
     `<!-- spec-wave:auto-review -->` na primeira linha: o que foi revisado, \
     quantos achados, quais foram corrigidos (com o hash do commit) e quais \
     ficaram pendentes para o humano. Use `gh pr comment <n> --body-file`.\n\
     \n\
     Regras OBRIGATÓRIAS:\n\
     - NUNCA aprove (`gh pr review --approve`), NUNCA mergeie, NUNCA feche e \
     NUNCA abra um PR novo. Aprovação e merge são de um humano.\n\
     - NUNCA use `git push --force` nem reescreva o histórico da branch do PR.\n\
     - Não mexa em nada fora do diff do PR além do necessário para corrigir um \
     achado. Nada de refatoração de oportunidade.\n\
     - PR já revisado é o que tem um comentário começando por \
     `<!-- spec-wave:auto-review -->` MAIS NOVO que o último commit dele \
     (compare o `createdAt` do comentário com o `committedDate` do último \
     commit: `gh pr view <n> --json comments,commits`). Esse você não revisa \
     de novo: só o inclua em `prs`. Se o comentário é mais antigo que o último \
     commit (merge da base, correção posterior), revise de novo o que mudou \
     desde a revisão anterior e feche com um comentário novo — o merge \
     automático só aceita o commit que a revisão viu.\n\
     - Você roda em RODADAS: se o seu turno terminar sem o marker de \
     conclusão, você será REINVOCADO. Ao iniciar, verifique o que já foi feito \
     (`git log --oneline -20` e os comentários do PR) e continue de onde parou.\n\
     - Processos em background que você deixar ao encerrar o turno são \
     MORTOS. Execute em foreground e aguarde.\n\
     - Ao terminar, volte o clone do hub para a branch em que ele estava e \
     escreva o arquivo `.spec-wave-agent-result.json` na raiz do clone do hub \
     com: {\"status\": \"ok\", \"prs\": [\"<url de cada PR revisado>\"], \
     \"detalhe\": \"<achados e correções em uma frase>\"}. NÃO commite esse \
     arquivo.\n"
        .into()
}
fn d_cooldown() -> u64 { 900 }
fn d_max_rounds() -> u32 { 8 }
fn d_max_failures() -> u32 { 3 }
fn d_pr_draft() -> bool { false }
fn d_ci_gate() -> bool { true }
fn d_checks_command() -> String { "gh pr view".into() }
fn d_ci_poll() -> u64 { 30 }
fn d_ci_timeout() -> u64 { 1800 }
fn d_ci_grace() -> u64 { 120 }
fn d_max_ci_fix_rounds() -> u32 { 3 }
fn d_feature_prompt() -> String {
    "Você está no clone do repositório, no branch de trabalho da Feature #{issue}.\n\
     Implemente a feature completa usando o spec-wave:\n\
     1. Rode `{cli} order {issue}` para obter as user stories e a \
     ordem de dependência.\n\
     2. Implemente cada story rodando `{cli} implement <número>`, \
     respeitando a ordem: uma story só pode começar depois que TODAS as suas \
     dependências estiverem concluídas.\n\
     3. Stories independentes entre si podem ser implementadas em paralelo \
     com sub-agentes.\n\
     4. Ao concluir cada story: rode a suíte de testes COMPLETA do projeto \
     (o comando que o repositório documenta — não só os testes novos da \
     story) e só commite com ela verde. Teste que falha é trabalho \
     pendente: corrija antes do commit. Commite neste branch (assunto \
     livre; inclua no RODAPÉ os `git trailer` que o contexto de \
     `{cli} implement <número>` pedir — \
     Spec-Wave-Story/Spec-Wave-Tasks/Spec-Wave-Agent, copiados literalmente) \
     e faça push.\n\
     5. Se uma story falhar, pule as que dependem dela e continue as \
     independentes; ao final, relate o que falhou.\n\
     6. Se `{cli} implement <número>` sair com código 3 e a linha \
     \"BLOQUEADA: Story #X aguarda #N\", a dependência dela ainda NÃO tem \
     código na base (outra Feature vai entregá-lo). PARE a Feature ali: não \
     implemente essa story nem as seguintes da ordem, mesmo as \
     independentes, e não crie o código que falta. Commite e pushe o que já \
     estava pronto e escreva o marker {\"status\": \"blocked\", \
     \"detalhe\": \"<a linha BLOQUEADA>\"}. O agente reporta a espera e a \
     Feature é retomada depois do merge da dependência — NÃO use \
     `--ignore-deps`.\n\
     \n\
     Regras OBRIGATÓRIAS:\n\
     - ESCOPO: implemente só o que a Feature #{issue} e as stories dela \
     pedem. Se uma story pressupõe código que pertence a OUTRA Feature \
     (módulo, serviço, rota, tipo, tabela, tela — ex.: \"o módulo de X da \
     FEAT-001\", \"a tela Y\" ou uma Feature listada em `Depende de:` ou no \
     aviso \"Dependências pendentes\" do contexto) e esse código NÃO existe \
     no branch, NÃO o crie, nem numa versão mínima: outra Feature vai \
     entregá-lo e duas versões do mesmo módulo colidem no merge. Trate a \
     story como BLOQUEADA: não commite nada dela, pule as que dependem \
     dela e continue as independentes. No marker, use \"partial\" com o \
     detalhe \"bloqueada: depende de <o que falta> da Feature #<N>, ausente \
     na base\" para cada story bloqueada. (Quando o próprio \
     `{cli} implement` acusa o bloqueio — código 3 —, vale o passo 6: pare.)\n\
     - Você roda em RODADAS: se o seu turno terminar sem o marker de \
     conclusão (abaixo), você será REINVOCADO para continuar. Ao iniciar, \
     verifique o que já foi feito (`git log --oneline -20` e \
     `{cli} order {issue}`) e continue de onde parou — não refaça \
     stories já commitadas.\n\
     - Commit NÃO prova que os testes passam. Antes de escrever o marker — \
     inclusive numa rodada em que todas as stories já estavam commitadas — \
     rode a suíte COMPLETA nesta rodada. Falhou: corrija, commite, pushe e \
     rode de novo. Se não for possível rodá-la (permissão, ambiente), diga \
     isso no marker em vez de presumir que passa.\n\
     - Depois do marker \"ok\" o agente abre o PR e ESPERA o CI. Se o CI \
     ficar vermelho, você é reinvocado com o log da falha para corrigir — \
     marker \"ok\" com teste quebrado só adia o trabalho.\n\
     - Processos em background que você deixar ao encerrar o turno são \
     MORTOS. Prefira executar cada implement em foreground e aguardar; se \
     usar background, aguarde a conclusão AINDA NESTE turno antes de \
     encerrar.\n\
     - Commite e pushe cada story assim que concluída, com o rodapé de \
     trailers que `{cli} implement <número>` pedir no contexto — \
     não invente formato de assunto, é o rodapé que é conferido.\n\
     - Se o contexto de `spec-wave order`/`implement` trouxer a seção \
     \"Workspace multi-repo\" (a Feature toca mais de um repositório de \
     código, RFC-005), o commit e o push de CADA story acontecem no \
     diretório do repositório que ela de fato tocou — não só no diretório \
     atual. Uma story que toca dois repositórios commita/pusha nos dois.\n\
     - Os cards do board (Etapas) são movidos AUTOMATICAMENTE pelo \
     `spec-wave implement` — não gerencie o board manualmente.\n\
     - SOMENTE quando TODAS as stories estiverem commitadas e pushadas (ou \
     declaradas como falha), escreva o arquivo \
     ./.spec-wave-agent-result.json (NÃO commite este arquivo) com \
     exatamente:\n\
     {\"status\": \"ok\"} se todas as stories foram implementadas, \
     commitadas e pushadas E a suíte completa passou nesta rodada; ou\n\
     {\"status\": \"ok\", \"detalhe\": \"suíte não executada: <motivo>\"} \
     se tudo foi implementado mas você não conseguiu rodar a suíte; ou\n\
     {\"status\": \"partial\", \"detalhe\": \"<o que falhou>\"} se alguma \
     story falhou definitivamente ou sobrou teste falhando que você não \
     conseguiu corrigir.\n\
     - NÃO escreva o marker se ainda houver stories pendentes que você \
     pretende continuar na próxima rodada.\n"
        .into()
}

fn d_bug_prompt() -> String {
    "Você está no clone do repositório, no branch de trabalho do Bug #{issue}.\n\
     Corrija o defeito em QUATRO FASES, nesta ordem.\n\
     \n\
     NÃO comece pelo fix. O modo de falha desta tarefa é encontrar o lugar \
     onde o sintoma aparece, remendar ali, e o defeito voltar na próxima \
     entrada. As fases 1 e 2 existem para impedir isso.\n\
     \n\
     1. REPRODUZIR — rode `{cli} implement {issue}` para obter o \
     contexto (relato, comentários e o bug.md, se existir). Escreva um teste \
     que FALHA por causa deste defeito e confirme que ele falha pelo motivo \
     certo. Se não conseguir reproduzir, PARE e relate no marker: sem \
     reprodução não há como provar que a correção funcionou.\n\
     2. CAUSA RAIZ — investigue até a ORIGEM, não até o ponto onde o erro se \
     manifesta. Se o bug.md já traz uma causa raiz, CONFIRME-A contra o \
     código antes de aceitar: ela foi escrita por outro modelo, sem executar \
     nada.\n\
     3. FIX MÍNIMO — corrija a causa raiz e APENAS ela. Se vir outros \
     problemas no caminho, anote-os no marker em vez de corrigi-los.\n\
     4. TESTE DE REGRESSÃO — o teste da fase 1 agora passa. Garanta que ele \
     fica no repositório e que falharia de novo se o fix fosse revertido. \
     Rode a suíte inteira: um fix que quebra outro teste não está pronto.\n\
     \n\
     Regras OBRIGATÓRIAS:\n\
     - Você roda em RODADAS: se o seu turno terminar sem o marker de \
     conclusão, você será REINVOCADO. Ao iniciar, verifique o que já foi \
     feito (`git log --oneline -20`) e continue de onde parou. Commit não \
     prova que os testes passam: rode a suíte inteira NESTA rodada antes \
     do marker.\n\
     - Depois do marker \"ok\" o agente abre o PR e ESPERA o CI. Se o CI \
     ficar vermelho, você é reinvocado com o log da falha para corrigir.\n\
     - Processos em background que você deixar ao encerrar o turno são \
     MORTOS. Execute em foreground e aguarde.\n\
     - Commite neste branch com a CAUSA RAIZ na mensagem: \
     \"fix: <o que estava errado> (#{issue}) [spec-wave-agent]\", com o corpo \
     explicando a origem — não o sintoma. Depois faça push.\n\
     - Se o contexto de `spec-wave implement` trouxer a seção \"Workspace \
     multi-repo\" (RFC-005), o commit e o push acontecem no diretório do \
     repositório onde a causa raiz de fato está — não necessariamente o \
     diretório atual.\n\
     - Os cards do board são movidos AUTOMATICAMENTE pelo `spec-wave \
     implement` — não gerencie o board manualmente.\n\
     - SOMENTE ao terminar, escreva ./.spec-wave-agent-result.json (NÃO \
     commite este arquivo) com:\n\
     {\"status\": \"ok\", \"causa_raiz\": \"<a origem, com arquivo e função>\", \
     \"fix\": \"<o que mudou>\", \"teste_regressao\": \"<o teste que prova>\", \
     \"arquivos\": [\"<caminhos tocados>\"]} se corrigiu e provou; ou\n\
     {\"status\": \"partial\", \"detalhe\": \"<o que faltou e por quê>\"} se não \
     conseguiu reproduzir, não achou a causa, ou o fix não passou nos testes.\n\
     - NÃO escreva o marker se pretende continuar na próxima rodada.\n"
        .into()
}

/// Split de linha de comando em argv: espaços separam, aspas simples/duplas
/// agrupam (sem escapes; SEM shell — sem pipes/expansões).
pub fn split_command(s: &str) -> Result<Vec<String>> {
    let mut argv = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => { quote = Some(c); has_token = true; }
            None if c.is_whitespace() => {
                if has_token { argv.push(std::mem::take(&mut cur)); has_token = false; }
            }
            None => { cur.push(c); has_token = true; }
        }
    }
    if quote.is_some() {
        bail!("aspas não fechadas em: {s:?}");
    }
    if has_token { argv.push(cur); }
    if argv.is_empty() {
        bail!("comando vazio");
    }
    Ok(argv)
}

/// Acrescenta ao argv do `claude` as flags de MCP (frota): `--mcp-config <path>`
/// e `--allowedTools <lista>`. Cada valor vai como argumento próprio (sem
/// shell). Se o argv já tem `--allowedTools`/`--allowed-tools`, a lista é
/// mesclada ao valor existente (vírgula, sem duplicar) em vez de repetir a
/// flag. Valores ausentes/vazios deixam o argv byte a byte igual.
pub fn apply_mcp_args(
    mut argv: Vec<String>, mcp_config: Option<&str>, allowed_tools: Option<&str>,
) -> Vec<String> {
    let mcp_config = mcp_config.map(str::trim).filter(|v| !v.is_empty());
    let allowed: Vec<&str> = allowed_tools.unwrap_or("").split(',')
        .map(str::trim).filter(|t| !t.is_empty()).collect();
    if let Some(path) = mcp_config {
        argv.push("--mcp-config".into());
        argv.push(path.to_string());
    }
    if allowed.is_empty() { return argv; }
    let merge = |existing: &str| -> String {
        let mut items: Vec<String> = existing.split(',')
            .map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect();
        for t in &allowed {
            if !items.iter().any(|i| i == t) { items.push((*t).to_string()); }
        }
        items.join(",")
    };
    for i in 0..argv.len() {
        let a = argv[i].clone();
        if a == "--allowedTools" || a == "--allowed-tools" {
            if let Some(v) = argv.get(i + 1).filter(|v| !v.starts_with("--")) {
                let merged = merge(v);
                argv[i + 1] = merged;
                return argv;
            }
        } else if let Some(v) = a.strip_prefix("--allowedTools=")
            .or_else(|| a.strip_prefix("--allowed-tools="))
        {
            let flag = &a[..a.find('=').unwrap()];
            argv[i] = format!("{flag}={}", merge(v));
            return argv;
        }
    }
    argv.push("--allowedTools".into());
    argv.push(allowed.join(","));
    argv
}

/// `apply_mcp_args` com `SPECWAVE_MCP_CONFIG` / `SPECWAVE_MCP_ALLOWED_TOOLS`.
pub fn apply_mcp_env(argv: Vec<String>) -> Vec<String> {
    apply_mcp_args(
        argv,
        std::env::var("SPECWAVE_MCP_CONFIG").ok().as_deref(),
        std::env::var("SPECWAVE_MCP_ALLOWED_TOOLS").ok().as_deref(),
    )
}

/// Substitui o placeholder {issue}.
pub fn render_template(t: &str, issue: u64) -> String {
    t.replace("{issue}", &issue.to_string())
}

/// Como chamar a CLI do spec-wave quando a config não diz (`cli_command`).
///
/// `npx spec-wave` resolve PRIMEIRO o `node_modules/.bin` do diretório
/// corrente — e o diretório corrente é o clone do produto. Num produto que
/// tem a CLI como dependência (o spec-wave-ui fixa a 1.0.6 no lockfile),
/// bastava o executor rodar `npm install` para todo `npx spec-wave` seguinte
/// virar essa cópia antiga: sem stream por story, sem progresso por Task, sem
/// os comandos novos. A CLI que conduz o fluxo é a da MÁQUINA (a da imagem do
/// Fleet Job, a instalada pelo dev), nunca a que o produto usa como
/// biblioteca. Por isso: o `spec-wave` do PATH, pelo caminho absoluto; só sem
/// ele, o `npx spec-wave` de sempre.
pub fn detect_cli(path_var: Option<&std::ffi::OsStr>) -> String {
    let found = path_var.into_iter().flat_map(std::env::split_paths)
        // Um PATH com o .bin de um projeto (npm run) cairia no mesmo problema.
        .filter(|dir| dir.is_absolute() && !dir.components().any(|c| c.as_os_str() == "node_modules"))
        .map(|dir| dir.join("spec-wave"))
        .find(|bin| is_executable(bin));
    match found.and_then(|p| p.to_str().map(str::to_string)) {
        // Caminho com espaço ou aspas não sobrevive ao split_command do
        // comando nem ao prompt: melhor o npx do que um argv quebrado.
        Some(p) if !p.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') => p,
        _ => "npx spec-wave".into(),
    }
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else { return false };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
fn d_workdir() -> String {
    dirs_home().join(".spec-wave-agent").to_string_lossy().into_owned()
}

pub fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Caminho da config, na ordem de precedência: argumento `--config <path>`
/// (passado pelo main), env `SPEC_WAVE_AGENT_CONFIG`, default no HOME.
///
/// O override existe porque um dev tem mais de um repositório sob spec-wave e
/// o default é um caminho único: sem ele, apontar o agente para outro repo
/// significa sobrescrever a config em uso.
pub fn config_path(override_path: Option<&str>) -> PathBuf {
    if let Some(p) = override_path {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("SPEC_WAVE_AGENT_CONFIG") {
        return PathBuf::from(p);
    }
    dirs_home().join(".config/spec-wave-agent/config.toml")
}

pub fn load_config() -> Result<Config> {
    load_config_from(None)
}

pub fn load_config_from(override_path: Option<&str>) -> Result<Config> {
    let path = config_path(override_path);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("config não encontrada em {}", path.display()))?;
    toml::from_str(&raw)
        .with_context(|| format!("config inválida em {}", path.display()))
}

impl Config {
    /// Config só com os defaults — base do modo `--once` do Fleet Job, que
    /// recebe o resto do ambiente montado pelo spec-wave-sandbox.
    pub fn defaults() -> Config {
        toml::from_str("").expect("os defaults da Config são válidos")
    }

    /// Como chamar a CLI do spec-wave (ver `cli_command` e `detect_cli`).
    pub fn cli(&self) -> String {
        self.cli_command.as_deref().map(str::trim).filter(|c| !c.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| detect_cli(std::env::var_os("PATH").as_deref()))
    }

    /// `render_template` com o `{cli}` resolvido.
    pub fn render(&self, t: &str, issue: u64) -> String {
        render_template(&t.replace("{cli}", &self.cli()), issue)
    }

    pub fn validate(&self) -> Result<()> {
        if split_command(&self.pr_command)?.is_empty() {
            bail!("pr_command vazio (default: \"{{cli}} pr open\")");
        }
        if !crate::lease::valid_lease_prefix(&self.lease_ref_prefix) {
            bail!("lease_ref_prefix inválido: {:?} (esperado refs/…, ex.: refs/spec-wave/claims)",
                  self.lease_ref_prefix);
        }
        match self.source {
            Source::GithubLabel => validate_repo(&self.repo)?,
            Source::Api => {
                let url = self.api_url.as_deref().unwrap_or("").trim();
                // Compara o host exato: prefixo aceitaria
                // "http://localhost.evil.com" e mandaria o token em claro.
                let local = url.strip_prefix("http://")
                    .map(|rest| {
                        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
                        let host = authority.rsplit_once(':')
                            .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
                            .map_or(authority, |(h, _)| h);
                        host == "localhost" || host == "127.0.0.1"
                    })
                    .unwrap_or(false);
                // Na fonte api cada item traz o próprio repositório: um remoto
                // FIXO faria todos os produtos dividirem clone e lease. Git
                // self-hosted continua possível com o placeholder {repo}.
                if self.remote_url.as_deref().is_some_and(|r| !r.contains("{repo}")) {
                    bail!("config: em source = \"api\", remote_url precisa do placeholder {{repo}} \
                           (ex.: \"git@git.interno:{{repo}}.git\") — um remoto fixo faria todos os \
                           produtos compartilharem o mesmo clone e lease");
                }
                if !(url.starts_with("https://") || local) {
                    bail!("config: source = \"api\" exige api_url https:// \
                           (ex.: \"https://app.specwave.dev/agent-api\"; recebido: {url:?})");
                }
                match self.agent_token() {
                    Some(t) if t.starts_with("swa_") => {}
                    Some(_) => bail!("config: agent_token não é um token de agente (começa com swa_)"),
                    None => bail!("config: source = \"api\" exige agent_token (ou a variável \
                                   SPEC_WAVE_AGENT_TOKEN) — gere em Configurações → Minha conta"),
                }
            }
        }
        for (name, v) in [
            ("poll_interval_secs", self.poll_interval_secs),
            ("heartbeat_secs", self.heartbeat_secs),
            ("implement_timeout_secs", self.implement_timeout_secs),
        ] {
            if v == 0 {
                bail!("config: {name} deve ser > 0");
            }
        }
        if self.poll_backoff_max_secs < self.poll_interval_secs {
            bail!("config: poll_backoff_max_secs ({}) deve ser >= poll_interval_secs ({}) \
                   — é o teto do backoff, não pode ser menor que a base",
                  self.poll_backoff_max_secs, self.poll_interval_secs);
        }
        if self.lease_ttl_secs <= 0 {
            bail!("config: lease_ttl_secs deve ser > 0");
        }
        if (self.lease_ttl_secs as u64) < 4 * self.heartbeat_secs {
            bail!("config: lease_ttl_secs ({}) deve ser >= 4x heartbeat_secs ({}) \
                   para tolerar lentidão de rede sem roubo indevido",
                  self.lease_ttl_secs, self.heartbeat_secs);
        }
        if self.workdir.trim().is_empty() {
            bail!("config: workdir vazio");
        }
        if self.bug_timeout_secs == 0 {
            bail!("config: bug_timeout_secs deve ser > 0");
        }
        if self.max_failures_per_issue == 0 {
            bail!("config: max_failures_per_issue deve ser > 0 (use 1 para desistir na 1a falha)");
        }
        split_command(&self.feature_command)
            .map_err(|e| anyhow::anyhow!("config: feature_command inválido: {e}"))?;
        split_command(&self.bug_command)
            .map_err(|e| anyhow::anyhow!("config: bug_command inválido: {e}"))?;
        if self.bug_prompt.trim().is_empty() {
            bail!("config: bug_prompt vazio");
        }
        if self.review_timeout_secs == 0 {
            bail!("config: review_timeout_secs deve ser > 0");
        }
        split_command(&self.review_command)
            .map_err(|e| anyhow::anyhow!("config: review_command inválido: {e}"))?;
        if self.review_prompt.trim().is_empty() {
            bail!("config: review_prompt vazio");
        }
        if self.qa_timeout_secs == 0 {
            bail!("config: qa_timeout_secs deve ser > 0");
        }
        split_command(&self.qa_command)
            .map_err(|e| anyhow::anyhow!("config: qa_command inválido: {e}"))?;
        if self.flow_timeout_secs == 0 {
            bail!("config: flow_timeout_secs deve ser > 0");
        }
        split_command(&self.flow_command)
            .map_err(|e| anyhow::anyhow!("config: flow_command inválido: {e}"))?;
        if self.feature_prompt.trim().is_empty() {
            bail!("config: feature_prompt vazio");
        }
        if self.max_executor_rounds == 0 {
            bail!("config: max_executor_rounds deve ser >= 1");
        }
        split_command(&self.checks_command)
            .map_err(|e| anyhow::anyhow!("config: checks_command inválido: {e}"))?;
        if self.ci_gate && (self.ci_poll_secs == 0 || self.ci_timeout_secs == 0) {
            bail!("config: ci_poll_secs e ci_timeout_secs devem ser > 0 \
                   (para desligar o gate use ci_gate = false)");
        }
        Ok(())
    }

    /// Token efetivo: a variável de ambiente vence o arquivo.
    pub fn agent_token(&self) -> Option<String> {
        std::env::var("SPEC_WAVE_AGENT_TOKEN").ok()
            .or_else(|| self.agent_token.clone())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    }

    /// Configuração de UM item recebido pela API: o repositório vem do item, e
    /// cada repositório tem o próprio diretório de trabalho (clones
    /// `issue-<n>` e `lease-repo`). Sem isso, a issue #12 de dois produtos
    /// diferentes cairia no mesmo clone — e no mesmo lease.
    pub fn for_repo(&self, repo: &str) -> Result<Config> {
        validate_repo(repo)?;
        let mut cfg = self.clone();
        cfg.repo = repo.to_string();
        cfg.remote_url = self.remote_url.as_ref().map(|r| r.replace("{repo}", repo));
        cfg.workdir = PathBuf::from(&self.workdir)
            .join("repos")
            .join(repo.replace('/', "__"))
            .to_string_lossy()
            .into_owned();
        Ok(cfg)
    }

    pub fn remote_url(&self) -> String {
        self.remote_url.clone()
            .unwrap_or_else(|| format!("https://github.com/{}.git", self.repo))
    }

    pub fn agent_id(&self) -> String {
        self.agent_id.clone().unwrap_or_else(|| {
            let user = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "agent".into());
            format!("{user}@{}", hostname())
        })
    }
}

fn validate_repo(repo: &str) -> Result<()> {
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|p| p.is_empty() || p.contains(char::is_whitespace) || *p == "." || *p == "..")
    {
        bail!("config: repo deve ter o formato \"owner/repo\" (recebido: {repo:?})");
    }
    Ok(())
}

pub fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-host".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> Config {
        toml::from_str(toml_str).unwrap()
    }

    // Token de exemplo no formato swa_ (não é um token real).
    const TOKEN: &str = "swa_dGVuYW50.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    #[test]
    fn fonte_padrao_e_label_e_exige_repo() {
        let cfg = parse(r#"poll_interval_secs = 60"#);
        assert_eq!(cfg.source, Source::GithubLabel);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn fonte_api_dispensa_repo_e_exige_url_e_token() {
        let ok = parse(&format!(
            "source = \"api\"\napi_url = \"https://app.specwave.dev/agent-api\"\nagent_token = \"{TOKEN}\""));
        assert_eq!(ok.source, Source::Api);
        assert!(ok.validate().is_ok());

        let sem_token = parse("source = \"api\"\napi_url = \"https://app.specwave.dev/agent-api\"");
        if std::env::var("SPEC_WAVE_AGENT_TOKEN").is_err() {
            assert!(sem_token.validate().is_err());
        }
        let http = parse(&format!(
            "source = \"api\"\napi_url = \"http://app.specwave.dev/agent-api\"\nagent_token = \"{TOKEN}\""));
        assert!(http.validate().is_err(), "http:// fora de localhost vazaria o token");
        let local = parse(&format!(
            "source = \"api\"\napi_url = \"http://localhost:3001/agent-api\"\nagent_token = \"{TOKEN}\""));
        assert!(local.validate().is_ok());
        let falso_local = parse(&format!(
            "source = \"api\"\napi_url = \"http://localhost.evil.com/agent-api\"\nagent_token = \"{TOKEN}\""));
        assert!(falso_local.validate().is_err(), "prefixo localhost não é localhost");
        let com_remote = parse(&format!(
            "source = \"api\"\napi_url = \"https://x.dev/agent-api\"\nagent_token = \"{TOKEN}\"\nremote_url = \"/tmp/o.git\""));
        assert!(com_remote.validate().is_err(), "remote_url fixo colapsaria os repositórios");
        let com_placeholder = parse(&format!(
            "source = \"api\"\napi_url = \"https://x.dev/agent-api\"\nagent_token = \"{TOKEN}\"\nremote_url = \"git@git.interno:{{repo}}.git\""));
        assert!(com_placeholder.validate().is_ok());
        assert_eq!(com_placeholder.for_repo("acme/api").unwrap().remote_url(), "git@git.interno:acme/api.git");
        let token_errado = parse(
            "source = \"api\"\napi_url = \"https://x.dev/agent-api\"\nagent_token = \"ghp_abc\"");
        if std::env::var("SPEC_WAVE_AGENT_TOKEN").is_err() {
            assert!(token_errado.validate().is_err());
        }
    }

    #[test]
    fn for_repo_isola_diretorio_por_repositorio() {
        let cfg = parse(&format!(
            "source = \"api\"\napi_url = \"https://x.dev/agent-api\"\nagent_token = \"{TOKEN}\"\nworkdir = \"/w\""));
        let a = cfg.for_repo("acme/api").unwrap();
        let b = cfg.for_repo("acme/web").unwrap();
        assert_eq!(a.repo, "acme/api");
        assert_eq!(a.workdir, "/w/repos/acme__api");
        assert_ne!(a.workdir, b.workdir);
        assert_eq!(a.remote_url(), "https://github.com/acme/api.git");
        assert!(cfg.for_repo("acme").is_err());
        assert!(cfg.for_repo("../etc").is_err());
    }

    #[test]
    fn defaults_com_apenas_repo() {
        let cfg = parse(r#"repo = "org/repo""#);
        assert_eq!(cfg.pr_command, "{cli} pr open");
        assert_eq!(cfg.progress_command, "{cli} progress {issue} --json");
        assert_eq!(cfg.queue_label, "spec-wave:dev-agent");
        assert_eq!(cfg.poll_interval_secs, 60);
        assert_eq!(cfg.heartbeat_secs, 120);
        assert_eq!(cfg.lease_ttl_secs, 600);
        assert_eq!(cfg.implement_timeout_secs, 14400);
        assert_eq!(cfg.poll_backoff_max_secs, 480);
        assert!(cfg.agent_id.is_none());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn valida_poll_backoff_max_vs_poll_interval() {
        let cfg = parse("repo = \"a/b\"\npoll_interval_secs = 60\npoll_backoff_max_secs = 59");
        assert!(cfg.validate().is_err());
        let cfg = parse("repo = \"a/b\"\npoll_interval_secs = 60\npoll_backoff_max_secs = 60");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn config_path_respeita_precedencia() {
        // Argumento explícito vence a env; a env vence o default no HOME.
        std::env::set_var("SPEC_WAVE_AGENT_CONFIG", "/tmp/da-env.toml");
        assert_eq!(config_path(Some("/tmp/do-arg.toml")),
                   PathBuf::from("/tmp/do-arg.toml"));
        assert_eq!(config_path(None), PathBuf::from("/tmp/da-env.toml"));
        std::env::remove_var("SPEC_WAVE_AGENT_CONFIG");
        assert_eq!(config_path(None),
                   dirs_home().join(".config/spec-wave-agent/config.toml"));
    }

    #[test]
    fn remote_url_default_e_override() {
        let cfg = parse(r#"repo = "org/repo""#);
        assert_eq!(cfg.remote_url(), "https://github.com/org/repo.git");
        let cfg = parse("repo = \"org/repo\"\nremote_url = \"/tmp/origin.git\"");
        assert_eq!(cfg.remote_url(), "/tmp/origin.git");
    }

    #[test]
    fn valida_formato_do_repo() {
        for bad in ["foo", "a/b/c", "/b", "a/", "a b/c"] {
            let cfg = parse(&format!("repo = \"{bad}\""));
            assert!(cfg.validate().is_err(), "deveria rejeitar {bad:?}");
        }
    }

    #[test]
    fn valida_ttl_vs_heartbeat() {
        let cfg = parse("repo = \"a/b\"\nheartbeat_secs = 120\nlease_ttl_secs = 479");
        assert!(cfg.validate().is_err());
        let cfg = parse("repo = \"a/b\"\nheartbeat_secs = 120\nlease_ttl_secs = 480");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn cli_e_o_spec_wave_do_path_fora_de_node_modules() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let exe = |dir: &str, mode: u32| {
            let d = tmp.path().join(dir);
            std::fs::create_dir_all(&d).unwrap();
            let f = d.join("spec-wave");
            std::fs::write(&f, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
            d
        };
        let produto = exe("clone/node_modules/.bin", 0o755); // a cópia do produto
        let sem_x = exe("sem-x", 0o644);
        let global = exe("global/bin", 0o755);
        let path = std::env::join_paths([&produto, &sem_x, &global]).unwrap();
        assert_eq!(detect_cli(Some(&path)), global.join("spec-wave").to_str().unwrap());
        // Só a do produto no PATH, ou PATH nenhum: o npx de sempre.
        let so_produto = std::env::join_paths([&produto]).unwrap();
        assert_eq!(detect_cli(Some(&so_produto)), "npx spec-wave");
        assert_eq!(detect_cli(None), "npx spec-wave");
    }

    #[test]
    fn cli_command_vence_a_deteccao_e_entra_no_lugar_de_cli() {
        let cfg = parse(r#"repo = "org/repo"
            cli_command = "npx @spec-wave/cli@1.9.0""#);
        assert_eq!(cfg.cli(), "npx @spec-wave/cli@1.9.0");
        assert_eq!(cfg.render(&cfg.progress_command, 7), "npx @spec-wave/cli@1.9.0 progress 7 --json");
        assert!(cfg.render(&cfg.feature_prompt, 7).contains("`npx @spec-wave/cli@1.9.0 order 7`"));
        assert!(!cfg.render(&cfg.feature_prompt, 7).contains("{cli}"));
        assert!(!cfg.render(&cfg.bug_prompt, 7).contains("{cli}"));
    }

    #[test]
    fn executor_de_qa_passa_o_prompt_antes_das_opcoes_variadicas() {
        // `--allowedTools` é variádico no `claude`: um prompt depois dele vira
        // mais uma "ferramenta" e o executor sai sem rodar nada.
        let cfg = parse(r#"repo = "org/repo""#);
        let argv = split_command(&cfg.qa_executor).unwrap();
        assert_eq!(argv[..2], ["claude", "-p"]);
        assert!(argv[2].starts_with("Execute o QA descrito em {contextFile}"), "{argv:?}");
        let at = argv.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(at + 2, argv.len(), "nada depois da lista de ferramentas: {argv:?}");
    }

    #[test]
    fn defaults_do_executor_de_feature() {
        let cfg = parse(r#"repo = "org/repo""#);
        assert_eq!(cfg.implement_timeout_secs, 14400);
        let argv = split_command(&cfg.feature_command).unwrap();
        assert_eq!(argv[0], "claude");
        assert!(argv.contains(&"acceptEdits".to_string()));
        // allowedTools com vírgulas fica em UM token (estava entre aspas)
        assert!(argv.iter().any(|a| a.starts_with("Bash,")));
        // Bash inteiro: o executor precisa rodar a suíte do repositório, seja
        // qual for a stack. Lista de prefixos volta a negar `npm test`.
        for cmd in [&cfg.feature_command, &cfg.bug_command, &cfg.review_command] {
            let argv = split_command(cmd).unwrap();
            let at = argv.iter().position(|a| a == "--allowedTools").unwrap();
            let tools = &argv[at + 1];
            assert!(tools.split(',').any(|t| t == "Bash"), "{tools}");
            assert!(!tools.contains("Bash("), "{tools}");
        }
        assert!(cfg.feature_prompt.contains("{issue}"));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn mcp_sem_env_nao_altera_argv() {
        let base = split_command("claude -p --allowedTools \"Bash(x:*),Read\"").unwrap();
        assert_eq!(apply_mcp_args(base.clone(), None, None), base);
        assert_eq!(apply_mcp_args(base.clone(), Some(" "), Some(",")), base);
    }

    #[test]
    fn mcp_acrescenta_flags_como_argumentos_separados() {
        let base = split_command("claude -p").unwrap();
        let out = apply_mcp_args(base, Some("/h/a b;rm.json"),
            Some("mcp__a__t1, mcp__a__t2"));
        assert_eq!(out, vec!["claude", "-p", "--mcp-config", "/h/a b;rm.json",
            "--allowedTools", "mcp__a__t1,mcp__a__t2"]);
    }

    #[test]
    fn mcp_mescla_com_allowed_tools_existente() {
        let base = split_command("claude -p --allowedTools \"Bash(x:*),Read\" --model m").unwrap();
        let out = apply_mcp_args(base, None, Some("mcp__a__t,Read"));
        assert_eq!(out, vec!["claude", "-p", "--allowedTools",
            "Bash(x:*),Read,mcp__a__t", "--model", "m"]);
        assert_eq!(out.iter().filter(|a| *a == "--allowedTools").count(), 1);
        let eq = apply_mcp_args(vec!["claude".into(), "--allowed-tools=Read".into()],
            None, Some("mcp__a__t"));
        assert_eq!(eq, vec!["claude", "--allowed-tools=Read,mcp__a__t"]);
    }

    #[test]
    fn split_command_com_aspas() {
        assert_eq!(split_command("a b c").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(split_command("claude --allowedTools \"Bash(npx spec-wave:*)\"").unwrap(),
                   vec!["claude", "--allowedTools", "Bash(npx spec-wave:*)"]);
        assert_eq!(split_command("x 'com espaço' z").unwrap(),
                   vec!["x", "com espaço", "z"]);
        assert_eq!(split_command("x ''").unwrap(), vec!["x", ""]);
        assert!(split_command("  ").is_err());
        assert!(split_command("a 'aberto").is_err());
    }

    #[test]
    fn render_do_placeholder() {
        assert_eq!(render_template("spec-wave order {issue} #{issue}", 7),
                   "spec-wave order 7 #7");
    }

    #[test]
    fn valida_executor() {
        let cfg = parse("repo = \"a/b\"\nfeature_command = \"\"");
        assert!(cfg.validate().is_err());
        let cfg = parse("repo = \"a/b\"\nfeature_prompt = \" \"");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn valida_intervalos_positivos() {
        for field in ["poll_interval_secs", "heartbeat_secs", "implement_timeout_secs"] {
            let cfg = parse(&format!("repo = \"a/b\"\n{field} = 0"));
            assert!(cfg.validate().is_err(), "deveria rejeitar {field}=0");
        }
        let cfg = parse("repo = \"a/b\"\nlease_ttl_secs = 0");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn gate_de_ci_ligado_por_default_e_validado() {
        let cfg = parse(r#"repo = "org/repo""#);
        assert!(cfg.ci_gate);
        assert_eq!(cfg.checks_command, "gh pr view");
        assert_eq!((cfg.ci_poll_secs, cfg.ci_timeout_secs, cfg.ci_no_checks_grace_secs), (30, 1800, 120));
        assert_eq!(cfg.max_ci_fix_rounds, 3);
        assert!(parse("repo = \"a/b\"\nci_poll_secs = 0").validate().is_err());
        assert!(parse("repo = \"a/b\"\nci_poll_secs = 0\nci_gate = false").validate().is_ok());
        assert!(parse("repo = \"a/b\"\nchecks_command = \"\"").validate().is_err());
    }

    #[test]
    fn flow_tem_defaults_e_valida_a_etapa() {
        let mut cfg = Config::defaults();
        cfg.flow_step = Some("spec".into());
        assert_eq!(cfg.flow_command, "{cli} step {step} --issue {issue}");
        assert_eq!(cfg.flow_timeout_secs, 3600);
        assert!(valid_flow_step("critique-fix"));
        assert!(!valid_flow_step(""));
        assert!(!valid_flow_step("spec;rm"));
        assert!(!valid_flow_step("Spec"));
        assert!(!valid_flow_step("--help"));
        assert!(!valid_flow_step("-"));
    }

    #[test]
    fn qa_tem_defaults_proprios() {
        let cfg = Config::defaults();
        assert_eq!(cfg.qa_command, "{cli} qa {issue} --outcome-file .spec-wave-agent-qa.json");
        assert!(cfg.render(&cfg.qa_command, 7).ends_with(" qa 7 --outcome-file .spec-wave-agent-qa.json"));
        assert!(cfg.qa_executor.contains("{contextFile}"));
        assert_eq!(cfg.qa_timeout_secs, 5400);
    }

    #[test]
    fn review_tem_defaults_proprios() {
        let cfg = parse("repo = \"a/b\"");
        assert!(cfg.review_prompt.contains("{issue}"));
        assert!(cfg.review_prompt.contains("NUNCA aprove"));
        assert!(cfg.review_prompt.contains("<!-- spec-wave:auto-review -->"));
        // A revisão usa o gh (ler o PR, publicar o review): coberto pelo
        // `Bash` inteiro do executor default.
        assert_eq!(cfg.review_command, cfg.feature_command);
        assert_eq!(cfg.review_timeout_secs, 3600);
        // A CLI que conduz o fluxo é a da máquina (`{cli}`), como nos outros prompts.
        assert!(cfg.review_prompt.contains("{cli} order {issue}"));
        assert!(!cfg.render(&cfg.review_prompt, 7).contains("{cli}"));
    }

    #[test]
    fn review_acha_o_pr_do_bug_filho_de_uma_story() {
        // O prReview do SaaS dispara para PR de QUALQUER item da Feature — o
        // Bug do QA é filho de uma Story. Se o prompt só olhasse Feature e
        // Stories, a revisão terminaria "nenhum PR elegível" (spec-wave-test#474).
        let cfg = parse("repo = \"a/b\"");
        let p = cfg.render(&cfg.review_prompt, 388);
        assert!(p.contains("sub_issues"), "o prompt precisa listar os filhos das Stories");
        assert!(p.contains("Bug"));
        assert!(p.contains("agent/issue-N"), "branch do dev do próprio item, não só da Feature");
        // `{owner}/{repo}` é do gh api (resolvido no clone), não da render.
        assert!(p.contains("repos/{owner}/{repo}/issues/<story>/sub_issues"));
    }

    #[test]
    fn review_revisa_de_novo_quando_ha_commit_depois_da_revisao() {
        // O codeMerge do SaaS só faz merge do head que a revisão viu e, com
        // commit novo, despacha uma nova revisão. Se o prompt pulasse todo PR
        // com o marker, a revisão terminava sem comentar e o merge esperava
        // para sempre (spec-wave-ui#1221/#1224/#1227, merge da main na branch).
        let cfg = parse("repo = \"a/b\"");
        let p = cfg.render(&cfg.review_prompt, 676);
        assert!(p.contains("MAIS NOVO que o último commit"));
        assert!(p.contains("committedDate"));
        assert!(p.contains("revise de novo o que mudou"));
        assert!(!p.contains("já foi revisado: não revise de novo"));
    }

    #[test]
    fn prompts_exigem_suite_completa_antes_do_marker() {
        let cfg = parse(r#"repo = "org/repo""#);
        for p in [&cfg.feature_prompt, &cfg.bug_prompt] {
            assert!(p.contains("ESPERA o CI"), "prompt sem o aviso do gate: {p}");
        }
        assert!(cfg.feature_prompt.contains("suíte de testes COMPLETA"));
        assert!(cfg.feature_prompt.contains("Commit NÃO prova que os testes passam"));
    }

    #[test]
    fn max_failures_zero_e_recusado() {
        // 0 significaria "nunca desistir", que é o comportamento que este
        // teto existe para eliminar. Quem quer desistir cedo usa 1.
        let base = r#"repo = "o/r""#;
        let cfg: Config = toml::from_str(base).unwrap();
        assert_eq!(cfg.max_failures_per_issue, 3, "default");
        assert!(cfg.validate().is_ok());

        let zero: Config = toml::from_str(
            &format!("{base}\nmax_failures_per_issue = 0")).unwrap();
        let err = zero.validate().unwrap_err().to_string();
        assert!(err.contains("max_failures_per_issue"), "erro: {err}");
    }
}
