# Based Rollup UI — Development Guide

## Overview

React 19 + TypeScript + Vite 6 SPA for interacting with the based rollup. Two views: **Dashboard** (default, with sub-tabs) and **Visualizer** (`#/visualizer`, with mode tabs). No routing library — uses `window.location.hash`. `#/monitor` redirects to visualizer.

## Stack

- **React 19** with hooks only (no classes)
- **Vite 6** with `@vitejs/plugin-react`
- **viem** for wallet signing (demo mode only — `privateKeyToAccount`) + event watching (Live feed)
- **zustand** for monitor/live feed state management (5 composed slices)
- **CSS Modules** (`.module.css`) for component styles
- **Global CSS** (`styles/global.css`) for variables, keyframes, resets
- No component library — everything custom

## Project Structure

```
ui/
├── vite.config.ts          # Dev server on :8080, serveSharedPlugin for /shared/rollup.env
├── src/
│   ├── main.tsx            # Entry — imports global.css, renders App
│   ├── App.tsx             # Root: view routing, hook wiring, state coordination
│   ├── config.ts           # RPC URLs, contract addresses, chain defs (from URL params + /shared/rollup.env)
│   ├── rpc.ts              # Low-level JSON-RPC fetch client
│   ├── types.ts            # Legacy shared types (LogEntry, ChainStats, WalletState)
│   ├── types/              # Typed modules for monitor/visualizer
│   │   ├── chain.ts        # Chain-related types
│   │   ├── events.ts       # Event types
│   │   ├── index.ts        # Re-exports
│   │   └── visualization.ts # Visualization types
│   ├── abi/
│   │   ├── rollups.ts              # Rollups.sol ABI
│   │   └── crossChainManagerL2.ts  # CrossChainManagerL2 ABI
│   ├── lib/
│   │   ├── crossChainEntries.ts    # Helpers for parsing cross-chain execution entries
│   │   ├── actionFormatter.ts      # Action display formatting
│   │   ├── actionHashDecoder.ts    # Action hash decoding
│   │   ├── addressBook.ts          # Known address → human name lookup
│   │   ├── autoDiscovery.ts        # Auto-discovery of contract addresses
│   │   ├── blockLogDecoder.ts      # Block log decoding
│   │   ├── bundleArchitecture.ts   # Bundle architecture building
│   │   ├── callFlowBuilder.ts      # Call flow diagram data
│   │   ├── crossChainCorrelation.ts # Union-find event correlation
│   │   ├── eventProcessor.ts       # Event processing pipeline
│   │   ├── gasEstimation.ts        # Gas estimation helpers
│   │   └── layout.ts               # Layout utilities
│   ├── store/                       # Zustand store (5 slices)
│   │   ├── index.ts                 # Store composition
│   │   ├── connectionSlice.ts       # RPC connection state
│   │   ├── eventsSlice.ts           # Event tracking
│   │   ├── executionTableSlice.ts   # Execution table state
│   │   ├── architectureSlice.ts     # Architecture diagram state
│   │   └── playbackSlice.ts         # Replay/playback state
│   ├── hooks/
│   │   ├── useConfig.ts             # Loads /shared/rollup.env, auto-detects chain IDs
│   │   ├── useWallet.ts             # MetaMask/Rabby connect + demo mode (anvil key #4)
│   │   ├── useDashboard.ts          # Polls L1/L2 block data, state roots, sync status
│   │   ├── useHealth.ts             # Polls /health endpoints for builder + fullnodes
│   │   ├── useCounter.ts            # Deploy/increment counter contract, tx lifecycle
│   │   ├── useCrossChain.ts         # Proxy creation + cross-chain call state machine
│   │   ├── useBridge.ts             # Bridge/deposit functionality
│   │   ├── useExecutionVisualizer.ts # Execution visualizer state synced with cross-chain phases
│   │   ├── useTxHistory.ts          # Transaction history tracking
│   │   ├── useLog.ts                # Event log entries
│   │   ├── useTheme.ts              # Light/dark theme toggling
│   │   ├── useBlockscoutAbi.ts      # Fetches ABIs from Blockscout explorer API
│   │   ├── useRecentAddresses.ts    # Recent address tracking
│   │   ├── useChainWatcher.ts       # viem event watching for monitor
│   │   ├── useEventStream.ts        # Event stream processing
│   │   ├── useDerivedState.ts       # Derived state computations
│   │   ├── useTxIntrospection.ts    # Transaction introspection
│   │   └── useAutoDiscovery.ts      # Auto-discovery of contracts
│   ├── components/
│   │   ├── Header.tsx + .module.css           # Nav bar, wallet connect, view switching
│   │   ├── NetworkStrip.tsx + .module.css      # L1/L2 chain switcher strip
│   │   ├── NodeHealth.tsx + .module.css        # Health status for builder + fullnodes
│   │   ├── ChainCard.tsx + .module.css         # L1/L2 block info cards
│   │   ├── CounterPanel.tsx + .module.css      # Counter deploy/increment demo
│   │   ├── CrossChainPanel.tsx + .module.css   # Cross-chain proxy + call UI
│   │   ├── CrossChainCallBuilder.tsx + .module.css  # Cross-chain call construction
│   │   ├── ProxyDeploySection.tsx + .module.css     # Proxy deployment section
│   │   ├── AbiMethodSelector.tsx + .module.css      # ABI method picker (Blockscout integration)
│   │   ├── GasLimitEditor.tsx + .module.css         # Gas limit editing UI
│   │   ├── BridgePanel.tsx + .module.css       # L1↔L2 bridge/deposit UI
│   │   ├── VisualizerView.tsx + .module.css    # Tab container: Block Explorer / Live / Debug TX
│   │   ├── TxHistoryPanel.tsx + .module.css    # Transaction history list
│   │   ├── EventLog.tsx + .module.css          # Log output panel
│   │   ├── ExplorerLink.tsx + .module.css      # Blockscout link with address book resolution
│   │   └── TxLink.tsx                          # Transaction hash link wrapper
│   ├── components/visualizer/                   # Visualizer sub-components
│   │   ├── BlockExplorer.tsx + .module.css      # Block explorer with navigation
│   │   ├── BlockFlowDiagram.tsx + .module.css   # Block flow visualization
│   │   ├── DebugTxMode.tsx                      # Debug TX mode (postBatch decoder, ~40KB)
│   │   ├── ExecutionFlow.tsx                    # Execution flow view
│   │   ├── L2BlockCard.tsx                      # L2 block detail card
│   │   └── TxCard.tsx                           # Transaction card
│   ├── components/monitor/                      # Monitor/Live feed sub-components
│   │   ├── LiveFeed.tsx + .module.css           # Real-time cross-chain block monitor
│   │   ├── MonitorView.tsx + .module.css        # Standalone monitor (legacy entry point)
│   │   ├── ConnectionBar.tsx + .module.css      # RPC connection config bar
│   │   ├── EventTimeline.tsx + .module.css      # Event timeline with replay
│   │   ├── EventCard.tsx + .module.css          # Single event display
│   │   ├── EventInfoBanner.tsx + .module.css    # Event info banner
│   │   ├── BundleList.tsx + .module.css         # Correlated event bundles
│   │   ├── BundleDetail.tsx + .module.css       # Full-screen bundle modal
│   │   ├── ArchitectureDiagram.tsx + .module.css # Architecture diagram
│   │   ├── CallFlowStrip.tsx + .module.css      # Call flow strip
│   │   ├── ExecutionTables.tsx + .module.css    # Execution tables view
│   │   ├── TablePanel.tsx + .module.css         # Table panel
│   │   ├── TableEntryRow.tsx + .module.css      # Table entry row
│   │   ├── ContractState.tsx + .module.css      # Contract state display
│   │   └── TxDetails.tsx + .module.css          # Transaction details
│   └── styles/
│       └── global.css      # CSS variables (dark + light themes), keyframes, resets
```

