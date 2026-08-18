# Handoff — local-llm

Continuar em outra sessão a partir daqui. Repo: `C:\GIT\projetos-paralelos\local-llm`.

## Pedido original

Chat P2P, criptografado de ponta a ponta, na rede interna da empresa. Motivo: Teams é monitorado e WhatsApp é proibido. A cara de chat de LLM no terminal é conforto (colega olhando o monitor), não camuflagem de tráfego. Não reinventar a roda — usar motor consolidado se existir.

Restrições que fecharam o recorte:

- Solução 100% local. Política da empresa, segundo o Pedro, não proíbe isso.
- Binário **pequeno** para compartilhar no Teams (exe cru o Teams bloqueia; vai zipado).
- Abre no PowerShell e parece client de modelo local.
- Criar ou entrar numa sala. Sala existe na LAN. Entra só com PIN.
- Histórico P2P tipo torrent: quem entra puxa dos peers online.
- Grupo de **4 pessoas**.
- “Saiu perdeu acesso” ficou assim: `/forget` apaga desta máquina. Fechar o terminal **não** apaga. Histórico fica no disco, cifrado, até apagar a sala.
- PIN na prática: código gerado tipo `7K2M-9QXP` (Crockford), não 6 dígitos.
- Repo: `C:\GIT\projetos-paralelos\local-llm`.

Fora de escopo (combinado): fingir HTTPS/Ollama na rede, relay público, app gráfico, mobile.

---

## Plano original

Repo: `C:\GIT\projetos-paralelos\local-llm`  
Binário: `local-llm.exe` (distribuído **zipado**, não cru)  
Grupo: 4 pessoas, Windows 11, PowerShell 7, só rede local.

### O que é

Um `.exe` pequeno que abre no terminal e parece um client de LLM local. Por baixo é uma sala P2P na LAN: quem tem o PIN entra, puxa o log dos peers que estão online (estilo torrent) e grava o histórico criptografado no disco até apagar a sala.

Não é camuflagem de tráfego. QUIC/Iroh na rede é o que é. A “cara de LLM” é só a TUI (ombro do colega, não DPI).

### Decisões travadas

| Tema | Escolha |
|---|---|
| Transporte | Iroh 1.0, **só mDNS**, relays públicos desligados. Sem `presets::N0`. |
| Fan-out ao vivo | `iroh-gossip`. `TopicId = blake3("local-llm/v1" \|\| pin_normalizado)`. Sem o PIN ninguém entra no overlay. |
| Histórico | Append-only no disco, AEAD. Sobrevive a fechar o app. `/forget` apaga pasta + chaves da RAM. |
| PIN | Código gerado Crockford `7K2M-9QXP` (8 chars, ~40 bits) + Argon2id (64 MiB, t=3). Na TUI chama “PIN”. |
| Identidade | Chave Ed25519 do endpoint Iroh assina cada mensagem. PIN cifra o conteúdo; a assinatura diz quem falou. Qualquer um com o PIN ainda pode inventar um endpoint novo — ok para 4 colegas. Sem OpenMLS. |
| “Saiu perdeu acesso” | Interpretado como **sair da sala = `/forget`**. Fechar o terminal **não** apaga. Sem PIN o arquivo no disco é inútil. |
| Join | Só o PIN. O nome da sala (`gpt-oss-20b`) é rótulo local do criador, viaja no header do log depois do sync. |
| Fora de escopo | Fingir HTTPS/Ollama na rede, relay na internet, app gráfico, mobile. |

### Por que não um produto pronto

Cabal/Jami/Keet não entregam TUI estilo modelo + `.exe` único Windows + PIN curto + LAN-only. Iroh é o motor de conexão; a sala e a TUI são o único código nosso.

### Experiência planejada

```
local-llm  0.1.0

  sessions
  > gpt-oss-20b     locked
    qwen2.5-coder   locked

  /new <name>    /join <pin>    /quit
```

1. `local-llm` no PowerShell.
2. `/new gpt-oss-20b` → imprime o PIN uma vez. Criador anota / fala no corredor. **PIN não vai no Teams.**
3. Colega `/join 7K2M-9QXP`. App acha peers via mDNS, entra no tópico, puxa o log.
4. Chat com papéis tipo `user` / `assistant` (depois virou `/nick`).

