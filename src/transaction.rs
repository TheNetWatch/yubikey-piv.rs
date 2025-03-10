//! YubiKey PC/SC transactions

use crate::{Buffer, CB_BUF_MAX, CB_OBJ_MAX, MgmKey, ObjectId, PIV_AID, YK_AID, apdu::Response, apdu::{Ins, StatusWords, APDU}, error::Error, key::{AlgorithmId, SlotId}, mgm::DES_LEN_3DES, serialization::*, yubikey::*};
use log::{error, trace};
use std::convert::TryInto;
use zeroize::Zeroizing;

const CB_PIN_MAX: usize = 8;

pub(crate) enum ChangeRefAction {
    ChangePin,
    ChangePuk,
    UnblockPin,
}

/// Exclusive transaction with the YubiKey's PC/SC card.
pub(crate) struct Transaction<'tx> {
    inner: pcsc::Transaction<'tx>,
}

impl<'tx> Transaction<'tx> {
    /// Create a new transaction with the given card.
    pub fn new(card: &'tx mut pcsc::Card) -> Result<Self, Error> {
        Ok(Transaction {
            inner: card.transaction()?,
        })
    }

    /// Transmit a single serialized APDU to the card this transaction is open
    /// with and receive a response.
    ///
    /// This is a wrapper for the raw `SCardTransmit` function and operates on
    /// single APDU messages at a time. For larger messages that need to be
    /// split into multiple APDUs, use the [`Transaction::transfer_data`]
    /// method instead.
    pub fn transmit(&self, send_buffer: &[u8], recv_len: usize) -> Result<Vec<u8>, Error> {
        trace!(">>> {:?}", send_buffer);

        let mut recv_buffer = vec![0u8; recv_len];

        let len = self
            .inner
            .transmit(send_buffer, recv_buffer.as_mut())?
            .len();

        recv_buffer.truncate(len);
        Ok(recv_buffer)
    }

    /// Select application.
    pub fn select_application(&self) -> Result<(), Error> {
        let response = APDU::new(Ins::SelectApplication)
            .p1(0x04)
            .data(&PIV_AID)
            .transmit(self, 0xFF)
            .map_err(|e| {
                error!("failed communicating with card: '{}'", e);
                e
            })?;

        if !response.is_success() {
            error!(
                "failed selecting application: {:04x}",
                response.status_words().code()
            );
            return Err(Error::GenericError);
        }

        Ok(())
    }

    /// Get the version of the PIV application installed on the YubiKey.
    pub fn get_version(&self) -> Result<Version, Error> {
        // get version from device
        let response = APDU::new(Ins::GetVersion).transmit(self, 261)?;

        if !response.is_success() {
            return Err(Error::GenericError);
        }

        if response.data().len() < 3 {
            return Err(Error::SizeError);
        }

        Ok(Version::new(response.data()[..3].try_into().unwrap()))
    }

    /// Get YubiKey device serial number.
    pub fn get_serial(&self, version: Version) -> Result<Serial, Error> {
        let response = if version.major < 5 {
            // YK4 requires switching to the yk applet to retrieve the serial
            let sw = APDU::new(Ins::SelectApplication)
                .p1(0x04)
                .data(&YK_AID)
                .transmit(self, 0xFF)?
                .status_words();

            if !sw.is_success() {
                error!("failed selecting yk application: {:04x}", sw.code());
                return Err(Error::GenericError);
            }

            let resp = APDU::new(0x01).p1(0x10).transmit(self, 0xFF)?;

            if !resp.is_success() {
                error!(
                    "failed retrieving serial number: {:04x}",
                    resp.status_words().code()
                );
                return Err(Error::GenericError);
            }

            // reselect the PIV applet
            let sw = APDU::new(Ins::SelectApplication)
                .p1(0x04)
                .data(&PIV_AID)
                .transmit(self, 0xFF)?
                .status_words();

            if !sw.is_success() {
                error!("failed selecting application: {:04x}", sw.code());
                return Err(Error::GenericError);
            }

            resp
        } else {
            // YK5 implements getting the serial as a PIV applet command (0xf8)
            let resp = APDU::new(Ins::GetSerial).transmit(self, 0xFF)?;

            if !resp.is_success() {
                error!(
                    "failed retrieving serial number: {:04x}",
                    resp.status_words().code()
                );
                return Err(Error::GenericError);
            }

            resp
        };

        response.data()[..4]
            .try_into()
            .map(|serial| Serial::from(u32::from_be_bytes(serial)))
            .map_err(|_| Error::SizeError)
    }