## Configuration

All config is in `config.ts`. Values come from URL params (highest priority) or auto-detection:

| Source | Variables |
|--------|-----------|
| URL params | `?l1=`, `?l2=`, `?l1proxy=`, `?rollups=`, `?rollupId=`, `?l1explorer=`, `?l2explorer=`, `?l2explorerapi=`, `?l1bridge=`, `?l2bridge=` |
| `/shared/rollup.env` | `ROLLUPS_ADDRESS`, `ROLLUP_ID`, `BRIDGE_L1_ADDRESS` (loaded by `useConfig.ts` on mount) |
| Defaults | L1=`:9555`, L2=`:9545`, L1Proxy=`:9556`, Explorer L1=`:4000`, L2=`:4001`, L2 API=`:4003` |

Runtime updates via `setConfig(updates)` — used by `useConfig.ts` after loading env files.

Chain IDs are auto-detected via `eth_chainId` calls on mount.

## Wallet Modes

Two modes, seamless:

1. **Wallet connected** (MetaMask/Rabby): uses `eth_sendTransaction` through wallet, auto-switches chains
2. **Demo mode** (no wallet): signs locally with anvil dev key #4 (`0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65`), sends via `eth_sendRawTransaction` (L2) or `eth_sendTransaction` (L1 dev node)

