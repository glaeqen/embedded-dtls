use crate::{
    buffer::{AllocU16Handle, EncodingBuffer, ParseBuffer},
    cipher_suites::DtlsCipherSuite,
    client_config::ClientConfig,
    handshake::{
        extensions::{
            ClientExtensions, ClientSupportedVersions, DtlsVersions, KeyShareEntry, NamedGroup,
            NewServerExtensions, OfferedPsks, PskKeyExchangeMode, PskKeyExchangeModes, SelectedPsk,
            ServerSupportedVersion,
        },
        ClientHandshake, ClientHello, Finished, ServerHandshake, ServerHello,
    },
    integers::U48,
    key_schedule::KeySchedule,
};
use digest::OutputSizeUser;
use num_enum::TryFromPrimitive;
use rand_core::{CryptoRng, RngCore};
use std::ops::Range;
use x25519_dalek::PublicKey;

#[derive(Clone, Debug, PartialOrd, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Encryption {
    Enabled,
    Disabled,
}

/// Helper when something needs to encode or parse something differently.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum EncodeOrParse<E, P> {
    /// The encoding branch.
    Encode(E),
    /// The parsing branch.
    Parse(P),
}

/// Holds positions of key positions in the payload data. Used for transcript hashing.
pub struct RecordPayloadPositions {
    pub start: usize,
    pub binders: Option<usize>,
    pub end: usize,
}

impl RecordPayloadPositions {
    pub fn into_pre_post_binders(self, buf: &[u8]) -> Option<(&[u8], &[u8])> {
        // Calculate indices
        let start = self.start.checked_sub(buf.as_ptr() as usize)?;
        let middle = self.binders?.checked_sub(buf.as_ptr() as usize)?;
        let end = self.end.checked_sub(buf.as_ptr() as usize)?;

        debug_assert!(start < middle);
        debug_assert!(middle < end);

        // Create the sub-slices around the middle element
        Some((buf.get(start..middle)?, buf.get(middle..end)?))
    }

    pub fn into_slice(self, buf: &[u8]) -> Option<&[u8]> {
        // Calculate indices
        let start = self.start.checked_sub(buf.as_ptr() as usize)?;
        let end = self.end.checked_sub(buf.as_ptr() as usize)?;

        debug_assert!(start < end);

        buf.get(start..end)
    }
}

/// Supported client records.
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ClientRecord<'a> {
    Handshake(ClientHandshake<'a>, Encryption),
    Alert(/* Alert, */ (), Encryption),
    Ack((), Encryption),
    Heartbeat(()),
    ApplicationData(/* &'a [u8] */),
}

impl<'a> ClientRecord<'a> {
    /// Create a client hello handshake.
    pub fn encode_client_hello<CipherSuite: DtlsCipherSuite, Rng: RngCore + CryptoRng>(
        buf: &mut EncodingBuffer,
        config: &'a ClientConfig<'a>,
        public_key: &PublicKey,
        rng: &mut Rng,
        key_schedule: &mut KeySchedule<CipherSuite>,
        transcript_hasher: &mut CipherSuite::Hash,
    ) -> Result<(), ()>
    where
        Rng: RngCore + CryptoRng,
    {
        let identities = &[config.psk.clone()];
        let client_hello = ClientHello {
            version: LEGACY_DTLS_VERSION,
            legacy_session_id: &[],
            cipher_suites: &(<CipherSuite as DtlsCipherSuite>::CODE_POINT as u16).to_be_bytes(),
            extensions: ClientExtensions {
                psk_key_exchange_modes: Some(PskKeyExchangeModes {
                    ke_modes: PskKeyExchangeMode::PskDheKe,
                }),
                key_share: Some(KeyShareEntry {
                    group: NamedGroup::X25519,
                    opaque: public_key.as_bytes(),
                }),
                supported_versions: Some(ClientSupportedVersions {
                    version: DtlsVersions::V1_3,
                }),
                pre_shared_key: Some(OfferedPsks {
                    identities: EncodeOrParse::Encode(identities),
                    hash_size: EncodeOrParse::Encode(
                        <<CipherSuite as DtlsCipherSuite>::Hash as OutputSizeUser>::output_size(),
                    ),
                }),
            },
        };

        l0g::debug!("Sending client hello: {:02x?}", client_hello);

        ClientRecord::Handshake(
            ClientHandshake::ClientHello(client_hello),
            Encryption::Disabled,
        )
        .encode::<CipherSuite, Rng>(buf, key_schedule, transcript_hasher, rng)
    }

