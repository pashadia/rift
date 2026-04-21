# Hash-Based Merkle Tree Storage — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Each TDD cycle dispatches 3 subagents: Red (smart model), Green (basic model), Refactor (smart model). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace level-based Merkle drill with hash-based lookup, enabling O(1) delta sync queries from any tree node.

**Architecture:** 64-ary Merkle tree stored in SQLite with two new tables (`merkle_tree_nodes`, `merkle_leaf_info`). Client sends a hash (empty = root), server returns that node's children. All existing `MerkleTree::build()` preserved, extended with `build_with_cache()` returning parent→children map.

**Tech Stack:** Rust, BLAKE3, bincode (serialization), SQLite (tokio-rusqlite), protobuf (prost), quinn (QUIC)

**Beads Epic:** rift-g5k

**Design Spec:** `docs/superpowers/specs/2026-04-20-hash-based-merkle-tree-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/rift-common/Cargo.toml` | Modify | Add `bincode` dependency |
| `crates/rift-common/src/crypto.rs` | Modify | Add `LeafInfo`, `MerkleChild` enum, `build_with_cache()` |
| `crates/rift-server/src/metadata/db.rs` | Modify | Add `merkle_tree_nodes` + `merkle_leaf_info` table DDL |
| `crates/rift-server/src/metadata/merkle.rs` | Modify | Add `put_tree()`, `get_children()`, `get_leaf_info()` |
| `proto/transfer.proto` | Modify | Replace `MerkleDrill` with hash-based version, add `MerkleDrillResponse`, `MerkleChildProto` |
| `crates/rift-protocol/src/messages.rs` | Modify | Update type IDs, add tests for new messages |
| `crates/rift-server/src/handler.rs` | Modify | Rewrite `merkle_drill_response` to hash-based lookup |
| `crates/rift-client/src/client.rs` | Modify | Update `merkle_drill()` signature → hash-based |
| `crates/rift-client/src/remote.rs` | Modify | Update `RemoteShare` trait `merkle_drill` signature |

---

## Subagent Model Selection

### Available Models

| Model | Rate (per 5h) | Quality | Role |
|-------|---------------|---------|------|
| GLM-5.1 | 880 | Smart (5/5) | Design, edge cases, quality review |
| Kimi K2.5 | 1,850 | Smart (5/5) | Design, review, architecture |
| MiMo-V2-Pro | 1,290 | Smart (4/5) | Review, edge cases |
| Qwen3.6 Plus | 3,300 | Standard (4/5) | Implementation, spec review |
| MiniMax M2.7 | 3,400 | Standard (3/5) | Implementation |
| MiMo-V2-Omni | 2,150 | Standard (3/5) | Implementation |
| MiniMax M2.5 | 6,300 | Basic (2/5) | Mechanical Green |
| Qwen3.5 Plus | 10,200 | Basic (2/5) | Mechanical Green, spec review |

Dispatch format: `opencode-go/MODEL` (e.g. `opencode-go/kimi-k2.5`)

### TDD Phase Assignments

| Phase | Model | Rationale |
|-------|-------|-----------|
| **Red** (write failing test) | `kimi-k2.5` primary, `glm-5.1` fallback | Tests define API contract + invariants. Must think hard about edge cases. |
| **Green** (make test pass) | `qwen3.5-plus` primary, `minimax-m2.5` fallback | Mechanical: test says what to build. High rate limits. |
| **Refactor** | `glm-5.1` primary, `mimo-v2-pro` fallback | Judgment: identify duplication, improve names, restructure. |

### Review Assignments

| Review | Model | Rationale |
|--------|-------|-----------|
| **Spec compliance** | `qwen3.5-plus` | Mechanical line-by-line comparison. High rate limit. |
| **Code quality** | `kimi-k2.5` primary, `mimo-v2-pro` fallback | Requires taste, pattern recognition. |

### Rate Limit Budget

~52 agent dispatches across 6 waves. Conservative: 8 tasks x 5 dispatches = 40 + integration tests.

Per model usage (conservative):
- `kimi-k2.5`: ~8 Red + 8 quality reviews = 16 (within 1,850/5h)
- `qwen3.5-plus`: ~8 Green + 8 spec reviews = 16 (within 10,200/5h)
- `glm-5.1`: ~8 Refactor = 8 (within 880/5h)
- `minimax-m2.5`: 0-8 Green fallback = less than 8 (within 6,300/5h)

No rate limit concerns.

### Fallback Plan

| Situation | Action |
|-----------|--------|
| Smart model rate-limited | `kimi-k2.5`, then `glm-5.1`, then `mimo-v2-pro`, then `qwen3.6-plus` |
| Basic model rate-limited | `qwen3.5-plus`, then `minimax-m2.5`, then `minimax-m2.7` |
| Model returns wrong code | Re-dispatch + error output + hint. Still wrong: escalate tier. |
| Green agent overbuilds | Spec reviewer catches. Re-dispatch: "Only make failing test pass. No extras." |
| Red agent writes bad test | I review before dispatching Green. Rewrite if unclear. |
| Refactor breaks tests | Revert to Green commit. Re-dispatch: "Don't change behavior." |
| Subagent reports BLOCKED | Context problem: more info (same tier). Design problem: escalate tier. Too large: split. |
| All models unavailable | I execute that phase myself as coordinator. |

