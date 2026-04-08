# Architecture Map — `sync-rollup-composer`

> **Purpose**: snapshot del estado pre-refactor del crate `based-rollup`. Este documento es la entrada al proyecto entero para alguien que va a ejecutar el refactor descrito en `docs/refactor/PLAN.md`.
>
> **Generado**: 2026-04-08 (paso 0.1 del PLAN, branch `refactor/phase-0-mapping`).
>
> **Audiencia**: dev que necesita entender los flujos de datos y los puntos de entrada antes de tocar código.

---

## 1. Vista de bloques

```
                            ┌──────────────────────────────────────────┐
                            │            L1 (reth --dev)               │
                            │  ┌────────────────────────────────────┐  │
                            │  │  Rollups.sol / CCM / Bridge / etc │  │
                            │  └─────────▲──────────┬───────────────┘  │
                            │            │postBatch │BatchPosted        │
                            └────────────┼──────────┼──────────────────┘
                                         │          │
                          ┌──────────────┴──┐    ┌──┴──────────────────┐
                          │   proposer.rs   │    │   derivation.rs     │
                          │  (sender L1)    │    │  (L1 sync, §4e/§4f)│
                          └──────────────▲──┘    └──────────┬──────────┘
                                         │                  │
                                         │                  ▼
   ┌────────────────────┐   ┌────────────┴──────────────────────────┐
   │ payload_builder.rs │   │              driver.rs                │
   │   block building   │◄──┤ step_builder │ flush_to_l1 │ verify  │
   └────────────────────┘   └────────────┬──────────────────────────┘
                                         │
            ┌────────────────────────────┴────────────────────────────┐
            │                                                          │
            ▼                                                          ▼
   ┌────────────────────────┐                          ┌──────────────────────────┐
   │   composer_rpc/        │                          │   cross_chain.rs +      │
   │  ┌──────────────────┐  │                          │   table_builder.rs     │
   │  │ l1_to_l2.rs (4k) │  │                          │  - ABI types           │
   │  │ l2_to_l1.rs (5k) │  │ ◄──────── builds ─────── │  - entry building      │
   │  │  trace.rs        │  │                          │  - state delta logic   │
   │  │  common.rs       │  │                          └──────────────────────────┘
   │  └──────────────────┘  │                                       ▲
   │  HTTP RPC interceptors │                                       │
   │  (hold-then-forward)   │                                       │
   └─────────▲──────────────┘                                       │
             │                                          ┌───────────┴────────────┐
             │                                          │       rpc.rs            │
             │            cliente JSON-RPC              │  (jsonrpsee trait)      │
             └──────────────────────────────────────────┤  syncrollups_*          │
                                                         └─────────────────────────┘
```

## 2. Métricas del estado actual

| Archivo | LOC | Función-tope | LOC tope |
|---|---|---|---|
| `driver.rs` | 4 632 | `step_builder` | ~760 |
| `driver.rs` | — | `flush_to_l1` | ~640 |
| `driver.rs` | — | `verify_local_block_matches_l1` | ~253 |
| `driver.rs` | — | `build_builder_protocol_txs` | ~280 |
| `composer_rpc/l1_to_l2.rs` | 4 345 | `trace_and_detect_internal_calls` | ~1 817 |
| `composer_rpc/l1_to_l2.rs` | — | `simulate_l1_to_l2_call_chained_on_l2` | ~491 |
| `composer_rpc/l1_to_l2.rs` | — | `build_and_run_l1_postbatch_trace` | ~327 |
| `composer_rpc/l2_to_l1.rs` | 5 459 | `trace_and_detect_l2_internal_calls` | ~1 693 |
| `composer_rpc/l2_to_l1.rs` | — | `simulate_l1_combined_delivery` | ~547 |
| `composer_rpc/l2_to_l1.rs` | — | `simulate_l1_delivery` | ~535 |
| `composer_rpc/l2_to_l1.rs` | — | `try_chained_l2_enrichment` | ~406 |
| `composer_rpc/l2_to_l1.rs` | — | `enrich_return_calls_via_l2_trace` | ~392 |
| `composer_rpc/trace.rs` | 1 385 | `walk_trace_tree` | ~150 |
| `cross_chain.rs` | 2 410 | `attach_chained_state_deltas` | ~70 |
| `table_builder.rs` | 2 524 | `build_l2_to_l1_continuation_entries` | ~310 |
| `derivation.rs` | 1 172 | `derive_next_batch` | ~573 |
| `proposer.rs` | 558 | `send_to_l1` | ~120 |
| `rpc.rs` | 1 281 | (RPC trait + serde structs) | — |

**Totales del crate**: ~16 k LOC producción + ~24 k LOC tests = ~40 k LOC. 534 tests, 0 warnings clippy, 48 `unwrap()` en producción (mayoría en `composer_rpc/trace.rs`), 1 sólo `TODO`.