    /// Verify device PIN.
    pub fn verify_pin(&self, pin: &[u8]) -> Result<(), Error> {
        if pin.len() > CB_PIN_MAX {
            return Err(Error::SizeError);
        }

        let mut query = APDU::new(Ins::Verify);
        query.params(0x00, 0x80);

        // Empty pin means we are querying the number of retries. We set no data in this
        // case; if we instead sent [0xff; CB_PIN_MAX] it would count as an attempt and
        // decrease the retry counter.
        if !pin.is_empty() {
            let mut data = Zeroizing::new([0xff; CB_PIN_MAX]);
            data[0..pin.len()].copy_from_slice(pin);
            query.data(data.as_ref());
        }

        let response = query.transmit(self, 261)?;

        match response.status_words() {
            StatusWords::Success => Ok(()),
            StatusWords::AuthBlockedError => Err(Error::WrongPin { tries: 0 }),
            StatusWords::VerifyFailError { tries } => Err(Error::WrongPin { tries }),
            _ => Err(Error::GenericError),
        }
    }

    /// Change the PIN.
    pub fn change_ref(
        &self,
        action: ChangeRefAction,
        current_pin: &[u8],
        new_pin: &[u8],
    ) -> Result<(), Error> {
        if current_pin.len() > CB_PIN_MAX || new_pin.len() > CB_PIN_MAX {
            return Err(Error::SizeError);
        }

        const PIN: u8 = 0x80;
        const PUK: u8 = 0x81;

        let templ = match action {
            ChangeRefAction::ChangePin => [0, Ins::ChangeReference.code(), 0, PIN],
            ChangeRefAction::ChangePuk => [0, Ins::ChangeReference.code(), 0, PUK],
            ChangeRefAction::UnblockPin => [0, Ins::ResetRetry.code(), 0, PIN],
        };

        let mut indata = Zeroizing::new([0xff; CB_PIN_MAX * 2]);
        indata[0..current_pin.len()].copy_from_slice(current_pin);
        indata[CB_PIN_MAX..CB_PIN_MAX + new_pin.len()].copy_from_slice(new_pin);

        let status_words = self
            .transfer_data(&templ, indata.as_ref(), 0xFF)?
            .status_words();

        match status_words {
            StatusWords::Success => Ok(()),
            StatusWords::AuthBlockedError => Err(Error::PinLocked),
            StatusWords::VerifyFailError { tries } => Err(Error::WrongPin { tries }),
            _ => {
                error!(
                    "failed changing pin, token response code: {:x}.",
                    status_words.code()
                );
                Err(Error::GenericError)
            }
        }
    }

    /// Set the management key (MGM).

    pub fn set_mgm_key(&self, new_key: &MgmKey, require_touch: bool) -> Result<(), Error> {
        let p2 = if require_touch { 0xfe } else { 0xff };

        let mut data = [0u8; DES_LEN_3DES + 3];
        data[0] = ALGO_3DES;
        data[1] = KEY_CARDMGM;
        data[2] = DES_LEN_3DES as u8;
        data[3..3 + DES_LEN_3DES].copy_from_slice(new_key.as_ref());

        let status_words = APDU::new(Ins::SetMgmKey)
            .params(0xff, p2)
            .data(&data)
            .transmit(self, 261)?
            .status_words();

        if !status_words.is_success() {
            return Err(Error::GenericError);
        }

        Ok(())
    }