## Dependency Graph

```
Task 1: MerkleChild enum ──────────────────┐
Task 2: Protocol update ───────────────────┤
                                           │
            ┌──────────────────────────────┘
Task 3: Tree construction ─────────┐
                                    │
            ┌───────────────────────┤
Task 4: DB merkle_tree_nodes ───────┤
Task 5: DB merkle_leaf_info ────────┤
            ┌───────────────────────┤
Task 6: DB methods (put_tree etc) ──┤
                                    │
            ┌───────────────────────┤
Task 7: Server handler rewrite ←────┤ (needs Task 2 + Task 6)
            ┌───────────────────────┤
Task 8: Integration tests ←─────────┘ (needs Task 7)

Wave 1: Tasks 1 + 2 (parallel, independent)
Wave 2: Task 3 (after Task 1)
Wave 3: Tasks 4 + 5 (parallel, after Task 3, independent of each other)
Wave 4: Task 6 (after Tasks 4 + 5)
Wave 5: Task 7 (after Tasks 2 + 6)
Wave 6: Task 8 (after Task 7)
```

---

## Task 1: MerkleChild Enum

**Beads:** rift-9vw  
**Files:** `crates/rift-common/Cargo.toml`, `crates/rift-common/src/crypto.rs`  
**Depends on:** Nothing

### Subagent 1A: Red (`kimi-k2.5`)

**Prompt:**

> You are implementing Task 1, Phase RED: Write failing tests for the `MerkleChild` enum.
>
> **Working directory:** `/home/bogdan/rift/.worktrees/delta-sync`
>
> **Context:** We're adding a `MerkleChild` enum to `crates/rift-common/src/crypto.rs` that represents nodes in a 64-ary Merkle tree. Each child is either a `Subtree(Blake3Hash)` (intermediate node) or a `Leaf { hash: Blake3Hash, length: u64, chunk_index: u32 }` (actual data chunk). We also need a `LeafInfo` struct with `{ hash: Blake3Hash, offset: u64, length: u64, chunk_index: u32 }` for DB storage of chunk metadata.
>
> The `Blake3Hash` type already exists in `crypto.rs` with `Serialize`/`Deserialize` support (via serde). We'll use `bincode` for serialization of `MerkleChild` into SQLite BLOBs.
>
> **Your job:**
> 1. Add `bincode = "1"` to `[dependencies]` in `crates/rift-common/Cargo.toml`
> 2. Add `use std::collections::HashMap;` to crypto.rs imports
> 3. Write these failing tests in the `#[cfg(test)]` module of `crates/rift-common/src/crypto.rs`:
>
> ```rust
> mod merkle_child_tests {
>     use super::*;
>     
>     #[test]
>     fn merkle_child_subtree_roundtrip() {
>         let hash = Blake3Hash::new(b"subtree data");
>         let child = MerkleChild::Subtree(hash.clone());
>         let encoded = bincode::serialize(&child).unwrap();
>         let decoded: MerkleChild = bincode::deserialize(&encoded).unwrap();
>         assert_eq!(decoded, child);
>     }
>     
>     #[test]
>     fn merkle_child_leaf_roundtrip() {
>         let hash = Blake3Hash::new(b"leaf data");
>         let child = MerkleChild::Leaf {
>             hash: hash.clone(),
>             length: 65536,
>             chunk_index: 42,
>         };
>         let encoded = bincode::serialize(&child).unwrap();
>         let decoded: MerkleChild = bincode::deserialize(&encoded).unwrap();
>         assert_eq!(decoded, child);
>     }
>     
>     #[test]
>     fn merkle_child_deterministic_serialization() {
>         let hash = Blake3Hash::new(b"deterministic");
>         let child1 = MerkleChild::Subtree(hash.clone());
>         let child2 = MerkleChild::Subtree(hash.clone());
>         let enc1 = bincode::serialize(&child1).unwrap();
>         let enc2 = bincode::serialize(&child2).unwrap();
>         assert_eq!(enc1, enc2);
>     }
>     
>     #[test]
>     fn merkle_child_leaf_preserves_all_fields() {
>         let hash = Blake3Hash::new(b"chunk");
>         let child = MerkleChild::Leaf {
>             hash: hash.clone(),
>             length: 131072,
>             chunk_index: 7,
>         };
>         let encoded = bincode::serialize(&child).unwrap();
>         let decoded: MerkleChild = bincode::deserialize(&encoded).unwrap();
>         match decoded {
>             MerkleChild::Leaf { hash: h, length, chunk_index } => {
>                 assert_eq!(h, hash);
>                 assert_eq!(length, 131072);
>                 assert_eq!(chunk_index, 7);
>             }
>             MerkleChild::Subtree(_) => panic!("expected Leaf variant"),
>         }
>     }
> }
> ```
>
> 4. Run `cargo test -p rift-common merkle_child -- --nocapture` and confirm it FAILS (MerkleChild not defined)
> 5. Commit with message: `test(rift-common): add MerkleChild enum tests [RED]`
>
> Do NOT implement MerkleChild. Only write failing tests and add the bincode dependency.

### Subagent 1B: Green (`qwen3.5-plus`)

**Prompt:**

