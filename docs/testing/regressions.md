# Catálogo de regressões

Cada linha representa um bug que já apareceu neste fork (ou num peer antigo) e
o teste que impede a volta dele. Regra: **bug corrigido sem teste aqui não
entra**.

Legenda de status: `coberto` = teste automatizado existente; `manual` = precisa
de hardware/portal; `todo` = teste ainda a criar na fase correspondente.

| ID | Sintoma | Causa raiz | Teste / camada | Status |
|---|---|---|---|---|
| R-01 | Windows voltava o controle ao passar por **qualquer** borda, não só a borda de entrada | backend de emulação reportava overshoot em qualquer borda; fallback do service aceitava | `input-capture/src/windows/event_thread.rs::the_remote_entry_edge_is_the_only_allowed_crossing` (L0) | coberto |
| R-02 | Primeiro clique/tecla/scroll durante `Enter`/`Ack` era engolido | capture reescrevia input em `Enter` durante o handshake | testes de capture/connect do fluxo Enter/Ack (L0/L3) | coberto |
| R-03 | Uma confirmação `Leave(0)` atrasada cancelava um capture recém-adquirido | capture liberava em qualquer `Leave`; agora distingue `Leave(0)` de pedido serializado | `src/capture.rs::leave_release_requests_are_distinguished_from_confirmations` (L0) | coberto |
| R-04 | Cursor entrava no centro da borda em vez da posição de cruzamento | `Enter` não levava coordenada; âncora no centro | `lan-mouse-proto` (EnterAt) + `input-emulation/src/libei.rs::entry_coordinate_parks_at_the_crossing_point` e `input-emulation/src/absolute.rs::entry_anchor_fraction_*` (L0) | coberto |
| R-05 | Primeiro movimento era aplicado antes do warp/arming | listener armava o backend só depois do `Entered` do service | ordenação no listener + testes de arming; E2E de pipeline com warm-up (L0/L3) | coberto |
| R-06 | Primeiro movimento esperava a janela de coalescing | `last_flush` herdado da tela anterior | `src/coalesce.rs::leading_edge_flushes_the_first_motion_of_a_handoff` (L0) | coberto |
| R-07 | Link de saída não subia quando DNS/hostname falhava; UI mostrava "Peer connected" + "last error: not connected" e o cursor ia à borda sem controlar o peer | cliente sem IPs; dial sem candidato apesar da sessão de entrada | `src/client.rs::peer_ip_from_an_incoming_session_is_learned_for_outgoing_dials` + `src/connect.rs::outgoing_addrs_*` (L0) | coberto |
| R-08 | Fechar a janela no Ubuntu matava a conexão | frontend e daemon no mesmo processo/exit code errado | `src/main.rs::a_closed_window_keeps_the_service_running` (L0) | coberto |
| R-09 | `Leave(0)` perdido forçava teardown com AuthRevoke (desconexão explícita) | reuse de `Emulation::disconnect` no caminho de falha de retorno | `close_session` sem AuthRevoke; testes de sessão (L0/L3) + `scripts/qa-two-instances.sh` (L4) | coberto/parcial |
| R-10 | Mesmo peer aparecia duas vezes / "Connected" falso por IPv4/IPv6 | identidade indexada só por IP; reverse client na porta default | `tests/qa_ip_family.rs` + `scripts/qa-two-instances.sh duplicate` (L3/L4) | coberto |
| R-11 | Configs/dispositivos antigos sobreviviam à instalação limpa | instalador/reset não removiam o estado per-user | `tests/qa_installer_clean.rs`, `tests/packaging.rs`, cenário reset (L0/L3) | coberto |
| R-12 | Clipboard de texto/imagem não sincronizava / limites errados | frames, transferência e permissões por direção | testes de `input-clipboard` (L0/L2) e `src/clipboard.rs` (L3) | coberto |
| R-13 | E2E do pipeline (capture→emu) era intermitente | backend lazy respondia `alive=false` no primeiro Pong; `Enter` recusado | `full_capture_to_emulation_and_clipboard_pipeline_e2e` com warm-up determinístico (L3) | coberto |
| R-14 | Build mismatch entre peers não era visível | frontend não comparava commit do peer | `lan-mouse-gpui` link state/detail tests (L0) — ampliar cobertura | coberto/parcial |
| R-15 | Cursor ficava preso no client ao voltar | estado de sessão/captura preso após troca de endereço/família | testes manuais + testes de sessão (L3/L5); precisa ampliar | manual |
| R-16 | Ao clicar Connect com o peer desligado e ir para Configurações, o cursor travava no topo da tela | o backend de captura já armava o grab no `Begin`, mas sem transporte o `Enter` nunca saía; o ponteiro ficava preso até o timeout e o compositor podia reativar o ciclo | `src/capture.rs::a_begin_without_a_connection_does_not_leave_the_pointer_grabbed` (L0) | coberto |
| R-17 | Cliente salvo com certificado antigo: todo Connect falhava e só funcionava apagando e pareando de novo | o pin DTLS antigo rejeitava o certificado novo do peer, mesmo quando esse certificado já era um dispositivo conhecido/autorizado | `src/connect.rs::a_saved_client_with_a_rotated_certificate_repins_a_known_device` (L2) | coberto |
| R-18 | Cliente salvo só com hostname (sem IP) falhava ao conectar depois de reiniciar | o IP aprendido numa sessão de entrada era mantido apenas em memória; sem DNS o dial ficava sem candidato até apagar e re-adicionar | `src/client.rs::discovery_fills_only_an_empty_saved_address_list` + `apply_discovered_hostname` (L0/L2) | coberto |
| R-19 | No Windows 10/11, minimizar o app derrubava a conexão com o outro PC | a saída inesperada/minimizada do frontend era tratada como quit e o launcher parava o daemon; agora só o código explícito de quit para o serviço | `src/main.rs::a_minimized_or_killed_frontend_keeps_the_service_running` (L0) | coberto |
| R-20 | Clicking Connect às vezes pedia permissão no próprio PC (ou não pedia nada) | a lista de endereços salvos podia conter o IP desta máquina (DHCP/hostname/entrada antiga), fazendo o dial chegar ao próprio listener; agora endereços locais são filtrados antes do dial | `src/connect.rs::candidates_never_dial_this_machine` (L0) | coberto |
| R-21 | Um pedido de permissão já existente/descartado não reabria; o outro PC parecia não receber o Connect | o serviço só emitia `ConnectionAttempt` para pedidos novos ou com troca de certificado, então um pending antigo suprimia o prompt para sempre; agora reabre após um cooldown de 10 s | `src/service_core.rs::connection_attempt_prompt_reopens_after_the_cooldown` + wiring (L0/L1) | coberto |
| R-22 | Na aba Network, clicar em Add não conectava e às vezes o cliente ficava com outro IP | todas as linhas usavam o mesmo id GPUI (`add-discovered`), então o clique podia resolver para a hitbox da primeira linha; além disso, o pending Add era consumido por qualquer `Created` (auto-reverse/config reload), configurando o card errado | `lan-mouse-gpui/src/lib.rs::network_add_buttons_have_unique_element_ids` + `pending_add_is_not_consumed_by_an_unrelated_created_event` (L0) | coberto |
| R-23 | No Ubuntu, desconectar o peer deixava o cursor preso na parte de cima do monitor; só matar o processo liberava | a sessão de captura/portal continuava ativa (ou o estado de remote-control ficava latched) quando a última sessão de entrada terminava; o release só acontecia no caminho de ClientLeft/`destroy` | `src/cursor_hide.rs::disconnect_clears_the_heartbeat_for_the_owning_handle` + `src/service.rs::incoming_teardown_always_clears_the_cursor_and_releases_a_stale_grab` (L0) | coberto |

## Como adicionar uma regressão

1. Reproduza o bug no código antigo e escreva um teste que falhe.
2. Escolha a camada mais baixa que reproduz o sintoma (L0 > L2 > L3 > L4).
3. Nomeie o teste pelo comportamento, não pelo número do bug:
   `outgoing_addrs_use_the_incoming_peer_address_when_dns_is_empty`.
4. Registre a linha nesta tabela com sintoma, causa, teste e status.
5. Rode `cargo fmt`, `cargo test`, `clippy` e, se tocar rede/OS, o smoke serial.
