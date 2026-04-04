//! Cyphal/CAN transport via the gs_usb / candleLight USB protocol.
//!
//! This driver implements the GS-USB protocol used by the candlelight firmware
//! (CANable, Canable-M, and compatible adapters).  It works on all platforms
//! – Linux, macOS, and Windows – via the `rusb` crate (libusb).
//!
//! # Linux note
//! If the `gs_usb` kernel module is loaded on Linux, it will claim the device
//! and this driver will fail to open it.  You must either:
//!   - Unload the module (`sudo rmmod gs_usb`) before using this driver, or
//!   - Use the SocketCAN driver instead (which relies on the kernel module).
//!
//! # macOS note
//! No additional drivers are needed.  `libusb` (installed via
//! `brew install libusb`) provides direct access to the USB device.

use std::fmt;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use canadensis::core::transfer::{MessageTransfer, ServiceTransfer};
use canadensis::node::{BasicNode, CoreNode};
use canadensis::requester::TransferIdFixedMap;
use canadensis::{Node, ServiceToken, TransferHandler};
use canadensis_can::driver::{ReceiveDriver, TransmitDriver};
use canadensis_can::queue::{ArrayQueue, SingleQueueDriver};
use canadensis_can::{
    CanNodeId, CanReceiver, CanTransmitter, CanTransport, Error as CanError, Mtu,
};
use canadensis_core::nb;
use canadensis_core::subscription::Subscription;
use canadensis_core::time::{Clock, milliseconds};
use canadensis_core::{OutOfMemoryError, Priority};
use canadensis_data_types::uavcan::node::get_info_1_0::{
    GetInfoRequest, GetInfoResponse, SERVICE as GET_INFO_SERVICE,
};
use canadensis_data_types::uavcan::node::heartbeat_1_0::{Heartbeat, SUBJECT as HEARTBEAT_SUBJECT};
use canadensis_encoding::Deserialize;
use rusb::{Device, DeviceHandle, Direction, GlobalContext, TransferType, UsbContext};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::config::CanConfig;
use crate::transport::{
    GetInfoResult, PendingGetInfo, PnpGetInfoPending, SystemClock, TransportCommand,
    TransportHandle, WorkerState,
};

use canadensis_data_types::uavcan::pnp::node_id_allocation_data_1_0::{
    NodeIDAllocationData as AllocationDataV1, SUBJECT as PNP_V1_SUBJECT,
};
use canadensis_data_types::uavcan::pnp::node_id_allocation_data_2_0::{
    NodeIDAllocationData as AllocationDataV2, SUBJECT as PNP_V2_SUBJECT,
};

use crate::config::effective_pnp_db_path;
use crate::pnp::PnpAllocator;

// ─── GS-USB protocol constants ───────────────────────────────────────────────

const USB_REQ_OUT_VENDOR_DEVICE: u8 = 0x40;
const USB_REQ_IN_VENDOR_DEVICE: u8 = 0xC0;
const USB_REQ_OUT_VENDOR_INTERFACE: u8 = 0x41;
const USB_REQ_IN_VENDOR_INTERFACE: u8 = 0xC1;

const GS_USB_BREQ_HOST_FORMAT: u8 = 0;
const GS_USB_BREQ_SET_BITTIMING: u8 = 1;
const GS_USB_BREQ_SET_MODE: u8 = 2;
const GS_USB_BREQ_BT_CONST: u8 = 4;
const GS_USB_BREQ_DEVICE_CONFIG: u8 = 5;

const GS_CAN_MODE_RESET: u32 = 0;
const GS_CAN_MODE_START: u32 = 1;

const GS_CAN_EFF_FLAG: u32 = 0x8000_0000; // extended frame format (29-bit ID)
const GS_CAN_RTR_FLAG: u32 = 0x4000_0000; // remote transmission request
const GS_CAN_ERR_FLAG: u32 = 0x2000_0000; // error frame

const GS_HOST_FRAME_SIZE: usize = 20; // echo_id(4) + can_id(4) + dlc(1) + channel(1) + flags(1) + pad(1) + data(8)

const BULK_IN_ENDPOINT: u8 = 0x81;
const BULK_OUT_ENDPOINT: u8 = 0x01;
const BULK_OUT_ENDPOINT_ALT: u8 = 0x02;
const GS_USB_HOST_CONFIG_BYTE_ORDER: u32 = 0x0000_beef;

/// Known VID/PID pairs for candleLight-compatible adapters.
const CANDLELIGHT_DEVICES: &[(u16, u16)] = &[
    (0x1d50, 0x606f), // CANable / candleLight firmware
    (0x1209, 0x2323), // candleLight firmware (pid.codes)
    (0x1209, 0xca01), // CANnectivity
    (0x1cd2, 0x606f), // CES CANext FD
    (0x16d0, 0x10b8), // ABE CANdebugger FD
    (0x16d0, 0x0f30), // Xylanta SAINT3
    (0x1d50, 0x5740), // CANable Pro
    (0x04d8, 0x0053), // Microchip CAN Bus Analyzer
];

// ─── Protocol structures ─────────────────────────────────────────────────────

