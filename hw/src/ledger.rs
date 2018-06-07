// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Ledger hardware wallet module. Supports Ledger Blue and Nano S.
/// See https://github.com/LedgerHQ/blue-app-eth/blob/master/doc/ethapp.asc for protocol details.

use std::cmp::min;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use std::thread;

use ethereum_types::{H256, Address};
use ethkey::Signature;
use hidapi;
use libusb;
use parking_lot::{Mutex, RwLock};

use super::{WalletInfo, KeyPath, Device, DeviceDirection, Wallet, USB_DEVICE_CLASS_DEVICE};

/// Ledger vendor ID
const LEDGER_VID: u16 = 0x2c97;
/// Ledger product IDs: [Nano S and Blue]
const LEDGER_PIDS: [u16; 2] = [0x0000, 0x0001];

const ETH_DERIVATION_PATH_BE: [u8; 17] = [4, 0x80, 0, 0, 44, 0x80, 0, 0, 60, 0x80, 0, 0, 0, 0, 0, 0, 0]; // 44'/60'/0'/0
const ETC_DERIVATION_PATH_BE: [u8; 21] = [5, 0x80, 0, 0, 44, 0x80, 0, 0, 60, 0x80, 0x02, 0x73, 0xd0, 0x80, 0, 0, 0, 0, 0, 0, 0]; // 44'/60'/160720'/0'/0

const APDU_TAG: u8 = 0x05;
const APDU_CLA: u8 = 0xe0;
const MAX_CHUNK_SIZE: usize = 255;

const LEDGER_POLLING_INTERVAL: Duration = Duration::from_millis(500);

const HID_PACKET_SIZE: usize = 64 + HID_PREFIX_ZERO;

#[cfg(windows)] const HID_PREFIX_ZERO: usize = 1;
#[cfg(not(windows))] const HID_PREFIX_ZERO: usize = 0;

mod commands {
	pub const GET_APP_CONFIGURATION: u8 = 0x06;
	pub const GET_ETH_PUBLIC_ADDRESS: u8 = 0x02;
	pub const SIGN_ETH_TRANSACTION: u8 = 0x04;
    pub const SIGN_ETH_MESSAGE: u8 = 0x08;
}

/// Hardware wallet error.
#[derive(Debug)]
pub enum Error {
	/// Ethereum wallet protocol error.
	Protocol(&'static str),
	/// Hidapi error.
	Usb(hidapi::HidError),
	/// Libusb error
	LibUsb(libusb::Error),
	/// Device with request key is not available.
	KeyNotFound,
	/// Signing has been cancelled by user.
	UserCancel,
	/// Invalid device
	InvalidDevice,
	/// Impossible error
	ImpossibleError,
    /// No device arrived
    NoDeviceArrived,
    /// No device left
    NoDeviceLeft,
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
		match *self {
			Error::Protocol(ref s) => write!(f, "Ledger protocol error: {}", s),
			Error::Usb(ref e) => write!(f, "USB communication error: {}", e),
			Error::LibUsb(ref e) => write!(f, "LibUSB communication error: {}", e),
			Error::KeyNotFound => write!(f, "Key not found"),
			Error::UserCancel => write!(f, "Operation has been cancelled"),
			Error::InvalidDevice => write!(f, "Unsupported product was entered"),
			Error::ImpossibleError => write!(f, "Placeholder error"),
            Error::NoDeviceArrived => write!(f, "No device arrived"),
            Error::NoDeviceLeft=> write!(f, "No device left"),
		}
	}
}

impl From<hidapi::HidError> for Error {
	fn from(err: hidapi::HidError) -> Error {
		Error::Usb(err)
	}
}

impl From<libusb::Error> for Error {
	fn from(err: libusb::Error) -> Error {
		Error::LibUsb(err)
	}
}

/// Ledger device manager.
pub struct Manager {
	usb: Arc<Mutex<hidapi::HidApi>>,
	devices: RwLock<Vec<Device>>,
	key_path: RwLock<KeyPath>,
}

impl Manager {
	/// Create a new instance.
	pub fn new(hidapi: Arc<Mutex<hidapi::HidApi>>, exiting: Arc<AtomicBool>) -> Result<Arc<Manager>, libusb::Error> {
		let manager = Arc::new(Manager {
			usb: hidapi,
			devices: RwLock::new(Vec::new()),
			key_path: RwLock::new(KeyPath::Ethereum),
		});

		let usb_context = Arc::new(libusb::Context::new()?);
		let m = manager.clone();

		// Subscribe to all Ledger devices
		// This means that we need to check that the given productID is supported
		// None => LIBUSB_HOTPLUG_MATCH_ANY, in other words that all are subscribed to
		// More info can be found: <http://libusb.sourceforge.net/api-1.0/group__hotplug.html#gae6c5f1add6cc754005549c7259dc35ea>
		usb_context.register_callback(
			Some(LEDGER_VID), None, Some(USB_DEVICE_CLASS_DEVICE),
			Box::new(EventHandler::new(Arc::downgrade(&manager)))).expect("usb_callback");

		// Ledger event handler thread
		thread::Builder::new()
			.spawn(move || {
				if let Err(e) = m.update_devices(DeviceDirection::Arrived) {
					debug!(target: "hw", "Ledger couldn't connect at startup, error: {}", e);
				}
				loop {
					usb_context.handle_events(Some(Duration::from_millis(500)))
							   .unwrap_or_else(|e| debug!(target: "hw", "Ledger event handler error: {}", e));
					if exiting.load(atomic::Ordering::Acquire) {
						break;
					}
				}
			})
			.ok();

		Ok(manager)
	}


