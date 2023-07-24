use cuckoofilter::{CuckooFilter, ExportedCuckooFilter};
use parking_lot::Mutex;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use subspace_core_primitives::PieceIndex;
use subspace_networking::libp2p::PeerId;
use subspace_networking::CuckooFilterDTO;
use tracing::debug;

const CONNECTED_PEERS_NUMBER_LIMIT: usize = 50;

#[derive(Clone, Default)]
pub struct ArchivalStorageInfo {
    peers: Arc<Mutex<HashMap<PeerId, CuckooFilter<DefaultHasher>>>>,
}

impl Debug for ArchivalStorageInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchivalStorageInfo")
            .field("peers (len)", &self.peers.lock().len())
            .finish()
    }
}

impl ArchivalStorageInfo {
    pub fn update_cuckoo_filter(&self, peer_id: PeerId, cuckoo_filter_dto: Arc<CuckooFilterDTO>) {
        let exported_filter = ExportedCuckooFilter {
            values: cuckoo_filter_dto.values.clone(),
            length: cuckoo_filter_dto.length as usize,
        };

        let cuckoo_filter = CuckooFilter::from(exported_filter);

        let mut peer_filters = self.peers.lock();

        peer_filters.insert(peer_id, cuckoo_filter);

        // Truncate current peer set by limits.
        let mut rng = StdRng::seed_from_u64({
            // Hash of PeerID
            let mut s = DefaultHasher::new();
            peer_id.hash(&mut s);
            s.finish()
        });

        // Remove random peer when we exceed the limit of storing peers (and their cuckoo-filters).
        if peer_filters.len() > CONNECTED_PEERS_NUMBER_LIMIT {
            let connected_peers = peer_filters.keys().cloned().collect::<Vec<_>>();
            let random_index = rng.gen_range(0..connected_peers.len());

            let removing_peer_id = *connected_peers
                .get(random_index)
                .expect("Index is checked to be present.");

            peer_filters.remove(&removing_peer_id);

            debug!(%removing_peer_id, "Removed disconnected peer from filter cache.");
        }
    }

    pub fn remove_peer_filter(&self, peer_id: &PeerId) -> bool {
        self.peers.lock().remove(peer_id).is_some()
    }

    pub fn peers_contain_piece(&self, piece_index: &PieceIndex) -> Vec<PeerId> {
        let mut result = Vec::new();
        for (peer_id, cuckoo_filter) in self.peers.lock().iter() {
            if cuckoo_filter.contains(piece_index) {
                result.push(*peer_id)
            }
        }

        result
    }
}
