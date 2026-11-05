# Plano: modelo de conexão e permissões entre PCs

> Status: proposta para revisão. Nada de código foi alterado além deste documento.
> Base: auditoria do working tree em 2026-11-05 (inclui as mudanças não commitadas de DPI/GUI/testes).

## 1. Sumário executivo

Hoje o lan-mouse tem **um único gate de autorização**: a fingerprint do certificado no handshake
DTLS do lado que escuta. A partir do momento em que um dispositivo é autorizado, ele pode
**tudo, para sempre e nas duas direções**: mover o mouse, digitar, tomar o capture via `Enter`,
ler/escrever o clipboard. Não existe "pode só clipboard", "pode só me controlar", "pode só nesta
sessão" nem revogação que interrompa uma conexão viva. Além disso, o lado que **disca** não
autentica o servidor (`insecure_skip_verify: true`), então um IP reutilizado/MITM recebe input e
clipboard sem nenhum aviso.

A proposta é transformar "autorizar" em um modelo de **dispositivo + política por direção +
capacidades**:

- **Device** = identidade estável (fingerprint SHA-256 + nome), independente de IP.
- **Política `incoming`** (autoritativa, imposta aqui): o que o outro pode fazer **neste** PC.
- **Política `outgoing`** (guard local): o que **este** PC pode fazer no outro.
- **Capacidades**: `pointer`, `keyboard`, `clipboard` (fase 3 separa clipboard leitura/escrita).
- **Modos**: `allow` / `ask` / `deny`, com deny-list que vence pairing e regras de endereço.
- **Aprovação explícita** com escopos + código de verificação (SAS) no primeiro pareamento.
- **Revogação efetiva**: derruba a sessão, bloqueia reconexão e é auditada.
- **Compatibilidade**: peers antigos (`PROTO_VERSION = 1`) continuam funcionando sob política
  legada explícita; config antigo é migrado com dual-write.

Entrega em 4 fases, cada uma utilizável sozinha. A **Fase 0** é correção de segurança e deve vir
primeiro; a **Fase 1** é o MVP pedido ("um permite a conexão e define o que o outro pode controlar").

---

## 2. Como funciona hoje (estado atual)

### 2.1 Descoberta e conexão

| Peça | Onde | Comportamento |
|---|---|---|
| Beacon LAN | `src/discovery.rs:409-434` | UDP a cada 10 s com hostname, porta, versão e **fingerprint declarada** (não autenticada) |
| Conexão | `src/connect.rs:449-509` | DTLS 1.2 sobre UDP (porta 4242); disca todos os IPs do client em paralelo (happy-eyeballs) e o primeiro handshake vence |
| Autenticação (ouvinte) | `src/listen.rs:99-104, 298-348` | `RequireAnyClientCert` + callback que compara a fingerprint com `authorized_keys`, `trusted_ips` ou janela de pairing |
| Autenticação (discador) | `src/connect.rs:467-473` | **Nenhuma**: `insecure_skip_verify: true`, `server_name: "ignored"` |
| Capacidades de wire | `lan-mouse-proto/src/lib.rs:24-44` | `ENTER_ACK` e `CLIPBOARD` são só negociação de features; **não** são permissões |

### 2.2 Quem aprova o quê

1. O peer disca; se a fingerprint não é conhecida, o ouvinte rejeita e emite
   `ListenEvent::Rejected` (`src/listen.rs:183-194`).
2. Isso vira `FrontendEvent::ConnectionAttempt` (`src/emulation.rs:298-303` →
   `src/service.rs:460-464`) e a GUI abre a janela "Unauthorized Device"
   (`lan-mouse-gtk/src/window.rs:955-1003`, `resources/authorization_window.ui`).
3. O usuário escolhe a posição e confirma → `AuthorizeDevice{fingerprint, name, position}`
   (`lan-mouse-ipc/src/lib.rs:426-439`) → grava a fingerprint (`src/service.rs:333-341`) e agenda
   um **cliente reverso** (`pending_reverse`).
4. Quando a conexão é aceita, o cliente reverso é **criado e ativado automaticamente**
   (`src/service.rs:541-566, 955-992`). Ou seja: **aprovar entrada = conceder as duas direções**.
5. `trusted_ips` (`src/listen.rs:326-341`, `src/trusted.rs`) e o modo pairing
   (`src/service.rs:815-832`, `src/listen.rs:320-343`) aceitam e **pinam permanentemente qualquer
   certificado** — sem prompt, até 600 s no pairing.
6. Revogar (`RemoveAuthorizedKey`, `src/service.rs:769-773`) só apaga a entrada do mapa:
   **não fecha conexões vivas**, não remove o cliente reverso e não cria deny-list.

### 2.3 O que um peer autorizado pode fazer

| Ação | Onde é consumida | Gate existente |
|---|---|---|
| `Enter` (toma o capture, dispara hooks) | `src/emulation.rs:216-227` | Nenhum além de estar autorizado |
| `Input` (mouse/teclado, a qualquer momento) | `src/emulation.rs:233` | Nenhum |
| Frames de clipboard | `src/emulation.rs:268-274`, `src/clipboard.rs` | Nenhum por peer; só o toggle global em `lan-mouse-ipc/src/lib.rs:448-449` |
| `Ping/Pong/Hello` | `src/connect.rs:727-744` | Nenhum (declarativo) |