#[repr(C, packed)]
#[derive(Copy, Clone)]
struct GsDeviceBtConst {
    feature: u32,
    fclk_can: u32,
    tseg1_min: u32,
    tseg1_max: u32,
    tseg2_min: u32,
    tseg2_max: u32,
    sjw_max: u32,
    brp_min: u32,
    brp_max: u32,
    brp_inc: u32,
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
struct GsDeviceBittiming {
    prop_seg: u32,
    phase_seg1: u32,
    phase_seg2: u32,
    sjw: u32,
    brp: u32,
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
struct GsDeviceMode {
    mode: u32,
    flags: u32,
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
struct GsHostConfig {
    byte_order: u32,
}

#[repr(C, packed)]
#[derive(Copy, Clone, Default)]
struct GsDeviceConfig {
    reserved1: u8,
    reserved2: u8,
    reserved3: u8,
    icount: u8,
    sw_version: u32,
    hw_version: u32,
}

#[derive(Copy, Clone)]
struct GsUsbInterfaceInfo {
    interface_number: u8,
    bulk_in_endpoint: u8,
    bulk_out_endpoint: u8,
}

// ─── GsUsbDriver ─────────────────────────────────────────────────────────────

/// Error type for the gs_usb driver.
#[derive(Debug)]
pub enum GsUsbError {
    Usb(rusb::Error),
    InvalidFrame,
    UnsupportedBitrate(u32),
    ProtocolStep {
        step: &'static str,
        source: rusb::Error,
    },
}

impl fmt::Display for GsUsbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GsUsbError::Usb(e) => write!(f, "USB error: {e}"),
            GsUsbError::InvalidFrame => write!(f, "Invalid CAN frame"),
            GsUsbError::UnsupportedBitrate(br) => write!(f, "Unsupported bitrate: {br}"),
            GsUsbError::ProtocolStep { step, source } => {
                write!(f, "USB error during {step}: {source}")
            }
        }
    }
}

impl From<rusb::Error> for GsUsbError {
    fn from(e: rusb::Error) -> Self {
        GsUsbError::Usb(e)
    }
}

/// Userspace CAN driver for gs_usb / candleLight devices.
pub struct GsUsbDriver {
    handle: DeviceHandle<GlobalContext>,
    channel: u8,
    interface_number: u8,
    bulk_in_endpoint: u8,
    bulk_out_endpoint: u8,
    echo_counter: u32,
}

impl GsUsbDriver {
    /// Open a gs_usb device and configure it for the requested bitrate.
    ///
    /// `index` selects which matching device to open (0 = first).
    /// Optional `vid`/`pid` narrow the search to a specific product.
    pub fn open(
        index: u32,
        vid: Option<u16>,
        pid: Option<u16>,
        bitrate: u32,
    ) -> Result<Self, GsUsbError> {
        let context = GlobalContext::default();
        let device_list = context.devices()?;

        let mut candidates = Vec::new();
        for device in device_list.iter() {
            let desc = match device.device_descriptor() {
                Ok(d) => d,
                Err(_) => continue,
            };
            let dev_vid = desc.vendor_id();
            let dev_pid = desc.product_id();

            let matches = if let (Some(v), Some(p)) = (vid, pid) {
                dev_vid == v && dev_pid == p
            } else if let Some(v) = vid {
                dev_vid == v
            } else {
                CANDLELIGHT_DEVICES
                    .iter()
                    .any(|&(v, p)| v == dev_vid && p == dev_pid)
            };

            if matches {
                candidates.push(device);
            }
        }

        let device = candidates
            .into_iter()
            .nth(index as usize)
            .ok_or(rusb::Error::NotFound)?;

        let interface = find_gsusb_interface(&device)?;

        let handle = device.open()?;

        // Detach any existing kernel driver (Linux only – no-op on macOS).
        #[cfg(target_os = "linux")]
        {
            if handle.kernel_driver_active(0).unwrap_or(false) {
                handle.detach_kernel_driver(0)?;
            }
        }

        handle.claim_interface(interface.interface_number)?;

        let mut driver = GsUsbDriver {
            handle,
            channel: 0,
            interface_number: interface.interface_number,
            bulk_in_endpoint: interface.bulk_in_endpoint,
            bulk_out_endpoint: interface.bulk_out_endpoint,
            echo_counter: 0,
        };

        driver.clear_halt_conditions()?;

        driver
            .configure_host_format()
            .map_err(|source| GsUsbError::ProtocolStep {
                step: "host format negotiation",
                source: source.into_usb_error(),
            })?;
        let device_config =
            driver
                .read_device_config()
                .map_err(|source| GsUsbError::ProtocolStep {
                    step: "device configuration read",
                    source: source.into_usb_error(),
                })?;
        let channel_count = device_config.icount.saturating_add(1);
        let sw_version = device_config.sw_version;
        let hw_version = device_config.hw_version;
        debug!(
            interface = driver.interface_number,
            channels = channel_count,
            sw_version,
            hw_version,
            "Opened gs_usb device configuration"
        );

        // Query device capabilities and compute bittiming.
        let bt_const = driver
            .read_bt_const()
            .map_err(|source| GsUsbError::ProtocolStep {
                step: "bit timing constants read",
                source: source.into_usb_error(),
            })?;
        driver
            .set_mode(GS_CAN_MODE_RESET)
            .map_err(|source| GsUsbError::ProtocolStep {
                step: "reset mode configuration",
                source: source.into_usb_error(),
            })?;
        let timing =
            compute_bittiming(bitrate, &bt_const).ok_or(GsUsbError::UnsupportedBitrate(bitrate))?;
        driver
            .set_bittiming(&timing)
            .map_err(|source| GsUsbError::ProtocolStep {
                step: "bit timing configuration",
                source: source.into_usb_error(),
            })?;

        // Put the device into normal operating mode.
        driver
            .set_mode(GS_CAN_MODE_START)
            .map_err(|source| GsUsbError::ProtocolStep {
                step: "normal mode configuration",
                source: source.into_usb_error(),
            })?;

        info!("Opened gs_usb CAN adapter, bitrate={} bit/s", bitrate);
        Ok(driver)
    }

