> **UPDATE (2026-11-06):** after this audit the product decision changed to
> **IPv4-only**. The listener, client address set, dialer and UI now ignore IPv6
> entirely; the IPv6 failover paths described below are historical and are no
> longer active. See README "Network tuning" and `bind_listener()`.
>
# Diagnóstico e correção — flapping IPv4/IPv6, peers duplicados e endereço ativo instável

- **Reportado em:** v0.2.4-custom (commit `2c8b201`), Windows 11 + Ubuntu 26; colega com Windows 10/11.
- **Sintomas:** o subtítulo do peer "predator" alterna entre `192.168.166.197` e `2804:2b24:0:6:ae30:175a:2160:bd4`; em outro setup o mesmo peer aparece como **dois dispositivos conectados**; às vezes o app diz "Connected" sem o input funcionar.
- **Escopo das mudanças deste documento:** `src/client.rs`, `src/connect.rs`, `src/listen.rs` (mais este doc). `src/discovery.rs` foi analisado e **não precisou de mudança** (ver seção própria). `src/service.rs` **não foi editado** (dono: gtk-phase0); o patch exato está na seção "Patch necessário em `src/service.rs`".

## Causas-raiz

1. **Dial paralelo A+AAAA no lado que inicia.** `connect_any` (código anterior a esta correção, `src/connect.rs:544-575` em `2c8b201`) disparava um handshake por endereço e devolvia o primeiro que completasse. `joinset.abort_all()` só cancela tentativas *em andamento*: um handshake que já foi concluído na rede deixa uma sessão “fantasma” no listener do peer, sem `close_notify` (o future abortado é descartado). Resultado: uma tentativa do nosso lado pode criar **duas sessões** no peer (uma IPv4 e uma IPv6).
2. **`active_addr` reescrito a cada `Pong`.** No handler de `Pong` o código antigo fazia `ctx.client_manager.set_active_addr(handle, Some(addr))` (`src/connect.rs:851-856` em `2c8b201`) para qualquer transporte que respondesse. Com uma segunda sessão viva (item 1 ou reconexão), o `active_addr` alterna entre IPv4 e IPv6 a cada rodada de ping (~2 s) — exatamente o subtítulo mudando.
3. **Transportes obsoletos nunca eram fechados/cancelados.** `disconnect` antigo só removia entradas de `conns`/`fast_conns` e limpava o estado; o `ping_pong` e o `receive_loop` do transporte antigo **continuavam rodando** e podiam voltar a escrever estado (mais flapping) e enviar input por um socket já substituído ("Connected" sem input).
4. **Listener identificava sessões por `SocketAddr`, não por identidade.** Duas conexões do mesmo peer (outra família, ou reconexão com nova porta de origem) criavam duas sessões lógicas; o serviço recebia dois `Accept`/`Connected` e criava duas entradas por endereço.
5. **Serviço criava cliente reverso por IP e adivinhava a porta** (`src/service.rs`: `AutoAuthorized`/`Connected` → `client_with_ip(addr.ip()).is_none()` → `add_reverse_client(ip)`; `add_reverse_client` sem porta e com `activate_client`, que desativa o cliente da mesma posição). Isso produce o segundo client na porta default 4242 e desativa o client configurado no meio da captura — semente provável do cursor preso (task-18) e do falso "Connected bidirecional" (task-19/20).

## O que foi corrigido (escopo desta task)

### `src/connect.rs` — um transporte por cliente configurado, endereço estável