### 2.4 Lacunas principais (ranqueadas)

**Alto**

- **H1 — Discador não autentica o servidor.** `src/connect.rs:467-473`. Um host que ocupe um IP
  configurado (DHCP, ARP/DNS spoof, máquina errada) completa o DTLS e recebe teclado, mouse e
  clipboard. Fecha com pin da fingerprint esperada no cliente.
- **H2 — Autorização tudo-ou-nada e permanente.** Aprovar concede input + capture + clipboard,
  para sempre e nas duas direções, sem TTL, sessão ou escopo (`src/listen.rs:314-316`).
- **H3 — Revogação não interrompe sessão viva.** `src/service.rs:769-773` não tem como fechar
  conexões do listener; o peer revogado continua controlando até cair.
- **H4 — Pairing/`trusted_ips` = trust-on-first-packet silencioso.** Qualquer cert vindo de IP
  permitido (inclusive `0.0.0.0/0`, aceito sem warning) ou durante a janela de pairing é pinado
  permanentemente (`src/listen.rs:318-343`).
- **H5 — Aprovação sem vínculo com a conexão.** O pin é mutado **dentro** do
  `verify_peer_certificate`, antes de o handshake terminar; não há nonce/SAS; a aprovação não
  carrega endereço nem sessão (`src/listen.rs:332-343`, `src/service.rs:333-341`). Um handshake
  que falhe depois deixa pin órfão que um `save_config` posterior persiste.

**Médio**

- **M1 — Sem capacidades nem direção**: mouse, teclado e clipboard são indivisíveis.
- **M2 — Identidade/nome fracos**: o nome exibido vem do beacon UDP não autenticado; um clique
  "Add this device" autoriza a fingerprint declarada no beacon **e** adiciona o IP a
  `trusted_ips` (`lan-mouse-gtk/src/window.rs:381-391`).
- **M3 — Identidade ↔ IP**: conexões de entrada são casadas com clients por IP, não por
  fingerprint (`src/service.rs:481-486, 994-996`); NAT/IP reutilizado funde estado e UI.
- **M4 — Troca de certificado** do peer: identidade é o hash do cert; regenerar o pem cria um
  dispositivo "novo" sem continuidade nem explicação.
- **M5 — Sem auditoria**: só `log::info`; impossível responder "quem me controlou ontem".
- **M6 — IPC local sem autenticação** no Windows (TCP `127.0.0.1:5252`): qualquer processo local
  pode mandar `AuthorizeDevice`/`StartPairing` (`lan-mouse-ipc/src/lib.rs:517`).

**Bugs de UX/consistência que atrapalham o modelo**

- Rótulo invertido no diálogo de posição (`authorization_window.ui:47` vs semântica em
  `lan-mouse-gtk/src/window.rs:743-747, 986-990`).
- Toggle de clipboard fica invisível no caminho feliz (`lan-mouse-gtk/src/window.rs:89, 887-917`).
- Sensibilidade: GUI 10–400 vs service 10–500 (`client_row.ui` vs `src/service.rs:1101-1108`).
- Hooks sem UI e sem `save_config` no caminho CLI (`src/service.rs:380-385`).
- `PairingActive(true)` é ignorado pela GUI e não entra em `sync_frontend`
  (`lan-mouse-gtk/src/lib.rs:352-356`, `src/service.rs:641-657`) — sem countdown, sem "encerrar".
- IPC sem versionamento: uma variante nova de `FrontendEvent` derruba GUI/CLI antiga
  (`lan-mouse-gtk/src/lib.rs:244-250`).
- `start_pairing` chamado 2x: o timer antigo fecha a janela nova (`src/service.rs:822-831`).

---

## 3. Objetivos e princípios de design

1. **Quem recebe decide.** `incoming` é autoritativo e imposto localmente (fail-closed); o peer
   nunca decide o que pode fazer aqui.
2. **Identidade ≠ rota.** Fingerprint é identidade; IP/hostname/porta são dicas de rota. Nome é
   cosmético e renomeável.
3. **Permissão explícita, granular e revogável**, com modo `allow`/`ask`/`deny` por direção.
4. **Princípio do menor privilégio**: pairing e regras de endereço concedem o mínimo por padrão.
5. **Autenticação mútua**: os dois lados provam posse da chave; pareamento inicial com verificação
   de código (SAS) contra MITM.
6. **Revogação imediata**: remover acesso derruba a sessão e sobrevive a pairing/regras.
7. **Compatibilidade**: peers/config antigos continuam funcionando sob política legada explícita,
   com downgrade seguro.
8. **Simples por padrão**: presets ("só me controla", "só eu controlo", "ambos", "bloqueado",
   "somente clipboard") e matriz avançada para quem quiser.

---

## 4. Modelo proposto

### 4.1 Conceitos

```
Device (fingerprint, nome, added_by, last_seen)
 ├── Link    (ips, hostname, porta, posição, sensitivity, hooks, activate_on_startup)
 ├── Policy incoming  { mode: allow|ask|deny, caps: Capabilities }   ← imposto AQUI
 ├── Policy outgoing  { enabled: bool, caps: Capabilities }          ← imposto AQUI (guard)
 └── Grant/Session    (session_id, caps concedidas, expira_em, origem)

Pending (pedido vivo): fingerprint, addr, nome, tentativas, first/last_seen, SAS, expira_em
Denied  (deny-list): fingerprint, motivo, expira_em
Audit   (JSONL): grant/revoke/session/input_blocked/pairing/...
```

