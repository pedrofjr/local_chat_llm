# Mudanças

Histórico de versões do local-llm, da mais nova para a mais antiga.

**Atualizem juntos.** Onde a versão aparece marcada com ⚠, o protocolo mudou.
Quem ficar para trás continua vendo e mandando **mensagens ao vivo** — isso
passa por um canal que nunca muda — mas **para de receber o histórico**: entra
na sala e ela aparece vazia, sem nenhum sinal de que algo está errado. Da 0.6.1
em diante o app avisa quando isso está acontecendo.

Para atualizar, `/update` dentro do app ou `local-llm update` no terminal.

---

## 0.7.4 — 21/08/2026

**Corrigido — imagem irreconhecível em terminal sem sixel**

A mesma imagem saía nítida numa máquina e em blocos gigantes na outra. Quem
tem sixel vê pixels de verdade; quem não tem cai em meio-bloco — e o
meio-bloco estava sendo desperdiçado.

Meio-bloco desenha **um pixel por meia célula**: a resolução final é
literalmente o número de células. Só que o layout usava o tamanho real do
caractere do terminal (10×20 px), que é a conta certa para sixel e a errada
aqui — a imagem era pedida num tamanho em que jamais seria desenhada. Um print
de 580×419 virava **58×40 pixels**.

Agora a conta usa a célula que o meio-bloco realmente tem, 1×2 px: o mesmo
print sai em 71×52 no feed e 77×56 em tela cheia — **1,6× a 1,9× mais pixels**.

Dito isso, sem enrolação: **meio-bloco nunca vai ficar bom**. Contra os
243.000 pixels que o sixel entrega, mesmo o dobro de quase nada continua sendo
pouco. Se a sua imagem está em blocos, o caminho é o sixel — veja abaixo.

**Novo — `/img proto` agora responde em vez de resetar**

Digitar `/img proto` sem argumento mostra o que está em uso e o tamanho de
caractere que o terminal informou. Antes ele **zerava** a detecção em silêncio,
que é a pior resposta possível a uma pergunta: mudava a coisa perguntada.

Se disser meio-bloco:

- `/img proto sixel` força pixels de verdade. Se a imagem aparecer certa,
  pronto. Se aparecer lixo, o terminal não suporta.
- Sixel precisa de **Windows Terminal 1.22 ou mais novo**. Console antigo
  (`conhost`) não tem.
- `/img proto auto` e reabrir o app refaz a detecção, para o caso de ela ter
  falhado uma vez e ficado gravada.

## 0.7.3 — 21/08/2026

**Corrigido — depois do `/update`, o teclado ia para o PowerShell**

O app voltava para a conversa, continuava desenhando na tela, e não recebia
mais nenhuma tecla — como se o terminal tivesse voltado a ser um prompt de
comando. Eram dois defeitos empilhados, e cada um sozinho já bastaria.

- **O processo que saía desfazia a configuração de quem entrava.** A versão
  nova começa ligando o modo bruto do teclado e entrando na tela alternada. A
  versão velha, que ainda estava terminando de sair, então desligava o modo
  bruto e saía da tela alternada — num console que já pertencia à outra. Daí a
  combinação estranha: a nova continuava desenhando (na tela comum, porque a
  antiga a tirou da alternada) num console de volta ao modo linha.
  Agora a troca é a **última** coisa que acontece, depois de o terminal já ter
  sido devolvido.
- **O PowerShell voltava a ler o teclado.** Ele espera o processo que lançou;
  a versão nova é neta dele, então no instante em que a antiga saía, o prompt
  voltava e passava a disputar as teclas com o app. Quem ganhava era o prompt.
  Agora a versão antiga **espera** a nova terminar antes de sair, então o
  shell continua ocupado e o console tem um dono só.

Efeito colateral aceito: o arquivo da versão substituída (`local-llm.old`) só
é apagado na abertura seguinte, porque o processo antigo fica vivo segurando
ele enquanto você usa o app.

## 0.7.2 — 21/08/2026

A tela cheia da 0.7.0 tinha um defeito grave e a qualidade continuava ruim por
um motivo que não era o que parecia.

**Corrigido — a tela cheia prendia o app**

- Abrir uma imagem em tela cheia **reencodava a imagem inteira a cada quadro
  do loop**, para sempre. A verificação de "já está no tamanho certo"
  comparava o tamanho da *imagem* — que mantém a proporção — com o tamanho da
  *janela*. Os dois praticamente nunca são iguais, então a conta nunca fechava.
  Com um GIF isso significa reencodar todos os quadros, a uns 90 ms cada: o
  app parava de responder ao teclado e matar era a única saída.