The `useWallet` hook exposes three send functions:
- `sendTx(params)` — sends to L2
- `sendL1Tx(params)` — sends to L1 directly
- `sendL1ProxyTx(params)` — sends to L1 via L1 proxy (port 9556) — **required for cross-chain calls**

## Cross-Chain Call Flow

The cross-chain state machine in `useCrossChain.ts` has these phases:

```
idle → creating-proxy → proxy-pending → [confirmed → idle]
                                          (proxy exists)

idle → sending → l1-pending → confirmed → idle
                             → failed
```

1. **Create Proxy**: calls `Rollups.createCrossChainProxy(address, rollupId)` on L1
2. **Send Call**: sends tx to proxy address via L1 proxy (port 9556)
   - L1 proxy traces the tx, detects `executeCrossChainCall`, populates L2 execution table
   - Then forwards tx to L1
   - Builder picks up execution entry and includes in next L2 block atomically

## CSS Architecture

- **CSS Modules**: each component has `.module.css` — class names are locally scoped
- **Global CSS** (`styles/global.css`): defines CSS custom properties and `@keyframes`
- **IMPORTANT**: `@keyframes` are NOT scoped by CSS modules — they stay global
- Spinners use `animation: spin 0.8s linear infinite` referencing the global `spin` keyframe
- The `.spinner` class is defined per-component in CSS modules (same pattern in CounterPanel and CrossChainPanel)

### Theming

Supports dark (default) and light themes via `data-theme` attribute on `:root`, toggled by `useTheme` hook.

### CSS Variables (defined in `:root`)

```
--bg, --bg-raised, --bg-card, --bg-card-hover, --bg-inset  # Background layers
--header-bg                                    # Header backdrop
--border, --border-bright, --border-hover      # Border colors
--text, --text-dim, --text-bright              # Text hierarchy
--accent, --accent-light, --accent-dim         # Indigo accent
--accent-glow, --accent-glow-strong            # Accent glow effects
--accent-shadow, --accent-shadow-hover         # Accent shadow effects
--green, --green-dim, --green-border           # Success state
--red, --red-dim, --red-border                 # Error state
--yellow, --yellow-dim, --yellow-border        # Warning state
--cyan, --cyan-dim, --cyan-border              # Info state
--mono, --sans                                 # Font stacks (IBM Plex Mono, Inter)
--sp-1 through --sp-8                          # Spacing scale (4px base)
--radius, --radius-sm, --radius-xs             # Border radii
--ease, --duration, --duration-slow            # Transitions
--shadow-card, --shadow-card-hover, --shadow-inset  # Shadows
--overlay-subtle, --overlay-light              # Hover overlays
--on-color                                     # Text on colored buttons
--spinner-track                                # Spinner border color
```

### Keyframes (global)

`fadeSlideUp`, `slideDown`, `fadeIn`, `spin`, `pop`, `pulseGlow`, `entryAdd`, `entryConsume`, `livePulse`, `shimmer`

## Key Components

### Dashboard View

The dashboard has sub-tabs (`DashboardTab`): **Dashboard** (default), **Counter Demo**, **Bridge**.

### CounterPanel
- 3-step flow: Deploy → Increment → Interact
- `StepIndicator` component shows progress
- `TxLifecycle` shows sending → pending → confirmed states with spinner
- Auto-refreshes count every 3s via polling

### CrossChainPanel / CrossChainCallBuilder / ProxyDeploySection
- `CrossChainPanel` is the top-level cross-chain UI container
- `ProxyDeploySection` handles proxy creation with `Rollups.createCrossChainProxy(address, rollupId)`
- `CrossChainCallBuilder` constructs cross-chain calls with ABI method selection
- `AbiMethodSelector` fetches ABIs from Blockscout (`useBlockscoutAbi`) for method picker UI
- `GasLimitEditor` allows manual gas limit override
- Saved proxies persisted in localStorage

### BridgePanel
- L1↔L2 bridge/deposit UI
- Uses `useBridge` hook for bridge transaction lifecycle

