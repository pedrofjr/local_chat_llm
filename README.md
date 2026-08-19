# local-llm

Chat P2P na LAN com cara de client de modelo local. Quatro pessoas, um `.exe`, um PIN.

Não é um LLM. Não fala com a internet. Relays públicos do Iroh ficam desligados.

## Uso

```powershell
local-llm
```

A interface é toda em inglês, para combinar com a fachada de client de modelo.

```
  local-llm  0.6.1

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
| `/w Diamante` | aponta o prompt para ele; as próximas linhas vão só pra ele até `Esc` |
| `/w Diamante <texto>` | manda e continua apontado. Nome com espaço funciona; `Tab` completa |
| `/nick Diamante` | muda o nome; mensagens antigas ficam com o nome de quando foram enviadas |
| `/notify` | `all`, `mention`, `off`, ou `30m` para calar por um tempo |
| `/img` | manda a imagem que está na área de transferência |
| `/img C:\...\erro.png` | manda um arquivo do disco |
| `/img proto sixel\|halfblocks\|auto` | como desenhar imagens, se o palpite errar |
| `/pin` | mostra a chave de novo e copia |
| `/ticket` | endereço Iroh desta máquina, copiado pro clipboard |
| `/peers` | **quem** está online agora, por nome |
| `/leave` | volta pra lista de sessões (o log fica) |
| `/lock` | para de guardar a chave nesta máquina |
| `/forget` | apaga a sala desta máquina — abre tela de confirmação |
| `/diag` | estado da rede quando ninguém aparece |
| `/update` | procura uma versão nova, pergunta, e instala se você quiser |
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
| `Ctrl+G` | abre/fecha a imagem da escolhida (ou clique na linha `image (+)`) |
| `Ctrl+Shift+V` | manda a imagem da área de transferência |
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

`/w <nome>` aponta o prompt para alguém, e ele **fica apontado**: o `> ` vira
`Fulano →` em âmbar, com a borda mudando de cor junto. As próximas linhas vão
para ele até você apertar `Esc`. Isso não é conforto — é o conserto do acidente
mais provável do app. Antes, sussurro era um comando por mensagem, e bastava
esquecer o `/w` em uma linha no meio de uma conversa privada para ela ir à sala
inteira, sem aviso nenhum.

`Esc` solta, mas em último lugar: primeiro a citação, depois a linha digitada, e
só então o sussurro. Uma tecla a mais nunca joga meia frase privada na sala. E
no `F12` o nome sai do prompt junto com o rascunho — nome real atravessando o
disfarce é exatamente o que o F12 existe para evitar.

O conteúdo é cifrado com uma chave que só existe entre vocês dois (X25519
derivada do `device.key`). E o registro que fica no disco de todo mundo **não
diz quem falou com quem**: remetente, nome, texto e assinatura vão todos para
dentro do envelope, e quem recebe descobre que a mensagem é sua tentando abrir
cada uma. De fora sobra um identificador aleatório, um horário e bytes ilegíveis.

O sussurro também não consome mais número de sequência junto com as mensagens
públicas. Isso era um vazamento por si só: um buraco entre duas mensagens
visíveis da mesma pessoa anunciava que algo privado tinha acontecido.

O que **ainda vaza**: que houve um sussurro, quando, e o tamanho aproximado.
Esconder isso exigiria tráfego de cobertura — mensagens falsas o tempo todo — e
numa sala de quatro pessoas o ganho seria pequeno.

Sussurro também responde: marque a mensagem com `Ctrl+R` (ou o `↩ reply`) e
mande o `/w` normalmente. A citação viaja **dentro** do texto cifrado, então
os outros continuam sem saber sequer que você respondeu àquela mensagem — de
fora, o metadado é o mesmo de qualquer sussurro.

O caminho contrário é barrado: se você marcar um **sussurro** e escrever uma
mensagem comum, ela não sai. O texto volta pronto como `/w Fulano ...` — um
Enter manda em privado, `Esc` solta a citação e aí vai pra sala.

O motivo não é o que parece. A sala **não** conseguiria ler o sussurro citado:
quem não tem a chave vê `(message not here yet)` no lugar. O risco é o
inverso — **você** veria a citação inteira na sua tela, colada numa mensagem
pública, e escreveria a frase seguinte como se todo mundo tivesse aquele
contexto. É assim que o sussurro vaza pelas suas próprias palavras.

A regra completa, para cada combinação:

| você marcou | e mandou | o que acontece |
|---|---|---|
| mensagem da sala | mensagem da sala | cita normal; todos leem a citação |
| mensagem da sala | `/w Fulano` | cita normal; Fulano lê, a sala nem vê o sussurro |
| mensagem da sala | imagem | cita normal |
| **sussurro do Fulano** | mensagem da sala | **não sai.** Volta como `/w Fulano …` |
| **sussurro do Fulano** | `/w Fulano` | cita; só vocês dois leem |
| **sussurro do Fulano** | `/w Beltrano` | vai **sem** a citação, e avisa |
| **sussurro do Fulano** | imagem | vai **sem** a citação, e avisa |

As três últimas linhas são a mesma regra dita de três jeitos: **a citação de
um sussurro só existe entre as duas pessoas daquele sussurro.** Para qualquer
outro destino ela é removida, porque quem recebe não conseguiria abri-la — e
o resultado seria de novo um contexto que só você enxerga.

Onde dá para devolver o que você digitou, o app recusa e devolve; onde não dá
(imagem, que teria de ser recortada de novo), ele manda sem a citação e diz
que fez isso.

No `F12` os sussurros **somem da tela** por inteiro, e as citações também —
elas carregariam um nome real através do disfarce.

## Imagens

`Win+Shift+S` recorta a tela, `Ctrl+V` manda. Também funciona arrastar o
arquivo pro Explorer, copiar e colar, ou `/img C:\caminho\erro.png`.

A imagem **chega fechada**, como uma linha de texto:

```
  Dale  14:20
  image (+)  1280x720  184 KB  png