## 3. Flujos críticos (walkthrough)

### 3.1 `postBatch` outbound (builder → L1)

Camino: composer → driver → flush_to_l1 → proposer → L1.

1. Un usuario manda `eth_sendRawTransaction` a `composer_rpc/l2_to_l1.rs:155` (`handle_request`) o a `composer_rpc/l1_to_l2.rs:196` (`handle_request`).
2. El composer detecta si es cross-chain via `composer_rpc/trace.rs::walk_trace_tree`. Si lo es:
   a. Invoca `debug_traceCallMany` recursivamente (`trace_and_detect_*` mamuts).
   b. Llama a `cross_chain.rs::build_l2_to_l1_call_entries` (línea 819) o `table_builder.rs::build_continuation_entries` (línea 264) para construir L1+L2 entries.
   c. Pushes a `Driver.queued_cross_chain_calls` o `queued_l2_to_l1_calls` (driver.rs:123, 129) — colas internas `Arc<Mutex<Vec<_>>>`.
   d. Espera "confirmation" del driver antes de forwardear el tx upstream (hold-then-forward).
3. `Driver::step_builder` (driver.rs:1036) drena las colas, fusiona entries en `pending_l1_entries` (driver.rs:132) usando los 4 vectores paralelos: `pending_l1_entries`, `pending_l1_group_starts`, `pending_l1_independent`, `pending_l1_trigger_metadata`.
4. `Driver::build_builder_protocol_txs` (driver.rs:3947) construye las txs protocolarias L2 que cargan las entries (`loadExecutionTable` + `executeIncomingCrossChainCall`).
5. `Driver::build_and_insert_block` (driver.rs:4328) construye el bloque L2 vía `payload_builder.rs` y lo persiste en reth (FCU).
6. `Driver::flush_to_l1` (driver.rs:1796) decide si hay que postear:
   a. Chequea `pending_entry_verification_block` (hold gate) y `last_submission_failure` (cooldown).
   b. Compara `pre_state_root` con on-chain state root para skipear bloques ya enviados.
   c. Llama a `proposer.send_to_l1(...)` con las entries acumuladas.
   d. Marca el bloque como pendiente de verificación (hold).
7. `proposer.rs::send_to_l1` envía el `postBatch` con explicit nonce vía `send_l1_tx_with_nonce`. Si falla, retorna error y `flush_to_l1` lo registra.

### 3.2 Derivation inbound (L1 → builder & fullnodes)

Camino: L1 BatchPosted event → derivation.rs → driver.rs → reth.

1. `DerivationPipeline::derive_next_batch` (derivation.rs:208) hace polling sobre L1:
   a. Obtiene logs `BatchPosted` desde el último `last_processed_l1_block`.
   b. Para cada log, decodea calldata del `postBatch` tx (`cross_chain.rs::parse_batch_posted_logs`, línea 1661).
   c. Aplica filtering §4f: `cross_chain.rs::filter_block_by_trigger_prefix` (línea 2196) — prefix counting, no all-or-nothing. Identifica trigger tx indices vía `identify_trigger_tx_indices` (línea 2172) y consume el prefijo via `compute_consumed_trigger_prefix` (línea 2237).
   d. Aplica state deltas via `cross_chain.rs::attach_generic_state_deltas` (línea 2316) o `attach_chained_state_deltas` (línea 1520).
   e. Convierte L1 entries a L2 pairs vía `cross_chain.rs::convert_l1_entries_to_l2_pairs` (línea 1169).
2. Output: `DerivedBatch` con `Vec<DerivedBlockMeta>`.
3. `Driver::step_builder` o `step_sync` consume el batch:
   a. Si el bloque ya existe localmente: `verify_local_block_matches_l1` (driver.rs:3204). Si no matchea: defer (hasta `MAX_ENTRY_VERIFY_DEFERRALS=3`) o rewind.
   b. Si no existe: `build_and_insert_block` aplicando el `apply_deferred_filtering` (driver.rs:3457) que filtra txs según las receipts.
4. Driver llama `update_fork_choice(block_hash)` (driver.rs:4415) — engine API FCU. Cuando reth retorna `Valid`, el bloque queda canónico en chain state.

### 3.3 `composer_rpc` hold-then-forward

Camino: usuario → composer RPC → driver queue → confirmation → forward upstream.

**Por qué existe**: en una sync rollup el orden importa. Si un user tx que dispara cross-chain se forwardea ANTES de que las entries existan en CCM, hay race con `ExecutionNotFound`. La solución es: encolar entries primero, esperar que el driver las incluya en un bloque L2, luego forwardear.