- **Sem race de famílias:** `dial_order` (`src/connect.rs:609`) ordena os candidatos de forma determinística — (1) o endereço que funcionou na sessão anterior, (2) IPv4, (3) IPv6 — e `connect_first` (`:635`) tenta **um por vez**, com `ADDRESS_TIMEOUT` (2 s) por endereço. Uma tentativa = no máximo uma sessão aberta no peer. O outro endereço/família permanece como failover.
- **Preferência documentada:** IPv4 primeiro porque funciona em qualquer LAN doméstica/corporativa sem RA/firewall IPv6 e é o que a maioria dos peers usa; IPv6 é usado quando for o único existente (**IPv6-only continua funcionando**) e passa à frente nas reconexões quando foi ele que estabeleceu a sessão por último.
- **Memória do endereço bom:** `ClientManager::record_working_addr`/`preferred_addr` (`src/client.rs:510-524`) guarda em memória (não persiste, não muda o IPC) o endereço que estabeleceu a sessão. `connect_to_handle` esquece a preferência se o IP sumiu do conjunto atual (mudança de DNS/DHCP) e cai na ordem padrão.
- **Transporte único + geração:** `ActiveConnection { generation, addr, conn }` (`src/connect.rs:65`), instalado só depois de handshake + pin TOFU; uma segunda instalação defensiva fecha a anterior. `ConnCtx::disconnect(handle, generation)` (`:221`) só age se aquela geração ainda for a atual, limpa o estado derivado **e fecha o transporte** — uma sessão antiga não pode mais ressuscitar `active_addr`.
- **`Pong` não mexe mais em `active_addr`:** `active_addr` é escrito uma única vez, no sucesso do handshake; `Pong` só atualiza `alive`, RTT e capabilities (e somente se a geração ainda for a atual).
- **Loops antigos se encerram:** `ping_pong`/`receive_loop` recebem a geração do transporte e saem (sem tocar em estado) quando ela deixa de ser a atual; `hello_sent`/`auth_sessions`/`ping_*` por endereço são limpos no disconnect (evita `Hello` omitido quando `ip:porta` se repete).

### `src/listen.rs` — identidade = fingerprint do certificado, uma sessão lógica por peer

- **`SessionMeta`/`plan_accept`** (`src/listen.rs:54-118`): a primeira conexão autenticada de um fingerprint cria a sessão lógica com o endereço de origem; conexões seguintes do **mesmo fingerprint** (outra família, nova porta, sessão antiga meio-aberta) **substituem o transporte sob o mesmo endereço lógico**, sem emitir um segundo `Accept`/`Connected`. É isso que impede o serviço de ver "dois devices" para o mesmo peer e impede o endereço de alternar.
- **Fecha a obsoleta:** o transporte anterior é fechado no replace; `read_loop` usa geração (`session_is_current`) e `finish_session` só remove/notifica se ainda for a sessão atual — um loop antigo não pode remover nem fechar a conexão nova.
- **`reply`/`reply_raw`/`disconnect`/`get_certificate_fingerprint`** passam a indexar por endereço lógico (um transporte por peer); `disconnect` (revogação) emite `Closed` imediatamente e remove a sessão antes de fechar.
- **IPv6:** o bind continua dual-stack no Linux e IPv4 em Windows/macOS (sem mudança); a normalização de endereços IPv4-mapped permanece. Sessões IPv6 do mesmo peer agora reutilizam a sessão lógica existente.

### `src/discovery.rs` — analisado, sem alteração necessária

A descoberta é **IPv4-only** por decisão de design atual (`local_interfaces` só retorna V4; anúncios IPv6 são ignorados) e não participa do caminho de conexão nem do cálculo de `active_addr`. O flapping e a duplicação vêm do dial A+AAAA + identidade por endereço, não do discovery. Fica registrada a limitação: **peers IPv6-only não aparecem na tabela de rede** (não afeta controle manual por IP/hostname). Mudar isso exigiria varrer/anunciar IPv6 e não é necessário para o bug reportado.

## Testes adicionados e executados

Novos testes unitários/integração (todos nos arquivos do escopo):

- `src/connect.rs`: `dial_order_prefers_ipv4_before_ipv6`, `dial_order_keeps_the_remembered_address_first`, `dial_order_is_deterministic_and_deduplicated`, `dial_order_keeps_ipv6_only_clients_working`, `dial_order_ignores_a_preference_outside_the_candidate_set`; integração `dual_stack_client_opens_a_single_session` (cliente com A+AAAA abre **uma** sessão) e `reconnect_with_same_certificate_replaces_the_session` (mesmo cert de outra porta/família substitui o transporte, endereço lógico não muda, nenhum `Accept` duplicado). O teste de reconexão é **orientado a eventos** (sem sleep fixo de 1 s), com deadline de 15 s e reenvio do `Enter` se um datagrama de loopback for perdido — endurecido após uma falha única observada em execução paralela de CI (5× suíte completa + 3× `--test-threads=32` + 15× isolado passaram na verificação).
- `src/listen.rs`: `first_connection_enrols_with_its_own_address`, `same_fingerprint_from_another_family_keeps_the_logical_address`, `reconnecting_from_the_same_family_replaces_the_old_transport`, `different_fingerprints_stay_separate_devices`, `unknown_fingerprint_is_never_current`.
- `src/client.rs`: `working_address_is_remembered_per_client`, `working_address_does_not_leak_into_a_reused_handle` (slab reusa handles), `record_working_addr_ignores_unknown_handles`.