    /// Encode the record into a buffer.
    pub fn encode<'buf, CipherSuite: DtlsCipherSuite, Rng: RngCore + CryptoRng>(
        &self,
        buf: &'buf mut EncodingBuffer,
        key_schedule: &mut KeySchedule<CipherSuite>,
        transcript_hasher: &mut CipherSuite::Hash,
        rng: &mut Rng,
    ) -> Result<(), ()> {
        if !self.is_encrypted() {
            let header = DTlsPlaintextHeader {
                type_: self.content_type(),
                epoch: 0,
                sequence_number: 0.into(),
                length: 0, // To be encoded later.
            };

            // ------ Start record

            // Create record header.
            let length_allocation = header.encode(buf)?;
            let content_length = {
                let mut inner_buf = buf.new_from_existing();

                // ------ Encode payload

                match self {
                    // NOTE: Each record encoder needs to update the transcript hash at their end.
                    ClientRecord::Handshake(handshake, _) => {
                        handshake.encode::<Rng, CipherSuite>(
                            &mut inner_buf,
                            key_schedule,
                            transcript_hasher,
                            rng,
                        )?;
                    }
                    ClientRecord::Alert(_, _) => todo!(),
                    ClientRecord::Ack(_, _) => todo!(),
                    ClientRecord::Heartbeat(_) => todo!(),
                    ClientRecord::ApplicationData() => todo!(),
                }

                inner_buf.len()
            };

            length_allocation.set(buf, content_length as u16);

            // ------ Finish record

            Ok(())
        } else {
            todo!()
        }
    }

    pub fn parse(buf: &mut ParseBuffer<'a>) -> Option<(Self, RecordPayloadPositions)> {
        // Parse record.

        let record_header = DTlsPlaintextHeader::parse(buf)?;

        // TODO: Check if encrypted.
        let encrypted = Encryption::Disabled;

        let record_payload = buf.pop_slice(record_header.length.into())?;
        l0g::trace!("Got record: {:?}", record_header);

        let mut buf = ParseBuffer::new(record_payload);
        let start = buf.current_pos_ptr();

        let (ret, binders_pos) = match record_header.type_ {
            ContentType::Handshake => {
                let (handshake, binders_pos) = ClientHandshake::parse(&mut buf)?;

                (ClientRecord::Handshake(handshake, encrypted), binders_pos)
            }
            ContentType::Ack => todo!(),
            ContentType::Heartbeat => todo!(),
            ContentType::Alert => todo!(),
            ContentType::ApplicationData => todo!(),
            ContentType::ChangeCipherSpec => todo!(),
        };

        let end = buf.current_pos_ptr();

        Some((
            ret,
            RecordPayloadPositions {
                start,
                binders: binders_pos,
                end,
            },
        ))
    }

    fn is_encrypted(&self) -> bool {
        match self {
            ClientRecord::Handshake(_, Encryption::Disabled) => false,
            ClientRecord::Alert(_, Encryption::Disabled) => false,
            ClientRecord::Ack(_, Encryption::Disabled) => false,
            _ => true,
        }
    }

    fn content_type(&self) -> ContentType {
        match self {
            ClientRecord::Handshake(_, _) => ContentType::Handshake,
            ClientRecord::Alert(_, _) => ContentType::Alert,
            ClientRecord::Ack(_, _) => ContentType::Ack,
            ClientRecord::Heartbeat(_) => ContentType::Heartbeat,
            ClientRecord::ApplicationData() => ContentType::ApplicationData,
        }
    }
}

pub(crate) trait GenericCipher {
    /// Encrypts a record.
    async fn encrypt_record(&mut self, args: CipherArguments) -> aead::Result<()>;

