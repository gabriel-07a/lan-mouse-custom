# Auditoria: "Connected" falso, linha duplicada e IP alternando (release v0.2.4-custom)

> **Resumo** — A release `2c8b201` mostrava "Connected"/"Connected (bidirectional)" porque a
> linha derivava o estado **apenas** de `active + active-addr + alive`: `active-addr` é gravado
> logo após o handshake DTLS (`src/connect.rs:617`) e `alive` é setado otimista no handshake
> (`:621`), então uma conexão sem nenhum `Pong` já aparecia conectada. O "dois dispositivos"
> era **duas entradas de cliente** para o mesmo peer (uma por família de endereço), cada uma
> virando uma linha; o IP alternava porque `ClientState::ips` é um `HashSet`
> (`lan-mouse-ipc/src/lib.rs:209`) consumido sem ordenação e o título usava
> `ips.first()` (`lan-mouse-gtk/src/client_row.rs:420`). Correção nesta entrega (frontend +
> campo opcional no IPC): **"Connected" só com prova recente de round-trip** (`last_pong_at` ou,
> na falta dele, a cadência de `FrontendEvent::State` com janela de 6 s), estados
> `Connecting`/`Reconnecting`/`ConnectedDegraded` distintos, **uma linha por peer agregada pelo
> fingerprint do certificado**, endereço ativo preferido com família (`IPv4`/`IPv6`) e
> **debounce de 3 s** para não alternar. O que ainda depende do serviço está com patch exato
> em §4 (net-auditor/task-17 e Lead). Sem commit; nada fora do escopo foi editado.

## 1. Diagnóstico (release `2c8b201`, evidência file:line)

### 1.1 "Connected" sem round-trip
- `row_connection_state` derivava só de `(active, alive, has_outgoing, has_incoming)`
  (`lan-mouse-gtk/src/client_row.rs:113-136` na release): `alive=true` + `active-addr` ⇒
  `Connected`/`ConnectedBidirectional` (`:126-127`).
- `active-addr` é escrito **no fim do handshake**, antes de qualquer resposta
  (`src/connect.rs:617`), e `alive` é forçado `true` no mesmo ponto (`:621`). O serviço só
  publica `alive` real e `rtt` a cada `Pong` (`:855-861`); o `FrontendEvent::State` sai em
  cada Pong (`:870` → `src/service.rs:2296-2301`).
- Queda do peer: o watchdog só fecha após **2 rodadas perdidas** (~4-6 s) ou **30 rodadas
  (~60 s)** se o peer nunca respondeu (`src/connect.rs:650,660,700-703`). Durante essa janela
  a UI continuava "Connected" sem nenhuma prova — exatamente o relato (peer build `52f8f441`,
  nosso `2c8b201`).
- Conclusão: faltava um conceito explícito de **prova recente**; `alive` é flag de emulação do
  peer, não liveness (já documentado em `docs/client-connection-state.md` §4).

### 1.2 Duas linhas para o mesmo peer
- `Window::new_client` (`lan-mouse-gtk/src/window.rs:471-523` na release) sempre adicionava um
  `ClientObject`/`ClientRow` por handle do serviço; `update_client_list` repetia isso para cada
  entrada de `Enumerate`. Não havia identidade de peer na UI.
- O serviço pode ter **duas entradas de cliente** para o mesmo peer (uma por família de
  endereço/IP configurado) — o fix de identidade é a task-17 (net-auditor, `src/client.rs` /
  `src/connect.rs` / `src/listen.rs`). A UI agora agrega defensivamente pelo **fingerprint do
  certificado** (`peer_identity`, `window.rs:140`), que é a identidade real do peer.
- O painel "Devices" (`window.rs:806-811` / `:224` na release) também exibia endereços do
  `DeviceView` sem ordenação quando havia IPv4+IPv6.

### 1.3 IPv4 ↔ IPv6 alternando
- `ClientState::ips` é `HashSet<IpAddr>` (`lan-mouse-ipc/src/lib.rs:209`): a ordem de
  iteração muda entre reconstruções do conjunto (DNS re-resolve, `update_ips`
  `src/client.rs:182`). A UI mapeava `state.ips.into_iter()` sem sort
  (`lan-mouse-gtk/src/window.rs:593-598` na release) e o título usava
  `let primary_ip = ips.first()` (`client_row.rs:420`) → título/subtítulo podiam alternar
  IPv4/IPv6 a cada atualização.
- O subtítulo "Connected · \<addr\>" usava `active-addr`, que o serviço pode reescrever a cada
  `Pong` (`src/connect.rs:855`); sem histerese a linha trocava de família a cada pacote.

