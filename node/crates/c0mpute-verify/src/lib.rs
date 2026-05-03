//! Verification: storage challenges (Proof-of-Replication-lite) and
//! reputation scoring. See PRD §14.

use serde::{Deserialize, Serialize};

/// A storage challenge: prove you hold this chunk by hashing a slice of it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StorageChallenge {
    pub chunk_hash: c0mpute_proto::Hash,
    pub offset: u32,
    pub length: u32,
}

impl StorageChallenge {
    /// Compute the expected response from the full chunk bytes.
    pub fn expected_response(&self, full_chunk: &[u8]) -> Option<c0mpute_proto::Hash> {
        let start = self.offset as usize;
        let end = start.checked_add(self.length as usize)?;
        if end > full_chunk.len() {
            return None;
        }
        Some(c0mpute_proto::Hash::of(&full_chunk[start..end]))
    }
}

/// Inputs for the reputation formula in PRD §14.
#[derive(Clone, Debug, Default)]
pub struct ReputationInputs {
    pub uptime_30d: f32,
    pub verification_pass_rate: f32,
    pub job_completion_rate: f32,
    pub recent_slash_weight: f32,
}

pub fn reputation(inputs: &ReputationInputs) -> f32 {
    let r = 0.5
        + 0.30 * inputs.uptime_30d.clamp(0.0, 1.0)
        + 0.15 * inputs.verification_pass_rate.clamp(0.0, 1.0)
        + 0.05 * inputs.job_completion_rate.clamp(0.0, 1.0)
        - 0.50 * inputs.recent_slash_weight.clamp(0.0, 1.0);
    r.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_provider_caps_at_one() {
        let r = reputation(&ReputationInputs {
            uptime_30d: 1.0,
            verification_pass_rate: 1.0,
            job_completion_rate: 1.0,
            recent_slash_weight: 0.0,
        });
        assert!((r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn slash_drives_reputation_down() {
        let r = reputation(&ReputationInputs {
            uptime_30d: 1.0,
            verification_pass_rate: 1.0,
            job_completion_rate: 1.0,
            recent_slash_weight: 1.0,
        });
        assert!(r < 0.6);
    }

    #[test]
    fn challenge_response_matches_slice() {
        let bytes = b"the quick brown fox jumps over the lazy dog";
        let chunk_hash = c0mpute_proto::Hash::of(bytes);
        let challenge = StorageChallenge {
            chunk_hash,
            offset: 4,
            length: 5,
        };
        let expected = challenge.expected_response(bytes).unwrap();
        assert_eq!(expected, c0mpute_proto::Hash::of(b"quick"));
    }
}