    // Transport Protocol
    //
    // Communication Channel Id (2 bytes BE) || Command Tag (1 byte) || Packet Sequence Id (2 bytes BE) || Payload
    //
    // Payload
    // 
    // APDU (2 bytes BE) || APPDU_CLA (1 byte) || APDU_INS (1 byte) || APDU_P1 (1 byte) || APDU_P2 (1 byte) || APDU_LENGTH (1 byte) || APDU_Payload
    //      
	fn send_apdu(handle: &hidapi::HidDevice, command: u8, p1: u8, p2: u8, data: &[u8]) -> Result<Vec<u8>, Error> {
		let mut offset = 0;
		let mut chunk_index = 0;
		loop {
			let mut hid_chunk: [u8; HID_PACKET_SIZE] = [0; HID_PACKET_SIZE];
			let mut chunk_size = if chunk_index == 0 { 12 } else { 5 };
			let size = min(64 - chunk_size, data.len() - offset);
			{
				let chunk = &mut hid_chunk[HID_PREFIX_ZERO..];
				&mut chunk[0..5].copy_from_slice(&[0x01, 0x01, APDU_TAG, (chunk_index >> 8) as u8, (chunk_index & 0xff) as u8 ]);

				if chunk_index == 0 {
					let data_len = data.len() + 5;
					&mut chunk[5..12].copy_from_slice(&[ (data_len >> 8) as u8, (data_len & 0xff) as u8, APDU_CLA, command, p1, p2, data.len() as u8 ]);
				}

				&mut chunk[chunk_size..chunk_size + size].copy_from_slice(&data[offset..offset + size]);
				offset += size;
				chunk_size += size;
			}
			trace!(target: "hw", "writing {:?}", &hid_chunk[..]);
			println!("write {:?}", &hid_chunk[..]);
            let n = handle.write(&hid_chunk[..])?;
			if n < chunk_size {
				return Err(Error::Protocol("Write data size mismatch"));
			}
			if offset == data.len() {
				break;
			}
			chunk_index += 1;
		}

		// Read response
		chunk_index = 0;
		let mut message_size = 0;
		let mut message = Vec::new();
		loop {
			let mut chunk: [u8; HID_PACKET_SIZE] = [0; HID_PACKET_SIZE];
			let chunk_size = handle.read(&mut chunk)?;
			trace!("read {:?}", &chunk[..]);
            println!("read: {:?}", &chunk[..]);
			if chunk_size < 5 || chunk[0] != 0x01 || chunk[1] != 0x01 || chunk[2] != APDU_TAG {
				return Err(Error::Protocol("Unexpected chunk header"));
			}
			let seq = (chunk[3] as usize) << 8 | (chunk[4] as usize);
			if seq != chunk_index {
				return Err(Error::Protocol("Unexpected chunk header"));
			}

			let mut offset = 5;
			if seq == 0 {
				// Read message size and status word.
				if chunk_size < 7 {
					return Err(Error::Protocol("Unexpected chunk header"));
				}
				message_size = (chunk[5] as usize) << 8 | (chunk[6] as usize);
				offset += 2;
			}
			message.extend_from_slice(&chunk[offset..chunk_size]);
			message.truncate(message_size);
			if message.len() == message_size {
				break;
			}
			chunk_index += 1;
		}
		if message.len() < 2 {
			return Err(Error::Protocol("No status word"));
		}
		let status = (message[message.len() - 2] as usize) << 8 | (message[message.len() - 1] as usize);
		debug!(target: "hw", "Read status {:x}", status);
		match status {
			0x6700 => Err(Error::Protocol("Incorrect length")),
			0x6982 => Err(Error::Protocol("Security status not satisfied (Canceled by user)")),
			0x6a80 => Err(Error::Protocol("Invalid data")),
			0x6a82 => Err(Error::Protocol("File not found")),
			0x6a85 => Err(Error::UserCancel),
			0x6b00 => Err(Error::Protocol("Incorrect parameters")),
			0x6d00 => Err(Error::Protocol("Not implemented. Make sure Ethereum app is running.")),
			0x6faa => Err(Error::Protocol("Your Ledger need to be unplugged")),
			0x6f00...0x6fff => Err(Error::Protocol("Internal error")),
			0x9000 => Ok(()),
			_ => Err(Error::Protocol("Unknown error")),

		}?;
		let new_len = message.len() - 2;
		message.truncate(new_len);
		Ok(message)
	}

	fn is_valid_ledger(device: &libusb::Device) -> Result<(), Error> {
		let desc = device.device_descriptor()?;
		let vendor_id = desc.vendor_id();
		let product_id = desc.product_id();

		if vendor_id == LEDGER_VID && LEDGER_PIDS.contains(&product_id) {
			Ok(())
		} else {
			Err(Error::InvalidDevice)
		}
	}

	// /// SIGN ETH PERSONAL MESSAGE
	// ///
	// /// Input:
	// ///
	// ///		CLA				INS						P1									P2			Data
	// ///		0xE0 (1 byte)	0x08 (1 byte)			0x00 (for first message)			0x00		Max 255 bytes
	// ///												0x80 (for subsequent blocks)
	// ///
	// /// Output:
	// ///
	// ///		V				R						S
	// ///		1 byte			32 bytes				32 bytes
    pub fn sign_personal_message(&self, address: &Address, msg: &[u8]) -> Result<Signature, Error> {
		let usb = self.usb.lock();
		let devices = self.devices.read();
		let device = devices.iter().find(|d| &d.info.address == address).ok_or(Error::KeyNotFound)?;
		let handle = self.open_path(|| usb.open_path(&device.path))?;

		let eth_path = &ETH_DERIVATION_PATH_BE[..];
		let etc_path = &ETC_DERIVATION_PATH_BE[..];
		let derivation_path = match *self.key_path.read() {
			KeyPath::Ethereum => eth_path,
			KeyPath::EthereumClassic => etc_path,
		};
        
        let mut chunk: [u8; MAX_CHUNK_SIZE] = [0; MAX_CHUNK_SIZE];
        // copy the key to `our buffer`
        &mut chunk[0..derivation_path.len()].copy_from_slice(derivation_path);
        let key_length = derivation_path.len();
		let mut result = Vec::new();
        let mut remaining_bytes = msg.len();
        let mut offset = 0;

        while remaining_bytes > 0 {
            let p1 = if offset == 0 { 0 } else { 0x80 };
            let p2 = 0;

            let take = min(MAX_CHUNK_SIZE - key_length, remaining_bytes);
            
            // Append transaction data to `our buffer`
            {
                let (_, dst) = &mut chunk.split_at_mut(key_length);
                let (dst, _) = &mut dst.split_at_mut(take);
                dst.copy_from_slice(&msg[offset..(offset + take)]);
            }

            result = Self::send_apdu(&handle, commands::SIGN_ETH_MESSAGE, p1, p2, &chunk)?;
            offset += take;
            remaining_bytes -= take;
        }

		if result.len() != 65 {
			return Err(Error::Protocol("Signature packet size mismatch"));
		}
		let v = (result[0] + 1) % 2;
		let r = H256::from_slice(&result[1..33]);
		let s = H256::from_slice(&result[33..65]);
		Ok(Signature::from_rsv(&r, &s, v))
    }
}

// Try to connect to the device using polling in at most the time specified by the `timeout`
fn try_connect_polling(ledger: Arc<Manager>, timeout: &Duration, device_direction: DeviceDirection) -> bool {
    let start_time = Instant::now();
    while start_time.elapsed() <= *timeout {
		if let Ok(d) = ledger.update_devices(device_direction) {
            trace!(target: "hw", "Detected {} new Ledger devices", d);
            return true;
        }
	}
    false
}

impl <'a>Wallet<'a> for Manager {
	type Error = Error;
	type Transaction = &'a [u8];