    fn clear_halt_conditions(&mut self) -> Result<(), GsUsbError> {
        self.handle.clear_halt(self.bulk_in_endpoint)?;
        self.handle.clear_halt(self.bulk_out_endpoint)?;
        Ok(())
    }

    fn write_control_candidates(
        &self,
        request: u8,
        candidates: &[(u8, u16, u16)],
        data: &[u8],
    ) -> Result<(), GsUsbError> {
        let timeout = Duration::from_millis(500);
        let mut last_error = None;

        for &(request_type, value, index) in candidates {
            match self
                .handle
                .write_control(request_type, request, value, index, data, timeout)
            {
                Ok(_) => return Ok(()),
                Err(rusb::Error::Pipe) => last_error = Some(rusb::Error::Pipe),
                Err(error) => return Err(error.into()),
            }
        }

        Err(last_error.unwrap_or(rusb::Error::Other).into())
    }

    fn read_control_candidates<const N: usize>(
        &self,
        request: u8,
        candidates: &[(u8, u16, u16)],
    ) -> Result<[u8; N], GsUsbError> {
        let timeout = Duration::from_millis(500);
        let mut last_error = None;

        for &(request_type, value, index) in candidates {
            let mut buf = [0u8; N];
            match self
                .handle
                .read_control(request_type, request, value, index, &mut buf, timeout)
            {
                Ok(n) if n >= N => return Ok(buf),
                Ok(_) => return Err(rusb::Error::Other.into()),
                Err(rusb::Error::Pipe) => last_error = Some(rusb::Error::Pipe),
                Err(error) => return Err(error.into()),
            }
        }

        Err(last_error.unwrap_or(rusb::Error::Other).into())
    }

    fn configure_host_format(&mut self) -> Result<(), GsUsbError> {
        let host_config = GsHostConfig {
            byte_order: GS_USB_HOST_CONFIG_BYTE_ORDER,
        };
        let buf = host_config.byte_order.to_le_bytes();
        self.write_control_candidates(
            GS_USB_BREQ_HOST_FORMAT,
            &[
                (USB_REQ_OUT_VENDOR_INTERFACE, 0, 0),
                (
                    USB_REQ_OUT_VENDOR_INTERFACE,
                    0,
                    self.interface_number as u16,
                ),
                (USB_REQ_OUT_VENDOR_DEVICE, 0, self.interface_number as u16),
                (USB_REQ_OUT_VENDOR_DEVICE, 0, 0),
            ],
            &buf,
        )?;
        Ok(())
    }

    fn read_device_config(&self) -> Result<GsDeviceConfig, GsUsbError> {
        let buf = self.read_control_candidates::<{ std::mem::size_of::<GsDeviceConfig>() }>(
            GS_USB_BREQ_DEVICE_CONFIG,
            &[
                (USB_REQ_IN_VENDOR_INTERFACE, 0, 0),
                (USB_REQ_IN_VENDOR_INTERFACE, 0, self.interface_number as u16),
                (USB_REQ_IN_VENDOR_DEVICE, 0, self.interface_number as u16),
                (USB_REQ_IN_VENDOR_DEVICE, 0, 0),
            ],
        )?;
        Ok(GsDeviceConfig {
            reserved1: buf[0],
            reserved2: buf[1],
            reserved3: buf[2],
            icount: buf[3],
            sw_version: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            hw_version: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
        })
    }

