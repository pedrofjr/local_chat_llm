# local-llm bot — integração para programas

Este texto é a especificação completa. É o mesmo que sai em
`local-llm bot --help`, então a versão instalada nunca discorda dele.

O modo `bot` põe um programa dentro de uma sala de chat. Ele não desenha nada:
uma linha JSON no stdout para cada coisa que ouve, uma linha JSON no stdin para
cada coisa que diz. Sem chaves, sem criptografia, sem formato de arquivo.

```
local-llm bot --room <PIN> [--nick <nome>]
```

`--room` é a chave da sala, no formato `7K2M-9QXP`. `--nick` é o nome com que
o bot aparece para os outros; fica gravado, então só precisa ser passado uma
vez.

## Saída — uma linha, um objeto

Cada linha do stdout é um objeto JSON completo, terminado por `\n`, com o campo
`type` dizendo o que é. Nada é impresso em mais de uma linha: quebras dentro do
texto vão escapadas como `\n`, então **uma linha lida é sempre um objeto
inteiro**.

### `ready` — a sala abriu

```json
{"type":"ready","room":"session","nick":"Godfrey","author":"6712e7…","online":true}
```

| campo | o quê |
|---|---|
| `room` | apelido da sala |
| `nick` | o nome do bot nesta sala |
| `author` | identificador do bot, 64 caracteres hex |
| `online` | `false` quando a rede não subiu; o bot ainda lê e escreve local |

Sai uma vez, antes de qualquer outra coisa.

### `message` — alguém falou

```json
{"type":"message","from":"Joao","author":"cb1732…","text":"@Godfrey qual o status?",
 "ts":1787333824,"mine":false,"mentioned":true}
```

| campo | o quê |
|---|---|
| `from` | nome de quem falou, como aparece na sala |
| `author` | identificador de quem falou, hex |
| `text` | o texto. Numa imagem, é a legenda |
| `ts` | unix seconds |
| `mine` | `true` se foi o próprio bot |
| `mentioned` | **`true` quando a mensagem cita o nick do bot** |
| `whisper` | presente só em mensagem privada, com o nome do outro lado |

**`mentioned` é o gatilho.** Já vem resolvido: casa por palavra inteira, com ou
sem `@`, e não dispara em substring — um bot chamado `Ana` não acorda com
"banana". Não refaça essa conta do seu lado.

Mensagens que já estavam no histórico **não** são repetidas na abertura. Um
reinício não faz o bot responder meses de conversa de uma vez.

### `sent` — o que o bot mandou saiu

```json
{"type":"sent","text":"tudo no ar","delivered":true}
```

`delivered` diz que o envio não deu erro — **não** que alguém já recebeu. Numa
sala vazia ele é `true` e ninguém ouviu. Para entrega garantida, veja
"disparar e sair" abaixo.

### `error` — algo não deu certo

```json
{"type":"error","message":"expected a json object, for example {\"text\":\"hello\"}"}
```

Não encerra o programa. Uma linha de entrada malformada vira um `error` e é
descartada.

## Entrada — uma linha, uma mensagem

```json
{"text":"tudo no ar"}
```

Só o campo `text` é obrigatório. **Outros campos são ignorados**, então dá para
carregar sua própria contabilidade no mesmo objeto:

```json
{"id":42,"text":"reiniciei o serviço","origem":"watchdog"}
```

Regras:

- Uma linha em branco é ignorada.
- Uma linha que **não** seja JSON é **recusada**, não enviada. Isso é
  deliberado: adivinhar significaria mandar para a sala inteira algo que
  ninguém quis dizer.
- Escapes JSON normais funcionam: `\"`, `\\`, `\n`, `\t`, `\uXXXX`.
- **Fechar o stdin encerra o bot.**

## Onde rodar — a parte que engana

O bot é sempre um participante próprio, com chave e nome dele. O que muda é
como ele é encontrado:

- **Na mesma máquina de alguém que usa o chat:** rode **sem**
  `LOCAL_LLM_HOME`. Os dois se acham por um arquivo de presença que
  compartilham. O mDNS não enxerga dois processos do mesmo computador, então
  esse arquivo é o único caminho — e ele fica na pasta base. Apontar o bot para
  outro `LOCAL_LLM_HOME` o coloca onde o chat nunca olha: os dois ficam na
  mesma sala **sem nunca se ver**.
- **Em máquina própria:** vale qualquer coisa, `LOCAL_LLM_HOME` inclusive.

## Disparar e sair

Mandar uma notificação e encerrar funciona:

```
echo {"text":"deploy terminou"} | local-llm bot --room 7K2M-9QXP --nick CI
```

O processo espera até 12 segundos por alguém a quem entregar antes de sair, e
emite um `error` se ninguém apareceu. Isso é necessário: a mensagem vai para os
vizinhos que existem **naquele instante**, e um processo que subiu há um segundo
ainda não tem nenhum.

Para um bot que fica de pé, nada disso importa — os vizinhos já foram achados
muito antes.

## Exemplo completo

```python
import json, subprocess, sys

bot = subprocess.Popen(
    ["local-llm", "bot", "--room", "7K2M-9QXP", "--nick", "Godfrey"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
)

def diga(texto):
    bot.stdin.write(json.dumps({"text": texto}) + "\n")
    bot.stdin.flush()

for linha in bot.stdout:
    evento = json.loads(linha)
    if evento["type"] == "ready":
        print("na sala como", evento["nick"], file=sys.stderr)
    elif evento["type"] == "message" and evento["mentioned"] and not evento["mine"]:
        diga(f"oi {evento['from']}, tudo no ar")
```

Duas coisas que esse exemplo faz de propósito:

- `bufsize=1` e `flush()`. Sem isso, o Python segura as linhas num buffer e o
  bot parece travado. O local-llm já dá flush em cada linha que escreve.
- Checa `not evento["mine"]`. Sem isso, um bot que se mencione na própria
  resposta conversa consigo mesmo para sempre.

## O que não fazer

**Não escreva no `log.bin`.** A tentação é natural — é um arquivo, está ali —
mas não funciona:

- Ele é **cifrado** com uma chave derivada da chave da sala.
- Cada registro é **assinado** com a chave do aparelho e conferido por todos os
  participantes. Um registro sem assinatura válida é recusado por todo mundo, e
  a mensagem não aparece para ninguém.
- O arquivo é **reescrito inteiro** a cada gravação, e se compacta sozinho.
  Não é um log de acrescentar linhas no fim, e qualquer leitura por deslocamento
  fixo quebra na primeira compactação.

Tudo isso o modo `bot` já faz. Ele existe exatamente para que um programa
precise lidar só com texto.