### VisualizerView (tab container)
- Thin container (~107 lines) with 3 mode tabs: **Block Explorer**, **Live**, **Debug TX**
- Can be opened from TxHistoryPanel "Debug" button (opens in Debug TX mode)
- Hash param support: `#/visualizer?block=123` opens Block Explorer at specific block

#### Block Explorer (`components/visualizer/BlockExplorer`)
- Navigate L1/L2 blocks, view transactions, execution entries
- `BlockFlowDiagram` visualizes block relationships
- `L2BlockCard` shows L2 block details
- `TxCard` shows transaction details with decoding

#### Live Feed (`components/monitor/LiveFeed`)
- Real-time cross-chain block monitor
- Can navigate to Block Explorer for a specific L1 block
- Uses the Zustand monitor store for event tracking

#### Debug TX Mode (`components/visualizer/DebugTxMode`)
- ~40KB, the largest single component
- Debugs cross-chain execution entries from L1 postBatch transactions
- Live mode syncs with cross-chain panel phases
- Parses BatchPosted events, decodes execution entries, shows state deltas
- Uses `detectProxy()` to check if an address is a CrossChainProxy via `authorizedProxies(address)` on Rollups.sol

### Monitor Infrastructure (Zustand + event pipeline)
- **State**: Zustand store (`src/store/`) with 5 slices: connection, events, executionTable, architecture, playback
- **Event flow**: viem `watchContractEvent` → `eventProcessor` → `crossChainCorrelation` → store
- **Bundle correlation**: Union-find algorithm groups events by shared actionHash/txHash
- **Components** (`src/components/monitor/`): LiveFeed, MonitorView, ConnectionBar, EventTimeline, EventCard, BundleList, BundleDetail, ArchitectureDiagram, CallFlowStrip, ExecutionTables, TablePanel, TableEntryRow, ContractState, EventInfoBanner, TxDetails
- **Hooks**: useChainWatcher, useEventStream, useDerivedState, useTxIntrospection, useAutoDiscovery
- **Lib**: actionFormatter, actionHashDecoder, addressBook, autoDiscovery, blockLogDecoder, bundleArchitecture, callFlowBuilder, crossChainCorrelation, eventProcessor, gasEstimation, layout
- **Replay mode**: Click events in timeline to time-travel through state; arrow keys navigate
- **Bundle detail**: Full-screen modal with step-by-step architecture diagram, execution tables, contract state

### ExplorerLink / TxLink
- Wraps addresses/tx hashes as clickable Blockscout links
- Resolves known addresses to human-readable names via `addressBook.ts`
- Falls back to copy-on-click when explorer URL not configured
- `ExplorerLink` has its own `.module.css` for link/copyable styling

## Docker Integration

- Vite serves on port 8080 (not exposed in docker-compose yet — run locally with `npm run dev`)
- `serveSharedPlugin` in vite.config.ts serves `/shared/*` from disk (Docker volume mount)
- `rollup.env` written by `deploy.sh` contains `ROLLUPS_ADDRESS` and `ROLLUP_ID`

## Known Issues / Gotchas

1. **Explorer links require Blockscout**: Explorer URLs default to `:4000`/`:4001` but are dead without the `--profile explorer`. ExplorerLink degrades to copy-on-click when the explorer doesn't respond.

2. **`detectProxy()` in DebugTxMode**: Uses `authorizedProxies(address)` on Rollups.sol (selector `0x360d95b6`). Earlier versions incorrectly used `ORIGINAL_ADDRESS()` / `ORIGINAL_ROLLUP_ID()` which are internal immutables with no public getter.

3. **Proxy cache invalidation**: `useCrossChain` prunes cached proxies on mount by checking `eth_getCode`. After a chain wipe, stale proxies are auto-removed.

4. **Counter cache invalidation**: `useCounter` checks `eth_getCode` on mount. If the chain was wiped, the cached counter address is auto-cleared.

5. **Demo sender nonce conflicts**: The demo account (`0x15d34...`) is shared. If multiple tabs or the tx-sender script use the same account, nonce conflicts can occur.

6. **L1 proxy is required for cross-chain**: Must use `sendL1ProxyTx` (port 9556) for cross-chain calls. Direct L1 sends skip the trace → execution table is empty → tx reverts.

## Development

```bash
cd ui
npm install
npm run dev      # Starts on http://localhost:8080
npm run build    # Outputs to ui/dist/
```

Requires the rollup stack running (L1 + builder + fullnodes) for any functionality.