## 2. Correção aplicada (escopo desta task)

### 2.1 Prova de round-trip (frontend + IPC aditivo)
- Novo campo **opcional** em `ClientState`: `last_pong_at: Option<u64>` (Unix ms), com
  `#[serde(default)]` (`lan-mouse-ipc/src/lib.rs:217`). Serviço antigo serializa sem o campo;
  a GUI lê `None` e cai no fallback. Serviço novo ainda não existe (patch em §4): o dado é
  exato quando presente.
- Novo tipo `LinkProof { Pending, Recent{age_secs}, Stale{age_secs} }`
  (`lan-mouse-gtk/src/client_row.rs:22`) e `link_proof_from_age` (`:71`; janela fresca de
  **6 s**: o serviço pinga em rodadas de ~2 s e fecha após 2 perdidas).
- `row_connection_state(active, alive, has_outgoing, has_incoming, proof)` (`:185`):
  - link de saída **sem prova fresca** ⇒ `Connecting` (Pending) ou **`Reconnecting`** (Stale);
    `alive` não é mais suficiente;
  - prova fresca + `alive` ⇒ `Connected` (ou `ConnectedBidirectional` com sessão de entrada);
  - prova fresca + `!alive` ⇒ `ConnectedDegraded` (link ok, emulação desligada no peer);
  - `Disabled`/`Receiving` inalterados.
- Novos rótulos/subtítulos/tooltips (`client_row.rs:116-137`, `:606-798`):
  `Connecting…` "Link to … up — waiting for a reply" (handshake sem resposta) ou
  "Trying … — no answer yet"; `Reconnecting…` "No reply for Ns — reconnecting…";
  tooltip de `Reconnecting` explica peer dormindo/offline/firewall. `Connected` só aparece com
  `LinkProof::Recent`.
- **Fallback sem patch de serviço**: `Window::note_state` (`window.rs:726`) conta os
  `FrontendEvent::State` recebidos: o serviço emite um por `Pong` (e `Hello`/`AuthResponse`
  são respostas). O primeiro `State` de um endereço é o do handshake e **não** conta; do
  segundo em diante, a idade do último é a prova (`window.rs:771-813`, `LINK_FRESH_WINDOW`
  `:77`). Um tick de 1 s (`:916`, `STATUS_TICK_INTERVAL` `:87`) reavalia as linhas, então uma
  linha sem eventos também expira o "Connected".
  - `State` causado por **edição local** (hostname/porta/posição/…) não é tratado como
    resposta do peer: `update_client_config` marca `local_config_changes`
    (`window/imp.rs:162`, `window.rs:738-764`).

### 2.2 Uma linha por peer (agregação por fingerprint)
- `peer_identity` (`window.rs:140`) usa o **fingerprint normalizado** do certificado; só ele é
  identidade (hostname/IP podem colidir entre máquinas distintas e não são usados para fundir).
- `merge_peer_states` (`:157`) monta o resumo da linha: `active` = OR dos membros; endereço/
  `alive`/`rtt` do membro **com link comprovado** (prefere o que já tem RTT medido);
  `incoming-addr` de qualquer membro; `ips` = união ordenada; hostname/porta/posição/
  sensibilidade do primeiro membro.
- `register_peer_identity` (`:643`) agrupa; linhas duplicadas continuam no modelo mas ficam
  **ocultas** (`set_client_row_visible`), e a linha primária mostra o resumo
  (`refresh_peer`, `:831`). Ao deletar a primária, um membro sobrevivente é promovido
  (`delete_client`).
- Ações da linha (Conectar/Desconectar/Cancelar, Delete, hostname, porta, posição,
  sensibilidade, DNS) são **fan-out** para todos os membros (`window.rs:930+`), para a linha
  oculta não voltar a divergir; `row_connection_state` continua sendo a fonte única da ação.
- Limite explícito: entradas **sem fingerprint** (config legada) não são fundidas — preferimos
  não esconder um device real. O serviço deve preencher o fingerprint (ele já faz para
  dispositivos adotados/autorizados); ver task-17.

### 2.3 Endereço estável + família
- `sorted_ips` (`client_row.rs:334`): IPv4 primeiro, ordem numérica, dedup — aplicado em todos
  os pontos que exibem/listam IPs (linha e Devices).
- Título não é mais `hostname (ip)` (era a fonte do "alterna IP no título"): mostra hostname,
  ou o endereço preferido quando não há hostname (`client_row.rs:606-640`).
- Subtítulo mostra o endpoint **em uso** + família: `192.168.166.197:4242 (IPv4)` /
  `[2804:…::1]:4242 (IPv6)` (`address_family_label` `:315`).