Capacidades (MVP): `pointer` (movimento + botões), `keyboard`, `clipboard`.
Fase 3: `clipboard_read` / `clipboard_write` separados, `wake`/`power` futuros.

Precedência de decisão: **deny-list** > **policy do device** > **regra de endereço** > **default
(`ask`)**. Pairing nunca atropela deny-list nem um `deny` explícito.

### Direção do controle (requisito explícito)

Cada dispositivo tem uma direção, gravada como a matriz `incoming`/`outgoing`:

| Modo (UI/CLI) | `incoming` (ele controla este PC) | `outgoing` (este PC controla ele) |
|---|---|---|
| `bidirectional` / "Ambos se controlam" | pointer+keyboard+clipboard | pointer+keyboard+clipboard |
| `outgoing` / "Só este PC controla o outro" | — (deny) | pointer+keyboard+clipboard |
| `incoming` / "Só o outro controla este PC" | pointer+keyboard+clipboard | — (disabled) |
| `clipboard-only` | clipboard | clipboard |
| `blocked` | deny | — |

O preset é só açúcar: a UI mostra a matriz completa (direção × capacidade) e qualquer
subconjunto de `pointer`/`keyboard`/`clipboard` pode ser marcado em cada direção. Exemplos:
"o outro controla este PC só com mouse e clipboard, e este PC não controla o outro";
"ambos se controlam, mas clipboard só do outro para este". Isso responde "só um controla o
outro (e qual) ou os dois se controlam" sem limitar a combinações fixas.

Presets de capacidade (açúcar de UI/CLI):

| Preset | incoming | outgoing |
|---|---|---|
| `both` (padrão de aprovação interativa) | pointer+keyboard+clipboard | pointer+keyboard+clipboard |
| `controlled` ("só me controla") | pointer+keyboard+clipboard | — |
| `controller` ("só eu controlo") | — | pointer+keyboard+clipboard |
| `clipboard-only` | clipboard | clipboard |
| `blocked` | deny | — |

### 4.2 Config TOML v2

```toml
config_version = 2

[[devices]]
name = "windows11"                 # cosmético, renomeável
fingerprint = "aa:bb:cc:…"         # identidade estável (não muda com IP)
position = "right"                 # onde ELE fica em relação a este PC
addresses = ["192.168.1.50"]       # dicas de rota (outbound)
port = 4242
sensitivity = 120                  # 10..500
enter_hook = "notify-send 'entrou no windows11'"
leave_hook = "notify-send 'saiu do windows11'"
activate_on_startup = true         # só faz sentido com outgoing.enabled

[devices.incoming]                 # o que ELE pode fazer AQUI (autoritativo)
mode = "allow"                     # allow | ask | deny
capabilities = ["pointer", "keyboard", "clipboard"]

[devices.outgoing]                 # o que EU posso fazer LÁ (guard local)
enabled = true
capabilities = ["pointer", "keyboard", "clipboard"]

[[address_rules]]                  # substitui trusted_ips, agora com escopo
address = "10.0.0.0/24"
incoming = ["clipboard"]           # devices adotados pela regra recebem só isso
outgoing = []                      # e não ganham cliente reverso automático
note = "rede da sala"              # aviso se prefixo for amplo (> /24 ou ::/64)

[[denied]]                         # vence pairing, address_rules e pedidos antigos
fingerprint = "cc:dd:…"
name = "kiosk"
reason = "revogado em 2026-11-05"
expires = "2026-12-01T00:00:00Z"   # omitido = para sempre

[clipboard]                        # inalterado
enabled = true
```

- `role`/preset é aceito na leitura e expandido por GUI/CLI; a verdade gravada são as duas tabelas.
- `capabilities` como array de strings (TOML/CLI/JSON legíveis) e `Capabilities(u8)` interno com
  serde custom.
- Migração na leitura: se `devices` não existir, derivar de `[[clients]]` +
  `[authorized_fingerprints]` + `trusted_ips`; `config_version = 2` na primeira escrita, mantendo
  `[[clients]]`/`[authorized_fingerprints]` espelhados por ≥1 release (downgrade seguro).
- Backup `config.toml.bak` antes da primeira migração; preservar comentários/ordem com
  `toml_edit::DocumentMut` (hoje `write_back` reescreve o arquivo — `src/config.rs:696-725`).
- `lan-mouse-cli migrate-config [--write]` e aviso no `doctor` para clients sem fingerprint.

### 4.3 IPC (`lan-mouse-ipc`)