Resultado (validação final na árvore compartilhada, após os patches concorrentes de service/capture/IPC/UI landarem):

- `cargo test -p lan-mouse --lib` → **172 passed, 0 failed** (default features; inclui os testes de fumaça de rede/loopback existentes e os dos demais agentes).
- `cargo test -p lan-mouse-ipc` → 23 passed (inclui payload de serviço antigo → `last_pong_at: None`).
- `cargo clippy --workspace --all-targets -- -D warnings` → **exit 0, sem warnings** (inclui o refactor de `read_loop` para `ReadLoopCtx`, que eliminou o `clippy::too_many_arguments`).
- `cargo fmt --check` do workspace → exit 0; `rustfmt --check` ok nos três arquivos do meu escopo.
- Validação intermediária em checkout limpo (`git archive HEAD` + meus 3 arquivos + `lan-mouse-ipc/src/lib.rs` com `last_pong_at`): 159 passed, 0 failed.

## Patch necessário em `src/service.rs` (dono: gtk-phase0; autorizado pelo Lead)

Objetivo: identidade do peer = **fingerprint**, nunca IP; nunca criar/ativar cliente reverso duplicado nem adivinhar porta.

**(1) Helper** (junto de `client_with_ip`):

```rust
    /// Client that represents this certificate identity, if any.
    fn client_with_fingerprint(&self, fingerprint: &str) -> Option<ClientHandle> {
        self.client_manager.find_by_fingerprint(fingerprint)
    }

    /// Incoming session addr already registered for this identity.
    fn incoming_addr_for_fingerprint(&self, fingerprint: &str) -> Option<SocketAddr> {
        self.incoming_conn_info
            .values()
            .find(|incoming| incoming.fingerprint == fingerprint)
            .map(|incoming| incoming.addr)
    }
```

**(2) `EmulationEvent::AutoAuthorized` e `EmulationEvent::Connected`:** antes de qualquer `add_reverse_client`, procurar `client_with_fingerprint`; se existir, apenas religar a sessão de entrada a esse handle e **retornar** (sem criar reverso, sem `activate_client`):

```rust
                if let Some(handle) = self.client_with_fingerprint(&fingerprint) {
                    // O peer já é um client configurado: uma sessão incoming não
                    // pode virar um segundo client (era isto que duplicava o
                    // device e desativava o link configurado).
                    self.client_manager.set_incoming_addr(handle, Some(addr));
                    self.broadcast_client(handle);
                } else if /* outgoing.grants_anything() */ {
                    self.add_reverse_client(name, addr.ip(), position, Some(fingerprint.clone()));
                }
```

No `Connected`, manter o `pending_reverse.remove(&fingerprint)` antes: se houver client configurado por fingerprint, aplicar apenas `set_incoming_addr` (e, se o usuário escolheu posição, `update_pos` nesse handle), nunca criar outro.

**(3) `add_reverse_client`:** nunca duplicar por IP e não adivinhar porta:

```rust
        // 1) mesmo fingerprint já é um client: reaproveita, não cria.
        if let Some(handle) = fingerprint
            .as_deref()
            .and_then(|fp| self.client_with_fingerprint(fp))
        {
            self.client_manager.set_incoming_addr(handle, Some(SocketAddr::new(ip, 0)));
            self.broadcast_client(handle);
            return;
        }
        // 2) porta: usa a anunciada no discovery, não DEFAULT_PORT.
        let discovered_port = self.discovered.get(&ip).and_then(|device| device.port);
        // ... cria o client, set_fix_ips([ip]), set_fingerprint(...)
        if let Some(port) = discovered_port {
            self.client_manager.set_port(handle, port);
            self.activate_client(handle);
        } else {
            // Sem porta conhecida: criar inativo evita dialar 4242 por engano.
            log::info!("reverse client {handle} created inactive: listen port unknown");
        }
```

**(4) `activate_client`:** não desativar o client da mesma posição quando é o **mesmo peer**:

```rust
        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                let same_peer = match (
                    self.client_manager.fingerprint(handle),
                    self.client_manager.fingerprint(other),
                ) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                };
                if same_peer {
                    log::warn!("not deactivating {other}: same peer as {handle}");
                } else {
                    self.deactivate_client(other);
                }
            }
        }
```