**Cómo funciona hoy** (a refactorizar en 1.6b+c):
1. Composer recibe `eth_sendRawTransaction`.
2. Detecta cross-chain vía `walk_trace_tree`.
3. Construye entries vía `cross_chain.rs` / `table_builder.rs`.
4. Pushes entries al driver queue (`Arc<Mutex<Vec<_>>>`) — fire-and-poll style.
5. Polling loop hasta detectar que el driver drenó la cola → asume que está incluido en un bloque.
6. Forwardea el user tx a la URL upstream.

**Riesgo actual**: el polling es timing-sensitive. Si el driver es lento o el composer asume confirmación demasiado pronto, hay race condition. Esto se cierra en el paso 1.6b+c con `EntryQueue` 3-state machine + `ForwardPermit` token que sólo se emite post-FCU.

## 4. Puntos de entrada (file:line)

| Función | Archivo | Línea | Rol |
|---|---|---|---|
| `run` (main loop driver) | `driver.rs` | 607 | Loop principal del driver |
| `step` | `driver.rs` | 724 | Una iteración del loop |
| `step_builder` | `driver.rs` | 1036 | Modo builder: construye + flushea |
| `step_sync` | `driver.rs` | 925 | Modo sync: deriva + verifica |
| `step_fullnode` | `driver.rs` | 2698 | Modo fullnode: deriva + aplica |
| `flush_to_l1` | `driver.rs` | 1796 | Decide y envía postBatch |
| `verify_local_block_matches_l1` | `driver.rs` | 3204 | Verifica bloque local vs derivado |
| `build_builder_protocol_txs` | `driver.rs` | 3947 | Construye txs protocolarias L2 |
| `build_and_insert_block` | `driver.rs` | 4328 | Construye + persiste bloque L2 |
| `update_fork_choice` | `driver.rs` | 4415 | Engine API FCU |
| `rewind_l2_chain` | `driver.rs` | 4445 | Re-derivación tras mismatch |
| `derive_next_batch` | `derivation.rs` | 208 | Sync L1 → batch derivado |
| `parse_batch_posted_logs` | `cross_chain.rs` | 1661 | Decodea BatchPosted events |
| `filter_block_by_trigger_prefix` | `cross_chain.rs` | 2196 | §4f filtering |
| `attach_chained_state_deltas` | `cross_chain.rs` | 1520 | Chained delta protocol |
| `attach_generic_state_deltas` | `cross_chain.rs` | 2316 | Generic state delta attachment |
| `convert_l1_entries_to_l2_pairs` | `cross_chain.rs` | 1169 | L1 → L2 pair conversion |
| `build_l2_to_l1_call_entries` | `cross_chain.rs` | 819 | L2→L1 entry construction |
| `reorder_for_swap_and_pop` | `table_builder.rs` | 124 | CCM swap-and-pop reorder |
| `build_continuation_entries` | `table_builder.rs` | 264 | L1→L2 continuation entries |
| `analyze_l2_to_l1_continuation_calls` | `table_builder.rs` | 1221 | L2→L1 multi-call analysis |
| `build_l2_to_l1_continuation_entries` | `table_builder.rs` | 1612 | L2→L1 continuation entries |
| `walk_trace_tree` | `composer_rpc/trace.rs` | ~150 | Generic cross-chain detection |
| `trace_and_detect_internal_calls` | `composer_rpc/l1_to_l2.rs` | 2280 | L1→L2 detection (mamut) |
| `trace_and_detect_l2_internal_calls` | `composer_rpc/l2_to_l1.rs` | 3558 | L2→L1 detection (mamut) |
| `simulate_l1_combined_delivery` | `composer_rpc/l2_to_l1.rs` | 2477 | Combined sim L2→L1 |
| `send_to_l1` | `proposer.rs` | ~140 | Envía postBatch a L1 |
| `sign_proof` | `proposer.rs` | ~250 | Firma ECDSA del publicInputsHash |

## 5. Referencias normativas

- **`docs/DERIVATION.md`** — spec normativa del protocolo. NO se modifica en este refactor.
- **`docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md`** — formal spec extraída de Solidity. NO se modifica.
- **`CLAUDE.md > Lessons Learned — Hard-Won Rules`** — ~50 reglas críticas. El refactor las codifica en el sistema de tipos (ver `INVARIANT_MAP.md`).
- **`contracts/sync-rollups-protocol/`** — submódulo Solidity. NO se modifica.

## 6. Relación con el PLAN

Este `ARCH_MAP.md` es la salida del **paso 0.1** del `PLAN.md`. Las referencias `file:line` aquí son las que los pasos del plan usan para anclar sus cambios. Si después del refactor estas líneas cambian, este documento queda como snapshot histórico — NO se actualiza, porque su propósito es documentar el "estado pre-refactor".

Para entender qué se va a cambiar en cada función de esta lista, ver la sección §8 del `PLAN.md` (plan detallado por fases).