```rust
pub enum PolicyMode { Allow, Ask, Deny }

pub struct Policy { pub mode: PolicyMode, pub caps: Capabilities }

pub struct DeviceView {
    pub fingerprint: String,
    pub name: String,
    pub position: Position,
    pub addresses: Vec<IpAddr>,
    pub incoming: Policy,
    pub outgoing: Policy,
    pub source: GrantSource,      // Manual | Pairing | AddressRule | Legacy | Discovery
    pub state: LinkState,         // Offline | WaitingApproval | Connected | Receiving | Revoked
    pub last_seen: Option<u64>,
    pub incoming_addr: Option<SocketAddr>,
    pub rtt_ms: Option<u16>,
}

pub struct PendingView {
    pub fingerprint: String,
    pub addr: SocketAddr,
    pub name: Option<String>,
    pub requested: Policy,
    pub sas: String,              // 6 dígitos, calculado dos dois fingerprints
    pub attempts: u32,
    pub first_seen: u64,
    pub last_seen: u64,
}

// Snapshots idempotentes (substituem eventos one-shot):
FrontendEvent::Devices(Vec<DeviceView>)
FrontendEvent::PendingRequests(Vec<PendingView>)
FrontendEvent::PairingState { active: bool, until: Option<u64>, incoming: Policy }
FrontendEvent::ConnectionHistory(Vec<HistoryEntry>)

FrontendRequest::ListDevices | ListPending | ListHistory
FrontendRequest::SetDevicePolicy { fingerprint, incoming: Policy, outgoing: Policy, position: Option<Position> }
FrontendRequest::RespondPending { fingerprint, decision: Accept{name, position, incoming, outgoing, session_only} | Deny{permanent} }
FrontendRequest::Rename { fingerprint, name }
FrontendRequest::Revoke { fingerprint, disconnect: bool, keep_link: bool }
FrontendRequest::SetPairing { seconds: u64, incoming: Policy, allow_reverse: bool }
```

- Manter `AuthorizeKey`/`AuthorizeDevice`/`RemoveAuthorizedKey`/`TrustAddress`/`UntrustAddress`
  como comandos legados (mapeados para o novo modelo) por pelo menos um release.
- `FrontendRequest::Hello { ipc_version, features }` + leitores tolerantes a variantes
  desconhecidas (GUI/CLI antigas não podem morrer — hoje o processo sai em erro de decode).

### 4.4 Protocolo (`lan-mouse-proto`)

Mensagens **aditivas** (ids 16+, ignoradas por peers antigos, que já fazem skip de ids
desconhecidos em `src/listen.rs:414-432` e `src/connect.rs:761-766`):

```rust
ProtoEvent::AuthRequest {
    session_id: u128,
    identity_version: u8,
    requested: IncomingPolicy,   // o que o discador pede para fazer no ouvinte
    label: Option<String>,
    sas: [u8; 3],                // 6 dígitos derivados dos dois fingerprints
}
ProtoEvent::AuthResponse { session_id: u128, granted: IncomingPolicy, expires_in: Option<u32> }
ProtoEvent::AuthRevoke    { session_id: u128, reason: RevokeReason }
```

- Nova capability `PERMISSIONS = 1 << 5` anunciada em `Hello`/`Pong`.
- **Enforcement é sempre local** (nunca condicionado ao peer); a capability só melhora a UX e
  evita pedir o que o peer não suporta.
- **Sessão**: cada conexão aceita recebe `session_id`; `Enter`/`Input`/clipboard são aceitos apenas
  se a sessão tem a capacidade correspondente. Input fora de escopo é descartado, contado e
  auditado (nunca panic).
- **Legacy**: peer sem `PERMISSIONS` continua sob `policy.incoming` normal, mas sem troca de
  `AuthResponse`; a UI marca "peer antigo — permissões locais aplicadas" e a política
  `allow_legacy_peers` decide se conecta (default: sim, com aviso).
- `PROTO_VERSION` continua `1` porque as mudanças são aditivas; só subir se algum formato
  existente mudar.

### 4.5 Enforcement por evento

| Evento recebido | Capacidade exigida | Sem permissão |
|---|---|---|
| `Enter` (capture) | `pointer` ou `keyboard` | responde `Leave(0)`, não dispara `ReleaseNotify`, audita |
| `Input(Pointer*)` | `pointer` | descarta + audita |
| `Input(Keyboard*)` | `keyboard` | descarta + audita |
| Frame de clipboard | `clipboard` (fase 3: `clipboard_write`) | ignora frame + audita |
| `Ping/Pong/Hello` | — | sempre permitido |
| `Enter`/`Input` de peer não autorizado | — | handshake nem completa (como hoje) |

No lado emissor, o capture respeita `outgoing`: não envia `Enter` para um cliente sem
`outgoing.enabled`/cap correspondente (senão o cursor entra numa zona morta).

### 4.6 Revogação e sessão

- Revogar = remover o pin + adicionar à deny-list + `ListenerCommand::Disconnect(session/addr)`
  + soltar teclas presas + abortar clipboard + `AuthRevoke` best-effort + audit.
- `keep_link = true` (default): mantém o link configurado (posição/IP) para re-aprovar depois;
  `false` remove o device e o link.
- Revogação sobrevive a restart (deny-list no config), a pairing e a `address_rules`.
- Desconexão normal não é revogação: o device continua na lista com estado `Offline`.

### 4.7 Auditoria

JSONL append-only em `$XDG_STATE_HOME/lan-mouse/audit.jsonl` (Windows:
`%LOCALAPPDATA%\lan-mouse\audit.jsonl`), rotação ~5 MB, timestamps UTC, sem segredos:

