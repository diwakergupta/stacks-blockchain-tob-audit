/*
 copyright: (c) 2013-2019 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

use std::io;
use std::io::prelude::*;
use std::io::Read;
use std::convert::TryFrom;
use std::collections::HashSet;

use burnchains::BurnchainHeaderHash;
use burnchains::PrivateKey;
use burnchains::PublicKey;
use burnchains::BurnchainView;

use chainstate::burn::ConsensusHash;
use chainstate::burn::BlockHeaderHash;

use chainstate::stacks::StacksBlock;
use chainstate::stacks::StacksBlockHeader;
use chainstate::stacks::StacksMicroblock;
use chainstate::stacks::StacksTransaction;

use chainstate::stacks::MAX_BLOCK_LEN;

use chainstate::stacks::StacksPublicKey;

use util::hash::DoubleSha256;
use util::hash::Hash160;
use util::hash::MerkleHashFunc;
use util::secp256k1::{Secp256k1PublicKey, Secp256k1PrivateKey};

use net::*;
use net::Error as net_error;
use net::db::LocalPeer;

use core::PEER_VERSION;

use sha2::Sha512Trunc256;
use sha2::Digest;

use util::secp256k1::MessageSignature;
use util::secp256k1::MESSAGE_SIGNATURE_ENCODED_SIZE;

use util::log;
use util::retry::BoundReader;

use rand;
use rand::Rng;

use std::mem;

// macro for determining how big an inv bitvec can be, given its bitlen 
macro_rules! BITVEC_LEN {
    ($bitvec:expr) => ((($bitvec) / 8 + if ($bitvec) % 8 > 0 { 1 } else { 0 }) as u32)
}

pub fn write_next<T: StacksMessageCodec, W: Write>(fd: &mut W, item: &T) -> Result<(), net_error> {
    item.consensus_serialize(fd)
}

pub fn read_next<T: StacksMessageCodec, R: Read>(fd: &mut R) -> Result<T, net_error> {
    let item: T = T::consensus_deserialize(fd)?;
    Ok(item)
}

fn read_next_vec<T: StacksMessageCodec + Sized, R: Read>(fd: &mut R, num_items: u32, max_items: u32) -> Result<Vec<T>, net_error> {
    let len = u32::consensus_deserialize(fd)?;

    if max_items > 0 {
        if len > max_items {
            // too many items
            return Err(net_error::DeserializeError(format!("Array has too many items ({} > {}", len, max_items)));
        }
    }
    else {
        if len != num_items {
            // inexact item count
            return Err(net_error::DeserializeError(format!("Array has incorrect number of items ({} != {})", len, num_items)));
        }
    }

    if (mem::size_of::<T>() as u128) * (len as u128) > MAX_MESSAGE_LEN  as u128 {
        return Err(net_error::DeserializeError(format!("Message occupies too many bytes (tried to allocate {})", (mem::size_of::<T>() as u128) * (len as u128))));
    }

    let mut ret = Vec::with_capacity(len as usize);
    for _i in 0..len {
        let next_item = T::consensus_deserialize(fd)?;
        ret.push(next_item);
    }

    Ok(ret)
}

pub fn read_next_at_most<R: Read, T: StacksMessageCodec + Sized>(fd: &mut R, max_items: u32) -> Result<Vec<T>, net_error> {
    read_next_vec::<T, R>(fd, 0, max_items)
}

pub fn read_next_exact<R: Read, T: StacksMessageCodec + Sized>(fd: &mut R, num_items: u32) -> Result<Vec<T>, net_error> {
    read_next_vec::<T, R>(fd, num_items, 0)
}

macro_rules! impl_stacks_message_codec_for_int {
    ($typ:ty; $array:expr) => {
        impl StacksMessageCodec for $typ {
            fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
                fd.write_all(&self.to_be_bytes()).map_err(net_error::WriteError)
            }
            fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, net_error> {
                let mut buf = $array;
                fd.read_exact(&mut buf).map_err(net_error::ReadError)?;
                Ok(<$typ>::from_be_bytes(buf))
            }
        }
    }
}

impl_stacks_message_codec_for_int!(u8; [0; 1]);
impl_stacks_message_codec_for_int!(u16; [0; 2]);
impl_stacks_message_codec_for_int!(u32; [0; 4]);
impl_stacks_message_codec_for_int!(u64; [0; 8]);
impl_stacks_message_codec_for_int!(i64; [0; 8]);

impl StacksPublicKeyBuffer {
    pub fn from_public_key(pubkey: &Secp256k1PublicKey) -> StacksPublicKeyBuffer {
        let pubkey_bytes_vec = pubkey.to_bytes_compressed();
        let mut pubkey_bytes = [0u8; 33];
        pubkey_bytes.copy_from_slice(&pubkey_bytes_vec[..]);
        StacksPublicKeyBuffer(pubkey_bytes)
    }
    
    pub fn to_public_key(&self) -> Result<Secp256k1PublicKey, net_error> {
        Secp256k1PublicKey::from_slice(&self.0)
            .map_err(|_e_str| net_error::DeserializeError("Failed to decode Stacks public key".to_string()))
    }
}

impl<T> StacksMessageCodec for Vec<T>
where
    T: StacksMessageCodec + Sized
{
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        let len = self.len() as u32;
        write_next(fd, &len)?;
        for i in 0..self.len() {
            write_next(fd, &self[i])?;
        }
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Vec<T>, net_error> {
        read_next_at_most::<R, T>(fd, u32::max_value())
    }
}

impl Preamble {
    /// Make an empty preamble with the given version and fork-set identifier, and payload length.
    pub fn new(peer_version: u32, network_id: u32, block_height: u64, consensus_hash: &ConsensusHash, stable_block_height: u64, stable_consensus_hash: &ConsensusHash, payload_len: u32) -> Preamble {
        Preamble {
            peer_version: peer_version,
            network_id: network_id,
            seq: 0,
            burn_block_height: block_height,
            burn_consensus_hash: consensus_hash.clone(),
            burn_stable_block_height: stable_block_height,
            burn_stable_consensus_hash: stable_consensus_hash.clone(),
            additional_data: 0,
            signature: MessageSignature::empty(),
            payload_len: payload_len,
        }
    }

    /// Given the serialized message type and bits, sign the resulting message and store the
    /// signature.  message_bits includes the relayers, payload type, and payload.
    pub fn sign(&mut self, message_bits: &[u8], privkey: &Secp256k1PrivateKey) -> Result<(), net_error> {
        let mut digest_bits = [0u8; 32];
        let mut sha2 = Sha512Trunc256::new();

        // serialize the premable with a blank signature
        let old_signature = self.signature.clone();
        self.signature = MessageSignature::empty();

        let mut preamble_bits = vec![];
        self.consensus_serialize(&mut preamble_bits)?;
        self.signature = old_signature;

        sha2.input(&preamble_bits[..]);
        sha2.input(message_bits);
        
        digest_bits.copy_from_slice(sha2.result().as_slice());

        let sig = privkey.sign(&digest_bits)
            .map_err(|se| net_error::SigningError(se.to_string()))?;

        self.signature = sig;
        Ok(())
    }

    /// Given the serialized message type and bits, verify the signature.
    /// message_bits includes the relayers, payload type, and payload
    pub fn verify(&mut self, message_bits: &[u8], pubkey: &Secp256k1PublicKey) -> Result<(), net_error> {
        let mut digest_bits = [0u8; 32];
        let mut sha2 = Sha512Trunc256::new();

        // serialize the preamble with a blank signature 
        let sig_bits = self.signature.clone();
        self.signature = MessageSignature::empty();

        let mut preamble_bits = vec![];
        self.consensus_serialize(&mut preamble_bits)?;
        self.signature = sig_bits;

        sha2.input(&preamble_bits[..]);
        sha2.input(message_bits);

        digest_bits.copy_from_slice(sha2.result().as_slice());
        
        let res = pubkey.verify(&digest_bits, &self.signature)
            .map_err(|_ve| net_error::VerifyingError("Failed to verify signature".to_string()))?;

        if res {
            Ok(())
        }
        else {
            Err(net_error::VerifyingError("Invalid message signature".to_string()))
        }
    }
}

impl StacksMessageCodec for Preamble {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.peer_version)?;
        write_next(fd, &self.network_id)?;
        write_next(fd, &self.seq)?;
        write_next(fd, &self.burn_block_height)?;
        write_next(fd, &self.burn_consensus_hash)?;
        write_next(fd, &self.burn_stable_block_height)?;
        write_next(fd, &self.burn_stable_consensus_hash)?;
        write_next(fd, &self.additional_data)?;
        write_next(fd, &self.signature)?;
        write_next(fd, &self.payload_len)?;
        Ok(())
    }
    
    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Preamble, net_error> {
        let peer_version: u32                           = read_next(fd)?;
        let network_id: u32                             = read_next(fd)?;
        let seq: u32                                    = read_next(fd)?;
        let burn_block_height: u64                      = read_next(fd)?;
        let burn_consensus_hash : ConsensusHash         = read_next(fd)?;
        let burn_stable_block_height: u64               = read_next(fd)?;
        let burn_stable_consensus_hash : ConsensusHash  = read_next(fd)?;
        let additional_data : u32                       = read_next(fd)?;
        let signature : MessageSignature                = read_next(fd)?;
        let payload_len : u32                           = read_next(fd)?;

        // test_debug!("preamble {}-{:?}/{}-{:?}, {} bytes", burn_block_height, burn_consensus_hash, burn_stable_block_height, burn_stable_consensus_hash, payload_len);

        // minimum is 5 bytes -- a zero-length vector (4 bytes of 0) plus a type identifier (1 byte)
        if payload_len < 5 {
            test_debug!("Payload len is too small: {}", payload_len);
            return Err(net_error::DeserializeError(format!("Payload len is too small: {}", payload_len)));
        }

        if payload_len >= MAX_MESSAGE_LEN {
            test_debug!("Payload len is too big: {}", payload_len);
            return Err(net_error::DeserializeError(format!("Payload len is too big: {}", payload_len)));
        }

        if burn_block_height <= burn_stable_block_height {
            test_debug!("burn block height {} <= burn stable block height {}", burn_block_height, burn_stable_block_height);
            return Err(net_error::DeserializeError(format!("Burn block height {} <= burn stable block height {}", burn_block_height, burn_stable_block_height)));
        }

        Ok(Preamble {
            peer_version,
            network_id,
            seq,
            burn_block_height,
            burn_consensus_hash,
            burn_stable_block_height,
            burn_stable_consensus_hash,
            additional_data,
            signature,
            payload_len,
        })
    }
}

impl StacksMessageCodec for GetBlocksInv {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.consensus_hash)?;
        write_next(fd, &self.num_blocks)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<GetBlocksInv, net_error> {
        let consensus_hash: ConsensusHash             = read_next(fd)?;
        let num_blocks : u16                          = read_next(fd)?;
        if (num_blocks as u32) > BLOCKS_INV_DATA_MAX_BITLEN {
            // requested too long of a range 
            return Err(net_error::DeserializeError(format!("Block diff is too big for inv ({})", num_blocks)));
        }

        Ok(GetBlocksInv {
            consensus_hash: consensus_hash,
            num_blocks: num_blocks
        })
    }
}

impl StacksMessageCodec for BlocksInvData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.bitlen)?;
        write_next(fd, &self.block_bitvec)?;
        write_next(fd, &self.microblocks_bitvec)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<BlocksInvData, net_error> {
        let bitlen : u16 = read_next(fd)?;
        if bitlen > BLOCKS_INV_DATA_MAX_BITLEN as u16 {
            return Err(net_error::DeserializeError(format!("bitlen is bigger than max bitlen inv ({})", bitlen)));
        }

        let block_bitvec : Vec<u8> = read_next_exact::<_, u8>(fd, BITVEC_LEN!(bitlen))?;
        let microblocks_bitvec : Vec<u8> = read_next_exact::<_, u8>(fd, BITVEC_LEN!(bitlen))?;

        Ok(BlocksInvData {
            bitlen,
            block_bitvec,
            microblocks_bitvec
        })
    }
}

impl BlocksInvData {
    pub fn empty() -> BlocksInvData {
        BlocksInvData {
            bitlen: 0,
            block_bitvec: vec![],
            microblocks_bitvec: vec![]
        }
    }

    pub fn compress_bools(bits: &Vec<bool>) -> Vec<u8> {
        let mut bitvec = vec![];
        for i in 0..(bits.len() / 8) {
            let mut next_octet = 0;
            for j in 0..8 {
                if bits[8*i + j] {
                    next_octet |= 1 << j;
                }
            }
            bitvec.push(next_octet);
        }
        if bits.len() % 8 != 0 {
            let mut last_octet = 0;
            let idx = (bits.len() as u64) & 0xfffffffffffffff8;     // (bits.len() / 8) * 8
            for (j, bit) in bits[(idx as usize)..].iter().enumerate() {
                if *bit {
                    last_octet |= 1 << j;
                }
            }
            bitvec.push(last_octet);
        }
        bitvec
    }

    #[cfg(test)]
    pub fn has_ith_block(&self, block_index: u16) -> bool {
        if block_index >= self.bitlen {
            return false;
        }

        let idx = block_index / 8;
        let bit = block_index % 8;
        (self.block_bitvec[idx as usize] & (1 << bit)) != 0
    }
    
    #[cfg(test)]
    pub fn has_ith_microblock_stream(&self, block_index: u16) -> bool {
        if block_index >= self.bitlen {
            return false;
        }

        let idx = block_index / 8;
        let bit = block_index % 8;
        (self.microblocks_bitvec[idx as usize] & (1 << bit)) != 0
    }
}

impl StacksMessageCodec for (ConsensusHash, BurnchainHeaderHash) {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.0)?;
        write_next(fd, &self.1)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<(ConsensusHash, BurnchainHeaderHash), net_error> {
        let consensus_hash : ConsensusHash = read_next(fd)?;
        let burn_header_hash: BurnchainHeaderHash = read_next(fd)?;
        Ok((consensus_hash, burn_header_hash))
    }
}

impl StacksMessageCodec for BlocksAvailableData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.available)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<BlocksAvailableData, net_error> {
        let available : Vec<(ConsensusHash, BurnchainHeaderHash)> = read_next_at_most::<_, (ConsensusHash, BurnchainHeaderHash)>(fd, BLOCKS_AVAILABLE_MAX_LEN)?;
        Ok(BlocksAvailableData {
            available: available
        })
    }
}

impl BlocksAvailableData {
    pub fn new() -> BlocksAvailableData {
        BlocksAvailableData {
            available: vec![]
        }
    }

    pub fn try_push(&mut self, ch: ConsensusHash, bhh: BurnchainHeaderHash) -> Result<(), net_error> {
        if self.available.len() < BLOCKS_AVAILABLE_MAX_LEN as usize {
            self.available.push((ch, bhh));
            return Ok(())
        }
        else {
            return Err(net_error::InvalidMessage);
        }
    }
}

impl StacksMessageCodec for (BurnchainHeaderHash, StacksBlock) {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.0)?;
        write_next(fd, &self.1)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<(BurnchainHeaderHash, StacksBlock), net_error> {
        let bhh : BurnchainHeaderHash = read_next(fd)?;
        let block = {
            let mut bound_read = BoundReader::from_reader(fd, MAX_BLOCK_LEN as u64);
            read_next(&mut bound_read)
        }?;

        Ok((bhh, block))
    }
}

impl BlocksData {
    pub fn new() -> BlocksData {
        BlocksData {
            blocks: vec![]
        }
    }

    pub fn push(&mut self, bhh: BurnchainHeaderHash, block: StacksBlock) -> () {
        self.blocks.push((bhh, block))
    }
}

impl StacksMessageCodec for BlocksData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.blocks)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<BlocksData, net_error> {
        let blocks : Vec<(BurnchainHeaderHash, StacksBlock)> = {
            // loose upper-bound
            let mut bound_read = BoundReader::from_reader(fd, MAX_MESSAGE_LEN as u64);
            read_next_at_most::<_, (BurnchainHeaderHash, StacksBlock)>(&mut bound_read, BLOCKS_PUSHED_MAX)
        }?;

        // only valid if there are no dups
        // TODO(PoX): replace burn_header_hash with consensus_hash
        let mut present = HashSet::new();
        for (burn_header_hash, _block) in blocks.iter() {
            if present.contains(burn_header_hash) {
                // no dups allowed
                return Err(net_error::DeserializeError("Invalid BlocksData: duplicate block".to_string()));
            }

            present.insert((*burn_header_hash).clone());
        }

        Ok(BlocksData {
            blocks
        })
    }
}

impl StacksMessageCodec for MicroblocksData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.index_anchor_block)?;
        write_next(fd, &self.microblocks)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R)-> Result<MicroblocksData, net_error> {
        let index_anchor_block = read_next(fd)?;
        let microblocks : Vec<StacksMicroblock> = {
            // loose upper-bound
            let mut bound_read = BoundReader::from_reader(fd, MAX_MESSAGE_LEN as u64);
            read_next(&mut bound_read)
        }?;
        
        Ok(MicroblocksData {
            index_anchor_block,
            microblocks
        })
    }
}

impl NeighborAddress {
    pub fn from_neighbor(n: &Neighbor) -> NeighborAddress {
        NeighborAddress {
            addrbytes: n.addr.addrbytes.clone(),
            port: n.addr.port,
            public_key_hash: Hash160::from_data(&n.public_key.to_bytes_compressed()[..])
        }
    }
}

impl StacksMessageCodec for NeighborAddress {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.addrbytes)?;
        write_next(fd, &self.port)?;
        write_next(fd, &self.public_key_hash)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<NeighborAddress, net_error> {
        let addrbytes: PeerAddress      = read_next(fd)?;
        let port : u16                  = read_next(fd)?;
        let public_key_hash: Hash160    = read_next(fd)?;

        Ok(NeighborAddress {
            addrbytes,
            port,
            public_key_hash
        })
    }
}

impl StacksMessageCodec for NeighborsData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.neighbors)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<NeighborsData, net_error> {
        // don't allow list of more than the pre-set number of neighbors
        let neighbors : Vec<NeighborAddress> = read_next_at_most::<_, NeighborAddress>(fd, MAX_NEIGHBORS_DATA_LEN)?;
        Ok(NeighborsData {
            neighbors
        })
    }
}

impl HandshakeData {
    pub fn from_local_peer(local_peer: &LocalPeer) -> HandshakeData {
        let (addrbytes, port) = match local_peer.public_ip_address {
            Some((ref public_addrbytes, ref port)) => {
                (public_addrbytes.clone(), *port)
            },
            None => {
                (local_peer.addrbytes.clone(), local_peer.port)
            }
        };

        // transmit the empty string if our data URL compels us to bind to the anynet address
        let data_url = 
            if local_peer.data_url.has_routable_host() {
                local_peer.data_url.clone()
            }
            else if let Some(data_port) = local_peer.data_url.get_port() {
                // deduce from public IP
                UrlString::try_from(format!("http://{}", addrbytes.to_socketaddr(data_port)).as_str()).unwrap()
            }
            else {
                // unroutable, so don't bother
                UrlString::try_from("").unwrap()
            };

        HandshakeData {
            addrbytes: addrbytes,
            port: port,
            services: local_peer.services,
            node_public_key: StacksPublicKeyBuffer::from_public_key(&Secp256k1PublicKey::from_private(&local_peer.private_key)),
            expire_block_height: local_peer.private_key_expire,
            data_url: data_url
        }
    }
}

impl StacksMessageCodec for HandshakeData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.addrbytes)?;
        write_next(fd, &self.port)?;
        write_next(fd, &self.services)?;
        write_next(fd, &self.node_public_key)?;
        write_next(fd, &self.expire_block_height)?;
        write_next(fd, &self.data_url)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<HandshakeData, net_error> {
        let addrbytes: PeerAddress                  = read_next(fd)?;
        let port : u16                              = read_next(fd)?;
        let services : u16                          = read_next(fd)?;
        let node_public_key : StacksPublicKeyBuffer = read_next(fd)?;
        let expire_block_height : u64               = read_next(fd)?;
        let data_url : UrlString                    = read_next(fd)?;
        Ok(HandshakeData {
            addrbytes,
            port,
            services,
            node_public_key,
            expire_block_height,
            data_url
        })
    }
}

impl HandshakeAcceptData {
    pub fn new(local_peer: &LocalPeer, heartbeat_interval: u32) -> HandshakeAcceptData {
        HandshakeAcceptData {
            handshake: HandshakeData::from_local_peer(local_peer),
            heartbeat_interval: heartbeat_interval,
        }
    }
}

impl StacksMessageCodec for HandshakeAcceptData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.handshake)?;
        write_next(fd, &self.heartbeat_interval)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<HandshakeAcceptData, net_error> {
        let handshake : HandshakeData               = read_next(fd)?;
        let heartbeat_interval : u32                = read_next(fd)?;
        Ok(HandshakeAcceptData {
            handshake,
            heartbeat_interval,
        })
    }
}

impl NackData {
    pub fn new(error_code: u32) -> NackData {
        NackData {
            error_code
        }
    }
}

impl StacksMessageCodec for NackData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.error_code)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<NackData, net_error> {
        let error_code : u32 = read_next(fd)?;
        Ok(NackData {
            error_code
        })
    }
}

impl PingData {
    pub fn new() -> PingData {
        let mut rng = rand::thread_rng();
        let n = rng.gen();
        PingData {
            nonce: n
        }
    }
}

impl StacksMessageCodec for PingData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.nonce)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<PingData, net_error> {
        let nonce : u32 = read_next(fd)?;
        Ok(PingData {
            nonce
        })
    }
}

impl PongData {
    pub fn from_ping(p: &PingData) -> PongData {
        PongData {
            nonce: p.nonce
        }
    }
}

impl StacksMessageCodec for PongData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.nonce)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<PongData, net_error> {
        let nonce: u32 = read_next(fd)?;
        Ok(PongData {
            nonce
        })
    }
}

impl StacksMessageCodec for NatPunchData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.addrbytes)?;
        write_next(fd, &self.port)?;
        write_next(fd, &self.nonce)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<NatPunchData, net_error> {
        let addrbytes : PeerAddress = read_next(fd)?;
        let port : u16 = read_next(fd)?;
        let nonce : u32 = read_next(fd)?;
        Ok(NatPunchData {
            addrbytes,
            port,
            nonce,
        })
    }
}

impl StacksMessageCodec for RelayData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.peer)?;
        write_next(fd, &self.seq)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<RelayData, net_error> {
        let peer : NeighborAddress          = read_next(fd)?;
        let seq : u32                       = read_next(fd)?;
        Ok(RelayData {
            peer,
            seq,
        })
    }
}

impl StacksMessageType {
    pub fn get_message_id(&self) -> StacksMessageID {
        match *self {
            StacksMessageType::Handshake(ref _m) => StacksMessageID::Handshake,
            StacksMessageType::HandshakeAccept(ref _m) => StacksMessageID::HandshakeAccept,
            StacksMessageType::HandshakeReject => StacksMessageID::HandshakeReject,
            StacksMessageType::GetNeighbors => StacksMessageID::GetNeighbors,
            StacksMessageType::Neighbors(ref _m) => StacksMessageID::Neighbors,
            StacksMessageType::GetBlocksInv(ref _m) => StacksMessageID::GetBlocksInv,
            StacksMessageType::BlocksInv(ref _m) => StacksMessageID::BlocksInv,
            StacksMessageType::BlocksAvailable(ref _m) => StacksMessageID::BlocksAvailable,
            StacksMessageType::MicroblocksAvailable(ref _m) => StacksMessageID::MicroblocksAvailable,
            StacksMessageType::Blocks(ref _m) => StacksMessageID::Blocks,
            StacksMessageType::Microblocks(ref _m) => StacksMessageID::Microblocks,
            StacksMessageType::Transaction(ref _m) => StacksMessageID::Transaction,
            StacksMessageType::Nack(ref _m) => StacksMessageID::Nack,
            StacksMessageType::Ping(ref _m) => StacksMessageID::Ping,
            StacksMessageType::Pong(ref _m) => StacksMessageID::Pong,
            StacksMessageType::NatPunchRequest(ref _m) => StacksMessageID::NatPunchRequest,
            StacksMessageType::NatPunchReply(ref _m) => StacksMessageID::NatPunchReply,
        }
    }

    pub fn get_message_name(&self) -> &'static str {
        match *self {
            StacksMessageType::Handshake(ref _m) => "Handshake",
            StacksMessageType::HandshakeAccept(ref _m) => "HandshakeAccept",
            StacksMessageType::HandshakeReject => "HandshakeReject",
            StacksMessageType::GetNeighbors => "GetNeighbors",
            StacksMessageType::Neighbors(ref _m) => "Neighbors",
            StacksMessageType::GetBlocksInv(ref _m) => "GetBlocksInv",
            StacksMessageType::BlocksInv(ref _m) => "BlocksInv",
            StacksMessageType::BlocksAvailable(ref _m) => "BlocksAvailable",
            StacksMessageType::MicroblocksAvailable(ref _m) => "MicroblocksAvailable",
            StacksMessageType::Blocks(ref _m) => "Blocks",
            StacksMessageType::Microblocks(ref _m) => "Microblocks",
            StacksMessageType::Transaction(ref _m) => "Transaction",
            StacksMessageType::Nack(ref _m) => "Nack",
            StacksMessageType::Ping(ref _m) => "Ping",
            StacksMessageType::Pong(ref _m) => "Pong",
            StacksMessageType::NatPunchRequest(ref _m) => "NatPunchRequest",
            StacksMessageType::NatPunchReply(ref _m) => "NatPunchReply",
        }
    }
}

impl StacksMessageCodec for StacksMessageID {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &(*self as u8))
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<StacksMessageID, net_error> {
        let as_u8 : u8 = read_next(fd)?;
        let id = match as_u8 {
            x if x == StacksMessageID::Handshake as u8 => StacksMessageID::Handshake,
            x if x == StacksMessageID::HandshakeAccept as u8 => StacksMessageID::HandshakeAccept,
            x if x == StacksMessageID::HandshakeReject as u8 => StacksMessageID::HandshakeReject,
            x if x == StacksMessageID::GetNeighbors as u8 => StacksMessageID::GetNeighbors,
            x if x == StacksMessageID::Neighbors as u8 => StacksMessageID::Neighbors,
            x if x == StacksMessageID::GetBlocksInv as u8 => StacksMessageID::GetBlocksInv,
            x if x == StacksMessageID::BlocksInv as u8 => StacksMessageID::BlocksInv,
            x if x == StacksMessageID::BlocksAvailable as u8 => StacksMessageID::BlocksAvailable,
            x if x == StacksMessageID::MicroblocksAvailable as u8 => StacksMessageID::MicroblocksAvailable,
            x if x == StacksMessageID::Blocks as u8 => StacksMessageID::Blocks,
            x if x == StacksMessageID::Microblocks as u8 => StacksMessageID::Microblocks,
            x if x == StacksMessageID::Transaction as u8 => StacksMessageID::Transaction,
            x if x == StacksMessageID::Nack as u8 => StacksMessageID::Nack,
            x if x == StacksMessageID::Ping as u8 => StacksMessageID::Ping,
            x if x == StacksMessageID::Pong as u8 => StacksMessageID::Pong,
            x if x == StacksMessageID::NatPunchRequest as u8 => StacksMessageID::NatPunchRequest,
            x if x == StacksMessageID::NatPunchReply as u8 => StacksMessageID::NatPunchReply,
            _ => { return Err(net_error::DeserializeError("Unknown message ID".to_string())); }
        };
        Ok(id)
    }
}       

impl StacksMessageCodec for StacksMessageType {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &(self.get_message_id() as u8))?;
        match *self {
            StacksMessageType::Handshake(ref m) => write_next(fd, m)?,
            StacksMessageType::HandshakeAccept(ref m) => write_next(fd, m)?,
            StacksMessageType::HandshakeReject => {},
            StacksMessageType::GetNeighbors => {},
            StacksMessageType::Neighbors(ref m) => write_next(fd, m)?,
            StacksMessageType::GetBlocksInv(ref m) => write_next(fd, m)?,
            StacksMessageType::BlocksInv(ref m) => write_next(fd, m)?,
            StacksMessageType::BlocksAvailable(ref m) => write_next(fd, m)?,
            StacksMessageType::MicroblocksAvailable(ref m) => write_next(fd, m)?,
            StacksMessageType::Blocks(ref m) => write_next(fd, m)?,
            StacksMessageType::Microblocks(ref m) => write_next(fd, m)?,
            StacksMessageType::Transaction(ref m) => write_next(fd, m)?,
            StacksMessageType::Nack(ref m) => write_next(fd, m)?,
            StacksMessageType::Ping(ref m) => write_next(fd, m)?,
            StacksMessageType::Pong(ref m) => write_next(fd, m)?,
            StacksMessageType::NatPunchRequest(ref nonce) => write_next(fd, nonce)?,
            StacksMessageType::NatPunchReply(ref m) => write_next(fd, m)?,
        }
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<StacksMessageType, net_error> {
        let message_id : StacksMessageID = read_next(fd)?;
        let message = match message_id {
            StacksMessageID::Handshake => { let m : HandshakeData = read_next(fd)?; StacksMessageType::Handshake(m) },
            StacksMessageID::HandshakeAccept => { let m : HandshakeAcceptData = read_next(fd)?; StacksMessageType::HandshakeAccept(m) },
            StacksMessageID::HandshakeReject => { StacksMessageType::HandshakeReject },
            StacksMessageID::GetNeighbors => { StacksMessageType::GetNeighbors },
            StacksMessageID::Neighbors => { let m : NeighborsData = read_next(fd)?; StacksMessageType::Neighbors(m) },
            StacksMessageID::GetBlocksInv => { let m : GetBlocksInv = read_next(fd)?; StacksMessageType::GetBlocksInv(m) },
            StacksMessageID::BlocksInv => { let m : BlocksInvData = read_next(fd)?; StacksMessageType::BlocksInv(m) },
            StacksMessageID::BlocksAvailable => { let m : BlocksAvailableData = read_next(fd)?; StacksMessageType::BlocksAvailable(m) },
            StacksMessageID::MicroblocksAvailable => { let m : BlocksAvailableData = read_next(fd)?; StacksMessageType::MicroblocksAvailable(m) },
            StacksMessageID::Blocks => { let m : BlocksData = read_next(fd)?; StacksMessageType::Blocks(m) },
            StacksMessageID::Microblocks => { let m : MicroblocksData = read_next(fd)?; StacksMessageType::Microblocks(m) },
            StacksMessageID::Transaction => { let m : StacksTransaction = read_next(fd)?; StacksMessageType::Transaction(m) },
            StacksMessageID::Nack => { let m : NackData = read_next(fd)?; StacksMessageType::Nack(m) },
            StacksMessageID::Ping => { let m : PingData = read_next(fd)?; StacksMessageType::Ping(m) },
            StacksMessageID::Pong => { let m : PongData = read_next(fd)?; StacksMessageType::Pong(m) },
            StacksMessageID::NatPunchRequest => { let nonce : u32 = read_next(fd)?; StacksMessageType::NatPunchRequest(nonce) },
            StacksMessageID::NatPunchReply => { let m : NatPunchData = read_next(fd)?; StacksMessageType::NatPunchReply(m) },
            StacksMessageID::Reserved => { return Err(net_error::DeserializeError("Unsupported message ID 'reserved'".to_string())); }
        };
        Ok(message)
    }
}

impl StacksMessageCodec for StacksMessage {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.preamble)?;
        write_next(fd, &self.relayers)?;
        write_next(fd, &self.payload)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<StacksMessage, net_error> {
        let preamble: Preamble = read_next(fd)?;
        if preamble.payload_len > MAX_MESSAGE_LEN - PREAMBLE_ENCODED_SIZE {
            return Err(net_error::DeserializeError("Message would be too big".to_string()));
        }

        let relayers: Vec<RelayData> = read_next_at_most::<_, RelayData>(fd, MAX_RELAYERS_LEN)?;
        let payload : StacksMessageType = read_next(fd)?;

        let message = StacksMessage {
            preamble,
            relayers,
            payload
        };
        Ok(message)
    }
}

impl StacksMessage {
    /// Create an unsigned Stacks p2p message
    pub fn new(peer_version: u32, network_id: u32, block_height: u64, consensus_hash: &ConsensusHash, stable_block_height: u64, stable_consensus_hash: &ConsensusHash, message: StacksMessageType) -> StacksMessage {
        let preamble = Preamble::new(peer_version, network_id, block_height, consensus_hash, stable_block_height, stable_consensus_hash, 0);
        StacksMessage {
            preamble: preamble, 
            relayers: vec![],
            payload: message
        }
    }

    /// Create an unsigned Stacks message
    pub fn from_chain_view(peer_version: u32, network_id: u32, chain_view: &BurnchainView, message: StacksMessageType) -> StacksMessage {
        StacksMessage::new(peer_version, network_id, chain_view.burn_block_height, &chain_view.burn_consensus_hash, chain_view.burn_stable_block_height, &chain_view.burn_stable_consensus_hash, message)
    }

    /// represent as neighbor key 
    pub fn to_neighbor_key(&self, addrbytes: &PeerAddress, port: u16) -> NeighborKey {
        NeighborKey {
            peer_version: self.preamble.peer_version,
            network_id: self.preamble.network_id,
            addrbytes: addrbytes.clone(),
            port: port
        }
    }

    /// Sign the stacks message
    fn do_sign(&mut self, private_key: &Secp256k1PrivateKey) -> Result<(), net_error> {
        let mut message_bits = vec![];
        self.relayers.consensus_serialize(&mut message_bits)?;
        self.payload.consensus_serialize(&mut message_bits)?;

        self.preamble.payload_len = message_bits.len() as u32;
        self.preamble.sign(&message_bits[..], private_key)
    }

    /// Sign the StacksMessage.  The StacksMessage must _not_ have any relayers (i.e. we're
    /// originating this messsage).
    pub fn sign(&mut self, seq: u32, private_key: &Secp256k1PrivateKey) -> Result<(), net_error> {
        if self.relayers.len() > 0 {
            return Err(net_error::InvalidMessage);
        }
        self.preamble.seq = seq;
        self.do_sign(private_key)
    }

    /// Sign the StacksMessage and add ourselves as a relayer.
    pub fn sign_relay(&mut self, private_key: &Secp256k1PrivateKey, our_seq: u32, our_addr: &NeighborAddress) -> Result<(), net_error> {
        while self.relayers.len() >= (MAX_RELAYERS_LEN as usize) {
            // remove (old) nodes at the front
            self.relayers.remove(0);
        }
        
        // don't sign if signed more than once 
        for relayer in &self.relayers {
            if relayer.peer.public_key_hash == our_addr.public_key_hash {
                return Err(net_error::InvalidMessage);
            }
        }

        // save relayer state 
        let our_relay = RelayData {
            peer: our_addr.clone(),
            seq: self.preamble.seq,
        };

        self.relayers.push(our_relay);
        self.preamble.seq = our_seq;
        self.do_sign(private_key)
    }

    pub fn deserialize_body<R: Read>(fd: &mut R) -> Result<(Vec<RelayData>, StacksMessageType), net_error> {
        let relayers: Vec<RelayData>    = read_next_at_most::<_, RelayData>(fd, MAX_RELAYERS_LEN)?;
        let payload : StacksMessageType = read_next(fd)?;
        Ok((relayers, payload))
    }

    /// Verify this message by treating the public key buffer as a secp256k1 public key.
    /// Fails if:
    /// * the signature doesn't match
    /// * the buffer doesn't encode a secp256k1 public key
    pub fn verify_secp256k1(&self, public_key: &StacksPublicKeyBuffer) -> Result<(), net_error> {
        let secp256k1_pubkey = public_key.to_public_key()?;
        
        let mut message_bits = vec![];
        self.relayers.consensus_serialize(&mut message_bits)?;
        self.payload.consensus_serialize(&mut message_bits)?;

        let mut p = self.preamble.clone();
        p.verify(&message_bits, &secp256k1_pubkey).and_then(|_m| Ok(()))
    }
}

impl MessageSequence for StacksMessage {
    fn request_id(&self) -> u32 {
        self.preamble.seq
    }

    fn get_message_name(&self) -> &'static str {
        self.payload.get_message_name()
    }
}

impl StacksP2P {
    pub fn new() -> StacksP2P {
        StacksP2P {}
    }
}

impl ProtocolFamily for StacksP2P {
    type Preamble = Preamble;
    type Message = StacksMessage;

    /// How big can a P2P preamble get?
    fn preamble_size_hint(&mut self) -> usize {
        PREAMBLE_ENCODED_SIZE as usize
    }
    
    /// How long is an encoded message payload going to be, if we can tell at all?
    fn payload_len(&mut self, preamble: &Preamble) -> Option<usize> {
        Some(preamble.payload_len as usize)
    }

    /// StacksP2P deals with Preambles
    fn read_preamble(&mut self, buf: &[u8]) -> Result<(Preamble, usize), net_error> {
        if buf.len() < PREAMBLE_ENCODED_SIZE as usize {
            return Err(net_error::UnderflowError("Not enough bytes to form a P2P preamble".to_string()));
        }

        let preamble : Preamble = read_next(&mut &buf[0..(PREAMBLE_ENCODED_SIZE as usize)])?;
        Ok((preamble, PREAMBLE_ENCODED_SIZE as usize))
    }
    
    /// StacksP2P messages are never streamed, since we always know how long they are.
    /// This should be unreachable, since payload_len() always returns Some(...)
    fn stream_payload<R: Read>(&mut self, _preamble: &Preamble, _fd: &mut R) -> Result<(Option<(StacksMessage, usize)>, usize), net_error> {
        panic!("BUG: tried to stream a StacksP2P message, even though their lengths are always known")
    }

    /// StacksP2P deals with StacksMessages
    fn read_payload(&mut self, preamble: &Preamble, bytes: &[u8]) -> Result<(StacksMessage, usize), net_error> {
        if bytes.len() < preamble.payload_len as usize {
            return Err(net_error::UnderflowError("Not enough bytes to form a StacksMessage".to_string()));
        }

        let mut cursor = io::Cursor::new(&bytes[0..(preamble.payload_len as usize)]);
        let (relayers, payload) = StacksMessage::deserialize_body(&mut cursor)?;
        let message = StacksMessage {
            preamble: preamble.clone(),
            relayers: relayers,
            payload: payload
        };
        Ok((message, cursor.position() as usize))
    }

    fn verify_payload_bytes(&mut self, key: &StacksPublicKey, preamble: &Preamble, bytes: &[u8]) -> Result<(), Error> {
        preamble.clone().verify(&bytes[0..(preamble.payload_len as usize)], key).and_then(|_m| Ok(()))
    }

    fn write_message<W: Write>(&mut self, fd: &mut W, message: &StacksMessage) -> Result<(), net_error> {
        message.consensus_serialize(fd)
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

    use util::hash::hex_bytes;
    use util::secp256k1::*;
    
    fn check_overflow<T>(r: Result<T, net_error>) -> bool {
        match r {
            Ok(_) => {
                test_debug!("did not get an overflow error, or any error");
                false
            },
            Err(e) => match e {
                net_error::OverflowError(_) => true,
                _ => {
                    test_debug!("did not get an overflow error, but got {:?}", &e);
                    false
                }
            }
        }
    }

    fn check_underflow<T>(r: Result<T, net_error>)  -> bool {
        match r {
            Ok(_) => {
                test_debug!("did not get an underflow error, or any error");
                false
            },
            Err(e) => match e {
                net_error::UnderflowError(_) => true,
                _ => {
                    test_debug!("did not get an underflow error, but got {:?}", &e);
                    false
                }
            }
        }
    }

    fn check_deserialize<T: std::fmt::Debug>(r: Result<T, net_error>) -> bool {
        match r {
            Ok(m) => {
                test_debug!("deserialized {:?}", &m);
                false
            },
            Err(e) => match e {
                net_error::DeserializeError(_) => true,
                _ => false
            }
        }
    }

    fn check_deserialize_failure<T: StacksMessageCodec + fmt::Debug + Clone + PartialEq>(obj: &T) -> bool {
        let mut bytes : Vec<u8> = vec![];
        obj.consensus_serialize(&mut bytes).unwrap();
        check_deserialize(T::consensus_deserialize(&mut &bytes[..]))
    }
    
    pub fn check_codec_and_corruption<T : StacksMessageCodec + fmt::Debug + Clone + PartialEq>(obj: &T, bytes: &Vec<u8>) -> () {
        // obj should serialize to bytes
        let mut write_buf : Vec<u8> = Vec::with_capacity(bytes.len());
        obj.consensus_serialize(&mut write_buf).unwrap();
        assert_eq!(write_buf, *bytes);
       
        // bytes should deserialize to obj
        let read_buf : Vec<u8> = write_buf.clone();
        let res = T::consensus_deserialize(&mut &read_buf[..]);
        match res {
            Ok(out) => {
                assert_eq!(out, *obj);
            },
            Err(e) => {
                test_debug!("\nFailed to parse to {:?}: {:?}", obj, bytes);
                test_debug!("error: {:?}", &e);
                assert!(false);
            }
        }

        // short message shouldn't parse, but should EOF
        if write_buf.len() > 0 {
            let mut short_buf = write_buf.clone();
            let short_len = short_buf.len() - 1;
            short_buf.truncate(short_len);

            let underflow_res = T::consensus_deserialize(&mut &short_buf[..]);
            match underflow_res {
                Ok(oops) => {
                    test_debug!("\nMissing Underflow: Parsed {:?}\nFrom {:?}\n", &oops, &write_buf[0..short_len].to_vec());
                }
                Err(net_error::ReadError(io_error)) => match io_error.kind() {
                    io::ErrorKind::UnexpectedEof => {}
                    _ => {
                        test_debug!("Got unexpected I/O error: {:?}", &io_error);
                        assert!(false);
                    }
                },
                Err(e) => {
                    test_debug!("Got unexpected Net error: {:?}", &e);
                    assert!(false);
                }
            };
        }
    }

    #[test]
    fn codec_primitive_types() {
        check_codec_and_corruption::<u8>(&0x01, &vec![0x01]);
        check_codec_and_corruption::<u16>(&0x0203, &vec![0x02, 0x03]);
        check_codec_and_corruption::<u32>(&0x04050607, &vec![0x04, 0x05, 0x06, 0x07]);
        check_codec_and_corruption::<u64>(&0x08090a0b0c0d0e0f, &vec![0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]);
    }

    #[test]
    fn codec_primitive_vector() {
        check_codec_and_corruption::<Vec<u8>>(&vec![], &vec![0x00, 0x00, 0x00, 0x00]);
        check_codec_and_corruption::<Vec<u8>>(&vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09], &vec![0x00, 0x00, 0x00, 0x0a, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09]);

        check_codec_and_corruption::<Vec<u16>>(&vec![], &vec![0x00, 0x00, 0x00, 0x00]);
        check_codec_and_corruption::<Vec<u16>>(&vec![0xf000, 0xf101, 0xf202, 0xf303, 0xf404, 0xf505, 0xf606, 0xf707, 0xf808, 0xf909],
                                               &vec![0x00, 0x00, 0x00, 0x0a,
                                                     0xf0, 0x00, 0xf1, 0x01, 0xf2, 0x02, 0xf3, 0x03, 0xf4, 0x04, 0xf5, 0x05, 0xf6, 0x06, 0xf7, 0x07, 0xf8, 0x08, 0xf9, 0x09]);

        check_codec_and_corruption::<Vec<u32>>(&vec![], &vec![0x00, 0x00, 0x00, 0x00]);
        check_codec_and_corruption::<Vec<u32>>(&vec![0xa0b0f000,
                                                    0xa1b1f101,
                                                    0xa2b2f202,
                                                    0xa3b3f303,
                                                    0xa4b4f404,
                                                    0xa5b5f505,
                                                    0xa6b6f606,
                                                    0xa7b7f707,
                                                    0xa8b8f808,
                                                    0xa9b9f909],
                                               &vec![0x00, 0x00, 0x00, 0x0a,
                                                    0xa0, 0xb0, 0xf0, 0x00,
                                                    0xa1, 0xb1, 0xf1, 0x01,
                                                    0xa2, 0xb2, 0xf2, 0x02,
                                                    0xa3, 0xb3, 0xf3, 0x03,
                                                    0xa4, 0xb4, 0xf4, 0x04,
                                                    0xa5, 0xb5, 0xf5, 0x05,
                                                    0xa6, 0xb6, 0xf6, 0x06,
                                                    0xa7, 0xb7, 0xf7, 0x07,
                                                    0xa8, 0xb8, 0xf8, 0x08,
                                                    0xa9, 0xb9, 0xf9, 0x09]);

        check_codec_and_corruption::<Vec<u64>>(&vec![], &vec![0x00, 0x00, 0x00, 0x00]);
        check_codec_and_corruption::<Vec<u64>>(&vec![0x1020304050607080,
                                                    0x1121314151617181,
                                                    0x1222324252627282,
                                                    0x1323334353637383,
                                                    0x1424344454647484,
                                                    0x1525354555657585,
                                                    0x1626364656667686,
                                                    0x1727374757677787,
                                                    0x1828384858687888],
                                               &vec![0x00, 0x00, 0x00, 0x09,
                                                    0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80,
                                                    0x11, 0x21, 0x31, 0x41, 0x51, 0x61, 0x71, 0x81,
                                                    0x12, 0x22, 0x32, 0x42, 0x52, 0x62, 0x72, 0x82,
                                                    0x13, 0x23, 0x33, 0x43, 0x53, 0x63, 0x73, 0x83,
                                                    0x14, 0x24, 0x34, 0x44, 0x54, 0x64, 0x74, 0x84,
                                                    0x15, 0x25, 0x35, 0x45, 0x55, 0x65, 0x75, 0x85,
                                                    0x16, 0x26, 0x36, 0x46, 0x56, 0x66, 0x76, 0x86,
                                                    0x17, 0x27, 0x37, 0x47, 0x57, 0x67, 0x77, 0x87,
                                                    0x18, 0x28, 0x38, 0x48, 0x58, 0x68, 0x78, 0x88]);

    }

    #[test]
    fn codec_Preamble() {
        let preamble = Preamble {
            peer_version: 0x01020304,
            network_id: 0x05060708,
            seq: 0x090a0b0c,
            burn_block_height: 0x00001122,
            burn_consensus_hash: ConsensusHash::from_bytes(&hex_bytes("1111111111111111111111111111111111111111").unwrap()).unwrap(),
            burn_stable_block_height: 0x00001111,
            burn_stable_consensus_hash: ConsensusHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222").unwrap()).unwrap(),
            additional_data: 0x33333333,
            signature: MessageSignature::from_raw(&vec![0x44; 65]),
            payload_len: 0x000007ff,
        };
        let preamble_bytes : Vec<u8> = vec![
            // peer_version
            0x01, 0x02, 0x03, 0x04,
            // network_id
            0x05, 0x06, 0x07, 0x08,
            // seq
            0x09, 0x0a, 0x0b, 0x0c,
            // burn_block_height
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22,
            // burn_consensus_hash
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            // stable_burn_block_height
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x11, 0x11,
            // stable_burn_consensus_hash
            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
            // additional_data
            0x33, 0x33, 0x33, 0x33,
            // signature
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
            0x44, 0x44, 0x44, 0x44, 0x44,
            // payload_len
            0x00, 0x00, 0x07, 0xff
        ];

        assert_eq!(preamble_bytes.len() as u32, PREAMBLE_ENCODED_SIZE);
        check_codec_and_corruption::<Preamble>(&preamble, &preamble_bytes);
    }

    #[test]
    fn codec_GetBlocksInv() {
        let getblocksdata = GetBlocksInv {
            consensus_hash: ConsensusHash([0x55; 20]),
            num_blocks: 32
        };

        let getblocksdata_bytes : Vec<u8> = vec![
            // consensus hash
            0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
            // num blocks
            0x00, 0x20
        ];

        check_codec_and_corruption::<GetBlocksInv>(&getblocksdata, &getblocksdata_bytes);

        // should fail to decode if the block range is too big 
        let getblocksdata_range_too_big = GetBlocksInv {
            consensus_hash: ConsensusHash([0x55; 20]),
            num_blocks: (BLOCKS_INV_DATA_MAX_BITLEN + 1) as u16,
        };

        assert!(check_deserialize_failure::<GetBlocksInv>(&getblocksdata_range_too_big));
    }

    #[test]
    fn codec_BlocksInvData() {
        // maximially big BlocksInvData
        let maximal_bitvec = vec![0xffu8; (BLOCKS_INV_DATA_MAX_BITLEN / 8) as usize];
        let mut too_big_bitvec : Vec<u8> = vec![];
        for i in 0..BLOCKS_INV_DATA_MAX_BITLEN+1 {
            too_big_bitvec.push(0xff);
        }
        
        let maximal_blocksinvdata = BlocksInvData {
            bitlen: BLOCKS_INV_DATA_MAX_BITLEN  as u16,
            block_bitvec: maximal_bitvec.clone(),
            microblocks_bitvec: maximal_bitvec.clone(),
        };

        let mut maximal_blocksinvdata_bytes : Vec<u8> = vec![];
        // bitlen 
        maximal_blocksinvdata_bytes.append(&mut (BLOCKS_INV_DATA_MAX_BITLEN as u16).to_be_bytes().to_vec());
        // block bitvec
        maximal_blocksinvdata_bytes.append(&mut (BLOCKS_INV_DATA_MAX_BITLEN / 8).to_be_bytes().to_vec());
        maximal_blocksinvdata_bytes.append(&mut maximal_bitvec.clone());
        // microblock bitvec
        maximal_blocksinvdata_bytes.append(&mut (BLOCKS_INV_DATA_MAX_BITLEN / 8).to_be_bytes().to_vec());
        maximal_blocksinvdata_bytes.append(&mut maximal_bitvec.clone());

        assert!((maximal_blocksinvdata_bytes.len() as u32) < MAX_MESSAGE_LEN);

        check_codec_and_corruption::<BlocksInvData>(&maximal_blocksinvdata, &maximal_blocksinvdata_bytes);
        
        // should fail to decode if the bitlen is too big 
        let too_big_blocksinvdata = BlocksInvData {
            bitlen: (BLOCKS_INV_DATA_MAX_BITLEN + 1) as u16,
            block_bitvec: too_big_bitvec.clone(),
            microblocks_bitvec: too_big_bitvec.clone(),
        };
        assert!(check_deserialize_failure::<BlocksInvData>(&too_big_blocksinvdata));

        // should fail to decode if the bitlen doesn't match the bitvec
        let long_bitlen = BlocksInvData {
            bitlen: 1,
            block_bitvec: vec![0xff, 0x01],
            microblocks_bitvec: vec![0xff, 0x01],
        };
        assert!(check_deserialize_failure::<BlocksInvData>(&long_bitlen));

        let short_bitlen = BlocksInvData {
            bitlen: 9,
            block_bitvec: vec![0xff],
            microblocks_bitvec: vec![0xff],
        };
        assert!(check_deserialize_failure::<BlocksInvData>(&short_bitlen));

        // empty 
        let empty_inv = BlocksInvData {
            bitlen: 0,
            block_bitvec: vec![],
            microblocks_bitvec: vec![],
        };
        let empty_inv_bytes = vec![
            // bitlen
            0x00, 0x00, 0x00, 0x00,
            // bitvec 
            0x00, 0x00, 0x00, 0x00,
            // microblock bitvec
            0x00, 0x00, 0x00, 0x00
        ];

        check_codec_and_corruption::<BlocksInvData>(&maximal_blocksinvdata, &maximal_blocksinvdata_bytes);
    }

    #[test]
    fn codec_NeighborAddress() {
        let data = NeighborAddress {
            addrbytes: PeerAddress([0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]),
            port: 12345,
            public_key_hash: Hash160::from_bytes(&hex_bytes("1111111111111111111111111111111111111111").unwrap()).unwrap(),
        };
        let bytes = vec![
            // addrbytes 
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            // port 
            0x30, 0x39,
            // public key hash 
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11
        ];

        assert_eq!(bytes.len() as u32, NEIGHBOR_ADDRESS_ENCODED_SIZE);
        check_codec_and_corruption::<NeighborAddress>(&data, &bytes);
    }

    #[test]
    fn codec_NeighborsData() {
        let data = NeighborsData {
            neighbors: vec![
                NeighborAddress {
                    addrbytes: PeerAddress([0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]),
                    port: 12345,
                    public_key_hash: Hash160::from_bytes(&hex_bytes("1111111111111111111111111111111111111111").unwrap()).unwrap(),
                },
                NeighborAddress {
                    addrbytes: PeerAddress([0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f]),
                    port: 23456,
                    public_key_hash: Hash160::from_bytes(&hex_bytes("2222222222222222222222222222222222222222").unwrap()).unwrap(),
                },
            ]
        };
        let bytes = vec![
            // length 
            0x00, 0x00, 0x00, 0x02,
            // addrbytes 
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            // port 
            0x30, 0x39,
            // public key hash 
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            // addrbytes 
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
            // port 
            0x5b, 0xa0,
            // public key hash 
            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22
        ];

        check_codec_and_corruption::<NeighborsData>(&data, &bytes);
    }

    #[test]
    fn codec_HandshakeData() {
        let data = HandshakeData {
            addrbytes: PeerAddress([0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]),
            port: 12345,
            services: 0x0001,
            node_public_key: StacksPublicKeyBuffer::from_bytes(&hex_bytes("034e316be04870cef1795fba64d581cf64bad0c894b01a068fb9edf85321dcd9bb").unwrap()).unwrap(),
            expire_block_height: 0x0102030405060708,
            data_url: UrlString::try_from("https://the-new-interwebs.com/data").unwrap()
        };
        let mut bytes = vec![
            // addrbytes 
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            // port 
            0x30, 0x39,
            // services 
            0x00, 0x01,
            // public key 
            0x03, 0x4e, 0x31, 0x6b, 0xe0, 0x48, 0x70, 0xce, 0xf1, 0x79, 0x5f, 0xba, 0x64, 0xd5, 0x81, 0xcf, 0x64, 0xba, 0xd0, 0xc8, 0x94, 0xb0, 0x1a, 0x06, 0x8f, 0xb9, 0xed, 0xf8, 0x53, 0x21, 0xdc, 0xd9, 0xbb,
            // expire block height 
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
        ];
        // data URL
        bytes.push(data.data_url.len() as u8);
        bytes.extend_from_slice(data.data_url.as_bytes());

        check_codec_and_corruption::<HandshakeData>(&data, &bytes);
    }

    #[test]
    fn codec_HandshakeAcceptData() {
        let data = HandshakeAcceptData {
            handshake: HandshakeData { 
                addrbytes: PeerAddress([0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]),
                port: 12345,
                services: 0x0001,
                node_public_key: StacksPublicKeyBuffer::from_bytes(&hex_bytes("034e316be04870cef1795fba64d581cf64bad0c894b01a068fb9edf85321dcd9bb").unwrap()).unwrap(),
                expire_block_height: 0x0102030405060708,
                data_url: UrlString::try_from("https://the-new-interwebs.com/data").unwrap()
            },
            heartbeat_interval: 0x01020304,
        };
        let mut bytes = vec![
            // addrbytes 
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            // port 
            0x30, 0x39,
            // services 
            0x00, 0x01,
            // public key 
            0x03, 0x4e, 0x31, 0x6b, 0xe0, 0x48, 0x70, 0xce, 0xf1, 0x79, 0x5f, 0xba, 0x64, 0xd5, 0x81, 0xcf, 0x64, 0xba, 0xd0, 0xc8, 0x94, 0xb0, 0x1a, 0x06, 0x8f, 0xb9, 0xed, 0xf8, 0x53, 0x21, 0xdc, 0xd9, 0xbb,
            // expire block height 
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
        ];
        // data URL
        bytes.push(data.handshake.data_url.len() as u8);
        bytes.extend_from_slice(data.handshake.data_url.as_bytes());

        bytes.extend_from_slice(&[
            // heartbeat 
            0x01, 0x02, 0x03, 0x04,
        ]);

        check_codec_and_corruption::<HandshakeAcceptData>(&data, &bytes);
    }

    #[test]
    fn codec_NackData() {
        let data = NackData {
            error_code: 0x01020304,
        };
        let bytes = vec![
            // error code 
            0x01, 0x02, 0x03, 0x04
        ];

        check_codec_and_corruption::<NackData>(&data, &bytes);
    }

    #[test]
    fn codec_RelayData() {
        let data = RelayData {
            peer: NeighborAddress {
                addrbytes: PeerAddress([0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]),
                port: 12345,
                public_key_hash: Hash160::from_bytes(&hex_bytes("1111111111111111111111111111111111111111").unwrap()).unwrap(),
            },
            seq: 0x01020304,
        };
        let bytes = vec![
            // peer.addrbytes
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            // peer.port
            0x30, 0x39,
            // peer.public_key_hash
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            // seq
            0x01, 0x02, 0x03, 0x04,
        ];

        check_codec_and_corruption::<RelayData>(&data, &bytes);
    }

    #[test]
    fn codec_BlocksAvailable() {
        let data = BlocksAvailableData {
            available: vec![(ConsensusHash([0x11; 20]), BurnchainHeaderHash([0x22; 32])), (ConsensusHash([0x33; 20]), BurnchainHeaderHash([0x44; 32]))]
        };
        let bytes = vec![
            // length
            0x00, 0x00, 0x00, 0x02,
            // first tuple
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
            // second tuple
            0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
        ];

        check_codec_and_corruption::<BlocksAvailableData>(&data, &bytes);
    }

    #[test]
    fn codec_NatPunch() {
        let data = NatPunchData {
            addrbytes: PeerAddress([0x1; 16]),
            port: 0x1234,
            nonce: 0x56789abc
        };
        let bytes = vec![
            // peer address
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            // port
            0x12, 0x34,
            // nonce
            0x56, 0x78, 0x9a, 0xbc
        ];

        check_codec_and_corruption::<NatPunchData>(&data, &bytes);
    }

    #[test]
    fn codec_StacksMessage() {
        let payloads: Vec<StacksMessageType> = vec![
            StacksMessageType::Handshake(HandshakeData {
                addrbytes: PeerAddress([0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]),
                port: 12345,
                services: 0x0001,
                node_public_key: StacksPublicKeyBuffer::from_bytes(&hex_bytes("034e316be04870cef1795fba64d581cf64bad0c894b01a068fb9edf85321dcd9bb").unwrap()).unwrap(),
                expire_block_height: 0x0102030405060708,
                data_url: UrlString::try_from("https://the-new-interwebs.com:4008/the-data").unwrap()
            }),
            StacksMessageType::HandshakeAccept(HandshakeAcceptData {
                heartbeat_interval: 0x01020304,
                handshake: HandshakeData {
                    addrbytes: PeerAddress([0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]),
                    port: 12345,
                    services: 0x0001,
                    node_public_key: StacksPublicKeyBuffer::from_bytes(&hex_bytes("034e316be04870cef1795fba64d581cf64bad0c894b01a068fb9edf85321dcd9bb").unwrap()).unwrap(),
                    expire_block_height: 0x0102030405060708,
                    data_url: UrlString::try_from("https://the-new-interwebs.com:4008/the-data").unwrap()
                },
            }),
            StacksMessageType::HandshakeReject,
            StacksMessageType::GetNeighbors,
            StacksMessageType::Neighbors(NeighborsData {
                neighbors: vec![
                    NeighborAddress {
                        addrbytes: PeerAddress([0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]),
                        port: 12345,
                        public_key_hash: Hash160::from_bytes(&hex_bytes("1111111111111111111111111111111111111111").unwrap()).unwrap(),
                    },
                    NeighborAddress {
                        addrbytes: PeerAddress([0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f]),
                        port: 23456,
                        public_key_hash: Hash160::from_bytes(&hex_bytes("2222222222222222222222222222222222222222").unwrap()).unwrap(),
                    },
                ]
            }),
            StacksMessageType::GetBlocksInv(GetBlocksInv {
                consensus_hash: ConsensusHash([0x55; 20]),
                num_blocks: 32,
            }),
            StacksMessageType::BlocksInv(BlocksInvData {
                bitlen: 2,
                block_bitvec: vec![0x03],
                microblocks_bitvec: vec![0x03],
            }),
            StacksMessageType::BlocksAvailable(BlocksAvailableData {
                available: vec![(ConsensusHash([0x11; 20]), BurnchainHeaderHash([0x22; 32])), (ConsensusHash([0x33; 20]), BurnchainHeaderHash([0x44; 32]))]
            }),
            StacksMessageType::MicroblocksAvailable(BlocksAvailableData {
                available: vec![(ConsensusHash([0x11; 20]), BurnchainHeaderHash([0x22; 32])), (ConsensusHash([0x33; 20]), BurnchainHeaderHash([0x44; 32]))]
            }),
            // TODO: Blocks
            // TODO: Microblocks
            // TODO: Transaction
            StacksMessageType::Nack(NackData {
                error_code: 0x01020304
            }),
            StacksMessageType::Ping(PingData {
                nonce: 0x01020304
            }),
            StacksMessageType::Pong(PongData {
                nonce: 0x01020304
            }),
            StacksMessageType::NatPunchRequest(0x12345678),
            StacksMessageType::NatPunchReply(NatPunchData {
                addrbytes: PeerAddress([0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]),
                port: 12345,
                nonce: 0x12345678
            }),
        ];

        let mut maximal_relayers : Vec<RelayData> = vec![];
        let mut too_many_relayers : Vec<RelayData> = vec![];
        for i in 0..MAX_RELAYERS_LEN {
            let next_relayer = RelayData {
                peer: NeighborAddress {
                    addrbytes: PeerAddress([i as u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]),
                    port: 12345 + (i as u16),
                    public_key_hash: Hash160::from_bytes(&hex_bytes("1111111111111111111111111111111111111111").unwrap()).unwrap(),
                },
                seq: 0x01020304 + i,
            };
            too_many_relayers.push(next_relayer.clone());
            maximal_relayers.push(next_relayer);
        }
        too_many_relayers.push(RelayData {
            peer: NeighborAddress {
                addrbytes: PeerAddress([0xff, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]),
                port: 65535,
                public_key_hash: Hash160::from_bytes(&hex_bytes("1111111111111111111111111111111111111111").unwrap()).unwrap(),
            },
            seq: 0x010203ff,
        });

        let mut relayers_bytes : Vec<u8> = vec![];
        maximal_relayers.consensus_serialize(&mut relayers_bytes).unwrap();

        let mut too_many_relayer_bytes : Vec<u8> = vec![];
        too_many_relayers.consensus_serialize(&mut too_many_relayer_bytes).unwrap();

        for payload in &payloads {
            // just testing codec; don't worry about signatures
            // (only payload_len must be valid)
            let mut payload_bytes : Vec<u8> = vec![];
            payload.consensus_serialize(&mut payload_bytes).unwrap();

            let preamble = Preamble {
                peer_version: 0x01020304,
                network_id: 0x05060708,
                seq: 0x090a0b0c,
                burn_block_height: 0x00001122,
                burn_consensus_hash: ConsensusHash::from_bytes(&hex_bytes("1111111111111111111111111111111111111111").unwrap()).unwrap(),
                burn_stable_block_height: 0x00001111,
                burn_stable_consensus_hash: ConsensusHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222").unwrap()).unwrap(),
                additional_data: 0x33333333,
                signature: MessageSignature::from_raw(&vec![0x44; 65]),
                payload_len: (relayers_bytes.len() + payload_bytes.len()) as u32,
            };

            let stacks_message = StacksMessage {
                preamble: preamble.clone(),
                relayers: maximal_relayers.clone(),
                payload: payload.clone()
            };

            let mut stacks_message_bytes : Vec<u8> = vec![];
            preamble.consensus_serialize(&mut stacks_message_bytes).unwrap();
            stacks_message_bytes.append(&mut relayers_bytes.clone());
            stacks_message_bytes.append(&mut payload_bytes.clone());

            test_debug!("Test {}-byte relayer, {}-byte payload {:?}", relayers_bytes.len(), payload_bytes.len(), &payload);
            check_codec_and_corruption::<StacksMessage>(&stacks_message, &stacks_message_bytes);

            // can't have too many relayers 
            let mut preamble_too_many_relayers = preamble.clone();
            preamble_too_many_relayers.payload_len = (too_many_relayer_bytes.len() + payload_bytes.len() + 1) as u32;

            let stacks_message_too_many_relayers = StacksMessage {
                preamble: preamble_too_many_relayers.clone(),
                relayers: too_many_relayers.clone(),
                payload: payload.clone()
            };
            assert!(check_deserialize_failure(&stacks_message_too_many_relayers));
        }
    }

    #[test]
    fn codec_sign_and_verify() {
        let privkey = Secp256k1PrivateKey::new();
        let pubkey_buf = StacksPublicKeyBuffer::from_public_key(&Secp256k1PublicKey::from_private(&privkey));

        let mut ping = StacksMessage::new(PEER_VERSION, 0x9abcdef0,
                                          12345,
                                          &ConsensusHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
                                          12339,
                                          &ConsensusHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
                                          StacksMessageType::Ping(PingData { nonce: 0x01020304 }));

        ping.sign(444, &privkey).unwrap();
        ping.verify_secp256k1(&pubkey_buf).unwrap();
    }

    #[test]
    fn codec_stacks_public_key_roundtrip() {
        for i in 0..100 {
            let privkey = Secp256k1PrivateKey::new();
            let pubkey = Secp256k1PublicKey::from_private(&privkey);

            let pubkey_buf = StacksPublicKeyBuffer::from_public_key(&pubkey);
            let pubkey_2 = pubkey_buf.to_public_key().unwrap();

            assert_eq!(pubkey, pubkey_2);
        }
    }

    #[test]
    fn blocks_inv_compress_bools() {
        let block_flags = vec![
            true,
            true,
            true,
            false,
            false,
            false,
            false,
            true,

            true,
            false,
            true
        ];
        let block_bitvec = BlocksInvData::compress_bools(&block_flags);
        assert_eq!(block_bitvec, vec![0x87, 0x05]);

        let short_block_flags = vec![
            true,
            false,
            true
        ];
        let short_block_bitvec = BlocksInvData::compress_bools(&short_block_flags);
        assert_eq!(short_block_bitvec, vec![0x05]);
    }
} 
