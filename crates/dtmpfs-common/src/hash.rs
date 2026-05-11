use crate::id::{BlockKey, NodeId};
use xxhash_rust::xxh3::xxh3_64_with_seed;

/// Pick up to `r` nodes for a block. Returns nodes in score order (index 0 = primary).
/// Generation is deliberately excluded from the hash — placement is per-(ino, idx)
/// and must be stable across rewrites of the same block.
pub fn pick_nodes(key: &BlockKey, nodes: &[NodeId], r: usize) -> Vec<NodeId> {
    if nodes.is_empty() { return Vec::new(); }
    let r = r.min(nodes.len());
    let pk = key.placement_key();    // u128: ino<<64 | block_idx
    let lo = pk as u64;
    let hi = (pk >> 64) as u64;

    let mut scored: Vec<(u64, &NodeId)> = nodes.iter()
        .map(|n| {
            let mut h = xxh3_64_with_seed(n.as_str().as_bytes(), lo);
            h ^= xxh3_64_with_seed(n.as_str().as_bytes(), hi);
            (h, n)
        })
        .collect();
    scored.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(r).map(|(_, n)| n.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{BlockIdx, BlockKey, Generation, InodeId, NodeId};

    fn nodes(n: usize) -> Vec<NodeId> {
        (0..n).map(|i| NodeId::new(format!("store-{i}"))).collect()
    }

    #[test]
    fn deterministic() {
        let ns = nodes(8);
        let k = BlockKey { ino: InodeId(42), block_idx: BlockIdx(7), generation: Generation(0) };
        assert_eq!(pick_nodes(&k, &ns, 2), pick_nodes(&k, &ns, 2));
    }

    #[test]
    fn empty_nodes() {
        let k = BlockKey { ino: InodeId(1), block_idx: BlockIdx(0), generation: Generation(0) };
        assert!(pick_nodes(&k, &[], 3).is_empty());
    }

    #[test]
    fn r_capped() {
        let ns = nodes(2);
        let k = BlockKey { ino: InodeId(1), block_idx: BlockIdx(0), generation: Generation(0) };
        assert_eq!(pick_nodes(&k, &ns, 5).len(), 2);
    }

    #[test]
    fn minimal_disruption() {
        let n8 = nodes(8);
        let n7: Vec<_> = n8.iter().cloned().filter(|n| n.as_str() != "store-3").collect();
        let mut moved = 0usize;
        for i in 0..1024 {
            let k = BlockKey { ino: InodeId(1), block_idx: BlockIdx(i), generation: Generation(0) };
            if pick_nodes(&k, &n8, 1)[0] != pick_nodes(&k, &n7, 1)[0] { moved += 1; }
        }
        assert!(moved < 1024 * 2 / 8, "moved={moved}");
    }
}
