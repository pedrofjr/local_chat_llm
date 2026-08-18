# local-llm

Chat P2P na LAN com cara de client de modelo local. Quatro pessoas, um `.exe`, um PIN.

Não é um LLM. Não fala com a internet. Relays públicos do Iroh ficam desligados.

## Uso

```powershell
local-llm
```

A interface é toda em inglês, para combinar com a fachada de client de modelo.

```
  local-llm  0.3.2

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
| `/w Diamante <texto>` | sussurro: só ele lê. `Tab` completa o nome |
| `/nick Diamante` | muda o nome; mensagens antigas ficam com o nome de quando foram enviadas |
| `/notify` | `all`, `mention`, `off`, ou `30m` para calar por um tempo |
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
| `Alt+↑` / `Alt+↓` | escolhe uma mensagem |
| `Ctrl+R` | responde a escolhida (ou clique no `↩ reply` que aparece no hover) |
| `Ctrl+Y` | copia a escolhida (ou clique no `⧉ copy`) |
| `Ctrl+H` | borra a escolhida **só na sua tela** (ou clique no `▨ hide`) |
| `Tab` | completa o nome no `/w` |
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

## Responder, sussurrar, cores

Passar o mouse sobre uma mensagem revela `↩ reply  ⧉ copy`; clicar age sobre
aquela mensagem. Pelo teclado, `Alt+↑`/`Alt+↓` escolhem e `Ctrl+R`/`Ctrl+Y`
respondem ou copiam. A resposta aparece citada:

```
  Dale  11:25
  fechou. lembra que o banco cai as 18h

                                      ┌ Dale: fechou. lembra que o banco…
                                                          11:25  Pedro
                                                  opa, subo 17h entao
```

Cada pessoa recebe uma **cor própria**, derivada do identificador dela — todo
mundo vê a mesma pessoa na mesma cor, sem ninguém configurar nada.

`/w <nome> <texto>` manda um sussurro. Ele é cifrado com uma chave que só
existe entre vocês dois (X25519 derivada do `device.key`), então os outros
guardam bytes ilegíveis mesmo tendo a chave da sala. O que **vaza** é o
metadado: quem leu o log vê que houve um sussurro, de quem para quem e quando.
Só o conteúdo é protegido.

No `F12` os sussurros **somem da tela** por inteiro, e as citações também —
elas carregariam um nome real através do disfarce.

## Esconder uma mensagem

`Ctrl+H`, ou o `▨ hide` que aparece ao passar o mouse, borra a mensagem:

```
  Dale  11:51
  ██████████████████████████████
```

Nome e horário continuam à vista — você sabe que tem mensagem e de quem, sem
ler. Isso vale **só na sua tela**: nada vai para o log nem para os outros, e
fica guardado nesta máquina (cifrado, junto com a sala). Alternar revela de
novo, e continua revelada até você esconder outra vez.

## Quem está online

O cabeçalho e o `/peers` mostram **quem está na sala**, não com quem seu app
tem conexão direta. Cada um anuncia presença a cada 5 s pelo gossip, e some da
lista depois de 20 s calado.

Esse anúncio carrega o endereço de quem o mandou, e é isso que faz a sala
fechar rápido: quem entra com um único `ticket` conhece **uma** pessoa, e o
gossip só apresenta o resto no shuffle dele, que roda na casa do minuto. Com o
anúncio, todo mundo se acha em segundos.

## Voltar pra sala

O app **lembra o endereço de quem já encontrou** naquela sala e volta a
procurar sozinho ao abrir, com intervalos crescentes enquanto ninguém aparece.
Na prática você não precisa mais pedir `/ticket` a ninguém para voltar — ele
só serve quando o colega é novo ou quando nada mais funcionou.

Os endereços ficam num arquivo cifrado com a chave da sala, dentro da pasta
dela: saber com quem você fala e em que máquina é tão sensível quanto o
histórico.

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

Colar funciona, inclusive **texto de várias linhas**, que vira uma mensagem só.
No Windows o terminal não avisa que houve uma colagem — o crossterm só entende
bracketed paste no Unix — então o app detecta pela rajada de eventos: só trata
a quebra como parte da mensagem depois de uma sequência de teclas separadas por
menos de 5 ms, coisa que ninguém digita. `Enter` digitado sempre envia.

Se ainda assim atrapalhar, `/paste off` desliga a detecção: `Enter` passa a
sempre enviar e um bloco colado volta a virar uma mensagem por linha.
`Shift+Enter` quebra linha em qualquer um dos modos.

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
Compress-Archive -Path target\release\local-llm.exe -DestinationPath local-llm-0.3.2-windows-x64.zip -Force
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
- Sync: ALPN `local-llm/2` — qualquer peer online serve o que o outro não tem.
  Os registros viajam como bytes opacos, então um registro que este build não
  entende custa aquele registro, não o lote inteiro.
- Identidade: Ed25519 persistente em `device.key`. Assina cada mensagem. Dela
  também sai, por derivação, a chave X25519 usada nos sussurros — publicada num
  registro assinado, para ninguém plantar chave em nome alheio.
- Um registro ilegível não impede a sala de abrir: ele é pulado e contado.

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
- `F12` disfarça nomes e avisos, não o texto das mensagens — exceto sussurros,
  que somem por completo.
- Sussurro não tem forward secrecy: as duas pontas derivam sempre a mesma
  chave, que é justamente o que deixa você reler o que mandou. Quem roubar um
  `device.key` depois consegue abrir sussurros antigos daquela pessoa.
- **Todos precisam atualizar juntos.** O formato de registro mudou na 0.2.0 e o
  ALPN foi para `local-llm/2`; versões diferentes não sincronizam.
- Endereço guardado envelhece (DHCP troca IP). A tentativa falha rápido e o
  identificador continua servindo pro mDNS resolver; `/ticket` segue como
  último recurso.
- Esconder mensagem é **visual e local**. A mensagem continua inteira no log, e
  quem tiver a chave da sala lê normalmente.
- Presença é o que cada um **diz** de si, assinado. Não é prova de que a pessoa
  está na frente da máquina, e some sozinha depois de 20 s sem sinal.
