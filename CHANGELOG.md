# Mudanças

Histórico de versões do local-llm, da mais nova para a mais antiga.

**Atualizem juntos.** Onde a versão aparece marcada com ⚠, o protocolo mudou.
Quem ficar para trás continua vendo e mandando **mensagens ao vivo** — isso
passa por um canal que nunca muda — mas **para de receber o histórico**: entra
na sala e ela aparece vazia, sem nenhum sinal de que algo está errado. Da 0.6.1
em diante o app avisa quando isso está acontecendo.

Para atualizar, `/update` dentro do app ou `local-llm update` no terminal.

---

## 0.6.3 — 19/08/2026

**Novo**

- `local-llm update` no terminal: baixa, confere e instala sem abrir o chat.
  Também `local-llm version` e `local-llm help`.
- Um comando que não existe agora dá erro e mostra o uso, em vez de abrir o
  chat como se o argumento tivesse valido.

**Atenção**

- Atualizando pelo terminal com uma janela do chat aberta, a janela continua na
  versão antiga até ser fechada e reaberta — o comando avisa. Atualizar sem
  reiniciar não existe: o programa que está rodando *é* o binário antigo já
  carregado na memória.

## 0.6.2 — 19/08/2026

**Corrigido**

- O `/update` jogava a pessoa numa segunda instância do app: outras salas,
  outro histórico, como se duas janelas estivessem abertas. O processo novo
  subia antes de o antigo morrer e acabava ocupando o lugar errado.
- O bilhete que diz para qual sala voltar depois do restart era gravado na
  pasta errada, então ninguém o lia.
- O aviso de que atualizou ia para a barra de status e sumia em segundos —
  quando aparecia. Agora é uma linha no chat, depois de a sala reabrir, dizendo
  de qual versão para qual.

## 0.6.1 — 19/08/2026

**Corrigido**

- Com alguém do grupo numa versão diferente, o histórico não sincroniza — e
  isso acontecia em silêncio: o app parecia saudável, as mensagens ao vivo
  chegavam, e o histórico simplesmente nunca vinha. Ninguém tinha como
  adivinhar. Agora aparece um aviso dizendo que todos precisam atualizar, e o
  `/diag` mostra quantos responderam.
- O `/update` passou a separar "não existe release publicado" de "sem rede".
  Eram a mesma mensagem e significam coisas opostas.

## 0.6.0 — 19/08/2026

Só a numeração: a 0.5.0 tinha circulado em dois binários diferentes, um com
`/update` e outro sem. Com um mecanismo comparando versões, isso passaria de
confuso a prejudicial. Não foi publicada.

## 0.5.0 — 19/08/2026 ⚠

**Novo — `/update`**

- O app procura a versão nova, pergunta, baixa, instala e reinicia voltando
  para a sala.
- O download passa pelo próprio app, e é por isso que funciona: arquivo escrito
  por um programa não recebe o carimbo de "veio da internet", então o
  SmartScreen deixa de aparecer. O antivírus da empresa ainda escaneia uma vez;
  isso só sai com certificado de assinatura ou liberação da TI.
- Antes de substituir o que está rodando, o binário é conferido de duas formas:
  o sha256, que pega download corrompido, e uma assinatura, que pega download
  intacto vindo das mãos erradas.
- Volta para a sala guardando só o endereço dela, nunca o PIN. Sala com chave
  lembrada reabre sozinha; sala trancada para na tela de destravar.

**Privacidade — o sussurro parou de publicar quem falou com quem**

- O registro gravado no disco trazia remetente e destinatário em claro. Todo
  mundo da sala tem a chave do log, então qualquer um dos quatro podia abrir o
  arquivo e ler a rede social inteira do grupo: quem sussurrou para quem,
  quando, quantas vezes. Só as palavras estavam protegidas — e como na tela
  nada disso aparecia, era pior, não melhor.
- Agora, do lado de fora do envelope, sobra um número aleatório e um horário.
  Remetente, nome, texto e citação vão todos para dentro.
- Os sussurros gravados **antes** desta versão já publicaram o que publicaram.
  Isso não tem volta.