**(5) `EmulationEvent::Entered`:** defesa em profundidade contra duas capture zones do mesmo fingerprint:

```rust
                if let Some(stale) = self.incoming_addr_for_fingerprint(&fingerprint) {
                    if stale != addr {
                        self.remove_incoming(stale);
                        self.notify_frontend(FrontendEvent::IncomingDisconnected(stale));
                    }
                }
                if !self.incoming_conns.contains(&addr) { self.add_incoming(...); ... }
```

## Revalidação combinada (harness do QA)

Com o listener já deduplicado por fingerprint nesta task + o patch de service acima:

```sh
cargo build -p lan-mouse --no-default-features
scripts/qa-two-instances.sh dual        # 1 linha por peer, 1 sessão
scripts/qa-two-instances.sh ipv6_only   # IPv6-only continua ok
scripts/qa-two-instances.sh duplicate   # esperado: 1 linha por peer, nenhum :4242
```

Hoje `duplicate` falha por causa do service (2 clients, um na 4242, client configurado desativado) — é exatamente o que o patch acima corrige.

## Integração task-19 — prova de último `Pong` (`last_pong_at`)

O conn-state-analyst adicionou `ClientState.last_pong_at: Option<u64>` (Unix ms, `serde(default)`) ao IPC. O lado do serviço foi fechado no meu escopo, conforme `docs/audit-connected-truth.md` §4:

- `ClientManager::set_last_pong_at` (`src/client.rs`), junto de `set_rtt_ms`: atualiza o estado; o broadcast continua sendo disparado pelo chamador via `notify_state_changed` (é assim que os demais setters funcionam — o `FrontendEvent::State` é emitido pelo service ao receber o sinal em `state_rx`).
- `deactivate_client` limpa `last_pong_at` junto de `active_addr`/`alive`/`rtt_ms` ("sem transporte não há prova de round-trip").
- `receive_loop`, braço `Pong` (`src/connect.rs`): grava `Some(unix_ms_now())` (helper `unix_ms_now`), e o `notify_state_changed(handle)` já existente no fim do braço publica o `State` atualizado.
- `ConnCtx::disconnect` limpa `last_pong_at` junto de `active_addr`/`alive`/`rtt_ms`.
- Payload antigo/default continua `None` (coberto por `lan-mouse-ipc`: `tests::client_state_from_an_older_peer_uses_defaults`, 23 testes passando) e por `last_pong_at_starts_empty_and_tracks_the_link` / `deactivate_clears_live_connection_state` em `src/client.rs`.

Com isso a UI deixa de depender só da heurística de cadência de `State`: "Connected" passa a ter prova exata de round-trip (janela de 6 s do frontend), e a queda do link limpa a prova imediatamente no disconnect/deactivate.

## O que ainda precisa de teste real (Windows 11/10 + Ubuntu 26)

1. **Peer com A+AAAA real** (não loopback): garantir uma única linha, endereço estável em ambos os lados por vários minutos, incluindo restart dos apps e reconexão após perda de Wi-Fi.
2. **Flapping:** derrubar temporariamente a família preferida (ex.: `nft add rule inet filter output ip daddr <ipv4> drop` por 30 s) e confirmar failover único para a outra família, sem segunda sessão no peer e sem "Connected" falso.
3. **Dois PCs Windows 10/11** (caso do colega): validar que o reverso agora aponta para o client configurado (fingerprint) e que nenhum client é desativado por colisão de posição (cursor preso).
4. **Cursor preso / Connected sem input (task-18/19):** com o service patchado, confirmar que `incoming_addr` não é reatribuído entre famílias e que `active_addr` só muda após falha real; a UI (task-19) usa `last_pong_at` para freshness — validar o estado "reconnecting" após 4–6 s de silêncio.
5. **Peers de versão mista:** cliente novo (sequencial) + listener antigo (sem dedupe) deve continuar abrindo só uma sessão; listener novo + cliente antigo (dial A+AAAA paralelo) deve deduplicar e fechar a obsoleta — testar as duas combinações.
6. **IPv6-only real** (rede sem IPv4, não só ::1) e **dual-stack atrás de NAT/firewall assimétrico**: verificar tempo de failover (até 2 s por endereço preferido morto) e histórico de log (`ordered addresses`, `reconnected from ..., keeping logical address`).
