# Plan: Generic Entry Filtering and Intermediate Root Computation

## Objetivo

Eliminar toda lógica específica de deposits/withdrawals/continuations del filtrado §4f y la computación de intermediate state roots. El protocolo es agnóstico al tipo de entry — el builder también debe serlo.

## Modelo del protocolo

Un bloque L2 con entries tiene la estructura:

```
[setContext, loadTable(entries), trigger_0, trigger_1, ..., trigger_T-1, user_txs...]
```

- `loadTable` carga TODAS las entries en el CCM. Se ejecuta SIEMPRE que haya entries.
- Cada trigger es un tx que consume ≥1 entry (produce `ExecutionConsumed` event en el CCM L2).
- Un trigger puede ser un protocol tx (`executeIncomingCrossChainCall`) o un user tx (via proxy).
- No nos importa cuál es cuál.

**Root chain:**
```
R(0) = root con loadTable + 0 triggers
R(k) = root con loadTable + primeros k triggers
R(T) = speculative = root con loadTable + todos los triggers
```

**IMMEDIATE entry:** `newState = R(0)` (con loadTable, sin triggers)

**DEFERRED entry i (del grupo trigger k):** primer entry del grupo: `currentState = R(k), newState = R(k+1)`. Entries subsiguientes del mismo grupo: `currentState = R(k+1), newState = R(k+1)`.

**§4f filtering:** consumed M triggers en L1 → mantener primeros M trigger txs en L2, eliminar el resto. `loadTable` se mantiene siempre.

**Consumo parcial:** on-chain = R(M). Re-derivación produce bloque con loadTable + M triggers → root = R(M). Match.

## Cambios por archivo

### 1. `cross_chain.rs` — Nuevas funciones genéricas

#### 1a. `identify_trigger_tx_indices`
```rust
/// Escanea receipts del bloque L2 buscando txs que producen
/// ExecutionConsumed events del CCM. Devuelve los índices de tx
/// (deduplicated, en orden) que son triggers.
pub fn identify_trigger_tx_indices(
    receipts: &[Receipt],
    ccm_address: Address,
) -> Vec<usize>
```
- Escanea todos los receipts por `ExecutionConsumed` event (topic0 = signature hash)
- Para cada log que matchea: añade el tx index al set
- Devuelve Vec ordenado de tx indices únicos
- **Genérico**: no usa selectores de función, solo el event signature

#### 1b. `filter_block_by_trigger_prefix`
```rust
/// Filtra un bloque manteniendo los primeros `keep_count` trigger txs
/// y eliminando el resto. Mantiene TODOS los non-trigger txs.
pub fn filter_block_by_trigger_prefix(
    encoded_transactions: &Bytes,
    trigger_tx_indices: &[usize],
    keep_count: usize,
) -> Result<Bytes>
```
- Decodifica txs
- Para cada tx: si su índice está en `trigger_tx_indices[keep_count..]`, la elimina
- Mantiene todas las demás txs (loadTable, setContext, user txs no-trigger)
- **Reemplaza**: `filter_block_entries` y `filter_unconsumed_execute_remote_calls`

#### 1c. `compute_consumed_trigger_prefix`
```rust
/// Determina cuántos trigger txs del bloque L2 fueron consumidos en L1.
/// Usa el consumed_map de L1 (ExecutionConsumed events) y los receipts L2
/// del trial-execution del bloque completo.
///
/// Recorre trigger txs en orden. Para cada uno, extrae los actionHashes
/// de sus ExecutionConsumed events en L2. Verifica que TODOS esos hashes
/// están en el consumed_map L1. Si alguno falta → STOP (prefix counting).
pub fn compute_consumed_trigger_prefix(
    l2_receipts: &[Receipt],
    ccm_address: Address,
    l1_consumed_remaining: &mut HashMap<B256, usize>,
    trigger_tx_indices: &[usize],
) -> usize
```
- Para cada trigger tx (en orden por índice):
  - Extrae actionHashes de sus ExecutionConsumed events
  - Verifica que cada hash tiene remaining > 0 en el L1 map
  - Si todos OK: decrementa remaining, continúa
  - Si alguno falla: STOP, devuelve count hasta ahora