Comandos do plano: `/new`, `/join`, `/peers`, `/pin`, `/forget`, `/quit`.

### Arquitetura

```
TUI (ratatui)  ── looks like local LLM
   │
sala (PIN → chave, tópico, papéis)
   │
┌─────────────┬──────────────────┐
│ iroh-gossip │ ALPN local-llm/1 │
│  ao vivo    │  sync do log     │
└─────────────┴──────────────────┘
   │
Iroh Endpoint + MdnsDiscovery only
   │
disco: %LOCALAPPDATA%\local-llm\
```

Disco:

```
%LOCALAPPDATA%\local-llm\
  index.toml          # alias, topic_id — sem chave
  rooms\<topic_hex>\
    log.bin           # header + records AEAD
  device.key
```

Rede: Iroh sem relay; gossip no tópico derivado do PIN; ALPN `local-llm/1` com `Have` / `Give` para sync do log.

Binário: LTO fat, `opt-level = "s"`, strip, panic=abort. Alvo &lt; 8 MB. Teams bloqueia `.exe` — mandar zip/OneDrive. Nome do binário: `local-llm`, não ollama/chatgpt.

### Fases do plano

1. Skeleton + TUI morta
2. Cripto + store
3. Rede (Iroh LAN, gossip, sync, dois processos no mesmo PC)
4. Release (tamanho, zip, README Windows)
5. Prova em 2 PCs na LAN real. Se mDNS morrer entre máquinas, documentar — sem relay público no MVP.

### Fora do plano original

- Rotação de PIN / revogação de membro
- OpenMLS
- Relays, internet, NAT traversal
- iOS/Android
- Fingir protocolo de LLM no fio

---

## Onde paramos (18/ago/2026)

Estado no git: branch `main`, versão **0.1.5**.

Exe: `C:\GIT\projetos-paralelos\local-llm\target\release\local-llm.exe` (5,64 MB)

### Bug do log infinito (achado no teste do 0.1.4, corrigido no 0.1.5)

O Pedro rodou o 0.1.4 e a tela virou um paredão de `session teste`. A causa é
anterior a toda a rodada de usabilidade: `RoomLog::missing_for` devolve **todos**
os `Record::Meta` em cada sync, e `append` só deduplicava registros com
`chat_key()` — que `Meta` não tem. Resultado: cada ciclo de sync (3 s) gravava um
`Meta` novo **no disco**, para sempre. A sala `teste` estava com **485 KB** e
~8.800 registros para meia dúzia de mensagens.

Isso também explicava os outros sintomas relatados: o `/help` "não funcionava"
porque escrevia no transcript e o spam o empurrava para fora em segundos, e o
scroll parecia travado porque rolar entre milhares de linhas idênticas não muda
nada na tela.

Correção em `store.rs`: `is_new()` passa a deduplicar `Meta` por alias, e `load()`
**compacta o arquivo** (escrita atômica via rename) quando encontra duplicatas —
então os logs já poluídos encolhem sozinhos na próxima abertura.