    fn read_bt_const(&self) -> Result<GsDeviceBtConst, GsUsbError> {
        let buf = self.read_control_candidates::<{ std::mem::size_of::<GsDeviceBtConst>() }>(
            GS_USB_BREQ_BT_CONST,
            &[
                (USB_REQ_IN_VENDOR_INTERFACE, self.channel as u16, 0),
                (USB_REQ_IN_VENDOR_DEVICE, self.channel as u16, 0),
                (USB_REQ_IN_VENDOR_DEVICE, 0, self.channel as u16),
                (USB_REQ_IN_VENDOR_INTERFACE, 0, self.channel as u16),
            ],
        )?;
        // Safety: we own `buf` and all bit patterns are valid for this struct.
        Ok(unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GsDeviceBtConst) })
    }

    fn set_bittiming(&self, timing: &GsDeviceBittiming) -> Result<(), GsUsbError> {
        let buf: [u8; std::mem::size_of::<GsDeviceBittiming>()] =
            unsafe { std::mem::transmute(*timing) };
        self.write_control_candidates(
            GS_USB_BREQ_SET_BITTIMING,
            &[
                (USB_REQ_OUT_VENDOR_INTERFACE, self.channel as u16, 0),
                (USB_REQ_OUT_VENDOR_DEVICE, self.channel as u16, 0),
                (USB_REQ_OUT_VENDOR_DEVICE, 0, self.channel as u16),
                (USB_REQ_OUT_VENDOR_INTERFACE, 0, self.channel as u16),
            ],
            &buf,
        )?;
        Ok(())
    }

    fn set_mode(&self, mode: u32) -> Result<(), GsUsbError> {
        let gs_mode = GsDeviceMode { mode, flags: 0 };
        let buf: [u8; std::mem::size_of::<GsDeviceMode>()] =
            unsafe { std::mem::transmute(gs_mode) };
        self.write_control_candidates(
            GS_USB_BREQ_SET_MODE,
            &[
                (USB_REQ_OUT_VENDOR_INTERFACE, self.channel as u16, 0),
                (USB_REQ_OUT_VENDOR_DEVICE, self.channel as u16, 0),
                (USB_REQ_OUT_VENDOR_DEVICE, 0, self.channel as u16),
                (USB_REQ_OUT_VENDOR_INTERFACE, 0, self.channel as u16),
            ],
            &buf,
        )?;
        Ok(())
    }

    fn encode_frame(frame: &canadensis_can::Frame, echo_id: u32) -> [u8; GS_HOST_FRAME_SIZE] {
        let mut buf = [0u8; GS_HOST_FRAME_SIZE];
        // echo_id (little-endian)
        buf[0..4].copy_from_slice(&echo_id.to_le_bytes());
        // can_id (little-endian) with EFF flag set (Cyphal always uses extended IDs)
        let raw_id: u32 = frame.id().into();
        let can_id = raw_id | GS_CAN_EFF_FLAG;
        buf[4..8].copy_from_slice(&can_id.to_le_bytes());
        // dlc
        let data = frame.data();
        buf[8] = data.len() as u8;
        // channel, flags, reserved
        buf[9] = 0;
        buf[10] = 0;
        buf[11] = 0;
        // data (up to 8 bytes)
        let len = data.len().min(8);
        buf[12..12 + len].copy_from_slice(&data[..len]);
        buf
    }

    fn decode_frame(
        buf: &[u8],
        now: canadensis_core::time::Microseconds32,
    ) -> Option<canadensis_can::Frame> {
        if buf.len() < GS_HOST_FRAME_SIZE {
            return None;
        }
        let can_id = u32::from_le_bytes(buf[4..8].try_into().ok()?);
        // Skip RTR and error frames; require EFF.
        if can_id & GS_CAN_RTR_FLAG != 0 || can_id & GS_CAN_ERR_FLAG != 0 {
            return None;
        }
        // Cyphal always uses extended frames.
        if can_id & GS_CAN_EFF_FLAG == 0 {
            return None;
        }
        let raw_id = can_id & 0x1FFF_FFFF;
        let dlc = buf[8] as usize;
        let len = dlc.min(8);
        let data = &buf[12..12 + len];
        let cyphal_id = raw_id.try_into().ok()?;
        Some(canadensis_can::Frame::new(now, cyphal_id, data))
    }
}

impl GsUsbError {
    fn into_usb_error(self) -> rusb::Error {
        match self {
            GsUsbError::Usb(error) => error,
            GsUsbError::ProtocolStep { source, .. } => source,
            _ => rusb::Error::Other,
        }
    }
}

impl Drop for GsUsbDriver {
    fn drop(&mut self) {
        let _ = self.set_mode(GS_CAN_MODE_RESET);
        let _ = self.handle.release_interface(self.interface_number);
    }
}

impl TransmitDriver<SystemClock> for GsUsbDriver {
    type Error = GsUsbError;

    fn try_reserve(&mut self, _frames: usize) -> Result<(), OutOfMemoryError> {
        Ok(())
    }

    fn transmit(
        &mut self,
        frame: canadensis_can::Frame,
        clock: &mut SystemClock,
    ) -> nb::Result<Option<canadensis_can::Frame>, GsUsbError> {
        let now = clock.now();
        if frame.timestamp() < now {
            warn!("Dropping gs_usb frame that has missed its deadline");
            return Ok(None);
        }
        let echo_id = self.echo_counter;
        self.echo_counter = self.echo_counter.wrapping_add(1);
        let buf = Self::encode_frame(&frame, echo_id);
        match self
            .handle
            .write_bulk(self.bulk_out_endpoint, &buf, Duration::from_millis(100))
        {
            Ok(_) => Ok(None),
            Err(rusb::Error::Timeout) => Err(nb::Error::WouldBlock),
            Err(e) => Err(nb::Error::Other(e.into())),
        }
    }

    fn flush(&mut self, _clock: &mut SystemClock) -> nb::Result<(), GsUsbError> {
        Ok(())
    }
}

impl ReceiveDriver<SystemClock> for GsUsbDriver {
    type Error = GsUsbError;

    fn receive(
        &mut self,
        clock: &mut SystemClock,
    ) -> nb::Result<canadensis_can::Frame, GsUsbError> {
        let mut buf = [0u8; GS_HOST_FRAME_SIZE];
        match self
            .handle
            .read_bulk(self.bulk_in_endpoint, &mut buf, Duration::from_millis(1))
        {
            Ok(_) => {
                let now = clock.now();
                Self::decode_frame(&buf, now).ok_or(nb::Error::Other(GsUsbError::InvalidFrame))
            }
            Err(rusb::Error::Timeout) => Err(nb::Error::WouldBlock),
            Err(e) => Err(nb::Error::Other(e.into())),
        }
    }