- **Reemplaza**: el walk de entries en derivation.rs:569-625 con `is_withdrawal_entry`

#### 1d. `attach_generic_state_deltas`
```rust
/// Asigna state deltas a L1 deferred entries usando la cadena de roots.
///
/// `group_starts[k]` = índice del primer entry del trigger group k.
/// `roots` tiene T+1 valores (T = número de trigger groups).
///
/// Group k (entries desde group_starts[k] hasta group_starts[k+1]):
///   - Primer entry: StateDelta(roots[k], roots[k+1], preservar ether_delta)
///   - Resto entries: StateDelta(roots[k+1], roots[k+1], preservar ether_delta)
pub fn attach_generic_state_deltas(
    entries: &mut [CrossChainExecutionEntry],
    roots: &[B256],
    rollup_id: u64,
    group_starts: &[usize],
)
```
- **Reemplaza**: `attach_unified_chained_state_deltas` y el bloque clean/speculative del driver

### 2. `driver.rs` — Cola unificada y roots genéricos

#### 2a. Reemplazar las tres colas por una

**Eliminar:**
```rust
pending_cross_chain_entries: Vec<CrossChainExecutionEntry>,
pending_continuation_l1_entries: Vec<CrossChainExecutionEntry>,
pending_withdrawal_l1_entries: Vec<CrossChainExecutionEntry>,
pending_withdrawal_metadata: Vec<WithdrawalMetadata>,
```

**Añadir:**
```rust
/// Todas las L1 deferred entries pendientes, en orden de submission.
pending_l1_entries: Vec<CrossChainExecutionEntry>,
/// Índice del primer entry de cada trigger group.
pending_l1_group_starts: Vec<usize>,
/// Metadata para L1 trigger txs (executeL2TX). Solo para groups que
/// necesitan trigger en L1. Entries indexadas por group.
pending_l1_trigger_metadata: Vec<Option<TriggerMetadata>>,
```

Donde `TriggerMetadata` es el rename genérico de `WithdrawalMetadata`:
```rust
pub struct TriggerMetadata {
    pub user: Address,
    pub amount: U256,
    pub rlp_encoded_tx: Vec<u8>,
    pub trigger_count: usize,
}
```

#### 2b. Encolar genéricamente

En el drain de `QueuedCrossChainCall`:
```rust
let group_start = self.pending_l1_entries.len();
// Si tiene l1_entries (continuation): usar directamente
// Si no: convert pairs to L1 format
self.pending_l1_entries.extend(l1_entries);
self.pending_l1_group_starts.push(group_start);
self.pending_l1_trigger_metadata.push(None); // protocol trigger, no metadata
```

En el drain de `QueuedWithdrawal`:
```rust
let group_start = self.pending_l1_entries.len();
self.pending_l1_entries.extend(w.l1_deferred_entries);
self.pending_l1_group_starts.push(group_start);
self.pending_l1_trigger_metadata.push(Some(TriggerMetadata {
    user: w.user,
    amount: w.amount,
    rlp_encoded_tx: w.rlp_encoded_tx,
    trigger_count: w.trigger_count,
}));
```

#### 2c. `compute_intermediate_roots` genérico

**Reemplaza** `compute_unified_intermediate_roots`.

```rust
fn compute_intermediate_roots(
    &self,
    parent_block_number: u64,
    timestamp: u64,
    l1_block_hash: B256,
    l1_block_number: u64,
    speculative_root: B256,
    block_encoded_txs: &Bytes,
) -> Result<Vec<B256>>
```