Já implementado além do plano: teclado Windows (thread `poll`/`read`), `/nick` (nome velho fica no histórico), segunda janela no mesmo PC (`#2` + `presence\` porque mDNS não se vê no localhost), `subscribe` sem bloquear a TUI.

### Rodada de usabilidade (0.1.4)

Decisões que o Pedro travou nesta rodada:

| Tema | Escolha |
|---|---|
| Disfarce | Híbrido. Dia a dia mostra nomes reais e termos claros; **F12** troca para fachada de inferência (nomes → papéis de modelo, avisos e rascunho somem). |
| Idioma | Interface **toda em inglês**. README e Handoff seguem em português. |
| Destrancar | PIN lembrado por sala via **DPAPI** do Windows; `/lock` reverte. Assume-se que quem senta na máquina logada lê a sala. |

Consertado: chat cola na mensagem nova (antes ficava no topo do log e PageUp/PageDown eram invertidos), chave não some mais da tela (avisos foram da status bar volátil para o corpo do chat), `Esc` limpa a linha em vez de derrubar a sessão (`/leave` sai), colar funciona via bracketed paste (era impossível usar `ticket` sem isso), `/forget` exige o nome da sala, mensagens têm hora e separador de dia, `/peers` diz **quem** está online, e o chat não pisca mais em branco durante o sync (o transcript agora é incremental, o `draw` não pega o lock da sala).

Novo: `/leave`, `/lock`, `/diag`, `LOCAL_LLM_OFFLINE=1`, campainha ao chegar mensagem, cursor/histórico/Ctrl+W/Ctrl+U no input, `Shift+Enter` para multi-linha.

Alinhamento (0.1.5): mensagens próprias vão para a **direita**, as dos outros ficam à esquerda, com bolha em ~70% da largura e horários espelhados (`Dale  09:51` de um lado, `09:51  Pedro` do outro). No `F12` isso é desligado — tudo volta flush left com papéis de modelo, porque texto escalonado em duas colunas denuncia que é conversa e não log de inferência. Há um `preview_the_chat_layout` (`#[ignore]`) que pinta as duas variantes num `TestBackend` para revisar layout sem abrir o app.

Depois do teste do Pedro (0.1.5): `/help` e `F1` viraram **tela própria** (overlay), porque no transcript o tráfego enterrava a ajuda; apagar sala virou tela de confirmação com `Enter`, alcançável por `Del` na lista de sessões ou por `/forget`; a roda do mouse rola o chat (captura de mouse ligada — `Shift` devolve a seleção de texto do terminal); `Ctrl+End` volta para a mensagem mais nova; e a barra de status agora diz quando você está com a visão rolada para cima e quantas linhas faltam.

Cuidado que já foi pago uma vez: AltGr no teclado ABNT chega como CONTROL+ALT. `is_shortcut()` em `tui.rs` só trata CONTROL puro como atalho — sem isso a acentuação quebra de novo (ver commit `a6785de`).

30 testes passando (mais um `#[ignore]` de preview visual), `cargo clippy --all-targets -- -D warnings` limpo, incluindo os de integração com `TestBackend` que travam exatamente esses comportamentos, dois que exercitam o DPAPI de verdade e dois que travam o bug do log infinito (um deles monta um log poluído com 500 `Meta` e exige que o arquivo encolha ao abrir).

**Ainda não validado:** as duas janelas se falando de verdade. Pedro testou `/join RW8Z-PF5M` no `#2` no `v0.1.2` e ficou 0 peers; o fix veio no `v0.1.3` e nunca foi conferido. Próximo passo: fechar tudo, abrir o release 0.1.4 duas vezes, `/new` numa e `/join <chave>` na outra — a chave agora já vem copiada no clipboard. Se ficar em 0 peers, rodar `/diag` nas duas: ele diz se a rede subiu, quantas rotas conhece e quantos arquivos há em `presence\`. Se passar, prova em 2 PCs na LAN.

Não commitar PIN, `device.key`, `pin.dpapi`, nem a pasta `%LOCALAPPDATA%\local-llm\`.

### Fila de usabilidade que ficou de fora

Levantados e não feitos, em ordem de dor:

1. **Sem confirmação de entrega por mensagem.** Você sabe quantos peers há, não se *aquela* mensagem chegou. Um `✓` por destinatário exigiria ack no protocolo.
2. **Onboarding na primeira execução.** Não explica chave, firewall nem que o PIN não vai no Teams antes de a pessoa se perder.
3. **Nome da sala trai o disfarce.** `/new fofoca-do-time` derruba a fachada; o `/new` deveria empurrar nomes de modelo.
4. **Sala sem nome até sincronizar.** Quem entra por `/join` vê `session` no header até o `Meta` chegar pelo sync.
5. **Notice de peer entrando/saindo pode virar ruído** se a rede oscilar — hoje não há supressão de flapping.
6. **Salt do Argon2id é derivado do próprio PIN** (`crypto.rs:77`), não é aleatório: é um salt global determinístico, então uma rainbow table do espaço de 2^40 é pré-computável. Além disso os parâmetros são 32 MiB, não os 64 MiB do plano. Ponto técnico, não de usabilidade, mas é o item de segurança mais relevante em aberto.