    fn apply_filters<S>(&mut self, _local_node: Option<CanNodeId>, _subscriptions: S)
    where
        S: IntoIterator<Item = Subscription>,
    {
        // Hardware filtering is not supported by the gs_usb protocol;
        // the device always delivers all received frames.
    }

    fn apply_accept_all(&mut self) {
        // Already accepting all frames; nothing to do.
    }
}

fn find_gsusb_interface(device: &Device<GlobalContext>) -> Result<GsUsbInterfaceInfo, GsUsbError> {
    let config = device.active_config_descriptor()?;

    // Prefer vendor-specific interfaces with expected GS-USB endpoints.
    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            if descriptor.class_code() != 0xff {
                continue;
            }

            let mut bulk_in_endpoint = None;
            let mut bulk_out_endpoint = None;

            for endpoint in descriptor.endpoint_descriptors() {
                if endpoint.transfer_type() != TransferType::Bulk {
                    continue;
                }

                match endpoint.direction() {
                    Direction::In if endpoint.address() == BULK_IN_ENDPOINT => {
                        bulk_in_endpoint = Some(endpoint.address())
                    }
                    Direction::Out
                        if endpoint.address() == BULK_OUT_ENDPOINT
                            || endpoint.address() == BULK_OUT_ENDPOINT_ALT =>
                    {
                        bulk_out_endpoint = Some(endpoint.address())
                    }
                    _ => {}
                }
            }

            if let (Some(bulk_in_endpoint), Some(bulk_out_endpoint)) =
                (bulk_in_endpoint, bulk_out_endpoint)
            {
                return Ok(GsUsbInterfaceInfo {
                    interface_number: descriptor.interface_number(),
                    bulk_in_endpoint,
                    bulk_out_endpoint,
                });
            }
        }
    }

    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            let mut bulk_in_endpoint = None;
            let mut bulk_out_endpoint = None;

            for endpoint in descriptor.endpoint_descriptors() {
                if endpoint.transfer_type() != TransferType::Bulk {
                    continue;
                }

                match endpoint.direction() {
                    Direction::In => bulk_in_endpoint = Some(endpoint.address()),
                    Direction::Out => bulk_out_endpoint = Some(endpoint.address()),
                }
            }

            if let (Some(bulk_in_endpoint), Some(bulk_out_endpoint)) =
                (bulk_in_endpoint, bulk_out_endpoint)
            {
                return Ok(GsUsbInterfaceInfo {
                    interface_number: descriptor.interface_number(),
                    bulk_in_endpoint,
                    bulk_out_endpoint,
                });
            }
        }
    }

    Ok(GsUsbInterfaceInfo {
        interface_number: 0,
        bulk_in_endpoint: BULK_IN_ENDPOINT,
        bulk_out_endpoint: BULK_OUT_ENDPOINT_ALT,
    })
}

// ─── Bittiming calculation ────────────────────────────────────────────────────

fn compute_bittiming(bitrate: u32, bt: &GsDeviceBtConst) -> Option<GsDeviceBittiming> {
    // Search for BRP and segment lengths that satisfy:
    //   bitrate = fclk_can / (brp * (1 + tseg1 + tseg2))
    // where tseg1 = prop_seg + phase_seg1.
    let fclk = bt.fclk_can;
    for tseg1 in (bt.tseg1_min..=bt.tseg1_max).rev() {
        for tseg2 in (bt.tseg2_min..=bt.tseg2_max).rev() {
            let total_tq = 1 + tseg1 + tseg2;
            if total_tq == 0 {
                continue;
            }
            let brp_exact = fclk / bitrate / total_tq;
            if brp_exact == 0 {
                continue;
            }
            // Round to a multiple of brp_inc
            let inc = bt.brp_inc.max(1);
            let brp = ((brp_exact + inc / 2) / inc) * inc;
            if brp < bt.brp_min || brp > bt.brp_max {
                continue;
            }
            let actual = fclk / brp / total_tq;
            if actual == bitrate {
                let sjw = 1u32.min(bt.sjw_max);
                // Split tseg1 evenly between prop_seg and phase_seg1
                let phase_seg1 = tseg1 / 2;
                let prop_seg = tseg1 - phase_seg1;
                return Some(GsDeviceBittiming {
                    prop_seg,
                    phase_seg1,
                    phase_seg2: tseg2,
                    sjw,
                    brp,
                });
            }
        }
    }
    None
}

// ─── Canadensis node type aliases ─────────────────────────────────────────────

const QUEUE_CAPACITY: usize = 1210;
const NUM_PUBLISHERS: usize = 8;
const NUM_REQUESTERS: usize = 4;
const NUM_TRANSFER_IDS: usize = 4;

type GsUsbQueue = SingleQueueDriver<SystemClock, ArrayQueue<QUEUE_CAPACITY>, GsUsbDriver>;

type GsUsbNode = BasicNode<
    CoreNode<
        SystemClock,
        CanTransmitter<SystemClock, GsUsbQueue>,
        CanReceiver<SystemClock, GsUsbQueue>,
        TransferIdFixedMap<CanTransport, NUM_TRANSFER_IDS>,
        GsUsbQueue,
        NUM_PUBLISHERS,
        NUM_REQUESTERS,
    >,
>;

// ─── Worker ───────────────────────────────────────────────────────────────────

