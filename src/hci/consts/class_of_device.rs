use std::fmt::{Debug, Formatter};
use bitflags::bitflags;
use num_enum::{FromPrimitive, IntoPrimitive};
use crate::utils::DebugFn;

/// Class of Device ([Assigned Numbers] Section 2.8).
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct ClassOfDevice {
    pub major_service_classes: MajorServiceClasses,
    pub major_device_classes: MajorDeviceClass,
    pub minor_device_classes: u8,
}

impl Debug for ClassOfDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClassOfDevice")
            .field("major_service_classes", &DebugFn(|f| {
                bitflags::parser::to_writer(&self.major_service_classes, f)
            }))
            .field("major_device_classes", &self.major_device_classes)
            .field("minor_device_classes", &self.minor_device_classes)
            .finish()
    }
}

impl From<u32> for ClassOfDevice {
    fn from(value: u32) -> Self {
        Self {
            major_service_classes: MajorServiceClasses::from_bits_truncate((value >> 13) as u16),
            major_device_classes: MajorDeviceClass::from(((value >> 8) & 0x1F) as u8),
            minor_device_classes: ((value >> 7) & 0xFE) as u8,
        }
    }
}

bitflags! {

    /// Major Service Classes ([Assigned Numbers] Section 2.8.1).
    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    pub struct MajorServiceClasses: u16 {
        const LimitedDiscoverableMode = 0x0001;
        const LeAudio = 0x0002;
        const Positioning = 0x0008;
        const Networking = 0x0010;
        const Rendering = 0x0020;
        const Capturing = 0x0040;
        const ObjectTransfer = 0x0080;
        const Audio = 0x0100;
        const Telephony = 0x0200;
        const Information = 0x0400;
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, IntoPrimitive, FromPrimitive)]
#[repr(u8)]
pub enum MajorDeviceClass {
    Miscellaneous = 0x00,
    Computer = 0x01,
    Phone = 0x02,
    LanAccessPoint = 0x03,
    AudioVideo = 0x04,
    Peripheral = 0x05,
    Imaging = 0x06,
    Wearable = 0x07,
    Toy = 0x08,
    Health = 0x09,
    #[num_enum(default)]
    Uncategorized = 0x1F,
}

//TODO create enums for all the minor classes