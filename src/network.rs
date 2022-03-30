use crate::peer::{Peer};
use std::hash::Hash;
use num::{Float, Unsigned};
use rand::RngCore;
use num::Zero;
use rand::prelude::SliceRandom;

pub trait NetworkConfig {
	type PeerIndex: Unsigned + From<usize> + Eq + Hash + Clone;
	type PeerScore: Float;
	const DELTA: f64;
}

pub struct Network<C: NetworkConfig> {
    peers: Vec<Peer<C::PeerIndex, C::PeerScore>>,
    is_converged: bool,
}

impl<C: NetworkConfig> Network<C> {
    pub fn new(size: usize, initial_trust_scores: Vec<C::PeerScore>) -> Self {
		let mut peers = Vec::with_capacity(size);
		for x in 0..size {
			let index = C::PeerIndex::from(x);
			peers.push(Peer::new(index, initial_trust_scores[x as usize]));
		}
        Self {
            peers,
            is_converged: false,
        }
    }

    pub fn connect_peers(&mut self, local_trust_matrix: Vec<Vec<C::PeerScore>>) {
        for (i, c_i) in local_trust_matrix.iter().enumerate() {
            for (j, c_ij) in c_i.iter().enumerate() {
                if i == j {
                    continue;
                }

                let peer_j = self.peers[j].clone();
                self.peers[i].add_neighbor(peer_j, *c_ij);
            }
        }
    }

    pub fn tick<R: RngCore>(&mut self, rng: &mut R) {
        let mut temp_peers = self.peers.clone();
        temp_peers.shuffle(rng);

        let mut is_converged = true;
        for peer in temp_peers.iter_mut() {
            peer.heartbeat(&self.peers, C::DELTA);
            is_converged = is_converged && peer.is_converged();
        }
        self.peers = temp_peers;
        self.is_converged = is_converged;
    }

    pub fn get_global_trust_scores(&self) -> Vec<C::PeerScore> {
        let mut sum = C::PeerScore::zero();
        for peer in self.peers.iter() {
            sum = sum + peer.get_ti();
        }

        let mut ti_vec = Vec::new();
        for peer in self.peers.iter() {
            ti_vec.push(peer.get_ti() / sum);
        }

        ti_vec
    }

    pub fn is_converged(&self) -> bool {
        self.is_converged
    }
}