Implementación:
1. Trial-execute full block → obtener receipts
2. `trigger_indices = identify_trigger_tx_indices(&receipts, ccm_address)`
3. Si `trigger_indices.is_empty()` → return `vec![speculative_root]`
4. Para k = 0..trigger_indices.len():
   - `filtered = filter_block_by_trigger_prefix(txs, &trigger_indices, k)`
   - `root = compute_state_root_with_entries(parent, timestamp, ..., &filtered)`
   - `roots.push(root)`
5. Return roots (T+1 valores, roots[0]=R(0), roots[T]=speculative)

**Elimina**: `num_deposits`, `num_withdrawals` como parámetros.

#### 2d. State delta attachment genérico

En `step_builder`, reemplazar los tres bloques de attachment (deposit, continuation, withdrawal) por:

```rust
if !self.pending_l1_entries.is_empty() {
    attach_generic_state_deltas(
        &mut self.pending_l1_entries,
        &roots,
        self.config.rollup_id,
        &self.pending_l1_group_starts,
    );
}
```

#### 2e. `build_builder_protocol_txs` con max_triggers

Cambiar firma:
```rust
fn build_builder_protocol_txs(
    &mut self,
    l2_block_number: u64,
    timestamp: u64,
    l1_block_hash: B256,
    l1_block_number: u64,
    execution_entries: &[CrossChainExecutionEntry],
    max_trigger_count: usize,  // NUEVO: cuántos triggers generar
) -> Result<Bytes>
```

Implementación:
- `partition_entries` → (table_entries, trigger_entries) (sin cambios)
- SIEMPRE generar `loadTable` si `table_entries` no está vacío
- Generar solo los primeros `min(max_trigger_count, trigger_entries.len())` trigger txs
- En builder mode: `max_trigger_count = usize::MAX` (todos)
- En derivación con §4f: `max_trigger_count = consumed_trigger_count`

#### 2f. §4f filtering genérico

**Reemplaza** `apply_section_4f_filtering`:

```rust
fn apply_generic_filtering(
    &self,
    block: &mut DerivedBlock,
    l1_consumed_remaining: &mut HashMap<B256, usize>,
) -> Result<Bytes>
```

Implementación:
1. Si `block.filtering.is_none()` → devolver txs sin filtrar
2. Construir full block con TODAS las entries + todos los triggers
   (llamar `build_builder_protocol_txs` con `max_trigger_count = usize::MAX`)
3. Trial-execute → obtener receipts
4. `trigger_indices = identify_trigger_tx_indices(&receipts, ccm)`
5. `consumed_count = compute_consumed_trigger_prefix(receipts, ccm, l1_consumed_remaining, &trigger_indices)`
6. Reconstruir block con `build_builder_protocol_txs(entries, consumed_count)`
7. Devolver encoded txs

### 3. `derivation.rs` — Simplificación

#### 3a. Simplificar `DeferredFiltering`

```rust
pub struct DeferredFiltering {
    /// true si hay entries no consumidas que requieren filtrado
    pub needs_filtering: bool,
}
```

El conteo de consumed triggers se mueve al driver (paso 2f). La derivación solo indica si hay entries no consumidas.

#### 3b. Eliminar el walk de entries con is_withdrawal_entry

El bloque en derivation.rs:569-625 se simplifica a:
```rust
let has_unconsumed = deferred.iter().any(|e| {
    let count = remaining.get(&e.action_hash).copied().unwrap_or(0);
    count == 0
});
if has_unconsumed {
    Some(DeferredFiltering { needs_filtering: true })
} else {
    None
}
```

(La cuenta exacta de consumed triggers se hace en el driver con trial-execution.)

### 4. `proposer.rs` — Sin cambios sustanciales

- `PendingBlock.intermediate_roots` sigue existiendo (ahora tiene T+1 roots)
- `send_to_l1` usa `pending_l1_entries` en vez de las tres colas separadas
- `clean_state_root = roots[0]` (sin cambios conceptuales)

### 5. `rpc.rs` — Sin cambios

- `QueuedCrossChainCall` y `QueuedWithdrawal` siguen existiendo como tipos RPC
- El driver los drena y los convierte a la cola unificada