/// Start a gs_usb CAN transport worker and return its handle.
pub fn start(cfg: CanConfig, shutdown: watch::Receiver<bool>) -> TransportHandle {
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<TransportCommand>(16);

    let name = cfg.name.clone();
    let name_clone = name.clone();
    let local_node_id = u16::from(cfg.node_id);

    tokio::task::spawn_blocking(move || {
        run_worker(cfg, cmd_rx, shutdown);
    });

    TransportHandle {
        name: name_clone,
        local_node_id: Some(local_node_id),
        cmd_tx,
    }
}

fn run_worker(
    cfg: CanConfig,
    cmd_rx: mpsc::Receiver<TransportCommand>,
    shutdown: watch::Receiver<bool>,
) {
    let name = cfg.name.clone();

    // Parse optional VID/PID overrides.
    let vid = cfg.vid.as_deref().and_then(parse_hex_u16);
    let pid = cfg.pid.as_deref().and_then(parse_hex_u16);

    // Open the USB device.
    let driver = match GsUsbDriver::open(cfg.usb_index, vid, pid, cfg.bitrate) {
        Ok(d) => d,
        Err(e) => {
            error!(transport = %name, "Failed to open gs_usb device: {e}");
            return;
        }
    };

    // Build the Canadensis node.
    let node_id = match CanNodeId::try_from(cfg.node_id) {
        Ok(id) => id,
        Err(_) => {
            error!(transport = %name, "Invalid node ID {}", cfg.node_id);
            return;
        }
    };

    let queue_driver = GsUsbQueue::new(ArrayQueue::new(), driver);
    let transmitter = CanTransmitter::new(Mtu::Can8);
    let receiver = CanReceiver::new(node_id);
    let core_node = CoreNode::new(
        SystemClock::new(),
        node_id,
        transmitter,
        receiver,
        queue_driver,
    );

    let node_info = crate::transport::make_node_info(&name);
    let mut node = match BasicNode::new(core_node, node_info) {
        Ok(n) => n,
        Err(e) => {
            error!(transport = %name, "Failed to create Cyphal node: {e:?}");
            return;
        }
    };

    // Subscribe to heartbeats from other nodes.
    if let Err(e) = node.subscribe_message(HEARTBEAT_SUBJECT, 7, milliseconds(2000)) {
        warn!(transport = %name, "Failed to subscribe to Heartbeat: {e:?}");
    }

    // Acquire the GetInfo service token for issuing requests to other nodes.
    let get_info_token: Option<ServiceToken<GetInfoRequest>> = node
        .start_sending_requests::<GetInfoRequest>(
            GET_INFO_SERVICE,
            milliseconds(3000),
            432,
            Priority::Nominal,
        )
        .map_err(|e| warn!(transport = %name, "Failed to start GetInfo requester: {e:?}"))
        .ok();

    let mut state = WorkerState::default();
    state.register_local_node(u16::from(cfg.node_id));
    let mut last_second = Instant::now();

    // ─── PNP allocator ────────────────────────────────────────────────────────
    let mut pnp_allocator: Option<PnpAllocator> = if cfg.pnp_enabled {
        let db_path = effective_pnp_db_path(&name, cfg.pnp_db_path.as_deref());
        match crate::pnp::AllocationDb::open(&db_path) {
            Ok(db) => {
                if let Err(e) = node.subscribe_message(PNP_V2_SUBJECT, 18, milliseconds(5000)) {
                    warn!(transport = %name, "Failed to subscribe to PNP v2: {e:?}");
                }
                if let Err(e) = node.subscribe_message(PNP_V1_SUBJECT, 9, milliseconds(5000)) {
                    warn!(transport = %name, "Failed to subscribe to PNP v1: {e:?}");
                }
                if let Err(e) =
                    node.start_publishing(PNP_V2_SUBJECT, milliseconds(500), Priority::Nominal)
                {
                    warn!(transport = %name, "Failed to start PNP v2 publisher: {e:?}");
                }
                if let Err(e) =
                    node.start_publishing(PNP_V1_SUBJECT, milliseconds(500), Priority::Nominal)
                {
                    warn!(transport = %name, "Failed to start PNP v1 publisher: {e:?}");
                }
                info!(transport = %name, db = %db_path.display(), "PNP allocator started");
                Some(PnpAllocator::new(db, u16::from(cfg.node_id), 127))
            }
            Err(e) => {
                warn!(transport = %name, "Failed to open PNP DB {}: {e}", db_path.display());
                None
            }
        }
    } else {
        None
    };

    info!(transport = %name, "gs_usb CAN worker started");

    loop {
        // Check for shutdown signal (non-blocking).
        if shutdown.has_changed().unwrap_or(true) && *shutdown.borrow() {
            break;
        }

        // Process pending commands.
        while let Ok(cmd) = cmd_rx.try_recv() {
            handle_command(
                cmd,
                &mut node,
                &get_info_token,
                &mut state,
                &name,
                cfg.node_id,
                pnp_allocator.as_ref(),
            );
        }

        // Check for timed-out GetInfo requests.
        state.check_get_info_timeout();

        // Dispatch next PNP-triggered GetInfo if none is in flight.
        if state.pnp_pending_get_info.is_none()
            && let Some(pnp) = pnp_allocator.as_mut()
            && let Some(node_id) = pnp.pop_get_info_request()
        {
            if node_id <= 127 {
                if let (Some(token), Ok(target)) =
                    (&get_info_token, CanNodeId::try_from(node_id as u8))
                {
                    match node.send_request(token, &GetInfoRequest {}, target) {
                        Ok(_) => {
                            state.on_get_info_request_sent();
                            state.pnp_pending_get_info = Some(PnpGetInfoPending {
                                node_id,
                                deadline: Instant::now() + Duration::from_secs(5),
                            });
                            debug!(transport = %name, node_id, "PNP GetInfo request sent");
                        }
                        Err(e) => {
                            debug!(transport = %name, node_id, "PNP GetInfo send failed: {e:?}");
                            pnp.on_get_info_result(node_id, None);
                        }
                    }
                } else {
                    pnp.on_get_info_result(node_id, None);
                }
            } else {
                pnp.on_get_info_result(node_id, None);
            }
        }

        // Report PNP GetInfo timeouts back to the allocator.
        if let Some(timed_out) = state.check_pnp_get_info_timeout()
            && let Some(pnp) = pnp_allocator.as_mut()
        {
            pnp.on_get_info_result(timed_out, None);
        }

        // Poll for incoming CAN frames.
        let mut handler = GsUsbHandler {
            state: &mut state,
            pnp: pnp_allocator.as_mut(),
        };
        match node.receive(&mut handler) {
            Ok(_) => {}
            Err(CanError::Driver(GsUsbError::Usb(rusb::Error::Timeout)))
            | Err(CanError::Driver(GsUsbError::Usb(rusb::Error::NoDevice))) => {}
            Err(e) => {
                state.record_error(format!("Receive error: {e:?}"));
                warn!(transport = %name, "Receive error: {e:?}");
            }
        }

        // Run per-second tasks.
        if last_second.elapsed() >= Duration::from_secs(1) {
            last_second = Instant::now();
            state.tick_local_node(u16::from(cfg.node_id));
            if let Err(e) = node.run_per_second_tasks() {
                debug!(transport = %name, "Per-second task error: {e:?}");
            }
            if let Err(e) = node.flush() {
                debug!(transport = %name, "Flush error: {e:?}");
            }
        }
    }

    info!(transport = %name, "gs_usb CAN worker stopped");
}