    /// Perform a YubiKey operation which requires authentication.
    ///
    /// This is the common backend for all public key encryption and signing
    /// operations.
    // TODO(tarcieri): refactor this to be less gross/coupled.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn authenticated_command(
        &self,
        sign_in: &[u8],
        algorithm: AlgorithmId,
        key: SlotId,
        decipher: bool,
    ) -> Result<Buffer, Error> {
        let in_len = sign_in.len();
        let mut indata = [0u8; 1024];
        let templ = [0, Ins::Authenticate.code(), algorithm.into(), key.into()];

        match algorithm {
            AlgorithmId::Rsa1024 | AlgorithmId::Rsa2048 => {
                let key_len = if let AlgorithmId::Rsa1024 = algorithm {
                    128
                } else {
                    256
                };

                if in_len != key_len {
                    return Err(Error::SizeError);
                }
            }
            AlgorithmId::EccP256 | AlgorithmId::EccP384 => {
                let key_len = if let AlgorithmId::EccP256 = algorithm {
                    32
                } else {
                    48
                };

                if (!decipher && (in_len > key_len)) || (decipher && (in_len != (key_len * 2) + 1))
                {
                    return Err(Error::SizeError);
                }
            }
        }

        let bytes = if in_len < 0x80 {
            1
        } else if in_len < 0xff {
            2
        } else {
            3
        };

        let offset = Tlv::write_as(&mut indata, 0x7c, in_len + bytes + 3, |buf| {
            assert_eq!(Tlv::write(buf, 0x82, &[]).expect("large enough"), 2);
            assert_eq!(
                Tlv::write(
                    &mut buf[2..],
                    match (algorithm, decipher) {
                        (AlgorithmId::EccP256, true) | (AlgorithmId::EccP384, true) => 0x85,
                        _ => 0x81,
                    },
                    sign_in
                )
                .expect("large enough"),
                1 + bytes + in_len
            );
        })?;

        let response = self
            .transfer_data(&templ, &indata[..offset], 1024)
            .map_err(|e| {
                error!("sign command failed to communicate: {}", e);
                e
            })?;

        if !response.is_success() {
            error!("failed sign command with code {:x}", response.code());

            if response.status_words() == StatusWords::SecurityStatusError {
                return Err(Error::AuthenticationError);
            } else {
                return Err(Error::GenericError);
            }
        }

        let (_, outer_tlv) = Tlv::parse(response.data())?;

        // skip the first 7c tag
        if outer_tlv.tag != 0x7c {
            error!("failed parsing signature reply (0x7c byte)");
            return Err(Error::ParseError);
        }

        let (_, inner_tlv) = Tlv::parse(outer_tlv.value)?;

        // skip the 82 tag
        if inner_tlv.tag != 0x82 {
            error!("failed parsing signature reply (0x82 byte)");
            return Err(Error::ParseError);
        }

        Ok(Buffer::new(inner_tlv.value.into()))
    }

    /// Send/receive large amounts of data to/from the YubiKey, splitting long
    /// messages into smaller APDU-sized messages (using the provided APDU
    /// template to construct them), and then sending those via
    /// [`Transaction::transmit`].
    pub fn transfer_data(
        &self,
        templ: &[u8],
        in_data: &[u8],
        max_out: usize,
    ) -> Result<Response, Error> {
        let mut in_offset = 0;
        let mut out_data = vec![];
        let mut sw;

        loop {
            let mut this_size = 0xff;

            let cla = if in_offset + 0xff < in_data.len() {
                0x10
            } else {
                this_size = in_data.len() - in_offset;
                templ[0]
            };

            trace!("going to send {} bytes in this go", this_size);

            let response = APDU::new(templ[1])
                .cla(cla)
                .params(templ[2], templ[3])
                .data(&in_data[in_offset..(in_offset + this_size)])
                .transmit(self, 261)?;

            sw = response.status_words().code();

            if !response.is_success() && (sw >> 8 != 0x61) {
                // TODO(tarcieri): is this really OK?
                return Ok(Response::new(sw.into(), out_data));
            }

            if !out_data.is_empty() && (out_data.len() - response.data().len() > max_out) {
                error!(
                    "output buffer too small: wanted to write {}, max was {}",
                    out_data.len() - response.data().len(),
                    max_out
                );

                return Err(Error::SizeError);
            }

            out_data.extend_from_slice(&response.data()[..response.data().len()]);

            in_offset += this_size;
            if in_offset >= in_data.len() {
                break;
            }
        }

        while sw >> 8 == 0x61 {
            trace!(
                "The card indicates there is {} bytes more data for us",
                sw & 0xff
            );

            let response = APDU::new(Ins::GetResponseApdu).transmit(self, 261)?;
            sw = response.status_words().code();

            if sw != StatusWords::Success.code() && (sw >> 8 != 0x61) {
                return Ok(Response::new(sw.into(), vec![]));
            }

            if out_data.len() + response.data().len() > max_out {
                error!(
                    "output buffer too small: wanted to write {}, max was {}",
                    out_data.len() + response.data().len(),
                    max_out
                );

                return Err(Error::SizeError);
            }

            out_data.extend_from_slice(&response.data()[..response.data().len()]);
        }

        Ok(Response::new(sw.into(), out_data))
    }

    /// Fetch an object.
    pub fn fetch_object(&self, object_id: ObjectId) -> Result<Buffer, Error> {
        let mut indata = [0u8; 5];
        let templ = [0, Ins::GetData.code(), 0x3f, 0xff];

        let mut inlen = indata.len();
        let indata_remaining = set_object(object_id, &mut indata);
        inlen -= indata_remaining.len();

        let response = self.transfer_data(&templ, &indata[..inlen], CB_BUF_MAX)?;

        if !response.is_success() {
            if response.status_words() == StatusWords::NotFoundError {
                return Err(Error::NotFound);
            } else {
                return Err(Error::GenericError);
            }
        }

        let (remaining, tlv) = Tlv::parse(response.data())?;

        if !remaining.is_empty() {
            error!(
                "invalid length indicated in object: total len is {} but indicated length is {}",
                tlv.value.len() + remaining.len(),
                tlv.value.len()
            );

            return Err(Error::SizeError);
        }

        Ok(Zeroizing::new(tlv.value.to_vec()))
    }

    /// Save an object.
    pub fn save_object(&self, object_id: ObjectId, indata: &[u8]) -> Result<(), Error> {
        let templ = [0, Ins::PutData.code(), 0x3f, 0xff];

        // TODO(tarcieri): replace with vector
        let mut data = [0u8; CB_BUF_MAX];

        if indata.len() > CB_OBJ_MAX {
            return Err(Error::SizeError);
        }

        let mut len = data.len();
        let mut data_remaining = set_object(object_id, &mut data);

        let offset = Tlv::write(data_remaining, 0x53, indata)?;
        data_remaining = &mut data_remaining[offset..];
        len -= data_remaining.len();

        let status_words = self
            .transfer_data(&templ, &data[..len], 255)?
            .status_words();

        match status_words {
            StatusWords::Success => Ok(()),
            StatusWords::SecurityStatusError => Err(Error::AuthenticationError),
            _ => Err(Error::GenericError),
        }
    }
}