**Atenção**

- ⚠ O histórico não sincroniza com quem estiver abaixo desta versão.

## 0.4.0 — 19/08/2026 ⚠

**Novo — imagens e GIF**

- `/img <caminho>` manda uma imagem. `/img` sozinho, ou Ctrl+V, manda o que
  estiver no clipboard — Win+Shift+S e Ctrl+V envia o print. Ctrl+Shift+V
  força, para o caso de o terminal engolir o Ctrl+V.
- A imagem chega **fechada**, como uma linha de texto (`image (+)`), e só vira
  pixel quando alguém abre com Ctrl+G ou com o clique. A tela em repouso
  continua parecendo log de inferência, e o F12 fecha tudo — inclusive o que já
  estava aberto.
- GIF anima.
- Limite de 2 MB e 1920 px no lado maior; acima disso a imagem é recomprimida.
  GIF acima do limite é recusado em vez de convertido: encolher mataria a
  animação, que era o motivo de mandar.

**Corrigido — sussurro**

- Responder não funcionava no sussurro. Pior que não funcionar: a citação
  ficava armada e saía na **próxima mensagem normal**, para a sala inteira.
- Não dá mais para responder um sussurro em voz alta. Quem não tem o sussurro
  vê "(message not here yet)" e nada vaza — mas na *sua* tela a citação
  renderizava inteira, colada numa mensagem que a sala toda lê. É assim que
  alguém escreve "pois é, melhor não contar pro Carlos" acreditando que o
  contexto está à vista. Agora o envio é recusado e o texto volta no input já
  como `/w Fulano ...`, a um Enter de ir em privado.
- A mesma armadilha estava aberta em mais dois caminhos: marcar um sussurro e
  mandar `/w` para uma terceira pessoa, e marcar um sussurro e mandar imagem.
  Nos dois a citação é removida, com aviso.
- **O sussurro virou modo.** `/w Fulano` aponta o prompt e ele fica apontado: o
  `>` vira `Fulano →` em âmbar e negrito, com a borda mudando de cor junto.
  Antes bastava esquecer o `/w` em uma linha no meio de uma conversa privada
  para ela ir à sala inteira, sem aviso nenhum. Esc solta — mas por último:
  primeiro a citação, depois o texto digitado, só então o sussurro.
- Sob F12 o nome sai do prompt junto com o rascunho. O modo continua ligado, e
  o engano cai para o lado seguro: o texto vai para o sussurro, não para a
  sala.

**Atenção**

- ⚠ O histórico não sincroniza com quem estiver abaixo desta versão.

## 0.3.4 — 18/08/2026

**Corrigido**

- "Essa sala eu entro só comigo": o endereço dos colegas estava sendo gravado
  como se todo mundo morasse na sua máquina, então o app discava localmente e
  não encontrava ninguém. Existia desde a 0.1.3, atingindo só o ticket colado à
  mão; a 0.3.1 espalhou para todo endereço aprendido e passou a gravar em
  disco, e aí virou permanente.

## 0.3.3 — 18/08/2026

Cinco coisas do primeiro dia de uso do grupo.

- Sussurro para nome com espaço ("Grok 4.5") quebrava: ele pegava a primeira
  palavra como destinatário e o resto virava a mensagem. Agora não precisa de
  aspas, e "Ana" não casa dentro de "Anabela". O Tab também completa através do
  espaço.
- Responder uma mensagem escondida colocava o texto dela de volta na tela, na
  linha de citação.
- A barra de status ficava repetindo que estava procurando outros nós, com a
  sala cheia.
- Qualquer aviso puxava a tela para o fim, tirando da linha quem estava lendo
  para trás.
- Alt+Enter e Ctrl+Enter viraram sinônimos de Shift+Enter para quebrar linha,
  caso o terminal engula a combinação.

## 0.3.2 — 18/08/2026

**Corrigido**

- Enter virava quebra de linha ao digitar, em vez de enviar: a detecção de
  colagem disparava em quase toda tecla. Se ainda incomodar, `/paste off` faz o
  Enter sempre enviar, ao custo de um bloco colado voltar a virar uma mensagem
  por linha.