    /// Decrypts a record.
    async fn decrypt_record(
        &mut self,
        ciphertext_header: &DTlsCiphertextHeader,
        args: CipherArguments,
    ) -> aead::Result<()>;

    /// Returns the size of the AEAD tag.
    fn tag_size(&self) -> usize;

    /// Get the current record number and increase it.
    fn write_record_number(&mut self) -> u64;

    /// Get the current epoch number.
    fn epoch_number(&self) -> u64;
}

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct CipherArguments<'a> {
    /// The header of the ciphertext.
    pub unified_hdr: &'a mut [u8],
    /// The location of the encoded sequence number in `unified_hdr`.
    pub sequence_number_position: Range<usize>,
    /// The location of the payload (plaintext/ciphertext) with the tag at the end.
    pub payload_with_tag: &'a mut [u8],
}

/// Supported client records.
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ServerRecord<'a> {
    Handshake(ServerHandshake<'a>, Encryption),
    Alert(/* Alert, */ (), Encryption),
    Ack((), Encryption),
    Heartbeat(()),
    ApplicationData(/* &'a [u8] */),
}

impl<'a> ServerRecord<'a> {
    /// Create a client hello handshake.
    pub async fn encode_server_hello<'buf>(
        legacy_session_id: &[u8],
        supported_version: DtlsVersions,
        public_key: PublicKey,
        selected_cipher_suite: u16,
        selected_psk_identity: u16,
        key_schedule: &mut impl GenericCipher,
        buf: &'buf mut EncodingBuffer<'_>,
    ) -> Result<(), ()> {
        let server_hello = ServerHello {
            version: LEGACY_DTLS_VERSION,
            legacy_session_id_echo: &legacy_session_id,
            cipher_suite_index: selected_cipher_suite,
            extensions: NewServerExtensions {
                selected_supported_version: Some(ServerSupportedVersion {
                    version: supported_version,
                }),
                key_share: Some(KeyShareEntry {
                    group: NamedGroup::X25519,
                    opaque: public_key.as_bytes(),
                }),
                pre_shared_key: Some(SelectedPsk {
                    selected_identity: selected_psk_identity,
                }),
            },
        };

        l0g::debug!("Sending server hello: {server_hello:02x?}");

        ServerRecord::Handshake(
            ServerHandshake::ServerHello(server_hello),
            Encryption::Disabled,
        )
        .encode(buf, key_schedule)
        .await
    }

    /// Create a server's finished message.
    pub fn finished(transcript_hash: &'a [u8]) -> Self {
        let finished = ServerRecord::Handshake(
            ServerHandshake::ServerFinished(Finished {
                verify: transcript_hash,
            }),
            Encryption::Enabled,
        );

        l0g::debug!("Sending server finished: {finished:02x?}");

        finished
    }

    /// Encode the record into a buffer. Returns (packet to send, content to hash).
    pub async fn encode<'buf>(
        &self,
        buf: &'buf mut EncodingBuffer<'_>,
        cipher: &mut impl GenericCipher,
    ) -> Result<(), ()> {
        let epoch = cipher.epoch_number();
        let record_number = cipher.write_record_number();

        l0g::error!("encoding record with record_number {record_number}");

        if self.is_encrypted() {
            encode_ciphertext(
                buf,
                cipher,
                self.content_type(),
                epoch as u8,
                CiphertextSequenceNumber::Long(record_number as u16),
                |buf| self.encode_content(buf),
            )
            .await
        } else {
            encode_plaintext(
                buf,
                self.content_type(),
                epoch as u16,
                record_number.into(), // TODO: Check if we should protect here
                |buf| self.encode_content(buf),
            )
        }
    }

    fn encode_content<'buf>(&self, buf: &'buf mut EncodingBuffer) -> Result<(), ()> {
        match self {
            // NOTE: Each record encoder needs to update the transcript hash at their end.
            ServerRecord::Handshake(handshake, _) => handshake.encode(buf),
            ServerRecord::Alert(_, _) => todo!(),
            ServerRecord::Heartbeat(_) => todo!(),
            ServerRecord::Ack(_, _) => todo!(),
            ServerRecord::ApplicationData() => todo!(),
        }
    }

    fn is_encrypted(&self) -> bool {
        match self {
            ServerRecord::Handshake(_, Encryption::Disabled)
            | ServerRecord::Alert(_, Encryption::Disabled)
            | ServerRecord::Ack(_, Encryption::Disabled) => false,
            _ => true,
        }
    }

    fn content_type(&self) -> ContentType {
        match self {
            ServerRecord::Handshake(_, _) => ContentType::Handshake,
            ServerRecord::Alert(_, _) => ContentType::Alert,
            ServerRecord::Ack(_, _) => ContentType::Ack,
            ServerRecord::Heartbeat(_) => ContentType::Heartbeat,
            ServerRecord::ApplicationData() => ContentType::ApplicationData,
        }
    }

    /// Parse a `ServerRecord`.
    pub fn parse(
        buf: &mut ParseBuffer<'a>,
        cipher: &mut impl GenericCipher,
    ) -> Option<(Self, RecordPayloadPositions)> {
        let record_header = DTlsHeader::parse(buf)?;
        l0g::trace!("Got record: {:?}", record_header);

        match record_header {
            DTlsHeader::Plaintext(record_header) => {
                parse_plaintext(&record_header, buf, |content_type, buf| {
                    Self::parse_content(content_type, buf)
                })
            }
            DTlsHeader::Ciphertext(record_header) => {
                parse_ciphertext(&record_header, buf, cipher, |content_type, buf| {
                    Self::parse_content(content_type, buf)
                })
            }
        }
    }

    fn parse_content(content_type: ContentType, buf: &mut ParseBuffer<'a>) -> Option<Self> {
        Some(match content_type {
            ContentType::Handshake => {
                let handshake = ServerHandshake::parse(buf)?;
                ServerRecord::Handshake(handshake, Encryption::Disabled)
            }
            ContentType::Ack => todo!(),
            ContentType::Heartbeat => todo!(),
            ContentType::Alert => todo!(),
            ContentType::ApplicationData => todo!(),
            ContentType::ChangeCipherSpec => todo!(),
        })
    }
}