> You are implementing Task 1, Phase GREEN: Make the failing MerkleChild tests pass with minimal code.
>
> **Working directory:** `/home/bogdan/rift/.worktrees/delta-sync`
>
> **Context:** Failing tests exist in `crates/rift-common/src/crypto.rs` for `MerkleChild` enum. The tests expect bincode roundtrip serialization. Red agent already added `bincode = "1"` to Cargo.toml and `use std::collections::HashMap` to imports.
>
> **Your job:**
> 1. Add `LeafInfo` struct and `MerkleChild` enum to `crates/rift-common/src/crypto.rs` (after `MerkleNode`):
>
> ```rust
> /// Metadata for a leaf (chunk) in the Merkle tree.
> #[derive(Debug, Clone, PartialEq, Eq)]
> pub struct LeafInfo {
>     pub hash: Blake3Hash,
>     pub offset: u64,
>     pub length: u64,
>     pub chunk_index: u32,
> }
> 
> /// A child node in the hash-based Merkle tree.
> #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
> pub enum MerkleChild {
>     Subtree(Blake3Hash),
>     Leaf {
>         hash: Blake3Hash,
>         length: u64,
>         chunk_index: u32,
>     },
> }
> ```
>
> 2. Ensure `Blake3Hash` has `Serialize`/`Deserialize` derives (add `serde::Serialize, serde::Deserialize` to its derives if missing)
> 3. Run `cargo test -p rift-common merkle_child -- --nocapture` — all 4 tests should PASS
> 4. Run `cargo nextest run -p rift-common` — all existing tests should still pass
> 5. Commit with message: `feat(rift-common): implement MerkleChild enum and LeafInfo [GREEN]`
>
> Write minimal code. Don't add extra features. Don't refactor anything else.

### Subagent 1C: Refactor (`glm-5.1`)

**Prompt:**