- A tela de ajuda tinha crescido além do que cabe e escondia o fim da lista;
  virou duas colunas.

## 0.3.1 — 18/08/2026

**Corrigido**

- O contador dizia "1 online" com três pessoas conversando — ele contava
  vizinhos de rede, não gente. Agora cada um anuncia presença a cada 5 s e sai
  da lista após 20 s calado.
- Entrar numa sala levava mais de um minuto para enxergar o resto do grupo.
  Agora quem chega aprende a sala inteira em segundos.
- O cabeçalho, o `/peers` e o `/diag` passaram a distinguir as duas coisas:
  quem está na sala, e quantos vizinhos de rede existem.

## 0.3.0 — 18/08/2026

**Novo**

- Colar texto de várias linhas virava várias mensagens; agora vira uma.
- Voltar para a sala pedia o endereço dos colegas mesmo com a sala já guardada
  na máquina. Agora cada sala lembra quem já encontrou, num arquivo cifrado —
  saber com quem você fala, e em que máquina, é tão sensível quanto o
  histórico.
- Esconder mensagem, por Ctrl+H ou pelo ícone que aparece com o mouse: o corpo
  vira blocos, com nome e horário à vista. É local — não vai para o log nem
  para os outros, e fica guardado nesta máquina.

## 0.2.0 — 18/08/2026 ⚠

**Novo**

- **Responder:** Alt+setas escolhem a mensagem, Ctrl+R responde, Ctrl+Y copia.
  Passar o mouse revela os ícones.
- **Sussurro:** `/w <nome> <texto>`, com Tab completando o nome. Terceiros
  guardam o registro e não conseguem ler nada. (O metadado ainda vazava nesta
  versão — corrigido na 0.5.0.)
- Cor por pessoa, igual em todas as máquinas, sem configurar nada.
- Notificações em `all`, `mention`, `off` ou `30m`. Menção casa por palavra
  inteira, para "ana" não disparar em "banana".

**Corrigido**

- O F12 vazava o nome real na linha de citação. Agora citações e sussurros
  somem inteiros.

**Atenção**

- ⚠ O histórico não sincroniza com quem estiver abaixo desta versão.

## 0.1.5 — 18/08/2026

**Corrigido**

- O log crescia para sempre: uma sala de teste acumulou 12.441 registros para
  meia dúzia de mensagens. Agora ele se compacta sozinho ao abrir.

**Interface**

- O chat cola na mensagem mais nova; PageUp e PageDown deixaram de ser
  invertidos; roda do mouse e Ctrl+End também rolam.
- Mensagens próprias à direita, dos outros à esquerda. F12 desliga tudo.
- Avisos e a chave da sala saíram da barra de status para o corpo do chat, onde
  o próximo evento de rede não os apaga.
- A ajuda virou tela própria (F1 ou `/help`), que o tráfego não enterra.
- Apagar sala pede confirmação. Esc limpa a linha em vez de derrubar a sessão.
- Horário nas mensagens e separador de dia; `/peers` diz quem está online.

**Segurança**

- PIN lembrado por sala, guardado pelo próprio Windows; `/lock` reverte.

## 0.1.3 — 17/08/2026

- Duas janelas no mesmo PC não se encontravam — o mDNS do Windows não vê dois
  processos da mesma máquina. Serve para testar sozinho.

## 0.1.2 — 17/08/2026

- `/join` com ticket travava a tela.

## 0.1.1 — 17/08/2026

- Teclado morto no Windows: nenhuma tecla chegava, no WezTerm e no PowerShell.
- A linha de digitação recortava o texto.

## 0.1.0 — 17/08/2026

Primeira versão. Sala por PIN, sem servidor no meio, histórico cifrado no disco
e sincronizado entre quem está na sala.

---

Não existiram uma 0.1.4 nem versões 0.4.x/0.5.x além das listadas. A 0.4.0 e a
0.5.0 circularam como binário entregue à mão, antes de existir o `/update`; os
releases no GitHub começam na 0.6.1.