pub trait Test {
    async fn encrypt(&mut self, buf: &mut [u8]);
    async fn decrypt(&mut self, buf: &mut [u8]);
}

fn to_header_and_payload_with_tag(
    buf: &mut [u8],
    header_position: Range<usize>,
    plaintext_position: Range<usize>,
    tag_position: Range<usize>,
) -> (&mut [u8], &mut [u8]) {
    l0g::trace!(
        "hp: {header_position:?}, pp: {plaintext_position:?}, tp: {tag_position:?}, bl: {}",
        buf.len()
    );

    // Enforce the ordering for tests.
    if !(header_position.start <= header_position.end
        && header_position.end <= plaintext_position.start
        && plaintext_position.start <= plaintext_position.end
        && plaintext_position.end <= tag_position.start
        && tag_position.start <= tag_position.end
        && plaintext_position.end == tag_position.start
        && tag_position.end <= buf.len())
    {
        panic!(
            "The order of data in the encryption is wrong or longer than the buffer they stem from: hp: {header_position:?}, pp: {plaintext_position:?}, tp: {tag_position:?}, bl: {}", buf.len()
        );
    }

    // We want to do this, but we can't due to the borrow checker:
    // (
    //     &mut buf[sequence_number_position],
    //     &mut buf[plaintext_position.start..tag_position.end],
    // )

    let mut curr_start = 0;

    // Extract sequence number slice.
    let (_, r) = buf.split_at_mut(header_position.start);
    curr_start += header_position.start;

    let (sequence_number, r) = r.split_at_mut(header_position.end - curr_start);
    curr_start += header_position.end - curr_start;

    // Extract plaintext slice compounded with the tag slice.
    let (_, r) = r.split_at_mut(plaintext_position.start - curr_start);
    curr_start += plaintext_position.start - curr_start;

    let (plaintext_with_tag, _) = r.split_at_mut(tag_position.end - curr_start);

    (sequence_number, plaintext_with_tag)
}