    // fn sign_transaction(&self, address: &Address, transaction: Self::Transaction) -> Result<Signature, Self::Error> {
    //     let usb = self.usb.lock();
    //     let devices = self.devices.read();
    //     let device = devices.iter().find(|d| &d.info.address == address).ok_or(Error::KeyNotFound)?;
    //     let handle = self.open_path(|| usb.open_path(&device.path))?;
    //
    //     let eth_path = &ETH_DERIVATION_PATH_BE[..];
    //     let etc_path = &ETC_DERIVATION_PATH_BE[..];
    //     let derivation_path = match *self.key_path.read() {
    //         KeyPath::Ethereum => eth_path,
    //         KeyPath::EthereumClassic => etc_path,
    //     };
    //     const MAX_CHUNK_SIZE: usize = 255;
    //     let mut chunk: [u8; MAX_CHUNK_SIZE] = [0; MAX_CHUNK_SIZE];
    //     &mut chunk[0..derivation_path.len()].copy_from_slice(derivation_path);
    //     let mut dest_offset = derivation_path.len();
    //     let mut data_pos = 0;
    //     let mut result;
    //     loop {
    //         let p1 = if data_pos == 0 { 0x00 } else { 0x80 };
    //         let dest_left = MAX_CHUNK_SIZE - dest_offset;
    //         let chunk_data_size = min(dest_left, transaction.len() - data_pos);
    //         &mut chunk[dest_offset..][0..chunk_data_size].copy_from_slice(&transaction[data_pos..][0..chunk_data_size]);
    //         result = Self::send_apdu(&handle, commands::SIGN_ETH_TRANSACTION, p1, 0, &chunk)?;
    //         // result = Self::send_apdu(&handle, commands::SIGN_ETH_TRANSACTION, p1, 0, &chunk[0..(dest_offset + chunk_data_size)])?;
    //         dest_offset = 0;
    //         data_pos += chunk_data_size;
    //         if data_pos == transaction.len() {
    //             break;
    //         }
    //     }
    //
    //     if result.len() != 65 {
    //         return Err(Error::Protocol("Signature packet size mismatch"));
    //     }
    //     let v = (result[0] + 1) % 2;
    //     let r = H256::from_slice(&result[1..33]);
    //     let s = H256::from_slice(&result[33..65]);
    //     println!("v: {} r: {:?} s: {:?}", v, r, s);
    //     Ok(Signature::from_rsv(&r, &s, v))
    // }

    fn sign_transaction(&self, address: &Address, transaction: Self::Transaction) -> Result<Signature, Self::Error> {
        let usb = self.usb.lock();
        let devices = self.devices.read();
        let device = devices.iter().find(|d| &d.info.address == address).ok_or(Error::KeyNotFound)?;
        let handle = self.open_path(|| usb.open_path(&device.path))?;

        let eth_path = &ETH_DERIVATION_PATH_BE[..];
        let etc_path = &ETC_DERIVATION_PATH_BE[..];
        let derivation_path = match *self.key_path.read() {
            KeyPath::Ethereum => eth_path,
            KeyPath::EthereumClassic => etc_path,
        };
        let mut chunk= [0u8; MAX_CHUNK_SIZE];

        // copy the key to `our buffer`
        &mut chunk[0..derivation_path.len()].copy_from_slice(derivation_path);
        let key_length = derivation_path.len();
        let mut result = Vec::new();
        let mut remaining_bytes = transaction.len();
        let mut offset = 0;
        
        println!("remaining_bytes: {}", remaining_bytes);

        while remaining_bytes > 0 {
            let p1 = if offset == 0 { 0 } else { 0x80 };
            let p2 = 0;

            let take = min(MAX_CHUNK_SIZE - key_length, remaining_bytes);

            // Append transaction data to `our buffer`
            {
                let (_, dst) = &mut chunk.split_at_mut(key_length);
                let (dst, _) = &mut dst.split_at_mut(take);
                dst.copy_from_slice(&transaction[offset..(offset + take)]);
            }

            // println!("{:?}", &transaction[0..32]);
            // panic!("{:?} \n\n\n {:?} \n\n\n {:?} \n\n\n {:?} \n\n\n {:?} \n\n\n {:?} \n\n\n {:?} \n\n\n {:?}", &chunk[0..32], &chunk[32..64], &chunk[64..96], &chunk[96..128], &chunk[128..160], &chunk[160..192], &chunk[192..224], &chunk[224..255]);
            result = Self::send_apdu(&handle, commands::SIGN_ETH_TRANSACTION, p1, p2, &chunk[0..(key_length + take)])?;
            offset += take;
            remaining_bytes -= take;
        }

        if result.len() != 65 {
            return Err(Error::Protocol("Signature packet size mismatch"));
        }
        let v = (result[0] + 1) % 2;
        let r = H256::from_slice(&result[1..33]);
        let s = H256::from_slice(&result[33..65]);
        println!("v: {:?}, r: {:?} s: {:?}", v, r, s);
        Ok(Signature::from_rsv(&r, &s, v))
    }

	fn set_key_path(&self, key_path: KeyPath) {
		*self.key_path.write() = key_path;
	}

	fn update_devices(&self, device_direction: DeviceDirection) -> Result<usize, Self::Error> {
		let mut usb = self.usb.lock();
		usb.refresh_devices();
		let devices = usb.devices();
        let num_prev_devices = self.devices.read().len();

        let detected_devices = devices.iter()
            .filter(|&d| d.vendor_id == LEDGER_VID && LEDGER_PIDS.contains(&d.product_id))
            .fold(Vec::new(), |mut v, d| {
                match self.read_device(&usb, &d) {
				    Ok(info) => {
					    debug!(target: "hw", "Found device: {:?}", info);
					    v.push(info);
                    }
				    Err(e) => debug!(target: "hw", "Error reading device info: {}", e),
                };
                v
            });

        let num_curr_devices = detected_devices.len();
        *self.devices.write() = detected_devices;

        match device_direction {
            DeviceDirection::Arrived => {
                if num_curr_devices > num_prev_devices {
                    Ok(num_curr_devices - num_prev_devices)
                } else {
                    Err(Error::NoDeviceArrived)
                }
            }
            DeviceDirection::Left => {
                if num_prev_devices > num_curr_devices {
                    Ok(num_prev_devices- num_curr_devices)
                } else {
                    Err(Error::NoDeviceLeft)
                }
            }
        }
	}

	fn read_device(&self, usb: &hidapi::HidApi, dev_info: &hidapi::HidDeviceInfo) -> Result<Device, Self::Error> {
		let handle = self.open_path(|| usb.open_path(&dev_info.path))?;
		let manufacturer = dev_info.manufacturer_string.clone().unwrap_or_else(|| "Unknown".to_owned());
		let name = dev_info.product_string.clone().unwrap_or_else(|| "Unknown".to_owned());
		let serial = dev_info.serial_number.clone().unwrap_or_else(|| "Unknown".to_owned());
		match self.get_address(&handle) {
			Ok(Some(addr)) => {
				Ok(Device {
					path: dev_info.path.clone(),
					info: WalletInfo {
						name: name,
						manufacturer: manufacturer,
						serial: serial,
						address: addr,
					},
				})
			}
			// This variant is not possible, but the trait forces this return type
			Ok(None) => Err(Error::ImpossibleError),
			Err(e) => Err(e),
		}
	}