- Fechar uma imagem agora limpa a camada de gráficos do terminal. Sixel não é
  texto — o terminal guarda esses pixels à parte, e escrever caracteres por
  cima não os apaga. A imagem ficava sobre uma conversa que já tinha seguido
  em frente, o que parece uma tecla que não funcionou.
- O cabeçalho passa a dizer **`esc closes the picture`** enquanto a imagem
  está aberta. Isso estava só na barra de status, que o próximo evento de rede
  substitui em segundos — e aí sobra uma imagem sem saída visível.

**Qualidade**

- A causa era o filtro de reamostragem. O padrão da biblioteca é `Nearest`,
  que é o mais rápido e o pior: ao reduzir, ele joga fora linhas inteiras de
  pixels, e letra fina de print vira ruído. Agora é Lanczos3, nos dois pontos
  onde a imagem é redimensionada — ao desenhar e, para prints acima de 1920px,
  ao preparar para envio. Custa **19 ms a mais** num print de 1365×767, contra
  um encode que já leva uns 90.
- Comprimir mais antes de enviar não ajudaria: o arquivo já vai sem perda. O
  estrago acontecia no redimensionamento, não no formato.

**Novo**

- Clicar **na imagem aberta** amplia para tela cheia. Antes só a linha
  `image (+)` respondia ao clique.

## 0.7.1 — 21/08/2026

**Corrigido**

- A medição do tamanho de caractere que a 0.7.0 introduziu nunca ia acontecer
  em quem já usava o app. A verificação de "já sei desenhar aqui" olhava só o
  protocolo, e quem já tinha o protocolo gravado saía antes de medir — ou
  seja, exatamente as quatro pessoas que a correção existia para atender
  continuariam com o chute de 10×20. Agora a conta só é dada por encerrada
  quando as duas respostas estão no arquivo.
- Um terminal que não responde passa a ter o valor assumido gravado, em vez de
  deixar o campo zerado e pagar os 3 segundos da consulta em toda abertura.

## 0.7.0 — 21/08/2026

Uma rodada inteira em cima da experiência com imagens, que estava ruim nas
duas pontas: demorava para carregar e saía mal desenhada. Eram três causas
diferentes, e nenhuma era a que parecia.

**Imagem em tela cheia**

- `Ctrl+G` numa imagem já aberta agora **põe ela na janela inteira**, e `Esc`
  volta para a conversa. Print de tela não se lê espremido num canto do feed.
- O feed continua com a imagem em tamanho modesto, de propósito: uma imagem
  que enche a tela enterra a conversa a que ela pertence. Cada tecla faz uma
  das duas coisas.
- O `F12` fecha a imagem da janela junto com todo o resto, e não a traz de
  volta quando o disfarce sai.
- GIF continua animando em tela cheia.

**A imagem saía com a escala errada**

- O app pergunta ao terminal, uma vez, como ele desenha imagens. A resposta
  vem com duas informações — o protocolo e o **tamanho de um caractere em
  pixels** — e a segunda estava sendo jogada fora e substituída por um chute
  de 10×20. Sixel desenha pixels de verdade dentro de uma área medida em
  caracteres, então errar esse número deforma toda imagem. Agora a medida do
  terminal é guardada e usada.

**A demora não era o que parecia**

- Não era o desenho: medido, custa **78 ms** montar um print de 1365×767 para
  a tela. Era a espera pelos bytes.
- Os pixels eram buscados no mesmo ciclo que sincroniza o histórico, **atrás**
  dele, e só de 3 em 3 segundos. As duas esperas são muito diferentes por
  fora: histórico atrasado ninguém percebe, imagem atrasada é alguém olhando
  para a tela. Agora a busca é uma tarefa separada, que **acorda no instante
  em que a imagem chega**.
- E o principal: um endereço lembrado de uma máquina que saiu da rede custava
  **30 segundos** — medido — antes de qualquer pixel ser pedido, porque a
  tentativa de conexão não tinha prazo. Agora tem: 4 segundos, e os peers que
  respondem deixam de esperar pelos que nunca vão responder.

**Atenção**

- Não exige que todos atualizem junto. O protocolo não mudou: uma 0.7.0
  conversa e sincroniza histórico normalmente com uma 0.6.x.

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