```json
{"ts":"2026-11-05T12:00:00Z","event":"grant_created","fp":"aa:…","source":"manual","incoming":["pointer","keyboard"],"outgoing":["clipboard"]}
{"ts":"…","event":"session_start","fp":"aa:…","addr":"192.168.1.50:4242","session":"…"}
{"ts":"…","event":"input_blocked","fp":"aa:…","scope":"keyboard","action":"keyboard"}
{"ts":"…","event":"revoke","fp":"aa:…","reason":"user"}
```

Tela "Atividade recente" (últimos N) e `lan-mouse doctor --json` com os últimos eventos;
`lan-mouse-cli history --json` para scripts.

---

## 5. Fluxos de UX

### 5.1 Primeiro pareamento (com SAS)

1. Usuário A adiciona o endereço de B (ou A descobre B na tabela de rede).
2. A disca; se não conhece o cert de B, o handshake outbound **falha fechado** e surge um pedido
   pendente nos dois lados (A mostra "B pediu para conectar"; B mostra "A quer conectar").
3. Os dois lados exibem o mesmo **SAS de 6 dígitos** (derivado dos dois fingerprints ordenados).
4. O usuário em B aprova com escopos: "A pode controlar este PC: [mouse] [teclado] [clipboard]"
   e "Este PC pode controlar A: [mouse] [teclado] [clipboard]".
5. Grant é gravado, sessão estabelecida, auditoria registra origem `manual`.
6. Se o SAS não bater entre as telas, a conexão é negada com aviso ("possível interceptação").

### 5.2 Pedido de escalonamento / novo device