/// Protocol version definition.
pub type ProtocolVersion = [u8; 2];

/// Value used for protocol version in DTLS 1.3.
pub const LEGACY_DTLS_VERSION: ProtocolVersion = [254, 253];

/// Helper to parse DTLS headers.
#[derive(Debug)]
pub enum DTlsHeader<'a> {
    Plaintext(DTlsPlaintextHeader),
    Ciphertext(DTlsCiphertextHeader<'a>),
}

impl<'a> DTlsHeader<'a> {
    pub fn parse(buf: &mut ParseBuffer<'a>) -> Option<Self> {
        let mut pt_buf = buf.clone(); // We need 2 parse buffers.

        if let Some(h) = DTlsCiphertextHeader::parse(buf) {
            Some(DTlsHeader::Ciphertext(h))
        } else if let Some(h) = DTlsPlaintextHeader::parse(&mut pt_buf) {
            *buf = pt_buf;
            Some(DTlsHeader::Plaintext(h))
        } else {
            None
        }
    }
}

/// DTls 1.3 plaintext header.
#[derive(Debug, PartialEq, Eq)]
pub struct DTlsPlaintextHeader {
    pub type_: ContentType,
    pub epoch: u16,
    pub sequence_number: U48,
    pub length: u16,
}

impl DTlsPlaintextHeader {
    /// Encode a DTlsPlaintext header, return the allocation for the length field.
    pub fn encode(&self, buf: &mut EncodingBuffer) -> Result<AllocU16Handle, ()> {
        // DTlsPlaintext structure:
        //
        // type: ContentType,
        // legacy_record_version: ProtocolVersion,
        // epoch: u16, always 0
        // sequence_number: U48,
        // length: u16, // we don't know this yes, only alloc for it
        // fragment: opaque[length]

        buf.push_u8(self.type_ as u8)?;
        buf.extend_from_slice(&LEGACY_DTLS_VERSION)?;
        buf.push_u16_be(self.epoch)?;
        buf.push_u48_be(self.sequence_number)?;
        buf.alloc_u16() // Allocate for teh length
    }

    /// Parse a plaintext header.
    pub fn parse(buf: &mut ParseBuffer) -> Option<Self> {
        let type_ = ContentType::try_from(buf.pop_u8()?).ok()?;

        if buf.pop_slice(2)? != LEGACY_DTLS_VERSION {
            return None;
        }

        let epoch = buf.pop_u16_be()?;
        let sequence_number = buf.pop_u48_be()?;
        let length = buf.pop_u16_be()?;

        Some(Self {
            type_,
            epoch,
            sequence_number,
            length,
        })
    }
}

/// DTlsCiphertext unified header.
#[derive(Copy, Clone, Debug, PartialOrd, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct DTlsCiphertextHeader<'a> {
    /// The shortened 2-bit epoch number of this record.
    // TODO: Make epoc only representable as 2 bits.
    pub epoch: u8,
    /// The shortened sequence number of this record.
    pub sequence_number: CiphertextSequenceNumber,
    /// The optional length of this record. If it's not provided then the size is extracted from
    /// the datagram. If there are many records in a single datagram then the length is required.
    /// See section 4.2 in RFC9147 for details.
    pub length: Option<u16>,
    /// The optional connection ID.
    pub connection_id: Option<&'a [u8]>,
}

impl<'a> DTlsCiphertextHeader<'a> {
    //
    //  0 1 2 3 4 5 6 7
    // +-+-+-+-+-+-+-+-+
    // |0|0|1|C|S|L|E E|
    // +-+-+-+-+-+-+-+-+
    // | Connection ID |   Legend:
    // | (if any,      |
    // /  length as    /   C   - Connection ID (CID) present
    // |  negotiated)  |   S   - Sequence number length
    // +-+-+-+-+-+-+-+-+   L   - Length present
    // |  8 or 16 bit  |   E   - Epoch
    // |Sequence Number|
    // +-+-+-+-+-+-+-+-+
    // | 16 bit Length |
    // | (if present)  |
    // +-+-+-+-+-+-+-+-+
    //
    // struct {
    //     opaque unified_hdr[variable];
    //     opaque encrypted_record[length];
    // } DTLSCiphertext;

