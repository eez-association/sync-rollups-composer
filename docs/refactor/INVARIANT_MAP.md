# Invariant Map — `sync-rollup-composer`

> **Purpose**: tabla expandida de las 23 invariantes críticas que el refactor debe codificar en el sistema de tipos o en tests/gates de CI dedicados. Cada fila documenta el owner actual (humano), el test actual (si existe), el tipo futuro, y la fase del PLAN que la cierra.
>
> **Generado**: 2026-04-08 (paso 0.2 del PLAN, branch `refactor/phase-0-mapping`).
>
> **Fuente**: `CLAUDE.md > Lessons Learned — Hard-Won Rules`. Este mapa es el contrato del refactor — cuando todas las invariantes tengan ✓ en "tipo futuro implementado", el refactor está cerrado.

---

## Leyenda

- **Compile-time** = el sistema de tipos rechaza la violación con error de compilación.
- **Test/gate** = la violación produce falla de test (unit, property, E2E) o lint de CI (clippy, grep).
- Las dos columnas finales (`Tipo futuro`, `Estado`) se actualizan a medida que el refactor avanza.

---

## Tabla de invariantes

| # | Invariante | Owner actual (humano) | Test actual | Tipo futuro | Tipo de cierre | Fase | Estado |
|---|---|---|---|---|---|---|---|
| 1 | Hold MUST be set BEFORE send_to_l1 | Comentario en `flush_to_l1` (`driver.rs:1796`) y disciplina | `test_hold_set_only_when_entries_non_empty` | `FlushPlan<HoldArmed>` typestate con `entry_block` dentro del plan; `send_to_l1` sólo acepta `Sendable` | Compile-time | 1.7 | ☐ |
| 2 | NEVER use auto-nonce, always reset on failure | Comentario en `proposer.rs` | `test_submission_failure_sets_cooldown` | `L1NonceReservation` + `#[must_use] NonceResetRequired` | Compile-time | 1.8 | ☐ |
| 3 | NEVER align state roots by overwriting pre_state_root | Comentario `flush_to_l1` | (sin test directo, sólo replay baseline) | `CleanStateRoot(B256)` newtype con `pub(crate)` constructor + `from_*_boundary` explícitos | Compile-time | 1.2 | ☐ |
| 4 | §4f filtering is per-call prefix counting, never all-or-nothing | Comentario `derivation.rs` y `cross_chain.rs:2237` (`compute_consumed_trigger_prefix`) | (sin property test) | `ConsumedPrefix(usize)` + property test de monotonicidad | Test/gate | 0.3 | ☐ |
| 5 | Continuation entries are NOT triggers (`hash(next_action) != action_hash`) | Comentario en `partition_entries` (`cross_chain.rs:2022`) | `test_partition_entries_continuation_not_trigger` | `enum EntryClass { Trigger, Continuation, Result, RevertContinue }` | Compile-time | 1.4 | ☐ |
| 6 | Result entry skipped when `extra_l2_entries` non-empty | Doble check en driver y rpc | `test_convert_l1_entries_skips_result_with_continuations` | `enum QueuedCallRequest::{Simple, WithContinuations}` — la variante `Simple` carga `result_entry`, `WithContinuations` no lo admite | Compile-time | 1.4b | ☐ |
| 7 | `parent_call_index` debe rebasarse después de combined_delivery | Comentario en `composer_rpc/l2_to_l1.rs:4724` | (sin test) | `enum ParentLink { Root, Child(AbsoluteCallIndex) }` + helper único `rebase_parent_links` | Compile-time | 1.3 + 3.3 | ☐ |
| 8 | First TRIGGER entry needs `currentState=clean` (post swap-and-pop reorder) | Lógica imperativa en `attach_chained_state_deltas` (`cross_chain.rs:1520`) | `test_first_trigger_needs_clean_root_after_reorder` | `RevertGroupBuilder::first_trigger_idx()` calculado correctamente, con test dedicado | Test/gate | 1.9b | ☐ |
| 9 | Deferral exhaustion → rewind, not accept | Lógica imperativa en `verify_local_block_matches_l1` (`driver.rs:3204`) | `test_full_rewind_cycle_state_transitions` | `enum VerificationDecision::MismatchRewind { target }` | Test/gate | 2.5 | ☐ |
| 10 | Rewind target is `entry_block - 1` | Comentario | (cubierto por `test_full_rewind_cycle_state_transitions`) | Método único `Driver::rewind_to_re_derive(entry_block)` que calcula target dentro | Test/gate | 2.5 | ☐ |
| 11 | Deposits + withdrawals can coexist in same block | Removed mutual exclusion check | `test_unified_deposit_withdrawal_block` | `enum BlockEntryMix { Empty, OnlyD, OnlyW, Mixed }` exportado por `PendingL1SubmissionQueue` | Compile-time | 1.5 | ☐ |
| 12 | Multi-call L2→L1 must use scope navigation on Entry 1 | Comentario en `table_builder.rs:1612` | `test_l2_to_l1_continuation_uses_scope_return` | `L2ToL1ContinuationBuilder::with_scope_return(scope)` requerido — `build()` falla sin él | Compile-time | 1.9c | ☐ |
| 13 | **Hold-then-forward: composer RPCs MUST await queue confirmation** | Comentario en `composer_rpc/*` | `test_composer_holds_until_queue_confirmation` | `ForwardPermit` token sólo se construye al transición `Reserved → Confirmed` post-FCU | Compile-time | 1.6b+c | ☐ |
| 14 | **Builder HALTS block production while hold is active** | Comentario en `step_builder` | `test_builder_halts_while_hold_active` | `EntryVerificationHold::is_blocking_build()` consultado en `BuilderStage::Build` | Compile-time | 1.6 | ☐ |
| 15 | **Withdrawal trigger revert on L1 causes REWIND** | Lógica en `flush_to_l1` post-submit | `test_withdrawal_trigger_revert_rewind` | `enum TriggerExecutionResult::RevertedNeedsRewind(entry_block)` `#[must_use]` | Test/gate | 2.7b | ☐ |
| 16 | **§4f filtering is generic (CrossChainCallExecuted events), NOT Bridge selectors** | `cross_chain.rs:2172` (`identify_trigger_tx_indices`) — ya es generic, sin selectors | `test_filter_uses_event_not_selector` | Test que verifica que `extract_l2_to_l1_tx_indices` no contiene strings hex de selectors | Test/gate | 0.3 | ☐ |
| 17 | **NEVER per-call simulate_l1_delivery for multi-call L2→L1** | Comentario en `composer_rpc/l2_to_l1.rs` | `test_multi_call_uses_combined_sim` | `enum SimulationPlan` + `simulation_plan_for(calls, promotion)` único punto de decisión | Compile-time | 3.6 | ☐ |
| 18 | **L1 and L2 entry structures must MIRROR** | Comentario CLAUDE.md | (cero coverage actual) | Mirror tests con `MirrorCase` DSL en `src/test_support/mirror_case.rs` | Test/gate | 0.5 | ☐ |
| 19 | **NEVER swap (dest, source) for L1→L2 return call children** | Comentario en `table_builder.rs` | `test_l1_to_l2_return_call_no_swap` | `enum CallOrientation { Forward, Return }` + helper único `address_pair_for(orientation)` | Compile-time | 1.9a | ☐ |
| 20 | **Return data shape: Void = 0 bytes; delivery_return_data → hashes; l2_return_data → scope resolution** | Múltiples comentarios CLAUDE.md (#245, #246) | (parcialmente) | `enum ReturnData { Void, NonVoid(Bytes) }` propagado por todos los builders | Compile-time | 1.10 | ☐ |
| 21 | **Single L2→L1 + terminal return still promotes to multi-call continuation** | Bool condition en `composer_rpc/l2_to_l1.rs` | `test_single_call_terminal_return_promotes` | `enum PromotionDecision { KeepSingle, PromoteToContinuation }` retornado por `Direction::promotion_rule` y consumido por `simulation_plan_for` | Compile-time | 3.1 + 3.6 | ☐ |
| 22 | **`publicInputsHash` uses block.timestamp, not block.number** | Código en `proposer.rs` + proxy sim | (cubierto por replay baseline) | `ProofContext { block_timestamp: U256, … }` aceptado obligatoriamente por `sign_proof` | Compile-time | 1.8 | ☐ |
| 23 | **NEVER hardcode function selectors — `sol!` only** | Convención (CLAUDE.md) | (sin gate actual) | CI grep gate: `grep -rn "0x[a-f0-9]\{8\}" crates/based-rollup/src/` debe retornar 0 matches fuera del bloque `sol!` | Test/gate | 4.4 | ☐ |

---

## Resumen por tipo de cierre

| Tipo | Cantidad | Invariantes |
|---|---|---|
| **Compile-time** | 14 | #1, #2, #3, #5, #6, #7, #11, #12, #13, #14, #17, #19, #20, #21, #22 |
| **Test/gate** | 9 | #4, #8, #9, #10, #15, #16, #18, #23 (#15 también) |

**Total**: 23 invariantes (15 compile-time si contamos #15 en ambas + 9 test/gate, con #15 en doble columna porque tanto el `enum TriggerExecutionResult` como el test E2E lo cubren).

## Resumen por fase

| Fase | Invariantes cerradas |
|---|---|
| 0 (Guardrails) | #4, #16, #18 |
| 1 (Tipos) | #1, #2, #3, #5, #6, #7, #8, #11, #12, #13, #14, #19, #20, #22 |
| 2 (Pipelines) | #9, #10, #15 |
| 3 (Direction) | #17, #21 |
| 4 (Layer split) | #23 |

## Procedimiento de verificación

Cuando cada invariante se cierre:
1. Crear el tipo / test / gate descrito en la columna "Tipo futuro".
2. Marcar la columna "Estado" como ☑ con `git commit` referenciando la invariante (`refactor(driver): close invariant #1 with FlushPlan typestate`).
3. Si la invariante es de tipo "Compile-time": agregar un test negativo en `crates/based-rollup/src/` que asegura `cargo build` falla cuando se viola (puede ser un `compile_fail` doctest o un `trybuild` test).
4. Si es de tipo "Test/gate": agregar el test y verificar que cubre el caso patológico real.

## Cuando todas las invariantes están en ☑

El refactor está completo. El criterio de DoD del `PLAN.md` §12 incluye: "Para cada invariante de §6, **al menos uno de los dos criterios pasa verde**". Este mapa es la auditoría operativa de ese criterio.
