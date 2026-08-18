# local-llm

Chat P2P na LAN com cara de client de modelo local. Quatro pessoas, um `.exe`, um PIN.

Não é um LLM. Não fala com a internet. Relays públicos do Iroh ficam desligados.

## Uso

```powershell
local-llm
```

A interface é toda em inglês, para combinar com a fachada de client de modelo.

```
  local-llm  0.1.4

  sessions
  >  gpt-oss-20b              ready
     qwen2.5-coder            locked

  enter opens the highlighted one    ready = key saved on this pc
  > _
```

`ready` quer dizer que a chave está guardada nesta máquina (DPAPI) e a sala abre
com um Enter. `locked` pede a chave.

| comando | o que faz |
|---|---|
| `/new gpt-oss-20b` | cria a sala, mostra a chave **no corpo do chat** e copia pro clipboard |
| `/join 7K2M-9QXP` | entra; puxa o histórico dos peers online |
| `/join 7K2M-9QXP <ticket>` | igual, mas disca um peer na unha (mDNS falhou) |
| `/nick Diamante` | muda o nome; mensagens antigas ficam com o nome de quando foram enviadas |
| `/pin` | mostra a chave de novo e copia |
| `/ticket` | endereço Iroh desta máquina, copiado pro clipboard |
| `/peers` | **quem** está online agora, por nome |
| `/leave` | volta pra lista de sessões (o log fica) |
| `/lock` | para de guardar a chave nesta máquina |
| `/forget` | apaga a sala desta máquina — abre tela de confirmação |
| `/diag` | estado da rede quando ninguém aparece |
| `/help` | tela de ajuda (também em `F1`) |
| `/quit` | sai |

Teclas:

| tecla | efeito |
|---|---|
| `F1` | ajuda — ocupa a tela inteira, nada empurra ela pra fora |
| `F12` | disfarce: nomes viram papéis de modelo, avisos e o rascunho somem |
| `PgUp` / `PgDn` | rola o histórico — a roda do mouse também rola |
| `Ctrl+End` | volta pra mensagem mais nova |
| `Del` | na lista de sessões: apaga a selecionada (pede confirmação) |
| `↑` / `↓` | repete o que você já digitou |
| `Shift+Enter` | quebra de linha na mesma mensagem |
| `Esc` | limpa a linha (**não** sai da sala) |
| `Ctrl+W` / `Ctrl+U` | apaga palavra / apaga até o começo |
| `Ctrl+C` | sai |

A roda do mouse rola porque o app captura o mouse. Isso tira a seleção de texto
com arrastar — segure `Shift` para selecionar como de costume.

## Layout

Suas mensagens ficam à direita, as dos outros à esquerda, como em qualquer chat:

```
  Dale  09:51
  e a daily, o que ficou?

                                                  09:51  Pedro
                               deploy quinta, eu pego o script
```

No `F12` isso **some**: tudo volta pro alinhamento à esquerda e os nomes viram
papéis de modelo, porque texto escalonado em dois lados lê como conversa, não
como log de inferência.

```
  gpt-oss
  e a daily, o que ficou?

  user
  deploy quinta, eu pego o script
```

Para conferir o layout sem abrir o app (ele precisa de terminal de verdade):

```powershell
cargo test preview_the_chat_layout -- --ignored --nocapture
```

Colar funciona (bracketed paste) — é assim que se passa um `ticket`, que é
grande demais para digitar.

A chave é um código Crockford (`7K2M-9QXP`). Fala no corredor. **Não manda no
Teams.** Quem lê o Teams lê a sala.

## Build

```powershell
cd C:\GIT\projetos-paralelos\local-llm
cargo test
cargo build --release
```

O exe sai em `target\release\local-llm.exe`. Alvo: &lt; 8 MB.

```powershell
Compress-Archive -Path target\release\local-llm.exe -DestinationPath local-llm-0.1.3-windows-x64.zip -Force
```

## Como compartilhar

O Teams **bloqueia `.exe` no chat**. Manda o **zip** ou um link de OneDrive/SharePoint.

Na máquina de quem recebe:

```powershell
Unblock-File .\local-llm.exe
.\local-llm.exe
```

SmartScreen: *More info → Run anyway*. Firewall do Windows: aceitar na **rede privada**. Sem isso o mDNS não acha ninguém.

Duas janelas no mesmo PC (testar sozinho): abre o exe de novo. A segunda vira instância `#2`. Numa você `/new`, na outra `/join PIN` — elas se acham sozinhas por um arquivo em `%LOCALAPPDATA%\local-llm\presence\` (mDNS no Windows não vê dois processos da mesma máquina). Ticket só precisa se for **outro computador**.

Variáveis de ambiente:

- `LOCAL_LLM_HOME` — diretório de dados. Padrão: `%LOCALAPPDATA%\local-llm\`.
- `LOCAL_LLM_OFFLINE` — sobe sem rede nenhuma. Lê e escreve o histórico local;
  nada sai nem entra. Útil quando o firewall barra e você só quer reler.

## Como funciona

- Transporte: [Iroh](https://www.iroh.computer) 1.0, só mDNS, sem relay.
- Ao vivo: `iroh-gossip`. O tópico é `blake3("local-llm/v1" \|\| pin)`.
- Histórico: log append-only no disco, ChaCha20-Poly1305, chave Argon2id do PIN.
- Sync: ALPN `local-llm/1` — qualquer peer online serve o que o outro não tem.
- Identidade: Ed25519 persistente em `device.key`. Assina cada mensagem.

- Chave lembrada: o PIN vai pro disco embrulhado em **DPAPI** (escopo do seu
  usuário do Windows), em `rooms\<topic>\pin.dpapi`. Outro usuário ou outra
  máquina não abre esse blob. `/lock` apaga.

Fechar o terminal **não** apaga o histórico. `/forget` apaga. Sem o PIN o arquivo no disco não abre.

## Limites

- Mesma LAN. VLAN diferente ou Wi-Fi com client isolation = mDNS morre. Aí vai de `/ticket`.
- PIN de 40 bits + Argon2id. Serve contra colega e contra dump casual da rede. Não é Signal.
- Quem tem o PIN entra e lê tudo, inclusive o log antigo.
- Dois grupos com o mesmo PIN em redes que nunca se falam criam dois históricos.
- Com a chave lembrada, **quem sentar na sua máquina logada lê a sala**. Esse é
  o preço de não digitar o PIN toda vez. `/lock` reverte.
- `F12` disfarça nomes e avisos, não o texto das mensagens.