fn handle_command(
    cmd: TransportCommand,
    node: &mut GsUsbNode,
    token: &Option<ServiceToken<GetInfoRequest>>,
    state: &mut WorkerState,
    _name: &str,
    local_node_id: u8,
    pnp: Option<&PnpAllocator>,
) {
    match cmd {
        TransportCommand::ListNodes { reply } => {
            let _ = reply.send(state.node_list());
        }
        TransportCommand::Diagnostics { reply } => {
            let mut diag = state.diagnostics(Some(u16::from(local_node_id)));
            diag.pnp_enabled = pnp.is_some();
            let _ = reply.send(diag);
        }
        TransportCommand::GetInfo { node_id, reply } => {
            if node_id > 127 {
                let _ = reply.send(Err(format!(
                    "CAN node ID {node_id} is out of range (0–127)"
                )));
                return;
            }
            let target = match CanNodeId::try_from(node_id as u8) {
                Ok(id) => id,
                Err(_) => {
                    let _ = reply.send(Err("Invalid node ID".into()));
                    return;
                }
            };
            if let Some(token) = token {
                match node.send_request(token, &GetInfoRequest {}, target) {
                    Ok(_) => {
                        state.on_get_info_request_sent();
                        state.pending_get_info = Some(PendingGetInfo {
                            node_id,
                            reply,
                            deadline: Instant::now() + Duration::from_secs(3),
                        });
                    }
                    Err(e) => {
                        state.record_error(format!("GetInfo request send failed: {e:?}"));
                        let _ = reply.send(Err(format!("Failed to send GetInfo request: {e:?}")));
                    }
                }
            } else {
                state.record_error("GetInfo requester not available");
                let _ = reply.send(Err("GetInfo requester not available".into()));
            }
        }
        TransportCommand::ListAllocations { reply } => {
            let entries = pnp.map(|a| a.list_allocations()).unwrap_or_default();
            let _ = reply.send(entries);
        }
    }
}

// ─── Transfer handler ─────────────────────────────────────────────────────────

struct GsUsbHandler<'a> {
    state: &'a mut WorkerState,
    pnp: Option<&'a mut PnpAllocator>,
}

impl<'a> TransferHandler<CanTransport> for GsUsbHandler<'a> {
    fn handle_message<N: Node<Transport = CanTransport>>(
        &mut self,
        node: &mut N,
        transfer: &MessageTransfer<Vec<u8>, CanTransport>,
    ) -> bool {
        let source = transfer.header.source.map(u16::from);

        if transfer.header.subject == HEARTBEAT_SUBJECT {
            let Ok(hb) = Heartbeat::deserialize_from_bytes(&transfer.payload) else {
                return false;
            };
            let node_id: u16 = source.unwrap_or(0xFFFF);
            if let Some(pnp) = &mut self.pnp {
                pnp.on_heartbeat(node_id);
            }
            self.state
                .on_heartbeat(node_id, hb.uptime, hb.health.value, hb.mode.value);
            return true;
        }

        if let Some(pnp) = &mut self.pnp {
            if transfer.header.subject == PNP_V2_SUBJECT {
                if let Ok(msg) = AllocationDataV2::deserialize_from_bytes(&transfer.payload)
                    && let Some(response) = pnp.on_allocation_v2(&msg, source)
                {
                    let _ = node.publish(PNP_V2_SUBJECT, &response);
                    self.state.on_pnp_allocated();
                }
                return true;
            }

            if transfer.header.subject == PNP_V1_SUBJECT {
                if let Ok(msg) = AllocationDataV1::deserialize_from_bytes(&transfer.payload)
                    && let Some(response) = pnp.on_allocation_v1(&msg, source)
                {
                    let _ = node.publish(PNP_V1_SUBJECT, &response);
                    self.state.on_pnp_allocated();
                }
                return true;
            }
        }

        false
    }

