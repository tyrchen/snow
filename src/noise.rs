use constants::*;
use types::*;
use handshakestate::*;
use wrappers;
use wrappers::rand_wrapper::*;
use wrappers::crypto_wrapper::*;
use cipherstate::*;
use session::*;
use utils::*;
use params::*;
use error::{ErrorKind, Result, InitStage, Prerequisite};

/// An object that resolves the providers of Noise crypto choices
pub trait CryptoResolver {
    fn resolve_rng(&self) -> Option<Box<Random>>;
    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<Dh>>;
    fn resolve_hash(&self, choice: &HashChoice) -> Option<Box<Hash>>;
    fn resolve_cipher(&self, choice: &CipherChoice) -> Option<Box<Cipher>>;
}

/// The default pure-rust crypto implementation resolver.
pub struct DefaultResolver;
impl CryptoResolver for DefaultResolver {
    fn resolve_rng(&self) -> Option<Box<Random>> {
        Some(Box::new(RandomOs::default()))
    }

    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<Dh>> {
        match *choice {
            DHChoice::Curve25519 => Some(Box::new(Dh25519::default())),
            _                    => None,
        }
    }

    fn resolve_hash(&self, choice: &HashChoice) -> Option<Box<Hash>> {
        match *choice {
            HashChoice::SHA256  => Some(Box::new(HashSHA256::default())),
            HashChoice::SHA512  => Some(Box::new(HashSHA512::default())),
            HashChoice::Blake2s => Some(Box::new(HashBLAKE2s::default())),
            HashChoice::Blake2b => Some(Box::new(HashBLAKE2b::default())),
        }
    }

    fn resolve_cipher(&self, choice: &CipherChoice) -> Option<Box<Cipher>> {
        match *choice {
            CipherChoice::ChaChaPoly => Some(Box::new(CipherChaChaPoly::default())),
            CipherChoice::AESGCM     => Some(Box::new(CipherAESGCM::default())),
        }
    }
}

/// Generates a `NoiseSession` and also validate that all the prerequisites for
/// the given parameters are satisfied.
///
/// # Examples
///
/// ```
/// # use snow::NoiseBuilder;
/// # let my_long_term_key = [0u8; 32];
/// # let their_pub_key = [0u8; 32];
/// let noise = NoiseBuilder::new("Noise_XX_25519_ChaChaPoly_BLAKE2s".parse().unwrap())
///                          .local_private_key(&my_long_term_key)
///                          .remote_public_key(&their_pub_key)
///                          .prologue("noise is just swell".as_bytes())
///                          .build_initiator()
///                          .unwrap();
/// ```
pub struct NoiseBuilder<'builder> {
    params:   NoiseParams,
    resolver: Box<CryptoResolver>,
    s:        Option<&'builder [u8]>,
    e_fixed:  Option<&'builder [u8]>,
    rs:       Option<&'builder [u8]>,
    psks:     [Option<&'builder [u8]>; 10],
    plog:     Option<&'builder [u8]>,
}

impl<'builder> NoiseBuilder<'builder> {
    /// Create a NoiseBuilder with the default crypto resolver.
    #[cfg(not(feature = "ring-accelerated"))]
    pub fn new(params: NoiseParams) -> Self {
        Self::with_resolver(params, Box::new(DefaultResolver))
    }

    #[cfg(feature = "ring-accelerated")]
    pub fn new(params: NoiseParams) -> Self {
        Self::with_resolver(params, Box::new(wrappers::ring_wrapper::RingAcceleratedResolver::new()))
    }

    /// Create a NoiseBuilder with a custom crypto resolver.
    pub fn with_resolver(params: NoiseParams, resolver: Box<CryptoResolver>) -> Self
    {
        NoiseBuilder {
            params: params,
            resolver: resolver,
            s: None,
            e_fixed: None,
            rs: None,
            plog: None,
            psks: [None; 10],
        }
    }

    /// Specify a PSK (only used with `NoisePSK` base parameter)
    pub fn psk(mut self, location: u8, key: &'builder [u8]) -> Self {
        self.psks[location as usize] = Some(key);
        self
    }

    /// Your static private key, can be generated by [`generate_private_key()`](#method.generate_private_key).
    pub fn local_private_key(mut self, key: &'builder [u8]) -> Self {
        self.s = Some(key);
        self
    }

    #[doc(hidden)]
    pub fn fixed_ephemeral_key_for_testing_only(mut self, key: &'builder [u8]) -> Self {
        self.e_fixed = Some(key);
        self
    }

    /// Arbitrary data to be hashed in to the handshake hash value.
    pub fn prologue(mut self, key: &'builder [u8]) -> Self {
        self.plog = Some(key);
        self
    }

    /// The responder's static public key.
    pub fn remote_public_key(mut self, pub_key: &'builder [u8]) -> Self {
        self.rs = Some(pub_key);
        self
    }

