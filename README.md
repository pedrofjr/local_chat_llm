# local-llm

Chat P2P na LAN com cara de client de modelo local. Quatro pessoas, um `.exe`, um PIN.

Não é um LLM. Não fala com a internet. Relays públicos do Iroh ficam desligados.

## Uso

```powershell
local-llm
```

```
  local-llm  0.1.2

  sessions
  > gpt-oss-20b     locked

  /new <name>    /join <pin> [ticket]    /nick <nome>    /quit
  > _
```

| comando | o que faz |
|---|---|
| `/new gpt-oss-20b` | cria a sala e mostra o PIN |
| `/join 7K2M-9QXP` | entra; puxa o histórico dos peers online |
| `/join 7K2M-9QXP <ticket>` | igual, mas disca um peer na unha (mDNS falhou) |
| `/nick Diamante` | muda o nome; mensagens antigas ficam com o nome de quando foram enviadas |
| `/nick` | mostra o nick atual |
| `/pin` | mostra o PIN de novo (sala destrancada) |
| `/ticket` | endereço Iroh desta máquina |
| `/peers` | quantos estão no overlay |
| `/forget` | apaga a sala **desta máquina** |
| `esc` | volta pra lista (o log criptografado fica) |
| `/quit` | sai |

O PIN é um código Crockford (`7K2M-9QXP`). Fala no corredor. **Não manda no Teams.** Quem lê o Teams lê a sala.

## Build

```powershell
cd C:\GIT\projetos-paralelos\local-llm
cargo test
cargo build --release
```

O exe sai em `target\release\local-llm.exe`. Alvo: &lt; 8 MB.

```powershell
Compress-Archive -Path target\release\local-llm.exe -DestinationPath local-llm-0.1.2-windows-x64.zip -Force
```

## Como compartilhar

O Teams **bloqueia `.exe` no chat**. Manda o **zip** ou um link de OneDrive/SharePoint.

Na máquina de quem recebe:

```powershell
Unblock-File .\local-llm.exe
.\local-llm.exe
```

SmartScreen: *More info → Run anyway*. Firewall do Windows: aceitar na **rede privada**. Sem isso o mDNS não acha ninguém.

Duas janelas no mesmo PC (testar sozinho): abre o exe de novo. A segunda vira instância `#2` com identidade própria. Numa você `/new`, na outra `/join PIN` (e o `/ticket` da primeira se o mDNS não achar). Não cole o ticket da **mesma** janela — isso não conecta consigo mesmo.

Variável `LOCAL_LLM_HOME` aponta o diretório de dados. O padrão é `%LOCALAPPDATA%\local-llm\`.

## Como funciona

- Transporte: [Iroh](https://www.iroh.computer) 1.0, só mDNS, sem relay.
- Ao vivo: `iroh-gossip`. O tópico é `blake3("local-llm/v1" \|\| pin)`.
- Histórico: log append-only no disco, ChaCha20-Poly1305, chave Argon2id do PIN.
- Sync: ALPN `local-llm/1` — qualquer peer online serve o que o outro não tem.
- Identidade: Ed25519 persistente em `device.key`. Assina cada mensagem.

Fechar o terminal **não** apaga o histórico. `/forget` apaga. Sem o PIN o arquivo no disco não abre.

## Limites

- Mesma LAN. VLAN diferente ou Wi-Fi com client isolation = mDNS morre. Aí vai de `/ticket`.
- PIN de 40 bits + Argon2id. Serve contra colega e contra dump casual da rede. Não é Signal.
- Quem tem o PIN entra e lê tudo, inclusive o log antigo.
- Dois grupos com o mesmo PIN em redes que nunca se falam criam dois históricos.