    fn handle_response<N: Node<Transport = CanTransport>>(
        &mut self,
        _node: &mut N,
        transfer: &ServiceTransfer<Vec<u8>, CanTransport>,
    ) -> bool {
        if transfer.header.service != GET_INFO_SERVICE {
            return false;
        }

        let source_node_id = u16::from(transfer.header.source);

        let matches_cli = self
            .state
            .pending_get_info
            .as_ref()
            .is_some_and(|p| p.node_id == source_node_id);
        let matches_pnp = self
            .state
            .pnp_pending_get_info
            .as_ref()
            .is_some_and(|p| p.node_id == source_node_id);

        if !matches_cli && !matches_pnp {
            return false;
        }

        let parsed = GetInfoResponse::deserialize_from_bytes(&transfer.payload);

        if matches_cli {
            let result = match &parsed {
                Ok(resp) => Ok(GetInfoResult {
                    node_id: source_node_id,
                    name: String::from_utf8_lossy(&resp.name).into_owned(),
                    hardware_version: (resp.hardware_version.major, resp.hardware_version.minor),
                    software_version: (resp.software_version.major, resp.software_version.minor),
                    unique_id: resp.unique_id,
                }),
                Err(_) => Err("Failed to parse GetInfo response".into()),
            };
            self.state.on_get_info_response(result);
        }

        if matches_pnp {
            self.state.pnp_pending_get_info = None;
            self.state.get_info_responses_received =
                self.state.get_info_responses_received.saturating_add(1);
            let uid = parsed.as_ref().map(|r| r.unique_id).ok();
            if let Some(pnp) = &mut self.pnp {
                pnp.on_get_info_result(source_node_id, uid);
            }
        }

        self.state.on_message_received();
        true
    }

    fn handle_request<N: Node<Transport = CanTransport>>(
        &mut self,
        _node: &mut N,
        _token: canadensis::ResponseToken<CanTransport>,
        _transfer: &ServiceTransfer<Vec<u8>, CanTransport>,
    ) -> bool {
        false
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn parse_hex_u16(s: &str) -> Option<u16> {
    let s = s.trim();
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u16::from_str_radix(s, 16).ok()
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        // Build a simple Cyphal CAN frame.
        use canadensis_core::time::Microseconds32;
        // Subject 7509 (Heartbeat), anonymous, not a service
        // A valid Cyphal extended ID for a message: priority=4, subject=7509, source=42
        // Encode as: (priority << 26) | subject_id << 8 | source_node_id | 0 (not service)
        // priority=4 (Nominal), subject_id=7509, source=42
        let raw_id: u32 = (4 << 26) | (7509 << 8) | 42;
        let cyphal_id = canadensis_can::CanId::try_from(raw_id).unwrap();
        let data = [0x01u8, 0x02, 0x03, 0x04, 0xe0];
        let frame = canadensis_can::Frame::new(Microseconds32::from_ticks(1000), cyphal_id, &data);

        let encoded = GsUsbDriver::encode_frame(&frame, 42);

        // Check echo_id
        assert_eq!(u32::from_le_bytes(encoded[0..4].try_into().unwrap()), 42);

        // can_id should have EFF flag set
        let can_id = u32::from_le_bytes(encoded[4..8].try_into().unwrap());
        assert!(can_id & GS_CAN_EFF_FLAG != 0);
        assert_eq!(can_id & 0x1FFF_FFFF, raw_id);

        // dlc
        assert_eq!(encoded[8], data.len() as u8);

        // Decode
        let decoded = GsUsbDriver::decode_frame(&encoded, Microseconds32::from_ticks(2000));
        let decoded = decoded.expect("decode should succeed");
        assert_eq!(decoded.data(), data);
    }

    #[test]
    fn test_parse_hex_u16() {
        assert_eq!(parse_hex_u16("0x1d50"), Some(0x1d50));
        assert_eq!(parse_hex_u16("0X606F"), Some(0x606f));
        assert_eq!(parse_hex_u16("606f"), Some(0x606f));
        assert_eq!(parse_hex_u16("gggg"), None);
    }

    #[test]
    fn test_compute_bittiming_1mbit() {
        // Simulate a 48 MHz candleLight device.
        let bt = GsDeviceBtConst {
            feature: 0,
            fclk_can: 48_000_000,
            tseg1_min: 1,
            tseg1_max: 16,
            tseg2_min: 1,
            tseg2_max: 8,
            sjw_max: 4,
            brp_min: 1,
            brp_max: 512,
            brp_inc: 1,
        };
        let timing = compute_bittiming(1_000_000, &bt).expect("should find bittiming for 1 Mbit/s");
        let actual = bt.fclk_can
            / timing.brp
            / (1 + timing.prop_seg + timing.phase_seg1 + timing.phase_seg2);
        assert_eq!(actual, 1_000_000);
    }
}