    // TODO this is inefficient as it computes the public key then throws it away
    // TODO also inefficient because it creates a new RNG and DH instance just for this.
    /// Generate a new private key. It's up to the user of this library how to store this.
    pub fn generate_private_key(&self) -> Result<Vec<u8>> {
        let mut rng = self.resolver.resolve_rng()
            .ok_or(ErrorKind::Init(InitStage::GetRngImpl))?;
        let mut dh = self.resolver.resolve_dh(&self.params.dh)
            .ok_or(ErrorKind::Init(InitStage::GetDhImpl))?;
        let mut private = vec![0u8; dh.priv_len()];
        dh.generate(&mut *rng);
        private[..dh.priv_len()].copy_from_slice(dh.privkey());
        Ok(private)
    }

    /// Build a NoiseSession for the side who will initiate the handshake (send the first message)
    pub fn build_initiator(self) -> Result<Session> {
        self.build(true)
    }

    /// Build a NoiseSession for the side who will be responder (receive the first message)
    pub fn build_responder(self) -> Result<Session> {
        self.build(false)
    }

    fn build(self, initiator: bool) -> Result<Session> {
        if !self.s.is_some() && self.params.handshake.pattern.needs_local_static_key(initiator) {
            bail!(ErrorKind::Prereq(Prerequisite::LocalPrivateKey));
        }

        if !self.rs.is_some() && self.params.handshake.pattern.need_known_remote_pubkey(initiator) {
            bail!(ErrorKind::Prereq(Prerequisite::RemotePublicKey));
        }

        let rng = self.resolver.resolve_rng().ok_or(ErrorKind::Init(InitStage::GetRngImpl))?;
        let cipher = self.resolver.resolve_cipher(&self.params.cipher).ok_or(ErrorKind::Init(InitStage::GetCipherImpl))?;
        let hash = self.resolver.resolve_hash(&self.params.hash).ok_or(ErrorKind::Init(InitStage::GetHashImpl))?;
        let mut s_dh = self.resolver.resolve_dh(&self.params.dh).ok_or(ErrorKind::Init(InitStage::GetDhImpl))?;
        let mut e_dh = self.resolver.resolve_dh(&self.params.dh).ok_or(ErrorKind::Init(InitStage::GetDhImpl))?;
        let cipher1 = self.resolver.resolve_cipher(&self.params.cipher).ok_or(ErrorKind::Init(InitStage::GetCipherImpl))?;
        let cipher2 = self.resolver.resolve_cipher(&self.params.cipher).ok_or(ErrorKind::Init(InitStage::GetCipherImpl))?;
        let handshake_cipherstate = CipherState::new(cipher);
        let cipherstates = CipherStates::new(CipherState::new(cipher1), CipherState::new(cipher2))?;

        let s = match self.s {
            Some(k) => {
                (&mut *s_dh).set(k);
                Toggle::on(s_dh)
            },
            None => {
                Toggle::off(s_dh)
            }
        };

        if let Some(fixed_k) = self.e_fixed {
            (&mut *e_dh).set(fixed_k);
        }
        let e = Toggle::off(e_dh);

        let mut rs_buf = [0u8; MAXDHLEN];
        let rs = match self.rs {
            Some(v) => {
                rs_buf[..v.len()].copy_from_slice(&v[..]);
                Toggle::on(rs_buf)
            },
            None => Toggle::off(rs_buf),
        };

        let re = Toggle::off([0u8; MAXDHLEN]);

        let mut psks = [None::<[u8; PSKLEN]>; 10];
        for (i, psk) in self.psks.iter().enumerate() {
            if let &Some(key) = psk {
                if key.len() != PSKLEN {
                    bail!(ErrorKind::Init(InitStage::ValidatePskLengths));
                }
                let mut k = [0u8; PSKLEN];
                k.copy_from_slice(key);
                psks[i] = Some(k);
            }
        }

        let hs = HandshakeState::new(rng, handshake_cipherstate, hash,
                                     s, e, self.e_fixed.is_some(), rs, re,
                                     initiator,
                                     self.params,
                                     psks,
                                     &self.plog.unwrap_or_else(|| &[0u8; 0]),
                                     cipherstates)?;
        Ok(hs.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder() {
        let _noise = NoiseBuilder::new("Noise_NN_25519_ChaChaPoly_SHA256".parse().unwrap())
            .prologue(&[2,2,2,2,2,2,2,2])
            .local_private_key(&[0u8; 32])
            .build_initiator().unwrap();
    }

    #[test]
    fn test_builder_keygen() {
        let builder = NoiseBuilder::new("Noise_NN_25519_ChaChaPoly_SHA256".parse().unwrap());
        let key1 = builder.generate_private_key();
        let key2 = builder.generate_private_key();
        assert!(key1.unwrap() != key2.unwrap());
    }

    #[test]
    fn test_builder_bad_spec() {
        let params: ::std::result::Result<NoiseParams, _> = "Noise_NK_25519_ChaChaPoly_BLAH256".parse();

        if let Ok(_) = params {
            panic!("NoiseParams should have failed");
        }
    }

    #[test]
    fn test_builder_missing_prereqs() {
        let noise = NoiseBuilder::new("Noise_NK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .prologue(&[2,2,2,2,2,2,2,2])
            .local_private_key(&[0u8; 32])
            .build_initiator(); // missing remote key, should result in Err

        if let Ok(_) = noise {
            panic!("builder should have failed on build");
        }
    }
}