> You are implementing Task 1, Phase REFACTOR: Clean up MerkleChild/LeafInfo implementation.
>
> **Working directory:** `/home/bogdan/rift/.worktrees/delta-sync`
>
> **Context:** MerkleChild enum and LeafInfo struct are now working. Red tests pass, Green implementation is minimal.
>
> **Your job:**
> 1. Run `cargo nextest run -p rift-common` — confirm all tests green
> 2. Review the code:
>    - Are derives correct? (`Debug, Clone, PartialEq, Eq` for LeafInfo; `Debug, Clone, PartialEq, Eq, Serialize, Deserialize` for MerkleChild)
>    - Is the placement in crypto.rs appropriate? (types should be near MerkleTree since they're related)
>    - Are there any style inconsistencies with existing code?
>    - Is `HashMap` import needed yet? (only needed later for `build_with_cache`)
> 3. If no improvements needed, skip. If improvements found, make them and re-run tests.
> 4. Commit with message: `refactor(rift-common): clean up MerkleChild/LeafInfo placement [REFACTOR]` (only if you made changes)

### Review: Spec Compliance (`qwen3.5-plus`)

> Review whether the MerkleChild enum implementation matches the design spec at `docs/superpowers/specs/2026-04-20-hash-based-merkle-tree-design.md`.
>
> Check:
> - Does `MerkleChild` have `Subtree(Blake3Hash)` and `Leaf { hash, length, chunk_index }`? ✅
> - Does `LeafInfo` have `hash`, `offset`, `length`, `chunk_index`? ✅
> - Does bincode serialization roundtrip work? ✅
>
> Verify by reading actual code, not trusting the implementer report.

### Review: Code Quality (`kimi-k2.5`)

> Review code quality of the MerkleChild/LeafInfo addition.
> Check: derives, naming, consistency with existing code in crypto.rs, placement.

---

## Task 2: Protocol Update — Hash-Based MerkleDrill

**Beads:** rift-wg1  
**Files:** `proto/transfer.proto`, `crates/rift-protocol/src/messages.rs`  
**Depends on:** Nothing

### Subagent 2A: Red (`kimi-k2.5`)

**Prompt:**

> You are implementing Task 2, Phase RED: Write failing tests for the hash-based MerkleDrill protocol.
>
> **Working directory:** `/home/bogdan/rift/.worktrees/delta-sync`
>
> **Context:** We're replacing the level-based MerkleDrill (which has `level` + `subtrees` fields) with a hash-based version (which has a single `hash` field). We're also replacing `MerkleLevelResponse` with `MerkleDrillResponse` containing `parent_hash` + `children` (list of `MerkleChildProto`). The proto file is at `proto/transfer.proto`.
>
> **Current proto state** (MerkleDrill section in `proto/transfer.proto`):
> ```protobuf
> message MerkleDrill {
>   bytes           handle   = 1;
>   uint32          level    = 2;
>   repeated uint32 subtrees = 3;
> }
> 
> message MerkleLevelResponse {
>   uint32          level         = 1;
>   repeated bytes  hashes        = 2;
>   repeated uint64 subtree_bytes = 3;
> }
> ```
>
> **Your job:**
> 1. Write these failing tests in `crates/rift-protocol/src/messages.rs` test module:
>
> ```rust
> #[test]
> fn merkle_drill_hash_based_roundtrip() {
>     let msg = MerkleDrill {
>         handle: b"file-handle".to_vec(),
>         hash: vec![0xAB; 32],
>     };
>     let encoded = msg.encode_to_vec();
>     let decoded = MerkleDrill::decode(encoded.as_slice()).unwrap();
>     assert_eq!(decoded.handle, b"file-handle");
>     assert_eq!(decoded.hash, vec![0xAB; 32]);
>     // Verify old fields no longer exist (compile-time check)
> }
> 
> #[test]
> fn merkle_drill_empty_hash_requests_root() {
>     let msg = MerkleDrill {
>         handle: b"root-request".to_vec(),
>         hash: vec![],
>     };
>     let encoded = msg.encode_to_vec();
>     let decoded = MerkleDrill::decode(encoded.as_slice()).unwrap();
>     assert!(decoded.hash.is_empty());
> }
> 
> #[test]
> fn merkle_drill_response_roundtrip() {
>     let msg = MerkleDrillResponse {
>         parent_hash: vec![0xFF; 32],
>         children: vec![
>             MerkleChildProto {
>                 child_type: MerkleChildType::Subtree as i32,
>                 hash: vec![0xAA; 32],
>                 length: 0,
>                 chunk_index: 0,
>             },
>             MerkleChildProto {
>                 child_type: MerkleChildType::Leaf as i32,
>                 hash: vec![0xBB; 32],
>                 length: 131072,
>                 chunk_index: 7,
>             },
>         ],
>     };
>     let encoded = msg.encode_to_vec();
>     let decoded = MerkleDrillResponse::decode(encoded.as_slice()).unwrap();
>     assert_eq!(decoded.parent_hash, vec![0xFF; 32]);
>     assert_eq!(decoded.children.len(), 2);
>     assert_eq!(decoded.children[0].child_type, MerkleChildType::Subtree as i32);
>     assert_eq!(decoded.children[0].hash, vec![0xAA; 32]);
>     assert_eq!(decoded.children[1].child_type, MerkleChildType::Leaf as i32);
>     assert_eq!(decoded.children[1].length, 131072);
>     assert_eq!(decoded.children[1].chunk_index, 7);
> }
> ```
>
> 2. Also remove the old `merkle_drill_root_level` and `merkle_drill_specific_subtrees` tests since they reference the old API.
> 3. Run `cargo test -p rift-protocol merkle_drill -- --nocapture` and confirm it FAILS
> 4. Commit with message: `test(protocol): add hash-based MerkleDrill tests [RED]`

### Subagent 2B: Green (`qwen3.5-plus`)

**Prompt:**

> You are implementing Task 2, Phase GREEN: Update proto files and message constants to make the new tests pass.
>
> **Working directory:** `/home/bogdan/rift/.worktrees/delta-sync`
>
> **Context:** Red tests added for hash-based MerkleDrill. Now implement the protocol changes.
>
> **Your job:**
> 1. Replace the MerkleDrill + MerkleLevelResponse section in `proto/transfer.proto` with:
>
> ```protobuf
> // MERKLE_DRILL (0x50) — hash-based tree traversal
> message MerkleDrill {
>   bytes handle = 1;
>   bytes hash    = 2;  // empty = request root's children
> }
> 
> // MERKLE_DRILL_RESPONSE (0x51)
> message MerkleDrillResponse {
>   bytes parent_hash              = 1;
>   repeated MerkleChildProto children = 2;
> }
> 
> enum MerkleChildType {
>   MERKLE_CHILD_SUBTREE = 0;
>   MERKLE_CHILD_LEAF    = 1;
> }
> 
> message MerkleChildProto {
>   MerkleChildType child_type  = 1;
>   bytes           hash        = 2;
>   uint64          length      = 3;
>   uint32          chunk_index = 4;
> }
> ```
>
> Keep `MerkleLeavesResponse` and `SubtreeLeaves` as-is.
>
> 2. In `crates/rift-protocol/src/messages.rs`:
>    - Change `MERKLE_LEVEL_RESPONSE` to `MERKLE_DRILL_RESPONSE` (same value 0x51)
>    - Remove the old `merkle_drill_root_level` and `merkle_drill_specific_subtrees` tests if they still exist (they reference old fields)
>    - Keep existing `merkle_level_response_round_trip` and `merkle_leaves_response_round_trip` tests (they may need to be removed if the types no longer exist — check what compiles)
>
> 3. Run `cargo build -p rift-protocol` to regenerate protos
> 4. Fix all compilation errors across the workspace. The types `MerkleLevelResponse` and `MerkleDrill` (old) will change. You MUST update:
>    - `crates/rift-server/src/handler.rs` — change `MerkleLevelResponse` → `MerkleDrillResponse`, change `msg::MERKLE_LEVEL_RESPONSE` → `msg::MERKLE_DRILL_RESPONSE`. For now, make the handler compile but return an empty `MerkleDrillResponse` (we'll flesh it out in Task 7).
>    - `crates/rift-client/src/client.rs` — change `merkle_drill` signature to take `hash: &[u8]` instead of `level: u32, subtrees: &[u32]`. Change `MerkleLevelResponse` → `MerkleDrillResponse`. Make it compile with minimal changes.
>    - `crates/rift-client/src/remote.rs` — update `MerkleDrillResult` and `RemoteShare` trait method `merkle_drill` signature to use `hash: &[u8]`. Update the `From<MerkleDrillResponse>` impl.
>    - The handler `merkle_drill_response` function — for now, have it construct a `MerkleDrillRequest` with the new `hash` field and return an empty `MerkleDrillResponse`. Don't implement the full logic yet.
>
> 5. Run `cargo nextest run` — all tests should pass
> 6. Commit with message: `feat(protocol): hash-based MerkleDrill protocol + workspace compilation fixes [GREEN]`

### Subagent 2C: Refactor (`glm-5.1`)

**Prompt:**

> You are implementing Task 2, Phase REFACTOR: Clean up protocol changes.
>
> **Working directory:** `/home/bogdan/rift/.worktrees/delta-sync`
>
> **Your job:**
> 1. Run `cargo nextest run` — confirm all green
> 2. Review:
>    - Are there any leftover references to `MerkleLevelResponse` or `level`/`subtrees` fields?
>    - Is the `MerkleDrillResult` wrapper in client.rs still useful, or should it be replaced with `MerkleDrillResponse` directly?
>    - Are the stub implementations in handler.rs clearly marked for future work?
>    - Is the proto schema clean (no unused messages)?
> 3. If improvements made, commit: `refactor(protocol): clean up MerkleDrill migration [REFACTOR]`
> 4. If no changes, skip this commit.

### Review: Spec Compliance (`qwen3.5-plus`)

> Verify protocol changes match the design spec:
> - `MerkleDrill` has only `handle` and `hash` (no `level`, no `subtrees`)
> - `MerkleDrillResponse` has `parent_hash` and `children` (list of `MerkleChildProto`)
> - `MerkleChildProto` has `child_type`, `hash`, `length`, `chunk_index`
> - Message ID 0x50 still maps to `MERKLE_DRILL`, 0x51 to `MERKLE_DRILL_RESPONSE`
> - `MerkleLeavesResponse` still exists unchanged
> - All workspace crates compile

### Review: Code Quality (`kimi-k2.5`)

> Review the protocol migration for quality:
> - Are all old type references gone?
> - Is the handler stub minimal and clear?
> - Is the client API clean?

---

## Task 3: Tree Construction with Cache

**Beads:** rift-2ay  
**Files:** `crates/rift-common/src/crypto.rs`  
**Depends on:** Task 1 (needs `MerkleChild` + `LeafInfo`)

### Subagent 3A: Red (`kimi-k2.5`)

**Prompt:**

> You are implementing Task 3, Phase RED: Write failing tests for `build_with_cache()`.
>
> **Working directory:** `/home/bogdan/rift/.worktrees/delta-sync`
>
> **Context:** `MerkleTree` in `crates/rift-common/src/crypto.rs` already has `build(&[Blake3Hash]) -> Blake3Hash`. We need `build_with_cache()` that returns `(Blake3Hash, HashMap<Blake3Hash, Vec<MerkleChild>>, Vec<LeafInfo>)`. The HashMap maps each intermediate node's hash to its children. Leaf nodes are NOT in the HashMap (they're in `Vec<LeafInfo>`). The root hash returned MUST match what `build()` returns for the same input.
>
> **Your job:**
> 1. Add these failing tests to a new `mod merkle_cache_tests` inside `crates/rift-common/src/crypto.rs`:
>
> ```rust
> mod merkle_cache_tests {
>     use super::*;
>     use std::collections::HashMap;
> 
>     #[test]
>     fn build_with_cache_single_leaf_is_identity() {
>         let tree = MerkleTree::default();
>         let leaf = Blake3Hash::new(b"single");
>         let (root, cache, leaf_infos) = tree.build_with_cache(std::slice::from_ref(&leaf));
>         assert_eq!(root, leaf);
>         assert!(cache.is_empty());
>         assert_eq!(leaf_infos.len(), 1);
>         assert_eq!(leaf_infos[0].chunk_index, 0);
>         assert_eq!(leaf_infos[0].hash, leaf);
>     }
> 
>     #[test]
>     fn build_with_cache_two_to_63_leaves_single_level() {
>         let tree = MerkleTree::default();
>         for n in [2, 5, 32, 63] {
>             let leaves: Vec<_> = (0..n).map(|i| Blake3Hash::new(&[i as u8])).collect();
>             let (root, cache, _leaf_infos) = tree.build_with_cache(&leaves);
>             assert_eq!(cache.len(), 1, "n={n}: should have exactly 1 intermediate node");
>             let children = &cache[&root];
>             assert_eq!(children.len(), n, "n={n}: root should have {n} children");
>         }
>     }
> 
>     #[test]
>     fn build_with_cache_64_leaves() {
>         let tree = MerkleTree::default();
>         let leaves: Vec<_> = (0..64).map(|i| Blake3Hash::new(&[i as u8])).collect();
>         let (root, cache, _) = tree.build_with_cache(&leaves);
>         // 64 leaves → 1 intermediate node (root) with 64 leaf children
>         assert_eq!(cache.len(), 1);
>         assert_eq!(cache[&root].len(), 64);
>     }
> 
>     #[test]
>     fn build_with_cache_65_leaves_two_intermediates() {
>         let tree = MerkleTree::default();
>         let leaves: Vec<_> = (0..65).map(|i| Blake3Hash::new(&[i as u8])).collect();
>         let (root, cache, _) = tree.build_with_cache(&leaves);
>         // 65 leaves → 2 groups (64+1) → root + 2 intermediate nodes = 3 total
>         assert_eq!(cache.len(), 3);
>         assert_eq!(cache[&root].len(), 2); // root has 2 subtree children
>     }
> 
>     #[test]
>     fn build_with_cache_matches_existing_build_root() {
>         let tree = MerkleTree::default();
>         for n in [1, 2, 10, 63, 64, 65, 128, 200, 500] {
>             let leaves: Vec<_> = (0..n).map(|i| Blake3Hash::new(&(i as u64).to_le_bytes())).collect();
>             let root_existing = tree.build(&leaves);
>             let (root_cached, _, _) = tree.build_with_cache(&leaves);
>             assert_eq!(root_existing, root_cached, "n={n}: root should match existing build()");
>         }
>     }
> 
>     #[test]
>     fn build_with_cache_children_count_invariant() {
>         let tree = MerkleTree::default();
>         for n in [1, 5, 64, 65, 200, 500] {
>             let leaves: Vec<_> = (0..n).map(|i| Blake3Hash::new(&(i as u64).to_le_bytes())).collect();
>             let (_, cache, _) = tree.build_with_cache(&leaves);
>             let total_leaves: usize = cache.values()
>                 .map(|children| children.iter().filter(|c| matches!(c, MerkleChild::Leaf { .. })).count())
>                 .sum();
>             assert_eq!(total_leaves, n, "n={n}: leaf count must match input");
>         }
>     }
> 
>     #[test]
>     fn build_with_cache_leaf_infos_populated() {
>         let tree = MerkleTree::default();
>         let leaves: Vec<_> = (0..5).map(|i| Blake3Hash::new(&[i as u8])).collect();
>         let (_, _, leaf_infos) = tree.build_with_cache(&leaves);
>         assert_eq!(leaf_infos.len(), 5);
>         for (i, info) in leaf_infos.iter().enumerate() {
>             assert_eq!(info.chunk_index, i as u32);
>             assert_eq!(info.hash, leaves[i]);
>         }
>     }
> 
>     #[test]
>     fn build_with_cache_empty() {
>         let tree = MerkleTree::default();
>         let (root, cache, leaf_infos) = tree.build_with_cache(&[]);
>         assert_eq!(root, Blake3Hash::new(&[]));
>         assert!(cache.is_empty());
>         assert!(leaf_infos.is_empty());
>     }
> }
> ```
>
> 2. Run `cargo test -p rift-common merkle_cache -- --nocapture` — confirm FAIL (method doesn't exist)
> 3. Commit: `test(rift-common): add build_with_cache() tests [RED]`

### Subagent 3B: Green (`qwen3.5-plus`)

**Prompt:**

> You are implementing Task 3, Phase GREEN: Make build_with_cache() tests pass.
>
> **Working directory:** `/home/bogdan/rift/.worktrees/delta-sync`
>
> **Context:** Failing tests in `merkle_cache_tests` module. `MerkleChild` and `LeafInfo` already exist in `crypto.rs`. The existing `MerkleTree::build()` already computes the root hash correctly by grouping leaves into 64-ary chunks.
>
> **Your job:**
> 1. Add `use std::collections::HashMap;` to crypto.rs imports (if not already there)
> 2. Implement on `MerkleTree`:
>
> ```rust
> pub fn build_with_cache(
>     &self,
>     leaf_hashes: &[Blake3Hash],
> ) -> (Blake3Hash, HashMap<Blake3Hash, Vec<MerkleChild>>, Vec<LeafInfo>) {
>     let leaf_infos: Vec<LeafInfo> = leaf_hashes
>         .iter()
>         .enumerate()
>         .map(|(i, hash)| LeafInfo {
>             hash: hash.clone(),
>             offset: 0,
>             length: 0,
>             chunk_index: i as u32,
>         })
>         .collect();
>     
>     if leaf_hashes.is_empty() {
>         return (Blake3Hash::new(&[]), HashMap::new(), Vec::new());
>     }
>     if leaf_hashes.len() == 1 {
>         return (leaf_hashes[0].clone(), HashMap::new(), leaf_infos);
>     }
>     
>     let mut cache: HashMap<Blake3Hash, Vec<MerkleChild>> = HashMap::new();
>     let mut current_level: Vec<Blake3Hash> = leaf_hashes.to_vec();
>     let leaf_hash_set: std::collections::HashSet<_> = leaf_hashes.iter().cloned().collect();
>     
>     while current_level.len() > 1 {
>         let mut next_level = Vec::new();
>         for chunk in current_level.chunks(self.fanout) {
>             let mut hasher = Hasher::new();
>             let mut children = Vec::with_capacity(chunk.len());
>             for hash in chunk {
>                 hasher.update(hash.as_bytes());
>                 if leaf_hash_set.contains(hash) {
>                     let info = leaf_infos.iter().find(|info| &info.hash == hash).unwrap();
>                     children.push(MerkleChild::Leaf {
>                         hash: hash.clone(),
>                         length: info.length,
>                         chunk_index: info.chunk_index,
>                     });
>                 } else {
>                     children.push(MerkleChild::Subtree(hash.clone()));
>                 }
>             }
>             let parent_hash = Blake3Hash(*hasher.finalize().as_bytes());
>             cache.insert(parent_hash.clone(), children);
>             next_level.push(parent_hash);
>         }
>         current_level = next_level;
>     }
>     
>     let root = current_level.into_iter().next().unwrap();
>     (root, cache, leaf_infos)
> }
> ```
>
> 3. Run `cargo test -p rift-common merkle_cache -- --nocapture` — all 8 tests should PASS
> 4. Run `cargo nextest run -p rift-common` — all existing tests should still pass
> 5. Commit: `feat(rift-common): implement build_with_cache() [GREEN]`

### Subagent 3C: Refactor (`glm-5.1`)

**Prompt:**

> You are implementing Task 3, Phase REFACTOR: Review `build_with_cache()` for quality.
>
> **Working directory:** `/home/bogdan/rift/.worktrees/delta-sync`
>
> Review:
> 1. The leaf detection logic (`leaf_hash_set`) — is it correct for trees with >1 level where leaf hashes might collide with intermediate hashes? (Very unlikely with BLAKE3, but worth a comment)
> 2. The `find()` call for leaf_infos is O(n) per child — for 64-ary trees with many levels, consider if this matters (it's per-build, not per-query, so probably fine)
> 3. Should `build_with_cache` take a `&[LeafInfo]` parameter with pre-filled offset/length instead of creating empty leaf_infos? (The caller has this data from chunking)
> 4. Method signature clarity — is `(Blake3Hash, HashMap<Blake3Hash, Vec<MerkleChild>>, Vec<LeafInfo>)` clean enough, or should we use a named struct?
>
> Make improvements if needed, re-run tests, commit if changes were made.

### Review (both models)

Spec: Does `build_with_cache` return root matching `build()`? Does it return correct parent→children mapping? Are leaf_infos populated?
Code quality: Algorithm clarity, naming, test coverage.

---

## Task 4: DB Schema — merkle_tree_nodes

**Beads:** rift-n37  
**Files:** `crates/rift-server/src/metadata/db.rs`  
**Depends on:** Task 3 (conceptually — needs MerkleChild type for later Task 6, but schema is just SQL DDL)

### Subagent 4A: Red (`kimi-k2.5`)

Write 3 failing tests: table creates, insert/query by (file_path, node_hash), primary key uniqueness. All in `db.rs` test module.

### Subagent 4B: Green (`qwen3.5-plus`)

Add `CREATE TABLE IF NOT EXISTS merkle_tree_nodes (file_path TEXT NOT NULL, node_hash BLOB NOT NULL, children BLOB NOT NULL, PRIMARY KEY (file_path, node_hash))` to both `open()` and `open_in_memory()`. Make tests pass.

### Subagent 4C: Refactor (`glm-5.1`)

Review: Is the schema clean? Are types appropriate? Consistent with existing `merkle_cache` table style?

---

## Task 5: DB Schema — merkle_leaf_info

**Beads:** rift-aks  
**Files:** `crates/rift-server/src/metadata/db.rs`  
**Depends on:** Task 3 (conceptually, same as Task 4)

### Subagent 5A: Red (`kimi-k2.5`)

Write 3 failing tests: table creates, insert/query by (file_path, chunk_hash), primary key uniqueness. In `db.rs` test module.

### Subagent 5B: Green (`qwen3.5-plus`)

Add `CREATE TABLE IF NOT EXISTS merkle_leaf_info (file_path TEXT NOT NULL, chunk_hash BLOB NOT NULL, chunk_offset INTEGER NOT NULL, chunk_length INTEGER NOT NULL, chunk_index INTEGER NOT NULL, PRIMARY KEY (file_path, chunk_hash))` in both `open()` and `open_in_memory()`.

### Subagent 5C: Refactor (`glm-5.1`)

Review: Consistent with merkle_tree_nodes schema? Proper column types?

---

## Task 6: Database Methods — put_tree(), get_children(), get_leaf_info()

**Beads:** rift-41b  
**Files:** `crates/rift-server/src/metadata/merkle.rs`  
**Depends on:** Tasks 4 + 5

### Subagent 6A: Red (`kimi-k2.5`)

**Prompt:**

> Write failing tests in `crates/rift-server/src/metadata/merkle.rs` test module for:
> 1. `put_tree_and_get_children_root` — put a small tree, query root → get children
> 2. `get_children_nonexistent_returns_none` — query unknown hash → None
> 3. `get_leaf_info_by_hash` — put tree with leaf metadata, query leaf hash → get LeafInfo back
>
> Import `rift_common::crypto::{MerkleChild, LeafInfo, MerkleTree}` and `std::collections::HashMap`.
> Use `Database::open_in_memory()` and temp files.
>
> Commit: `test(rift-server): add put_tree/get_children/get_leaf_info tests [RED]`

### Subagent 6B: Green (`qwen3.5-plus`)

Implement `put_tree()`, `get_children()`, `get_leaf_info()` on `Database` in `merkle.rs`.

`put_tree()`:
1. Delete old tree data for file_path from both tables
2. Insert all intermediate nodes from cache (serialize `Vec<MerkleChild>` with bincode)
3. Insert all leaf_infos
4. Also update merkle_cache (call existing `put_merkle`)

`get_children(path, node_hash)`:
1. Query `merkle_tree_nodes WHERE file_path = ? AND node_hash = ?`
2. Deserialize `children` blob from bincode
3. Return `Option<Vec<MerkleChild>>`

`get_leaf_info(path, chunk_hash)`:
1. Query `merkle_leaf_info WHERE file_path = ? AND chunk_hash = ?`
2. Return `Option<LeafInfo>`

### Subagent 6C: Refactor (`glm-5.1`)

Review: Error handling (bincode deserialization failures), method signatures, consistency with existing `get_merkle`/`put_merkle` patterns.

---

## Task 7: Server Handler Rewrite — Hash-Based MerkleDrill

**Beads:** rift-gzg  
**Files:** `crates/rift-server/src/handler.rs`, `crates/rift-client/src/client.rs`, `crates/rift-client/src/remote.rs`  
**Depends on:** Tasks 2 + 6

### Subagent 7A: Red (`kimi-k2.5`)

Write 3 failing integration-style tests. Since this requires full server/client setup, the tests should be in `crates/rift-server/tests/` and test the handler behavior through the protocol:
1. `merkle_drill_empty_hash_returns_root_children` 
2. `merkle_drill_valid_subtree_hash_returns_children`
3. `merkle_drill_unknown_hash_returns_empty`

These may need to be unit-level tests if integration test infrastructure is limited. Alternative: test the handler function directly with a mock `RiftStream`.

### Subagent 7B: Green (`qwen3.5-plus`)

Rewrite `merkle_drill_response()` in handler.rs:
1. Parse `MerkleDrill` (hash-based)
2. Resolve handle to canonical path
3. Read file, chunk, `build_with_cache()`
4. Fill leaf_infos with real offsets/lengths from chunk boundaries
5. `put_tree()` into database
6. If `hash` is empty → query root; otherwise query by hash
7. Convert `Vec<MerkleChild>` → `Vec<MerkleChildProto>`
8. Send `MerkleDrillResponse`

Update client `merkle_drill()` to send hash-based request and parse `MerkleDrillResponse`.

### Subagent 7C: Refactor (`glm-5.1`)

Review: Handler readability, error handling paths, database caching strategy (currently rebuilds every request — note TODO for caching optimization), client API ergonomics.

---

## Task 8: Integration Tests

**Beads:** rift-r3j  
**Files:** `crates/rift-server/tests/server.rs` (or new test file)  
**Depends on:** Task 7

### Subagent 8A: Red (`kimi-k2.5`)

Write 4 failing end-to-end tests:
1. Full sync: file → tree → drill root → verify children contains expected hashes
2. Drill into subtree: query a subtree hash → get grandchildren
3. Unknown hash: random 32-byte hash → empty children
4. Rebuild on change: modify file → drill again → different root

### Subagent 8B: Green (`qwen3.5-plus`)

Make tests pass using existing server/client test infrastructure.

### Subagent 8C: Refactor (`glm-5.1`)

Review: Test clarity, assertions, coverage.

---

## Execution Wave Summary

| Wave | Tasks | Dispatches | Parallel? |
|------|-------|-----------|-----------|
| 1 | Task 1 (MerkleChild) + Task 2 (Protocol) | T1: kimi-k2.5 + qwen3.5-plus + glm-5.1 + reviews. T2: kimi-k2.5 + qwen3.5-plus + glm-5.1 + reviews | Yes |
| 2 | Task 3 (build_with_cache) | kimi-k2.5 + qwen3.5-plus + glm-5.1 + reviews | No (needs T1) |
| 3 | Task 4 (tree_nodes) + Task 5 (leaf_info) | Each: kimi-k2.5 + qwen3.5-plus + glm-5.1 + reviews | Yes (needs T3) |
| 4 | Task 6 (DB methods) | kimi-k2.5 + qwen3.5-plus + glm-5.1 + reviews | No (needs T4+T5) |
| 5 | Task 7 (handler rewrite) | kimi-k2.5 + qwen3.5-plus + glm-5.1 + reviews | No (needs T2+T6) |
| 6 | Task 8 (integration tests) | kimi-k2.5 + qwen3.5-plus + glm-5.1 + reviews | No (needs T7) |

**Total:** 30 TDD dispatches (8 tasks x 3 phases) + 16 review dispatches (8 tasks x 2 reviews) = **46 agent dispatches**

**Per wave:** Each task = 1 Red (kimi-k2.5) + 1 Green (qwen3.5-plus) + 1 Refactor (glm-5.1) + 1 Spec review (qwen3.5-plus) + 1 Quality review (kimi-k2.5) = 5 dispatches

**Rate impact per wave:**
- Wave 1 (2 tasks parallel): 10 dispatches. kimi-k2.5 gets 4, qwen3.5-plus gets 4, glm-5.1 gets 2
- Wave 2 (1 task): 5 dispatches
- Wave 3 (2 tasks parallel): 10 dispatches
- Wave 4 (1 task): 5 dispatches
- Wave 5 (1 task): 5 dispatches
- Wave 6 (1 task): 5 dispatches

**Worst case burst:** Wave 1 or 3 = 10 dispatches within ~30 min. kimi-k2.5: 4 calls (well within 1,850/5h). glm-5.1: 2 calls (within 880/5h). qwen3.5-plus: 4 calls (within 10,200/5h).

Within each task, phases are sequential: Green needs Red's tests, Refactor needs Green's code. Reviews happen after task completion.