## Código a eliminar

| Función/Variable | Archivo | Razón |
|---|---|---|
| `filter_block_entries()` | cross_chain.rs:2033 | Reemplazada por `filter_block_by_trigger_prefix` |
| `filter_unconsumed_execute_remote_calls()` | cross_chain.rs:1929 | Legacy, ya no se usa |
| `attach_unified_chained_state_deltas()` | cross_chain.rs:2096 | Reemplazada por `attach_generic_state_deltas` |
| `is_withdrawal_entry()` | cross_chain.rs:857 | Reemplazada por event-based detection |
| `is_ccm_execute_remote_call()` | cross_chain.rs:1888 | Solo usada en filter/roots (eliminados) |
| `extract_l2_to_l1_tx_indices()` | cross_chain.rs:1998 | Reemplazada por `identify_trigger_tx_indices` |
| `compute_unified_intermediate_roots()` | driver.rs:3675 | Reemplazada por `compute_intermediate_roots` |
| `extract_l2_to_l1_tx_indices_via_receipts()` | driver.rs:3577 | Reemplazada por trial-execution genérica |
| `pending_cross_chain_entries` | driver.rs:123 | Reemplazada por `pending_l1_entries` |
| `pending_continuation_l1_entries` | driver.rs:136 | Reemplazada por `pending_l1_entries` |
| `pending_withdrawal_l1_entries` | driver.rs:139 | Reemplazada por `pending_l1_entries` |
| `pending_withdrawal_metadata` | driver.rs:141 | Reemplazada por `pending_l1_trigger_metadata` |
| `num_deposits` / `num_withdrawals` variables | driver.rs | Eliminadas |
| `consumed_deposit_count` / `unconsumed_deposit_count` | derivation.rs | Eliminadas |
| `unconsumed_withdrawal_pair_count` | derivation.rs | Eliminada |
| Bloque clean/speculative (1648-1708) | driver.rs | Reemplazado por `attach_generic_state_deltas` |
| `DeferredFiltering.consumed_deposit_count` etc. | derivation.rs | Simplificado a `needs_filtering: bool` |

## Orden de implementación

```
Paso 1: cross_chain.rs — nuevas funciones genéricas (1a, 1b, 1c, 1d)
         Compila, tests unitarios para las nuevas funciones
         NO eliminar funciones viejas aún

Paso 2: driver.rs — compute_intermediate_roots genérico (2c)
         Usa las nuevas funciones de paso 1
         Coexiste con compute_unified_intermediate_roots (old)
         Cambiar step_builder para usar la nueva

Paso 3: driver.rs — cola unificada (2a, 2b)
         Reemplazar las tres colas
         Actualizar drain, flush_to_l1, y attachment (2d)

Paso 4: driver.rs — build_builder_protocol_txs con max_triggers (2e)
         Derivación usa max_triggers para §4f

Paso 5: driver.rs + derivation.rs — §4f genérico (2f, 3a, 3b)
         DeferredFiltering simplificado
         apply_generic_filtering con trial-execution

Paso 6: Eliminar código muerto
         Todas las funciones/variables listadas arriba

Paso 7: Tests
         Actualizar tests existentes
         Añadir tests para nuevas funciones genéricas
```

Cada paso compila y pasa `cargo nextest run --workspace` antes de avanzar al siguiente.

## Invariantes a verificar en cada paso

1. `roots[0]` = R(0) = root con loadTable, sin triggers
2. `roots[T]` = speculative = root con loadTable + todos los triggers
3. IMMEDIATE entry `newState = roots[0]`
4. DEFERRED entries tienen deltas encadenados correctos
5. Re-derivación con consumed=0 produce root = R(0) (con loadTable)
6. Re-derivación con consumed=M produce root = R(M)
7. Fullnodes convergen con el builder
8. No hay variables/funciones que referencien "deposit" o "withdrawal" como tipo