    /// Encode a DTlsCiphertext unified header, return the sequence number location in the buffer
    /// and allocation for the length field in case the length is not `None`.
    ///
    /// Follows section 4 in RFC9147.
    pub fn encode(
        &self,
        buf: &mut EncodingBuffer,
    ) -> Result<(Range<usize>, Option<AllocU16Handle>), ()> {
        let header = {
            let epoch = self.epoch & 0b11;
            let length = match self.length {
                Some(_) => 1 << 2,
                None => 0,
            };
            let seq_num = match self.sequence_number {
                CiphertextSequenceNumber::Short(_) => 0,
                CiphertextSequenceNumber::Long(_) => 1 << 3,
            };
            let cid = match self.connection_id {
                Some(_) => 1 << 4,
                None => 0,
            };
            0b00100000 | epoch | length | seq_num | cid
        };

        buf.push_u8(header)?;

        if let Some(cid) = self.connection_id {
            buf.extend_from_slice(cid)?;
        }

        let sequence_number_start = buf.len();
        match self.sequence_number {
            CiphertextSequenceNumber::Short(s) => buf.push_u8(s)?,
            CiphertextSequenceNumber::Long(l) => buf.push_u16_be(l)?,
        }
        let sequence_number_position = sequence_number_start..buf.len();

        if self.length.is_some() {
            Ok((sequence_number_position, Some(buf.alloc_u16()?)))
        } else {
            Ok((sequence_number_position, None))
        }
    }

    /// Parse a ciphertext header.
    pub fn parse(buf: &mut ParseBuffer) -> Option<Self> {
        let header = buf.pop_u8()?;

        // Check the header bits that this is actually a ciphertext.
        if header >> 5 != 0b001 {
            l0g::error!("Not a ciphertext, header = {header:02x}");
            return None;
        }

        let epoch = header & 0b11;
        let connection_id = if (header >> 4) & 1 != 0 {
            // TODO: No support for CID for now.
            l0g::error!("Ciphertext specified CID, we don't support that");
            return None;
        } else {
            None
        };
        let sequence_number = if (header >> 3) & 1 != 0 {
            CiphertextSequenceNumber::Long(buf.pop_u16_be()?)
        } else {
            CiphertextSequenceNumber::Short(buf.pop_u8()?)
        };
        let length = if (header >> 2) & 1 != 0 {
            Some(buf.pop_u16_be()?)
        } else {
            None
        };

        Some(Self {
            epoch,
            sequence_number,
            length,
            connection_id,
        })
    }
}

// struct {
//      opaque content[DTLSPlaintext.length];
//      ContentType type;
//      uint8 zeros[length_of_padding];
// } DTLSInnerPlaintext;

/// The payload within the `encrypted_record` in a DTLSCiphertext.
pub struct DtlsInnerPlaintext<'a> {
    pub content: &'a [u8], // Only filled on decode.
    pub type_: ContentType,
}

impl<'a> DtlsInnerPlaintext<'a> {
    /// Encode a DTlsInnerPlaintext.
    ///
    /// Follows section 4 in RFC9147.
    pub fn encode(
        type_: ContentType,
        buf: &mut EncodingBuffer,
        content_size: usize,
        aead_tag_size: usize,
    ) -> Result<(), ()> {
        buf.push_u8(type_ as u8)?;

        // In accordance with Section 4.2.3 in RFC9147 we need that the cipher text, including tag,
        // to have a minimum of 16 bytes length. Else we must pad the packet. Usually the tag is
        // large enough to not need any padding.
        let padding_size = 16usize.saturating_sub(aead_tag_size + content_size + 1);
        for _ in 0..padding_size {
            buf.push_u8(0)?;
        }

        Ok(())
    }