	fn list_devices(&self) -> Vec<WalletInfo> {
		self.devices.read().iter().map(|d| d.info.clone()).collect()
	}

	// Not used because it is not supported by Ledger
	fn list_locked_devices(&self) -> Vec<String> {
		vec![]
	}

	fn get_wallet(&self, address: &Address) -> Option<WalletInfo> {
		self.devices.read().iter().find(|d| &d.info.address == address).map(|d| d.info.clone())
	}

	fn get_address(&self, device: &hidapi::HidDevice) -> Result<Option<Address>, Self::Error> {
        trace!(target: "hw", "read_device");

		let ver = Self::send_apdu(device, commands::GET_APP_CONFIGURATION, 0, 0, &[])?;
		if ver.len() != 4 {
			return Err(Error::Protocol("Version packet size mismatch"));
		}

		let (major, minor, patch) = (ver[1], ver[2], ver[3]);
		if major < 1 || (major == 1 && minor == 0 && patch < 3) {
			return Err(Error::Protocol("App version 1.0.3 is required."));
		}

		let eth_path = &ETH_DERIVATION_PATH_BE[..];
		let etc_path = &ETC_DERIVATION_PATH_BE[..];
		let derivation_path = match *self.key_path.read() {
			KeyPath::Ethereum => eth_path,
			KeyPath::EthereumClassic => etc_path,
		};
		let key_and_address = Self::send_apdu(device, commands::GET_ETH_PUBLIC_ADDRESS, 0, 0, derivation_path)?;
		if key_and_address.len() != 107 { // 1 + 65 PK + 1 + 40 Addr (ascii-hex)
			return Err(Error::Protocol("Key packet size mismatch"));
		}
		let address_string = ::std::str::from_utf8(&key_and_address[67..107])
			.map_err(|_| Error::Protocol("Invalid address string"))?;

		let address = Address::from_str(&address_string)
			.map_err(|_| Error::Protocol("Invalid address string"))?;

		Ok(Some(address))
	}

	fn open_path<R, F>(&self, f: F) -> Result<R, Self::Error>
		where F: Fn() -> Result<R, &'static str>
	{
		f().map_err(Into::into)
	}
}

/// Ledger event handler
/// A separate thread is handling incoming events
///
/// Note, that this run to completion and race-conditions can't occur but this can
/// therefore starve other events for being process with a spinlock or similar
struct EventHandler {
	ledger: Weak<Manager>,
}

impl EventHandler {
	/// Ledger event handler constructor
	fn new(ledger: Weak<Manager>) -> Self {
		Self { ledger: ledger }
	}
}

impl libusb::Hotplug for EventHandler {
	fn device_arrived(&mut self, device: libusb::Device) {
		debug!(target: "hw", "Ledger arrived");
		if let (Some(ledger), Ok(_)) = (self.ledger.upgrade(), Manager::is_valid_ledger(&device)) {
			if try_connect_polling(ledger, &LEDGER_POLLING_INTERVAL, DeviceDirection::Arrived) != true {
				debug!(target: "hw", "Ledger connect timeout");
			}
		}
	}

	fn device_left(&mut self, device: libusb::Device) {
		debug!(target: "hw", "Ledger left");
		if let (Some(ledger), Ok(_)) = (self.ledger.upgrade(), Manager::is_valid_ledger(&device)) {
			if try_connect_polling(ledger, &LEDGER_POLLING_INTERVAL, DeviceDirection::Left) != true {
				debug!(target: "hw", "Ledger disconnect timeout");
			}
		}
	}
}


#[cfg(test)]
mod tests {
	use rustc_hex::FromHex;
    use super::*;

	#[test]
	// #[ignore]
	fn dummy() {
		let manager = Manager::new(
                Arc::new(Mutex::new(hidapi::HidApi::new().expect("HidApi"))),
                Arc::new(AtomicBool::new(false))
        ).expect("HardwareWalletManager");

		// Update device list
        manager.update_devices(DeviceDirection::Arrived).expect("No Ledger found");

		// Fetch the ethereum address of a connected ledger device
		let address = manager.list_devices()
			.iter()
			.filter(|d| d.manufacturer == "Ledger".to_string())
			.nth(0)
			.map(|d| d.address.clone())
			.expect("No ledger device detected");

        let tx = FromHex::from_hex("eb018504a817c80082520894a6ca2e6707f2cc189794a9dd459d5b05ed1bcd1c8703f26fcfb7a22480018080").unwrap();
            let signature = manager.sign_personal_message(&address, &tx);
        panic!("{:?}", signature);
	}