- Device já conhecido que pede capacidade nova gera `PendingView` com a diferença ("pediu teclado,
  já tem mouse") e botões Aprovar/Negar/Só desta vez.
- Device desconhecido fora de qualquer grant: pendente com TTL de 5 min; peer reconecta sozinho
  (retry 250 ms–5 s já existente) e a aprovação vale no próximo retry.

### 5.3 Pairing sem mouse (`lan-mouse cli pair`)

- Abre por N s (5–600) com escopo explícito (`--incoming pointer,keyboard`) e `--allow-reverse`.
- Fecha automaticamente no **primeiro** dispositivo aceito (ou no timeout).
- Banner na GUI com countdown e Encerrar; ao fim, lista "N dispositivos autorizados durante o
  pairing" para revisão (persistência só depois do review, se `--review` estiver ativo).
- Nunca aceita quem está na deny-list; `PairingState` entra no `sync_frontend`.

### 5.4 GUI: aba "Dispositivos"

Substitui os três grupos atuais ("Connected & Paired Devices", "Authorized Certificates",
"Allowed addresses"):

- **Solicitações** (condicional, topo): nome, IP, fingerprint curto, SAS, escopos pedidos.
- **Dispositivos**: ExpanderRow com nome editável, badge de estado, subtítulo
  `posição · caps · rtt`; expandida: presets, matriz de capacidades in/out, copiar fingerprint,
  Revogar, Esquecer, conectar/desconectar.
- **Regras de endereço**: CIDR + caps + aviso para prefixos amplos; remover não revoga devices já
  adotados (ação explícita separada).
- **Histórico/auditoria**: colapsável, filtros por device/tipo.

Ajustes pontuais: corrigir o rótulo de posição, mover o toggle de clipboard para o grupo de
clipboard, alinhar sensibilidade 10–500, expor hooks.

### 5.5 CLI

```sh
lan-mouse-cli devices [--json]
lan-mouse-cli allow <fp|id> [--incoming pointer,keyboard,clipboard] [--outgoing ...] [--position right] [--once]
lan-mouse-cli deny  <fp|id> [--for 1h|forever]
lan-mouse-cli revoke <fp|id> [--disconnect] [--forget-link]
lan-mouse-cli rename <fp|id> "novo nome"
lan-mouse-cli pending [--json] | approve <fp|id> … | reject <fp|id>
lan-mouse-cli trust-add <cidr> [--incoming clipboard] | trust-list | trust-remove <cidr>
lan-mouse-cli history [--limit 20] [--json]
lan-mouse-cli pair [--seconds 60] [--incoming clipboard] [--allow-reverse] [--review]
lan-mouse-cli status --json     # inclui pairing_until e policy por device
```

Aliases mantidos: `add-client`, `authorize-device`, `authorize-key`, `trust`, `untrust`.

---

## 6. Segurança: ameaças × mitigações

| Ameaça | Hoje | Mitigação no plano | Fase |
|---|---|---|---|
| MITM/impersonação do servidor (H1) | Discador aceita qualquer cert | Pin da fingerprint esperada no `verify_peer_certificate` do cliente; TOFU explícito com aviso; `allow_unverified` só por opt-in | 0 |
| Peer malicioso controla tudo (H2) | Autorização binária | Capabilities + direções + política `incoming` fail-closed | 1 |
| Revogação ineficaz (H3) | Não fecha sessão | Disconnect + deny-list + release de teclas + audit | 0 |
| Pairing sequestra controle (H4) | Qualquer cert por até 600 s, permanente | Escopo explícito, single-use, deny-list, review, SAS | 0/2 |
| IP spoof/allowlist ampla (H4) | `0.0.0.0/0` aceito, adoção permanente | `address_rules` com caps, warning para prefixos > /24, deny-list vence | 2 |
| Aprovar dispositivo errado (H5) | Sem vínculo/nonce | SAS nos dois lados + sessão amarrada ao handshake | 2 |
| Regenerar cert e "virar outro" (M4) | Pin antigo órfão, sem aviso | `cert_changed` event + prompt com continuidade por nome/link + audit | 2 |
| Beacon spoofado (M2) | Nome e fingerprint não autenticados | Beacon só como dica; autorização sempre pelo handshake+SAS; não adicionar `trusted_ips` no clique de descoberta | 1 |
| IPC local (M6) | TCP loopback sem auth no Windows | Fase 0: documentar e limitar a comandos idempotentes/não destrutivos; fase 2: named pipe com ACL / token de sessão; Unix: checar SO_PEERCRED | 0/2 |
| Replay/reordem | DTLS cobre registro | IDs de sessão + dedupe existente; inputs sem janela válida rejeitados | 1 |
| DoS (handshakes, conns, pendings) | Semáforo 32 só para handshake | Limite por IP e total de sessões; poda de pendings/rejected; rate-limit | 3 |

Nota: no Windows a chave privada ainda não tem ACL (`src/crypto.rs:60-67`, FIXME); incluir
`protect key file` (fase 3).

---

## 7. Roadmap por fases

### Fase 0 — Segurança e alicerce (P, pré-requisito) — *implementada neste ciclo*

> Estado: pin outbound + TOFU persistido, revogação efetiva (disconnect + deny-list),
> pairing single-use com estado sincronizado, auto-autorização persistida só após handshake,
> decode IPC tolerante + handshake de versão, normalização de fingerprint, hooks persistidos,
> aviso de CIDR amplo e correções de UX GTK. Critérios de aceite abaixo cobertos por testes
> (`outgoing_connection_learns_the_server_fingerprint`, `..._rejects_a_mismatched_pin`,
> `denied_fingerprint_is_rejected_before_anything_else`,
> `pairing_window_accepts_only_one_device`, `disconnect_closes_a_live_incoming_session`).

Escopo:
1. **Pin outbound**: `expected_fingerprint` no device/link; validar cert do servidor no discador;
   clientes sem pin → TOFU com aviso e gravação da fingerprint observada (mata H1).
2. **Revogação efetiva**: `ListenerCommand::Disconnect(addr|session)`; `RemoveAuthorizedKey`
   desconecta, solta teclas, e cria entrada em deny-list (mata H3).
3. **Pairing/trusted_ips**: fechar no primeiro aceite, TTL, `PairingState` no `sync_frontend`,
   warning para CIDR amplo, adoção de device registrada com origem e caps atuais (mata H4).
4. **Aprovação fora do handshake**: o verify só decide; a persistência acontece após
   handshake+confirmação, com nonce/sessão (mitiga H5; SAS completo na fase 2).
5. **IPC versionado** (`Hello`/features) + leitores tolerantes (evita quebrar GUI/CLI).
6. Bugs: rótulo de posição, clipboard toggle, sensibilidade 10–500, hooks persistidos,
   `start_pairing` 2x, normalização/validação de fingerprint.

Aceite: um peer não pinado não recebe keystrokes ao ocupar o IP de um client; revogar derruba a
sessão em <1 s e a reconexão falha; pairing fecha no primeiro device e não persiste sem review;
`cargo test --workspace` verde e testes de integração de auth cobrindo os casos.

### Fase 1 — MVP de permissões (G, entrega principal) — *implementada neste ciclo*

> Estado: `src/devices.rs` (DeviceStore + policies + pendências), config v2 (`[[devices]]` com
> dual-write de `authorized_fingerprints`/`[[clients]]`), enforcement real por capacidade no
> listener (eventos filtrados antes da emulação) e no capture (direção de saída), pendências
> duráveis com TTL e `RespondPending`, snapshots `Devices`/`PendingRequests`, CLI
> `devices/pending/allow/deny/revoke/rename`, GUI de dispositivos com checkboxes de capacidade e
> presets de direção. Testes: filtragem de eventos por capacidade (listener), políticas
> por direção, migração de config, serde IPC.

Escopo:
1. `Device`/`Link`/`Policy`/`Capabilities` no service (novo `src/devices.rs`); migração do config
   para v2 na leitura + dual-write.
2. Enforcement no `ListenTask`/emulação: tabela do §4.5; gate de clipboard; `outgoing` no capture.
3. Pendências duráveis (`PendingView`, TTL, retry) + `RespondPending`.
4. GUI: aba **Dispositivos** unificada + diálogo de autorização com escopos.
5. CLI: `devices`, `allow`, `deny`, `revoke`, `rename`, `pending/approve/reject`.
6. Revogação/deny-list integradas ao novo modelo.

Aceite: device com `capabilities=["clipboard"]` não move o mouse nem digita; "só me controla" não
cria cliente reverso; revogar derruba e bloqueia; aprovação reflete no próximo retry; config v1
migra sem perder clients/pins (roundtrip testado).

Pendências conhecidas desta entrega (para a fase 2): `Ask` por sessão, `address_rules` com
capacidades (as regras de endereço hoje adotam com acesso total), gate do clipboard *de saída*
(o lado de entrada já é filtrado), auditoria persistida e UI de histórico. O pareamento continua
com TOFU + fingerprint (SAS entra na fase 2).

### Fase 2 — Pareamento seguro e UX completa (M)

Escopo: SAS nos dois lados + confirmação; pairing escopado/single-use/review; `address_rules` com
caps; eventos `AuthRequest/AuthResponse/AuthRevoke` + capability `PERMISSIONS`; `cert_changed`;
histórico persistido e tela de auditoria; `doctor`/`status --json`; remover auto-`TrustAddress` no
clique de descoberta.

> Estado (fase 2): `address_rules` com capacidades (config, migração de `trusted_ips`,
> listener/service/CLI/eventos `AddressRules`); pairing escopado com review
> (`StartPairingScoped`, janela com `incoming`/`outgoing`, evento `PairingReview`);
> auditoria persistida em JSONL com rotação, `history` na CLI, `ConnectionHistory` no IPC
> e caminho/contagem no `doctor`; negociação de permissões no protocolo
> (`AuthRequest`/`AuthResponse`/`AuthRevoke`, capability `PERMISSIONS` com gating de
> peers legados, grants do peer no `ClientState`); SAS de 6 dígitos no diálogo/pending e
> detecção de troca de certificado (`cert_changed`, evento `CertificateChanged`).
> Fase 2 concluída; próximo: fase 3 (sessões/--once, clipboard read/write, rate-limit,
> quiosque, export/import, IPC auth e ACL de chave).

Aceite: MITM no primeiro pareamento detectado por SAS; peer antigo conecta sob política legada
marcada na UI; regra de endereço com `incoming=["clipboard"]` só entrega clipboard.

### Fase 3 — Hardening e avançado (M)

Escopo: grants de sessão/TTL (`--once`), clipboard read/write separados, rate-limit/limites por
IP e total, deny com expiração, auditoria exportável, "modo quiosque" (`incoming=deny` global),
ACL da chave privada no Windows, autenticação do IPC local, import/export de política.

Aceite: grant `--once` expira ao desconectar; auditoria responde "quem me controlou e quando";
limites resistem a flood de handshakes.

---

## 8. Plano de testes

**Unitários**

- `lan-mouse-proto`: roundtrip/golden vectors das mensagens novas; IDs estáveis; decode legado de
  `Hello`/`Pong`/`Enter` inalterado; truncamento não entra em panic.
- `src/devices.rs`: matriz de precedência (deny > device > address rule > default), expiração com
  clock injetável, SAS determinístico/simétrico, canonicalização de fingerprint.
- `src/config.rs`: migração v1→v2 (authorized sem client, trusted_ips, hooks), roundtrip v2,
  dual-write, chaves desconhecidas, `role`→caps, serde de `Capabilities` em TOML e JSON.
- IPC: roundtrip de todos os requests/events; leitor antigo pulando variante desconhecida.
- `src/trusted.rs`: prefixos amplos (`/0`) geram warning/recusa; famílias não se misturam.

**Integração** (reusar `raw_dtls_client`/`LanMouseListener` de `src/connect.rs:868-935`)

- T1: grant só `pointer` + `Input(Keyboard)` → `DummyEmulation` não recebe; audit `input_blocked`.
- T2: `Input` sem `Enter` funciona; `Enter` sem `pointer`/`keyboard` não dispara `ReleaseNotify`.
- T3: clipboard sem `clipboard` não aplica; com escopo aplica; outbound não envia a quem não tem.
- T4: pin outbound correto conecta; fingerprint errada falha o handshake e nenhum input sai.
- T5: `address_rule` concede só as caps configuradas; untrust fecha sessão e rejeita novos.
- T6: pairing com 2 peers desconhecidos → grants expiram/ficam para review; depois de fechado,
  cert novo é rejeitado.
- T7: revogação em sessão ativa → disconnect, teclas liberadas, inputs bloqueados, clipboard
  abortado, audit.
- T8: corrida de aprovação → handshake abortado não persiste pin.
- T9: cert rotation → `cert_changed` + prompt; pin antigo não transfere.
- T10: peer legado (sem `PERMISSIONS`) conecta sob política legada e é marcado na UI.
- T11: auditoria pairing→grant→revoke produz o JSONL esperado.
- T12: downgrade — config v2 com dual-write lido por binário antigo sem perda.

**Manual** (matriz Linux ↔ Windows 10 ↔ Windows 11, com escalas 100/150% e
`scripts/vm-test-lab.sh`): pareamento com SAS, aprovação por capacidade, revogação com sessão
viva, pairing sem mouse, restart preservando policy, edição manual do config.

Comandos: `cargo test --workspace`, `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets --all-features`.

---

## 9. Compatibilidade e migração

| Item | Estratégia |
|---|---|
| Peers antigos (sem `PERMISSIONS`) | Conectam sob política legada (`allow_legacy_peers=true`), marcados na UI; enforcement local continua valendo |
| `PROTO_VERSION` | Continua `1` (mudanças aditivas); bump só se formato existente mudar |
| Eventos novos | IDs 16+ ignorados por leitores antigos (já é o comportamento de leitura) |
| Config v1 | Migração na leitura + dual-write por ≥1 release + `config.toml.bak` |
| `authorized_fingerprints` | Viram devices `incoming=allow full`, `outgoing.disabled` (comportamento atual de "pode me controlar") |
| `trusted_ips` | Viram `address_rules` preservando o comportamento atual (full) + warning recomendando estreitar |
| `[[clients]]` | Viram `Link` de devices; clients sem fingerprint ficam "sem pin" (TOFU com aviso) |
| IPC | `Hello` + features; requests legados como aliases |
| Downgrade | Binário antigo ignora `[[devices]]` e lê `[[clients]]`/`authorized_fingerprints` espelhados |

---

## 10. Decisões (com recomendação)

1. **Granularidade** — *decidido*: permissões independentes para `pointer`, `keyboard` e
   `clipboard`, marcáveis em qualquer subconjunto (os três ou apenas alguns) em cada direção.
   A UI oferece checkboxes por capacidade e presets que marcam todos; nunca "tudo ou nada".
   Granularidade mais fina (botões separados, clipboard leitura/escrita) fica para a fase 3.
2. **Direção do controle** — *decidido*: três modos por dispositivo ("este PC controla o outro",
   "o outro controla este PC", "ambos"), gravados na matriz `incoming`/`outgoing` e combináveis
   com as capacidades (ver §4.1).
3. **SAS obrigatório no primeiro pareamento?** → Recomendo obrigatório quando os dois lados
   suportam `PERMISSIONS`; fallback "comparar fingerprint na tela" para peers antigos.
4. **`trusted_ips`/`address_rules`**: manter conveniência? → Recomendo manter, mas com escopo,
   warning para prefixo amplo, e nunca adicionar regra automaticamente pelo clique de descoberta.
5. **Auto-cliente reverso**: hoje acontece sempre. → Recomendo transformar em opção explícita no
   diálogo (pré-marcada para manter o fluxo atual) e desligada por padrão em pairing/address rule.
6. **Pairing sem mouse**: persistir os grants do pairing automaticamente ou exigir review? →
   Recomendo review (lista no fim da janela) quando a GUI estiver disponível; daemon-only persiste
   com audit + `--review` opcional.
7. **Escopo** — *decidido*: Fase 0 primeiro (segurança/base), depois Fase 1 (MVP de permissões e
   direção). SAS fica na fase 2 com caminho de migração.

---

## 11. Matriz de arquivos impactados

| Arquivo/módulo | Mudança | Fase |
|---|---|---|
| `src/connect.rs` | pin do servidor no DTLS outbound; sessão; exposição de estado | 0-2 |
| `src/listen.rs` | decisão de auth fora do handshake; `Disconnect`; caps por sessão; pendências/metadata | 0-2 |
| `src/emulation.rs` | gate de `Enter`/`Input`/clipboard por sessão/caps; audit `input_blocked` | 1 |
| `src/emulation.rs` / `src/clipboard.rs` | escopo de clipboard por peer/direção | 1-3 |
| `src/trusted.rs` → `src/devices.rs` (novo) | store de devices/policies/deny/pending/audit | 1 |
| `src/config.rs` | schema v2, migração, dual-write, `toml_edit`, backup | 1 |
| `src/service.rs` | orquestração de pendências, grants, revogação, pairing state, histórico | 0-3 |
| `src/client.rs` | vincular fingerprint ao client (identidade ≠ IP) | 1 |
| `lan-mouse-proto/src/lib.rs` | `AuthRequest/Response/Revoke`, capability `PERMISSIONS` | 2 |
| `lan-mouse-ipc/src/lib.rs` | `DeviceView`/`PendingView`/`Policy`, requests/events, `Hello` IPC | 0-1 |
| `lan-mouse-gtk/src/window.rs` + `resources/*.ui` | aba Dispositivos, solicitações, pairing banner, diálogo com escopos/SAS | 1-2 |
| `lan-mouse-cli/src/lib.rs` | comandos `devices/allow/deny/revoke/pending/history/pair` | 1 |
| `README.md`/`DOC.md` | documentar modelo, migração, segurança | 1-2 |
| `src/doctor.rs` | policies, pins ausentes, warning de CIDR amplo, auditoria recente | 2 |
| `tests/` | integração T1-T12 + fixtures de config legado | 0-2 |

---

## 12. Ordem de implementação sugerida (PRs pequenos)

1. PR-1: pin outbound + teste de MITM/impersonação (Fase 0.1).
2. PR-2: revogação efetiva + deny-list + `Disconnect` (Fase 0.2).
3. PR-3: pairing single-use + `PairingState` no sync + warnings de CIDR (Fase 0.3).
4. PR-4: IPC `Hello`/features + leitores tolerantes + fixes de UX/consistência (Fase 0.5-0.6).
5. PR-5: `Device`/`Policy`/`Capabilities` + config v2 read/dual-write + migração (Fase 1.1).
6. PR-6: enforcement no listener/emulation/clipboard + audit básico (Fase 1.2).
7. PR-7: pendências + `RespondPending` + diálogo escopado + GUI Dispositivos (Fase 1.3-1.4).
8. PR-8: CLI + `doctor`/`status` (Fase 1.5).
9. PR-9+: SAS, pairing escopado, address rules, histórico, auth messages (Fase 2).
10. PR-10+: sessão/TTL, clipboard read/write, rate-limit, IPC auth, ACL de chave (Fase 3).

Critérios de pronto por PR: testes novos passando, `cargo fmt`, `cargo clippy --workspace
--all-targets`, docs/README atualizados quando a superfície pública mudar, e migração coberta por
teste quando tocar o config.