    /// Parse a DtlsInnerPlaintext.
    pub fn parse(buf: &mut ParseBuffer<'a>, length: usize) -> Option<Self> {
        let payload = buf.pop_slice(length)?;

        // Remove padding.
        let no_padding = Self::remove_trailing_zeros(payload);

        if no_padding.is_empty() {
            return None;
        }
        let (last, content) = no_padding.split_last().unwrap();

        Some(Self {
            content,
            type_: ContentType::try_from(*last).ok()?,
        })
    }

    fn remove_trailing_zeros(slice: &[u8]) -> &[u8] {
        if let Some(last_non_zero_pos) = slice.iter().rposition(|&x| x != 0) {
            &slice[..=last_non_zero_pos]
        } else {
            &[]
        }
    }
}

/// The two types of sequence numbers supported by the DTls ciphertext unified header.
#[derive(Copy, Clone, Debug, PartialOrd, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum CiphertextSequenceNumber {
    /// Short single byte sequence number.
    Short(u8),
    /// Long two byte sequence number.
    Long(u16),
}

impl CiphertextSequenceNumber {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() == 1 {
            Some(CiphertextSequenceNumber::Short(bytes[0]))
        } else if bytes.len() == 2 {
            Some(CiphertextSequenceNumber::Long(u16::from_be_bytes(
                bytes.try_into().unwrap(),
            )))
        } else {
            None
        }
    }
}

impl From<CiphertextSequenceNumber> for u64 {
    fn from(value: CiphertextSequenceNumber) -> Self {
        match value {
            CiphertextSequenceNumber::Short(s) => s as u64,
            CiphertextSequenceNumber::Long(l) => l as u64,
        }
    }
}

/// TLS content type. RFC 9147 - Appendix A.1
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialOrd, PartialEq, Eq, TryFromPrimitive)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ContentType {
    ChangeCipherSpec = 20,
    Alert = 21,
    Handshake = 22,
    ApplicationData = 23,
    Heartbeat = 24,
    // Tls12Cid = 25,
    Ack = 26,
}

/// Encode a plaintext.
fn encode_plaintext<'buf>(
    buf: &'buf mut EncodingBuffer,
    content_type: ContentType,
    epoch: u16,
    sequence_number: U48,
    encode_content: impl FnOnce(&mut EncodingBuffer) -> Result<(), ()>,
) -> Result<(), ()> {
    let header = DTlsPlaintextHeader {
        type_: content_type,
        epoch,
        sequence_number,
        length: 0, // To be encoded later.
    };
    // Create record header.
    let length_allocation = header.encode(buf)?;

    // ------ Start record

    let content_length = {
        let mut inner_buf = buf.new_from_existing();

        // ------ Encode payload
        encode_content(&mut inner_buf)?;

        inner_buf.len()
    };

    length_allocation.set(buf, content_length as u16);

    // ------ Finish record
    Ok(())
}

/// Encode a ciphertext.
async fn encode_ciphertext<'buf>(
    buf: &'buf mut EncodingBuffer<'_>,
    cipher: &mut impl GenericCipher,
    content_type: ContentType,
    epoch: u8,
    sequence_number: CiphertextSequenceNumber,
    encode_content: impl FnOnce(&mut EncodingBuffer) -> Result<(), ()>,
) -> Result<(), ()> {
    let buf = &mut buf.new_from_existing();

    let header_start = buf.len();

    // Create record header.
    let (sequence_number_position, length_allocation) = DTlsCiphertextHeader {
        epoch,
        sequence_number,
        length: Some(0),
        connection_id: None,
    }
    .encode(buf)?;

    l0g::debug!("encoded header = {:02x?}", buf);

    // ------ Start record

    let content_start = buf.len();
    let header_position = header_start..content_start;

    // ------ Encode payload
    encode_content(buf)?;

    let content_length = buf.len() - content_start;

    // Encode the tail of the DTLSInnerPlaintext.
    DtlsInnerPlaintext::encode(content_type, buf, content_length, cipher.tag_size())?;

    let innerplaintext_end = buf.len();

    // Allocate space for the AEAD tag.
    let aead_tag_allocation = buf.alloc_slice(cipher.tag_size())?;
    let tag_position = aead_tag_allocation.at();
    aead_tag_allocation.fill(buf, 0);

    // Write the ciphertext length to the header.
    let ciphertext_length = buf.len() - content_start;
    if let Some(length_allocation) = length_allocation {
        l0g::debug!("ciphertext length: {ciphertext_length}, {ciphertext_length:02x}, {length_allocation:?}");
        length_allocation.set(buf, ciphertext_length as u16);
    }

    // ------ Encrypt payload
    {
        let plaintext_position = content_start..innerplaintext_end as usize;

        // Split the buffer into the 2 slices.
        let (unified_hdr, payload_with_tag) =
            to_header_and_payload_with_tag(buf, header_position, plaintext_position, tag_position);

        let cipher_args = CipherArguments {
            unified_hdr,
            sequence_number_position,
            payload_with_tag,
        };

        cipher.encrypt_record(cipher_args).await.map_err(|_| ())?;
    }

    // ------ Finish record
    l0g::debug!("encoded record = {:02x?}", buf);

    Ok(())
}