- **Debounce de 3 s** (`sticky_endpoint` `:364`, `ENDPOINT_STICKY_WINDOW` `window.rs:82`): um
  endereço novo só substitui o exibido depois de persistir; um flip por pacote não aparece.
  Perda/reconexão limpa o endereço exibido (`window.rs:726-764`).
- Devices: endereços ordenados no subtítulo (`window.rs:224`) e linhas deduplicadas/ordenadas
  por fingerprint+nome (`render_devices`).

## 3. Comportamento esperado (mapa estado → UI)

| Situação | UI (badge / subtítulo) |
|---|---|
| Handshake ok, nenhum Pong ainda | `Connecting…` / "Link to \<endpoint\> up — waiting for a reply" |
| Pong recente, emulação do peer ligada | `Connected` / "Connected · \<endpoint (família)\> · Screen · RTT" |
| Pong recente + sessão de entrada | `Connected` / "Connected (bidirectional) · …" |
| Pong recente, emulação do peer desligada | `Connected` âmbar / "… — emulation disabled on the other PC" |
| Já respondeu, silêncio > 6 s | `Reconnecting…` âmbar + spinner / "No reply for Ns — reconnecting…" |
| Peer nunca respondeu | `Connecting…` (após o watchdog do serviço volta a "Trying …") |
| Só sessão de entrada | `Peer connected` / "Incoming session from \<endpoint\>" (nunca "Connected"/input) |
| Duas entradas de cliente, mesmo fingerprint | **uma linha** com estado agregado e todos os IPs no tooltip do DNS |
| IPv4 e IPv6 no mesmo peer | endereço em uso + família, estável por ≥3 s |

## 4. O que depende do serviço (patch exato — Lead/net-auditor)

O frontend funciona sem isso (fallback por cadência de `State`), mas o patch elimina a
heurística e torna o "último Pong" exato (inclusive para receber `last_pong_at` no CLI/doctor
no futuro). Nada em `src/service.rs` é necessário: `FrontendEvent::State` já carrega o
`ClientState` inteiro a cada Pong.

1. `src/client.rs`, em `impl ClientManager` (perto de `set_rtt_ms`):
```rust
/// Timestamp (Unix ms) of the last Pong received on the outgoing link.
pub(crate) fn set_last_pong_at(&self, handle: ClientHandle, timestamp_ms: Option<u64>) {
    if let Some((_, state)) = self.clients.borrow_mut().get_mut(handle as usize) {
        state.last_pong_at = timestamp_ms;
    }
}
```
2. `src/client.rs`, em `deactivate_client` (junto de `s.active_addr = None;`):
```rust
s.last_pong_at = None;
```
3. `src/connect.rs`, no braço `ProtoEvent::Pong` do `receive_loop` (depois de
   `set_rtt_ms(...)`):
```rust
ctx.client_manager
    .set_last_pong_at(handle, Some(unix_ms_now()));
```
   com o helper:
```rust
fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}
```
4. `src/connect.rs`, em `ConnCtx::disconnect` (junto de `set_rtt_ms(handle, None)`):
```rust
self.client_manager.set_last_pong_at(handle, None);
```
5. **Identidade do peer (task-17, net-auditor):** o fix real da duplicação é o serviço manter
   **um cliente por fingerprint de certificado** (uma única entrada por peer, com IPv4+IPv6 no
   mesmo `ips`/failover). Até lá a UI agrega quando o fingerprint está presente; entradas
   legadas sem fingerprint permanecem separadas de propósito.

## 5. Testes e validação

Comandos executados (somente os pacotes do escopo; árvore tem mudanças de outros agentes):
```
cargo fmt -p lan-mouse-gtk -p lan-mouse-ipc                       # ok
cargo check -p lan-mouse-gtk -p lan-mouse-ipc                    # ok
cargo clippy -p lan-mouse-gtk -p lan-mouse-ipc --all-targets     # ok (sem warnings)
cargo test  -p lan-mouse-ipc                                     # 23 passed
cargo test  -p lan-mouse-gtk --lib                               # 45 passed
```

Regressões cobertas:
- `client_row.rs` (21 testes): sem prova não existe "Connected" (handshake/Pending);
  `alive=true` com prova stale → `Reconnecting` (regressão do relato); prova fresca +
  `alive=false` → `ConnectedDegraded`; `Receiving` não muda com prova; varredura de todas as
  combinações garantindo que o badge só diz "Connected" com link de saída + prova fresca;
  spinner/ação/classe por estado; `link_proof_from_age`; `LinkProof::best`; família de
  endereço; `sorted_ips` determinístico (IPv4 primeiro); endereço preferido; debounce
  (`sticky_endpoint` não alterna famílias); título/subtítulo derivados de propriedades.