```

`Ctrl+G` — ou um clique na linha — abre. `Ctrl+G` de novo fecha. Isso não é
economia de espaço: é o que mantém o disfarce. A tela em repouso continua
sendo texto, e no `F12` **toda** imagem fecha, inclusive as que você tinha
aberto, e a linha passa a ler como entrada multimodal (`image input`).

GIF anima. Os quadros são preparados uma vez, na hora de abrir; depois disso
animar é barato. E a animação **para** quando você fecha, quando ela rola pra
fora da tela, ou quando o `F12` entra — um app que continua trabalhando
enquanto finge estar parado é exatamente o tipo de detalhe que entrega.

Quem estiver no **Windows Terminal 1.22 ou mais novo** vê a imagem de verdade
(sixel). Quem não estiver vê a mesma imagem em meio-bloco, com menos
resolução — não vê um erro. O app decide isso sozinho, uma vez, na primeira
execução; `/img proto sixel` ou `/img proto halfblocks` corrige se o palpite
sair errado.

Limites: **2 MB** por imagem e 1920px no maior lado. Acima disso a imagem é
reduzida e recomprimida sozinha. GIF acima do limite é **recusado** em vez de
convertido, porque encolher mataria a animação, que era o ponto.

Os pixels não viajam junto com a mensagem. O que vai pelo gossip é só a
descrição — hash, tamanho, formato — e os bytes vêm depois, por uma conexão
própria, guardados cifrados com a chave da sala em `blobs/`. Cada sala gasta
no máximo **200 MB** com imagens; passando disso, as mais antigas saem e a
linha passa a dizer `image (unavailable)`.

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
`Shift+Enter` quebra linha em qualquer um dos modos — e `Alt+Enter` e
`Ctrl+Enter` fazem o mesmo, para o caso de o terminal engolir o Shift.

A chave é um código Crockford (`7K2M-9QXP`). Fala no corredor. **Não manda no
Teams.** Quem lê o Teams lê a sala.

## Build

```powershell
cd C:\GIT\projetos-paralelos\local-llm
cargo test
cargo build --release
```

O exe sai em `target\release\local-llm.exe`. Alvo: &lt; 8 MB — hoje em
**7,2 MB**, sendo ~0,8 MB do cliente HTTP que o `/update` usa. A folga ficou
curta; a próxima feature grande provavelmente exige rever o alvo.

```powershell
Compress-Archive -Path target\release\local-llm.exe -DestinationPath local-llm-0.6.1-windows-x64.zip -Force
```

## Atualizar

```
/update
```

Ele olha o release mais novo, compara com o que você tem e **pergunta** antes de
qualquer coisa. Aceitando: baixa, confere, troca o executável, reinicia e volta
pra sala em que você estava.

Isso é a única coisa no app que fala com fora da LAN, e **só** quando você
digita o comando. Não há checagem automática ao abrir.

O motivo de existir não é preguiça. Baixar o `.exe` pelo navegador faz o Windows
carimbar o arquivo como vindo da internet, e aí o SmartScreen entra na frente
com o "More info → Run anyway". Arquivo escrito por um programa **não** leva
esse carimbo — então o app baixando o próprio binário elimina aquela parede.

O que **não** elimina: o antivírus da empresa ainda analisa o exe uma vez. Isso
só sai com certificado de assinatura ou com a TI colocando o app na allowlist —
nenhum dos dois está na nossa mão.

### Por que é seguro deixar o app se atualizar

Duas verificações independentes, que pegam coisas diferentes:

- **sha256** — download truncado ou corrompido.
- **assinatura Ed25519** — download inteiro, mas vindo das mãos erradas. O TLS
  do GitHub prova que os bytes chegaram sem alteração; **não** prova que o
  GitHub estava servindo o nosso binário. Só roda o que foi assinado com a
  chave de release, cuja metade pública está dentro do exe.

Falhando qualquer uma, os bytes não chegam a virar arquivo executável.

A troca usa a única brecha que o Windows deixa: não dá para sobrescrever um exe
rodando, mas dá para renomeá-lo. O que está rodando sai como `.old`, o novo
assume o nome, e é lançado; ele varre o `.old` na primeira execução. Se o
segundo passo falhar, o antigo volta ao lugar — a máquina nunca fica sem
executável.

Voltar pra sala grava **só o identificador da sala, nunca a chave**. Sala com
chave lembrada reabre sozinha; sala trancada para na tela de chave, que é o
certo — um restart não pode tirar alguém da conversa em silêncio.

### Publicar uma versão (quem mantém)

```powershell
.\scripts\publish-release.ps1 -KeyFile $HOME\local-llm-release.key
# quando o formato de rede muda e as versões velhas não conectam mais:
.\scripts\publish-release.ps1 -KeyFile $HOME\local-llm-release.key -MinVersion 0.6.0
```

Ele roda testes e clippy, builda, assina, e **recusa publicar se a chave não for
a mesma embutida no app** — errar isso seria publicar algo que ninguém instala.
A chave privada é lida do arquivo e descartada; não passa pelo console nem pelo
repositório. `-DryRun` mostra tudo sem taggear nem subir.

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
- Sync: ALPN `local-llm/4` — qualquer peer online serve o que o outro não tem.
  Os registros viajam como bytes opacos, então um registro que este build não
  entende custa aquele registro, não o lote inteiro.
- Imagem: o log carrega só a descrição; os pixels ficam num blob cifrado com a
  mesma chave e viajam numa conexão à parte. O que a mensagem assina é o
  **hash** do blob, e um blob cujo conteúdo pare de bater com o nome não abre
  — junto, isso autentica a imagem sem que ela passe pelo log.
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
- O sussurro esconde **com quem** você falou, não **que** você falou. Quem abrir
  o `log.bin` vê que houve um sussurro e quando, só não de quem para quem.
  Sumir com isso exigiria mandar mensagens falsas o tempo todo.
- Sussurros escritos até a 0.4.0 continuam legíveis, mas aqueles registros
  **já publicaram** quem falou com quem — o que está no disco não muda.
- **Todos precisam atualizar juntos.** O ALPN foi para `local-llm/4` na 0.5.0;
  versões diferentes não se conectam.
- Endereço guardado envelhece (DHCP troca IP). A tentativa falha rápido e o
  identificador continua servindo pro mDNS resolver; `/ticket` segue como
  último recurso.
- Esconder mensagem é **visual e local**. A mensagem continua inteira no log, e
  quem tiver a chave da sala lê normalmente.
- Presença é o que cada um **diz** de si, assinado. Não é prova de que a pessoa
  está na frente da máquina, e some sozinha depois de 20 s sem sinal.
- Imagem fechada é discrição, não segurança: os bytes estão na máquina de todo
  mundo que estava na sala. Fechar tira da tela, não do disco alheio.
- Tráfego de imagem tem outra cara na rede que tráfego de texto. O app deixa de
  ser miúdo e invisível quando vocês passam o dia trocando print.
- Quem não tem sixel vê a imagem em meio-bloco: dá pra saber o que é, não dá
  pra ler texto miúdo num print de tela.