fn parse_ciphertext<'a, Content>(
    header: &DTlsCiphertextHeader,
    buf: &mut ParseBuffer<'a>,
    cipher: &mut impl GenericCipher,
    parse_content: impl FnOnce(ContentType, &mut ParseBuffer<'a>) -> Option<Content>,
) -> Option<(Content, RecordPayloadPositions)> {
    // TODO: Parse encrypted record

    // cipher.decrypt_record(header, CipherArguments { unified_hdr: , sequence_number_position: , payload_with_tag:  })

    todo!()
}

fn parse_plaintext<'a, Content>(
    header: &DTlsPlaintextHeader,
    buf: &mut ParseBuffer<'a>,
    parse_content: impl FnOnce(ContentType, &mut ParseBuffer<'a>) -> Option<Content>,
) -> Option<(Content, RecordPayloadPositions)> {
    let record_payload = buf.pop_slice(header.length.into())?;

    let mut buf = ParseBuffer::new(record_payload);
    let start = buf.current_pos_ptr();

    let ret = parse_content(header.type_, &mut buf)?;

    let end = buf.current_pos_ptr();

    Some((
        ret,
        RecordPayloadPositions {
            start,
            end,
            binders: None,
        },
    ))
}

#[cfg(test)]
mod test {
    use super::{ContentType, DTlsPlaintextHeader};
    use crate::{
        buffer::{EncodingBuffer, ParseBuffer},
        record::DTlsCiphertextHeader,
    };

    #[test]
    fn plaintext_header_rount_trip() {
        let header = DTlsPlaintextHeader {
            type_: ContentType::Handshake,
            epoch: 2,
            sequence_number: 123456.into(),
            length: 321,
        };

        let mut buf = [0; 32];
        let buf = &mut EncodingBuffer::new(&mut buf);

        let len_alloc = header.encode(buf).unwrap();
        len_alloc.set(buf, header.length);

        let recv_buf: &[u8] = &buf;

        let parse_buffer = &mut ParseBuffer::new(recv_buf);
        let parsed_header = DTlsPlaintextHeader::parse(parse_buffer).unwrap();

        assert_eq!(header, parsed_header)
    }

    #[test]
    fn ciphertext_header_rount_trip() {
        let header = DTlsCiphertextHeader {
            epoch: 2,
            sequence_number: crate::record::CiphertextSequenceNumber::Long(123),
            length: Some(321),
            connection_id: None,
        };

        let mut buf = [0; 32];
        let buf = &mut EncodingBuffer::new(&mut buf);

        let (_sn_pos, len_alloc) = header.encode(buf).unwrap();
        len_alloc.unwrap().set(buf, header.length.unwrap());

        let recv_buf: &[u8] = &buf;

        let parse_buffer = &mut ParseBuffer::new(recv_buf);
        let parsed_header = DTlsCiphertextHeader::parse(parse_buffer).unwrap();

        assert_eq!(header, parsed_header)
    }
}