- `window.rs` (5 testes): identidade só por fingerprint; resumo de membro único;
  **duas entradas do mesmo peer viram um resumo** (OR de `active`, link com RTT, união de IPs);
  preferência pelo membro com round-trip medido; lista vazia.
- `lan-mouse-ipc` (23 testes, incl. serde): roundtrip com `last_pong_at` e payload de serviço
  antigo (campo ausente → `None`; a GUI não inventa liveness).

### Validação manual (Windows 10/11 ↔ Ubuntu 26)
1. **Falso Connected**: conectar; no peer, `kill`/parar o daemon ou dropar UDP 4242
   (`nft`/firewall). Esperado: no máximo ~6 s de `Connected`, depois
   `Reconnecting… — No reply for 6s`; quando o serviço detectar a queda (~4-6 s), a linha vira
   `Connecting…` e volta a `Connected` só com novo Pong. Sem o patch §4, os 6 s são o limite do
   fallback; com o patch, o valor é exato.
2. **Handshake sem resposta**: suspender o peer imediatamente após o handshake
   (`kill -STOP` no serviço do Linux; suspender processo no Windows) — badge não pode dizer
   "Connected"; deve mostrar "Link … up — waiting for a reply" até o watchdog.
3. **Emulação desligada no peer**: desligar emulação no outro PC → `Connected` âmbar com
   "emulation disabled on the other PC" (não Reconnecting).
4. **Duplicado**: config com dois `[[clients]]`/devices para o mesmo peer com o mesmo
   fingerprint (um IPv4, um IPv6) → **uma linha**; tooltip do DNS lista os dois; deletar a
   entrada primária promove a duplicada sem sumir da tela.
5. **IPv4/IPv6**: peer dual-stack; o subtítulo deve mostrar a família e não alternar; ao
   bloquear o IPv4 ativo, após ~3 s muda para o IPv6 (uma vez, não a cada pacote).
6. **Devices**: ordem/endereço estáveis entre `ListDevices` (sem reordenar a cada snapshot).

## 6. Riscos e limitações conhecidas

- **Heurística de fallback**: sem o patch §4, qualquer `State` extra (raro: `Hello`,
  `AuthResponse`, `PairingActive` não emitido por cliente) pode contar como round-trip; o
  primeiro evento é sempre ignorado e edições locais são suprimidas. O erro é para *mais*
  liveness por no máximo uma janela (6 s), nunca para esconder um link caído por mais tempo.
- **6 s de defasagem por design**: um peer que morre segue "Connected" por até 6 s (antes:
  até ~60 s em peer write-only). O watchdog do serviço continua sendo a fonte de verdade final.
- **Agregação depende de fingerprint**: entradas legadas sem fingerprint não são fundidas
  (evita esconder um device distinto); o fix de serviço (task-17) é necessário para o caso
  completo. Linhas ocultas continuam no modelo e recebem fan-out; nada é apagado do serviço.
- **Devices panel** ainda usa `DeviceView.connected` do serviço (que agrega `alive ||
  incoming`), apenas com ordem/endereços estabilizados; um `ConnectionPhase` no serviço seria a
  evolução natural (fora do escopo desta task).
- **Cursor preso** é outra causa (task-18, gtk-phase0); **identidade IPv4/IPv6 no serviço** é
  task-17 (net-auditor). Esta task não mascara nenhuma dessas: apenas impede a UI de afirmar
  "Connected" sem prova e de exibir linhas duplicadas/instáveis.

## 7. Arquivos alterados

- `lan-mouse-ipc/src/lib.rs` — campo `last_pong_at` (serde default) + testes.
- `lan-mouse-gtk/src/client_row.rs` — `LinkProof`, `Reconnecting`, derivação com prova,
  subtitles/tooltips/badge, endereço/família estáveis, `sticky_endpoint`, testes.
- `lan-mouse-gtk/src/window.rs` — heartbeat fallback + tick 1 s, agregação por fingerprint
  (ocultar/promover/fan-out), resumo de peer, ordenação/estabilização de IPs e Devices, testes.
- `lan-mouse-gtk/src/window/imp.rs` — `LinkUi`, `peer_states`, `peer_groups`, `peer_primary`,
  `local_config_changes`, `status_tick`.

Sem commit/push; `src/service.rs`, `src/connect.rs`, `input-*/**` e `installer/**` intocados.
