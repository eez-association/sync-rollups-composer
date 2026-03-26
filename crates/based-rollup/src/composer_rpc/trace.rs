//! Generic trace-based cross-chain call detection.
//!
//! Walks a `callTracer` trace tree to find cross-chain proxy calls using
//! protocol-level detection only (ICrossChainManager interface).
//!
//! Two detection mechanisms:
//! 1. Persistent proxies: `authorizedProxies(address)` query on the manager
//! 2. Ephemeral proxies: `createCrossChainProxy(address,uint256)` calls in the trace