	/// This test can't be run without an actual ledger device connected
	#[test]
    #[ignore]
	fn smoke() {
		let manager = Manager::new(
                Arc::new(Mutex::new(hidapi::HidApi::new().expect("HidApi"))),
                Arc::new(AtomicBool::new(false))
        ).expect("HardwareWalletManager");

		// Update device list
        manager.update_devices(DeviceDirection::Arrived).expect("No Ledger found");

		// Fetch the ethereum address of a connected ledger device
		let address = manager.list_devices()
			.iter()
			.filter(|d| d.manufacturer == "Ledger".to_string())
			.nth(0)
			.map(|d| d.address.clone())
			.expect("No ledger device detected");

        let tx = FromHex::from_hex("eb018504a817c80082520894a6ca2e6707f2cc189794a9dd459d5b05ed1bcd1c8703f26fcfb7a22480018080").unwrap();
        let signature = manager.sign_transaction(&address, &tx);
        println!("Got {:?}", signature);
        assert!(signature.is_ok());


        // let large_tx = FromHex::from_hex("f8cb81968504e3b2920083024f279475b02a3c39710d6a3f2870d0d788299d48e790f180b8a4b61d27f6000000000000000000000000e1af840a5a1cb1efdf608a97aa632f4aa39ed199000000000000000000000000000000000000000000000000105ff43f46a9a800000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000018080").unwrap();
        // let signature = manager.sign_transaction(&address, &large_tx);
        // println!("Got {:?}", signature);
        // panic!("{:?}", signature);
        // assert!(signature.is_ok());


        // let huge_tx = FromHex::from_hex("f935e98201048505d21dba00833b82608080b935946103e86003908155620d2f00600455601460055560a060405260608190527f2e2e2e00000000000000000000000000000000000000000000000000000000006080908152600d805460008290527f2e2e2e00000000000000000000000000000000000000000000000000000000068255909260008051602062003474833981519152602060026001851615610100026000190190941693909304601f0192909204820192909190620000dc565b82800160010185558215620000dc579182015b82811115620000dc578251825591602001919060010190620000bf565b5b50620001009291505b80821115620000fc5760008155600101620000e6565b5090565b5050600e8054600360ff199182168117909255604080518082019091528281527f2e2e2e00000000000000000000000000000000000000000000000000000000006020918201908152600f80546000829052825160069516949094178155937f8d1108e10bcb7c27dddfc02ed9d693a074039d026cf4ea4240b40f7d581ac80260026101006001871615026000190190951694909404601f019290920483019290620001d7565b82800160010185558215620001d7579182015b82811115620001d7578251825591602001919060010190620001ba565b5b50620001fb9291505b80821115620000fc5760008155600101620000e6565b5090565b50506010805460ff19166001179055346200000057604051620034943803806200349483398101604090815281516020830151918301516060840151919390810191015b5b5b60068054600160a060020a0319166c01000000000000000000000000338102041790558151600d80546000829052909160008051602062003474833981519152602060026101006001861615026000190190941693909304601f908101849004820193870190839010620002c157805160ff1916838001178555620002f1565b82800160010185558215620002f1579182015b82811115620002f1578251825591602001919060010190620002d4565b5b50620003159291505b80821115620000fc5760008155600101620000e6565b5090565b505080600f9080519060200190828054600181600116156101000203166002900490600052602060002090601f016020900481019282601f106200036557805160ff191683800117855562000395565b8280016001018555821562000395579182015b828111156200039557825182559160200191906001019062000378565b5b50620003b99291505b80821115620000fc5760008155600101620000e6565b5090565b5050600980546c01000000000000000000000000808602819004600160a060020a0319928316179092556007805487840293909304929091169190911790555b505050505b613066806200040e6000396000f300606060405236156102035760e060020a600035046306fdde03811461034b578063095ea7b3146103c65780630b0b6d5b146103ed5780631b1ccc47146103fc57806320e870931461047757806323b872dd1461049657806325b29d84146104c057806327187991146104df578063277ccde2146104f15780632e1fbfcd14610510578063308b2fdc14610532578063313ce5671461055457806338cc48311461057757806340eddc4e146105a057806341f4793a146105bf578063467ed261146105de578063471ad963146105fd5780634e860ebb1461060f5780634efbe9331461061e57806354786b4e1461064257806354e35ba2146106bd57806358793ad4146106d25780635abedab21461073f5780635af2f8211461074e57806360483a3f1461076d57806360d12fa0146107da578063698f2e84146108035780636a749986146108155780636d5f66391461082a5780636e9c36831461083c57806370a082311461085e5780637a290fe5146108805780637e7541461461088f57806394c41bdb1461090a57806395d89b4114610929578063962a64cd146109a4578063a0b6533214610a09578063a9059cbb14610a2b578063ab62438f14610a52578063b63ca98114610aa9578063b7c54c6f14610abb578063c4e41b2214610ada578063ca7c4dba14610af9578063cb79e31b14610b18578063dd62ed3e14610b3a575b6103495b60006000600c546000141561021b57610000565b600354600c54670de0b6b3a764000091349091020204915060009050816001600030600160a060020a031681526020019081526020016000205410156102c557600160a060020a033016600090815260016020526040902054600c54909250828115610000570466038d7ea4c68000023403905033600160a060020a03166108fc829081150290604051809050600060405180830381858888f1935050505015156102c557610000565b5b600160a060020a03338116600081815260016020908152604080832080548801905530909416825283822080548790039055601380543487900301908190559154600c548551908152918201879052845190949293927f5a0391f2a67f11ed0034b68f8cf14e7e41d6f86e0a7622f2af5ea8f07b488396928290030190a45b5050565b005b3461000057610358610b5f565b60405180806020018281038252838181518152602001915080519060200190808383829060006004602084601f0104600302600f01f150905090810190601f1680156103b85780820380516001836020036101000a031916815260200191505b509250505060405180910390f35b34610000576103d9600435602435610bed565b604080519115158252519081900360200190f35b3461000057610349610c58565b005b3461000057610358610dbc565b60405180806020018281038252838181518152602001915080519060200190808383829060006004602084601f0104600302600f01f150905090810190601f1680156103b85780820380516001836020036101000a031916815260200191505b509250505060405180910390f35b3461000057610484610e5a565b60408051918252519081900360200190f35b34610000576103d9600435602435604435610ef9565b604080519115158252519081900360200190f35b3461000057610484610ff3565b60408051918252519081900360200190f35b3461000057610349600435611002565b005b346100005761048461105a565b60408051918252519081900360200190f35b3461000057610484600435611061565b60408051918252519081900360200190f35b346100005761048460043561108d565b60408051918252519081900360200190f35b34610000576105616110b9565b6040805160ff9092168252519081900360200190f35b34610000576105846110c2565b60408051600160a060020a039092168252519081900360200190f35b34610000576104846110c7565b60408051918252519081900360200190f35b34610000576104846110ce565b60408051918252519081900360200190f35b34610000576104846110d5565b60408051918252519081900360200190f35b3461000057610349600435611174565b005b34610000576103496113b5565b005b34610000576103d9600435611407565b604080519115158252519081900360200190f35b3461000057610358611549565b60405180806020018281038252838181518152602001915080519060200190808383829060006004602084601f0104600302600f01f150905090810190601f1680156103b85780820380516001836020036101000a031916815260200191505b509250505060405180910390f35b34610000576103496004356024356115e7565b005b346100005760408051602060046024803582810135601f8101859004850286018501909652858552610726958335959394604494939290920191819084018382808284375094965061167e95505050505050565b6040805192835290151560208301528051918290030190f35b3461000057610349611c13565b005b3461000057610484611d46565b60408051918252519081900360200190f35b346100005760408051602060046024803582810135601f81018590048502860185019096528585526107269583359593946044949392909201918190840183828082843750949650611d4d95505050505050565b6040805192835290151560208301528051918290030190f35b3461000057610584612303565b60408051600160a060020a039092168252519081900360200190f35b3461000057610349600435612313565b005b3461000057610349600435602435612347565b005b346100005761034960043561252f565b005b3461000057610484600435612941565b60408051918252519081900360200190f35b346100005761048460043561298d565b60408051918252519081900360200190f35b34610000576103496129ac565b005b3461000057610358612a13565b60405180806020018281038252838181518152602001915080519060200190808383829060006004602084601f0104600302600f01f150905090810190601f1680156103b85780820380516001836020036101000a031916815260200191505b509250505060405180910390f35b3461000057610484612ab1565b60408051918252519081900360200190f35b3461000057610358612ab8565b60405180806020018281038252838181518152602001915080519060200190808383829060006004602084601f0104600302600f01f150905090810190601f1680156103b85780820380516001836020036101000a031916815260200191505b509250505060405180910390f35b3461000057610484600480803590602001908201803590602001908080601f01602080910402602001604051908101604052809392919081815260200183838082843750949650612b4695505050505050565b60408051918252519081900360200190f35b3461000057610484600435612b63565b60408051918252519081900360200190f35b34610000576103d9600435602435612b8c565b604080519115158252519081900360200190f35b3461000057610349600480803590602001908201803590602001908080601f016020809104026020016040519081016040528093929190818152602001838380828437509496505093359350612c3c92505050565b005b3461000057610349600435612f38565b005b3461000057610484612f90565b60408051918252519081900360200190f35b346100005761048461300c565b60408051918252519081900360200190f35b3461000057610484613013565b60408051918252519081900360200190f35b346100005761048460043561301a565b60408051918252519081900360200190f35b3461000057610484600435602435613039565b60408051918252519081900360200190f35b600d805460408051602060026001851615610100026000190190941693909304601f81018490048402820184019092528181529291830182828015610be55780601f10610bba57610100808354040283529160200191610be5565b820191906000526020600020905b815481529060010190602001808311610bc857829003601f168201915b505050505081565b600160a060020a03338116600081815260026020908152604080832094871680845294825280832086905580518681529051929493927f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925929181900390910190a35060015b92915050565b601a54600090600160a060020a03161515610c7257610000565b600160a060020a0333166000908152600a60205260409020541515610c9657610000565b600160a060020a0333166000908152601d602052604090205460ff1615610cbc57610000565b601b54426212750090910111610cd157610000565b600160a060020a0333166000818152601d60209081526040808320805460ff19166001179055600a8252918290208054601c805490910190555482519384529083015280517f475c7605c08471fdc551a58d2c318b163628c5852f69323a1b91c34eb0bb09339281900390910190a150601154601c54606490910490604682029010610db857601a5460068054600160a060020a031916606060020a600160a060020a0393841681020417908190556040805191909216815290517f6b8184e23a898262087be50aa3ea5de648451e63f94413e810586c25282d58c2916020908290030190a15b5b50565b604080516020808201835260008252600d8054845160026001831615610100026000190190921691909104601f810184900484028201840190955284815292939091830182828015610e4f5780601f10610e2457610100808354040283529160200191610e4f565b820191906000526020600020905b815481529060010190602001808311610e3257829003601f168201915b505050505090505b90565b600f805460408051602060026001851615610100026000190190941693909304601f8101849004840282018401909252818152600093610ef39391929091830182828015610ee95780601f10610ebe57610100808354040283529160200191610ee9565b820191906000526020600020905b815481529060010190602001808311610ecc57829003601f168201915b5050505050612b46565b90505b90565b600160a060020a038316600090815260016020526040812054829010801590610f495750600160a060020a0380851660009081526002602090815260408083203390941683529290522054829010155b8015610f555750600082115b15610fe757600160a060020a03808516600081815260016020908152604080832080548890039055878516808452818420805489019055848452600283528184203390961684529482529182902080548790039055815186815291517fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9281900390910190a3506001610feb56610feb565b5060005b5b9392505050565b600160a060020a033016315b90565b60065433600160a060020a0390811691161461101d57610000565b600c8190556040805182815290517f0bbd501ef336990995d82b5e3fd82a15abe1ff10c982757a1698ac5d1c3e79579181900360200190a15b5b50565b600b545b90565b6000601882815481101561000057906000526020600020906007020160005b506004015490505b919050565b6000601882815481101561000057906000526020600020906007020160005b506001015490505b919050565b600e5460ff1681565b305b90565b6013545b90565b601c545b90565b600d805460408051602060026001851615610100026000190190941693909304601f8101849004840282018401909252818152600093610ef39391929091830182828015610ee95780601f10610ebe57610100808354040283529160200191610ee9565b820191906000526020600020905b815481529060010190602001808311610ecc57829003601f168201915b5050505050612b46565b90505b90565b600654600090819033600160a060020a0390811691161461119457610000565b60008381526014602052604090205415156111ae57610000565b60008381526014602052604090206005015433600160a060020a039081169116146111d857610000565b60008381526014602052604090206005015460a060020a900460ff16156111fe57610000565b601154600084815260146020526040902060040154606490910460370292508290111561122a57610000565b60008381526014602052604081206005015460a860020a900460ff16600181116100005714156112eb57600954600084815260146020908152604080832060058101546001909101548251840185905282517fa9059cbb000000000000000000000000000000000000000000000000000000008152600160a060020a0392831660048201526024810191909152915194169363a9059cbb93604480840194938390030190829087803b156100005760325a03f1156100005750611389915050565b60008381526014602052604080822060058101546001909101549151600160a060020a039091169282156108fc02929190818181858888f160008881526014602090815260409182902060058101546001909101548351600160a060020a0390921682529181019190915281519297507f2648a7e2f9c34700b91370233666e5118fa8be3e0c21fed4f7402b941df8efdd9650829003019350915050a15b6000838152601460205260409020600501805460a060020a60ff02191660a060020a1790555b5b505050565b60065433600160a060020a039081169116146113d057610000565b6010805460ff191690556040517fb48c7f694f0a3b9b22d7e61c60ff8aebbb107314b6b698fc489ff3f017cb57e090600090a15b5b565b600060006000600760009054906101000a9004600160a060020a0316600160a060020a031663d4884b566000604051602001526040518160e060020a028152600401809050602060405180830381600087803b156100005760325a03f115610000575050604051514210905061147c57610000565b60085433600160a060020a0390811691161461149757610000565b5050600b54600160a060020a03328181166000908152600a6020526040902080549386029384019055601180548401905560128054860190556008549092916114e39130911683610ef9565b506114ee8282612b8c565b50600054601154600b5460408051918252602082018590528051600160a060020a038716927fb4d6befef2def3d17bcb13c2b882ec4fa047f33157446d3e0e6094b2a21609ac92908290030190a4600192505b5b5050919050565b604080516020808201835260008252600f8054845160026001831615610100026000190190921691909104601f810184900484028201840190955284815292939091830182828015610e4f5780601f10610e2457610100808354040283529160200191610e4f565b820191906000526020600020905b815481529060010190602001808311610e3257829003601f168201915b505050505090505b90565b60065433600160a060020a0390811691161461160257610000565b60105460ff16151561161357610000565b600160a060020a0330166000908152600160209081526040808320805485019055600c859055825484019283905580518481529051839286927f10cb430288a1696de11938bc5362c6f8c60e58808237bce4436b93a8573e00c3929081900390910190a45b5b5b5050565b6040805161010081018252600080825260208083018290528351908101845281815292820192909252606081018290526080810182905260a0810182905260c0810182905260e08101829052600654829182918291829133600160a060020a039081169116146116ed57610000565b60115460649004935060056016541115801561170c5750836005540288115b1561171657610000565b61171e612f90565b8811156117305761172d612f90565b97505b60003642604051808484808284378201915050828152602001935050505060405180910390209250600454420191506101006040519081016040528084815260200189815260200188815260200183815260200160008152602001338152602001600081526020016000815260200150905080601460008560001916815260200190815260200160002060008201518160000155602082015181600101556040820151816002019080519060200190828054600181600116156101000203166002900490600052602060002090601f016020900481019282601f1061182057805160ff191683800117855561184d565b8280016001018555821561184d579182015b8281111561184d578251825591602001919060010190611832565b5b5061186e9291505b8082111561186a5760008155600101611856565b5090565b5050606082015160038201556080820151600482015560a08201516005909101805460c084015160e09094015160f860020a90810281900460a860020a0260a860020a60ff02199582029190910460a060020a0260a060020a60ff0219606060020a95860295909504600160a060020a031990931692909217939093161792909216179055601880546001810180835582818380158290116119c9576007028160070283600052602060002091820191016119c991905b8082111561186a5760006000820160009055600182016000905560028201805460018160011615610100020316600290046000825580601f10611968575061199a565b601f01602090049060005260206000209081019061199a91905b8082111561186a5760008155600101611856565b5090565b5b50506000600382018190556004820155600581018054600160b060020a0319169055600701611925565b5090565b5b505050916000526020600020906007020160005b83909190915060008201518160000155602082015181600101556040820151816002019080519060200190828054600181600116156101000203166002900490600052602060002090601f016020900481019282601f10611a4a57805160ff1916838001178555611a77565b82800160010185558215611a77579182015b82811115611a77578251825591602001919060010190611a5c565b5b50611a989291505b8082111561186a5760008155600101611856565b5090565b5050606082015181600301556080820151816004015560a08201518160050160006101000a815481600160a060020a030219169083606060020a90810204021790555060c08201518160050160146101000a81548160ff021916908360f860020a90810204021790555060e08201518160050160156101000a81548160ff021916908360f860020a90810204021790555050505060166000815460010191905081905550426017819055507f1a1eea7d2a0f099c2f19efb4e101fcf220558c9f4fbc6961b33fbe02d3a7be908389848a3360405180866000191681526020018581526020018481526020018060200183600160a060020a031681526020018281038252848181518152602001915080519060200190808383829060006004602084601f0104600302600f01f150905090810190601f168015611bee5780820380516001836020036101000a031916815260200191505b50965050505050505060405180910390a1826001955095505b5b505050509250929050565b60065460009033600160a060020a03908116911614611c3157610000565b600760009054906101000a9004600160a060020a0316600160a060020a031663d4884b566000604051602001526040518160e060020a028152600401809050602060405180830381600087803b156100005760325a03f1156100005750506040515162dd7c00014210159050611ca657610000565b604051600160a060020a0333811691309091163180156108fc02916000818181858888f1600954909550600160a060020a0316935063a9059cbb9250339150611cef9050612f90565b6000604051602001526040518360e060020a0281526004018083600160a060020a0316815260200182815260200192505050602060405180830381600087803b156100005760325a03f115610000575050505b5b50565b6016545b90565b6040805161010081018252600080825260208083018290528351908101845281815292820192909252606081018290526080810182905260a0810182905260c0810182905260e08101829052600654829182918291829133600160a060020a03908116911614611dbc57610000565b60105460ff1615611dcc57610000565b6000611dd73061298d565b1115611de257610000565b6017546212750001421015611df657610000565b6013546064900493508360055402881115611e1057610000565b30600160a060020a031631881115611e305730600160a060020a03163197505b60003642604051808484808284378201915050828152602001935050505060405180910390209250600454420191506101006040519081016040528084815260200189815260200188815260200183815260200160008152602001338152602001600081526020016001815260200150905080601460008560001916815260200190815260200160002060008201518160000155602082015181600101556040820151816002019080519060200190828054600181600116156101000203166002900490600052602060002090601f016020900481019282601f10611f2057805160ff1916838001178555611f4d565b82800160010185558215611f4d579182015b82811115611f4d578251825591602001919060010190611f32565b5b50611f6e9291505b8082111561186a5760008155600101611856565b5090565b5050606082015160038201556080820151600482015560a08201516005909101805460c084015160e09094015160f860020a90810281900460a860020a0260a860020a60ff02199582029190910460a060020a0260a060020a60ff0219606060020a95860295909504600160a060020a031990931692909217939093161792909216179055601880546001810180835582818380158290116120c9576007028160070283600052602060002091820191016120c991905b8082111561186a5760006000820160009055600182016000905560028201805460018160011615610100020316600290046000825580601f10612068575061209a565b601f01602090049060005260206000209081019061209a91905b8082111561186a5760008155600101611856565b5090565b5b50506000600382018190556004820155600581018054600160b060020a0319169055600701612025565b5090565b5b505050916000526020600020906007020160005b83909190915060008201518160000155602082015181600101556040820151816002019080519060200190828054600181600116156101000203166002900490600052602060002090601f016020900481019282601f1061214a57805160ff1916838001178555612177565b82800160010185558215612177579182015b8281111561217757825182559160200191906001019061215c565b5b506121989291505b8082111561186a5760008155600101611856565b5090565b5050606082015181600301556080820151816004015560a08201518160050160006101000a815481600160a060020a030219169083606060020a90810204021790555060c08201518160050160146101000a81548160ff021916908360f860020a90810204021790555060e08201518160050160156101000a81548160ff021916908360f860020a908102040217905550505050426017819055507f1a1eea7d2a0f099c2f19efb4e101fcf220558c9f4fbc6961b33fbe02d3a7be908389848a3360405180866000191681526020018581526020018481526020018060200183600160a060020a031681526020018281038252848181518152602001915080519060200190808383829060006004602084601f0104600302600f01f150905090810190601f168015611bee5780820380516001836020036101000a031916815260200191505b50965050505050505060405180910390a1826001955095505b5b505050509250929050565b600654600160a060020a03165b90565b600854600160a060020a03161561232957610000565b60088054600160a060020a031916606060020a838102041790555b50565b60065433600160a060020a0390811691161461236257610000565b60105460ff16151561237357610000565b600760009054906101000a9004600160a060020a0316600160a060020a031663d4884b566000604051602001526040518160e060020a028152600401809050602060405180830381600087803b156100005760325a03f11561000057505060405151421090506123e257610000565b600760009054906101000a9004600160a060020a0316600160a060020a031663cdd933326000604051602001526040518160e060020a028152600401809050602060405180830381600087803b156100005760325a03f11561000057505060405151421015905061245257610000565b600854600160a060020a0316151561246957610000565b6000805482018155600160a060020a03308116808352600160209081526040808520805487019055600b8790556002825280852060088054861687529083529481902080548701905593548451868152945193169391927f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b9259281900390910190a3600054601154600b546040805185815290517f10cb430288a1696de11938bc5362c6f8c60e58808237bce4436b93a8573e00c39181900360200190a45b5b5b5b5b5050565b604080516101008082018352600080835260208084018290528451808201865282815284860152606084018290526080840182905260a0840182905260c0840182905260e0840182905285825260148152848220855180850187528154815260018083015482850152600280840180548a51600019948216159099029390930190921604601f8101859004850287018501895280875296979496879692959394938601938301828280156126245780601f106125f957610100808354040283529160200191612624565b820191906000526020600020905b81548152906001019060200180831161260757829003601f168201915b505050918352505060038201546020820152600482015460408201526005820154600160a060020a038116606083015260ff60a060020a820481161515608084015260a09092019160a860020a909104166001811161000057905250600085815260146020526040902054909350151561269d57610000565b60008481526014602052604090206005015460a060020a900460ff16156126c357610000565b60008481526014602052604090206003015442106126e057610000565b6000848152601460209081526040808320600160a060020a033316845260060190915290205460ff161561271357610000565b600160a060020a0333166000818152600a6020908152604080832054888452601483528184206004810180548301905594845260069094019091529020805460ff19166001179055915061276684612941565b6000858152601460205260409020601880549293509091839081101561000057906000526020600020906007020160005b50600082015481600001556001820154816001015560028201816002019080546001816001161561010002031660029004828054600181600116156101000203166002900490600052602060002090601f016020900481019282601f10612801578054855561283d565b8280016001018555821561283d57600052602060002091601f016020900482015b8281111561283d578254825591600101919060010190612822565b5b5061285e9291505b8082111561186a5760008155600101611856565b5090565b5050600382810154908201556004808301549082015560059182018054929091018054600160a060020a031916606060020a600160a060020a0394851681020417808255825460a060020a60ff021990911660f860020a60a060020a9283900460ff908116820282900490930291909117808455935460a860020a60ff021990941660a860020a9485900490921681020490920291909117905560408051868152339092166020830152818101849052517f8f8bbb8c1937f844f6a094cd4c6eeab8ed5e36f83952e6306ffb2c11fffe5bce916060908290030190a15b50505050565b6000805b60185481101561298657601881815481101561000057906000526020600020906007020160005b505483141561297d57809150612986565b5b600101612945565b5b50919050565b600160a060020a0381166000908152600160205260409020545b919050565b60065433600160a060020a039081169116146129c757610000565b600160a060020a03301660009081526001602052604080822080548354038355829055517fe0987873419fe09d3c9a3e0267f4daf163e812d512f867abaf6bf9822f141a8b9190a15b5b565b60408051602080820183526000825260198054845160026001831615610100026000190190921691909104601f810184900484028201840190955284815292939091830182828015610e4f5780601f10610e2457610100808354040283529160200191610e4f565b820191906000526020600020905b815481529060010190602001808311610e3257829003601f168201915b505050505090505b90565b6011545b90565b600f805460408051602060026001851615610100026000190190941693909304601f81018490048402820184019092528181529291830182828015610be55780601f10610bba57610100808354040283529160200191610be5565b820191906000526020600020905b815481529060010190602001808311610bc857829003601f168201915b505050505081565b6000602082511115612b5757610000565b5060208101515b919050565b6000601882815481101561000057906000526020600020906007020160005b505490505b919050565b600160a060020a033316600090815260016020526040812054829010801590612bb55750600082115b15612c2d57600160a060020a03338116600081815260016020908152604080832080548890039055938716808352918490208054870190558351868152935191937fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef929081900390910190a3506001610c5256610c52565b506000610c52565b5b92915050565b600160a060020a0333166000908152600a60205260409020541515612c6057610000565b600760009054906101000a9004600160a060020a0316600160a060020a031663d4884b566000604051602001526040518160e060020a028152600401809050602060405180830381600087803b156100005760325a03f11561000057505060405151626ebe00014210159050612cd557610000565b601b5415801590612cef5750426019600201546212750001115b15612cf957610000565b6040805160808101825283815260208082018490524262127500018284015233600160a060020a03166000908152600a8252928320546060830152815180516019805495819052939484937f944998273e477b495144fb8794c914197f3ccb46be2900f4698fd0ef743c969560026001841615610100026000190190931692909204601f90810182900483019490910190839010612da257805160ff1916838001178555612dcf565b82800160010185558215612dcf579182015b82811115612dcf578251825591602001919060010190612db4565b5b50612df09291505b8082111561186a5760008155600101611856565b5090565b505060208201518160010160006101000a815481600160a060020a030219169083606060020a908102040217905550604082015181600201556060820151816003015590505060016019600401600033600160a060020a0316815260200190815260200160002060006101000a81548160ff021916908360f860020a9081020402179055507f854a9cc4d907d23cd8dcc72af48dc0e6a87e6f76376a309a0ffa3231ce8e13363383426212750001846040518085600160a060020a031681526020018060200184815260200183600160a060020a031681526020018281038252858181518152602001915080519060200190808383829060006004602084601f0104600302600f01f150905090810190601f168015612f235780820380516001836020036101000a031916815260200191505b509550505050505060405180910390a15b5050565b60065433600160a060020a03908116911614612f5357610000565b600b819055600080546011546040519192909184917f17a7f53ef43da32c3936b4ac2b060caff5c4b03cd24b1c8e96a191eb1ec48d1591a45b5b50565b6000600960009054906101000a9004600160a060020a0316600160a060020a03166370a08231306000604051602001526040518260e060020a0281526004018082600160a060020a03168152602001915050602060405180830381600087803b156100005760325a03f115610000575050604051519150505b90565b6000545b90565b600c545b90565b600160a060020a0381166000908152600a60205260409020545b919050565b600160a060020a038083166000908152600260209081526040808320938516835292905220545b9291505056d7b6990105719101dabeb77144f2a3385c8033acd3af97e9423a695e81ad1eb500000000000000000000000069381683bde924cef65f1c97f7c8fb769a20409300000000000000000000000014f37b574242d366558db61f3335289a5035c506000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000018546573742074657374657220746573746573742063616d700000000000000000000000000000000000000000000000000000000000000000000000000000000354455300000000000000000000000000000000000000000000000000000000001ba033230fce515bea9de982fe85e0ec8fe892984bc3070ad633daab20eb370864d5a05deb41870e2117197b84cb71110fc0508fa7457165cb8cb82cb8d4d801e6e3f1").unwrap();
        // let signature = manager.sign_transaction(&address, &huge_tx);
        // println!("Got {:?}", signature);
        // assert!(signature.is_ok());
	}
}
